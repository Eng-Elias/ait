//! The agent conversation panel (Spec R06 §5.6 / issue #85): the
//! right-side split `Alt+A` toggles on the active tab.
//!
//! The panel is a widget, not a loop — the model↔tools work lives in
//! `ait-agent` behind the [`AgentSession`] channel handle
//! (CODING_STANDARDS.md §9: the TUI holds no business logic). What
//! lives here is exactly what the frontend owns:
//!
//! - the **conversation scrollback** ([`Entry`]) that
//!   [`AgentEvent`]s accumulate into — `TextDelta`s append to the
//!   newest assistant entry; tool calls land as tool cards
//!   ([`Entry::Tool`], status-transitioned by `ToolUpdate`); an
//!   `ApprovalRequest` parks the gate on the approve card until
//!   [`decide`](AgentPanel::decide) answers it; an `AskRequest` —
//!   `ask_user_question`, gate-exempt (us-10) — parks the ask card
//!   until [`submit_answer`](AgentPanel::submit_answer) sends the
//!   user's choice or free-text reply;
//!   `Done`/`Cancelled`/`BudgetReached`/`Error` are terminal and
//!   leave a status line.
//! - the **input line** typing lands in while the panel is focused —
//!   a plain append/pop buffer whose cursor sits at the end. It is
//!   app-side text, never PTY-bound, so no `InputShadow` and no
//!   sanitizer apply (the never-execute boundary governs shell input;
//!   the panel's text only ever reaches the request channel).
//! - the **focus/toggle state**: `Alt+A` opens focused, refocuses an
//!   unfocused panel, and closes a focused one; plain Esc cancels a
//!   running task or — while idle — returns focus to the terminal.
//!   Closing hides the split without ending the conversation: a task
//!   keeps running and keeps accumulating events until it finishes
//!   or the tab dies (the `Tab` drop drops the session handle, which
//!   cancels and joins the worker).
//! - the **paint** ([`paint_agent_panel`]): a `│` separator on the
//!   shared edge, a title row reversed while focused, the wrapped
//!   conversation and the input line at the bottom.

use std::cell::Cell;
use std::time::Instant;

use ait_agent::{
    AgentContext, AgentEvent, AgentRequest, AgentSession, ApprovalVerdict, PlanTodo,
    PlanTodoStatus, ToolStatus, TranscriptItem,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};

use crate::status::Toast;
use crate::suggest;
use crate::tabs::Tab;
use crate::transcript::Transcript;

/// The input line's prompt marker — user text (a submitted task and a
/// mid-run steering message alike) carries the same marker in the
/// scrollback.
const INPUT_PROMPT: &str = "› ";

/// [`INPUT_PROMPT`]'s cell width — `›` is three bytes but one column;
/// input-line math works in cells, never `str::len`.
const PROMPT_WIDTH: usize = 2;

/// The pinned plan block's row cap — header, visible todos and the
/// trailing separator together (Spec R08-T5). A longer plan folds
/// into a `… N more` row rather than crowding the scrollback out of
/// the panel.
const PLAN_BLOCK_MAX: usize = 10;

/// One conversation entry — what the panel's scrollback is made of.
/// Each variant owns its display shape in [`display_lines`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Entry {
    /// A submitted task — the user's words, verbatim.
    Task(String),
    /// A steering message queued while a task ran — also the user's
    /// words (Spec R06: enqueued mid-run, injected at the next model
    /// turn, never mid-stream).
    Steering(String),
    /// Assistant text — `TextDelta`s accumulate onto the newest entry
    /// of this kind; concatenating it yields one model turn's reply.
    Text(String),
    /// A tool card (Spec R06 us-5): name, argument summary and the
    /// status its `ToolUpdate`s transition through.
    Tool(ToolCard),
    /// The `ask_user_question` card (us-10): question, choices and
    /// the answer — its own interactive shape, exempt from the
    /// approval gate because the card IS the ask.
    Ask(AskCard),
    /// A terminal outcome line: the task finished, the user cancelled,
    /// the step budget tripped, or a failure message.
    Status(String),
}

/// A tool card's status — `ait_agent::ToolStatus` plus
/// `AwaitingApproval`, the frontend-only state a gated call sits in
/// between its `ApprovalRequest` and the verdict's `ToolUpdate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardStatus {
    /// Suspended on the approve card — no execution yet.
    AwaitingApproval,
    /// Executing (a fresh `ToolCall`, or an allow verdict landed).
    Running,
    /// Executed; `detail` previews the result.
    Done,
    /// The user denied it; `detail` is the denial text.
    Denied,
    /// The tool reported an error; `detail` is the error.
    Failed,
}

/// One call's card in the scrollback — built from the event's
/// `call`/`summary` (the panel never parses tool arguments, standard
/// 9), transitioned by matching `ToolUpdate`s on `id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolCard {
    /// The provider call id `ToolUpdate`s address.
    pub(crate) id: String,
    /// The registry name.
    pub(crate) name: String,
    /// The renderable argument summary.
    pub(crate) summary: String,
    /// Where the call stands.
    pub(crate) status: CardStatus,
    /// The one-line result preview or error/denial text.
    pub(crate) detail: Option<String>,
}

/// The `ask_user_question` card — built from an
/// [`AgentEvent::AskRequest`] (question plus choices-or-free-text,
/// already parsed worker-side), resolved by
/// [`submit_answer`](AgentPanel::submit_answer) which records the
/// user's reply in `answer`. `selected` highlights the option the
/// ↑/↓ keys move over while the card waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AskCard {
    /// The provider call id `ToolUpdate`s address.
    pub(crate) id: String,
    /// The question the model asked.
    pub(crate) question: String,
    /// The offered choices; empty means a free-text answer.
    pub(crate) choices: Vec<String>,
    /// The highlighted choice while the card waits.
    pub(crate) selected: usize,
    /// The user's answer — `Some` once the card is resolved.
    pub(crate) answer: Option<String>,
}

/// The approve card's state — which call waits on a verdict and what
/// the card titles it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingApproval {
    /// The call the worker is parked on.
    call_id: String,
    /// Its tool name — the card's title.
    name: String,
    /// Whether `d` switched the input row into denial-reason mode:
    /// every key then edits the reason (verdict letters type like any
    /// other) and Enter answers `Deny` — the reason can hold `a`/`t`/
    /// `d` ("don't" is typable, Spec R06 us-8).
    denying: bool,
}

/// The ask card's parked state — which call the worker waits on an
/// answer for. Everything else lives on the [`Entry::Ask`] card
/// itself (question, choices, highlighted selection).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingAsk {
    /// The call the worker is parked on.
    call_id: String,
}

/// A tab's agent conversation panel (Spec R06 us: per-tab ownership).
/// Created lazily by the first `Alt+A` and owned by its `Tab` — the
/// `Tab` drop drops the [`AgentSession`], which is what cancels and
/// joins the worker when a tab closes.
///
/// `scrollback` state deserves a note: [`Entry`]s are append-only and
/// the paint pass owns the wrap math (it is the only place that knows
/// the width), so the view position is kept as a plain absolute line
/// index the paint clamps each frame — appended lines can never shift
/// what the user is reading, and no growth bookkeeping is needed (the
/// `ScrollState::on_output` problem does not exist for append-only
/// content).
pub(crate) struct AgentPanel {
    /// The channel handle owning the agent loop on its worker thread
    /// (Spec R06-T1) — `submit`/`steer`/`cancel`/`events`; Drop cancels
    /// and joins, which is what makes closing the tab kill the agent
    /// cleanly.
    session: AgentSession,
    /// The conversation scrollback, oldest first — what
    /// [`display_lines`] flattens for the paint.
    entries: Vec<Entry>,
    /// The input line's text — an append/pop buffer; the cursor sits
    /// at its end (no mid-line editing this round).
    input: String,
    /// Whether the split is open — `false` after `Alt+A` closes it;
    /// the conversation and any running task live on either way.
    open: bool,
    /// Whether keystrokes route to `input` — opening focuses, plain
    /// Esc while idle unfocuses, `Alt+A` refocuses.
    focused: bool,
    /// A task is in flight: set by [`submit`](Self::submit), released
    /// by the terminal `AgentEvent`. Enter steers while `true` and
    /// submits while `false`; Esc cancels while `true` and unfocuses
    /// while `false`.
    running: bool,
    /// The call the gate is parked on, if any — `a`/`t`/`d` on an
    /// empty input answer it or open its denial-reason mode
    /// ([`decide`](Self::decide)), and the approve card row renders
    /// above the input line.
    pending: Option<PendingApproval>,
    /// The call the ask card is parked on, if any — ↑/↓ + Enter
    /// answer it ([`submit_answer`](Self::submit_answer)) and its
    /// prompt row renders above the input line.
    pending_ask: Option<PendingAsk>,
    /// Whether the conversation view follows the tail — `false` once
    /// the user wheel-scrolls up; re-pinned by a wheel-down that
    /// lands on the bottom or by a fresh submit/steer.
    pinned: bool,
    /// The top display line while unpinned — an absolute index into
    /// the wrapped line list, so appended content never shifts the
    /// view. `Cell` because the paint path clamps it against each
    /// frame's height.
    top: Cell<usize>,
    /// What the last paint actually showed — `(top line, max legal
    /// top)` — the wheel handlers' bounds; `Cell` for the same
    /// reason.
    painted: Cell<(usize, usize)>,
    /// A mutation counter folded into `app`'s per-frame snapshot: any
    /// change the paint can see — an edit, an event, a focus change —
    /// bumps it, so a stale frame can never survive one.
    revision: u64,
    /// The persisted-vocabulary mirror (Spec R08-T3): every applied
    /// event and every successful send folds into the accumulated
    /// [`TranscriptItem`](ait_agent::TranscriptItem)s — the store's
    /// append source and the `prior` continuity record alike.
    transcript: Transcript,
    /// The newest `update_plan` snapshot (Spec R08-T5, us-14) — what
    /// the pinned block under the title renders. Each `PlanUpdated`
    /// replaces it wholesale; an empty list clears the block.
    plan: Vec<PlanTodo>,
}

impl AgentPanel {
    /// A fresh panel, opened and focused — what `Alt+A` leaves behind
    /// on a tab that had none.
    pub(crate) fn new() -> Self {
        Self::with_session(AgentSession::new())
    }

    /// The transcript items accumulated since the last take — the
    /// store's per-drain batch (Spec R08-T3; the
    /// [`take_persisted_blocks`](Tab::take_persisted_blocks)
    /// precedent: the take drains regardless of a live store).
    pub(crate) fn take_transcript_items(&mut self) -> Vec<ait_agent::TranscriptItem> {
        self.transcript.take_new()
    }

    /// The whole accumulated transcript — the items a resumed tab's
    /// next `AgentRequest.prior` hands the worker for in-tab
    /// continuity (R08-T4).
    pub(crate) fn transcript_items(&self) -> &[ait_agent::TranscriptItem] {
        self.transcript.items()
    }

    /// [`new`](Self::new) over a caller's session — the seam tests
    /// inject a `with_secret_store` handle through (the
    /// `CommandSuggester::with_secret_store` precedent).
    pub(crate) fn with_session(session: AgentSession) -> Self {
        Self {
            session,
            entries: Vec::new(),
            input: String::new(),
            open: true,
            focused: true,
            running: false,
            pending: None,
            pending_ask: None,
            pinned: true,
            top: Cell::new(0),
            painted: Cell::new((0, 0)),
            revision: 0,
            transcript: Transcript::default(),
            plan: Vec::new(),
        }
    }

    /// The current plan snapshot — what the pinned block paints
    /// (Spec R08-T5); empty while no `update_plan` has landed.
    #[allow(dead_code)] // the paint path + tests and T6's restore read this
    pub(crate) fn plan(&self) -> &[PlanTodo] {
        &self.plan
    }

    /// Whether the split is open — drives the layout and the tab's
    /// winsize derivation.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// Whether keystrokes route to the input line — implies `open`.
    /// The `suggestion_here`-style gate `app`'s pass-through arm
    /// checks before PTY input.
    pub(crate) fn is_focused(&self) -> bool {
        self.open && self.focused
    }

    /// Whether a task is in flight — Enter steers vs. submits, Esc
    /// cancels vs. unfocuses, and the title row's hint reads it.
    pub(crate) fn is_running(&self) -> bool {
        self.running
    }

    /// Whether the gate is parked on a call — `a`/`t`/`d` verdicts
    /// apply while `true` (the approve card shows), and the paint
    /// reserves its row.
    pub(crate) fn awaiting_verdict(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether the parked approve card is in denial-reason mode —
    /// every key edits the input (verdict letters type like any
    /// other) and Enter answers `Deny` (us-8). Set by `d` on an
    /// empty input ([`begin_deny_reason`](Self::begin_deny_reason)).
    pub(crate) fn deny_reason_active(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.denying)
    }

    /// `d` on the approve card with an empty input: the input row
    /// becomes the denial-reason prompt — the verdict keys stop
    /// firing so the reason itself can contain them.
    pub(crate) fn begin_deny_reason(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.denying = true;
            self.revision += 1;
        }
    }

    /// Whether the input line is empty — the verdict keys only fire
    /// on an empty buffer, so typed text can never be mistaken for a
    /// verdict and a verdict never swallows a draft.
    pub(crate) fn input_is_empty(&self) -> bool {
        self.input.is_empty()
    }

    /// Whether an `ask_user_question` card waits on an answer — the
    /// ask keys (↑/↓ + Enter for choices, typed input for free text)
    /// apply while `true`, and the paint reserves its prompt row.
    pub(crate) fn awaiting_answer(&self) -> bool {
        self.pending_ask.is_some()
    }

    /// Whether the pending ask offers choices — `false` means the
    /// card takes a free-text answer from the input line.
    pub(crate) fn ask_has_choices(&self) -> bool {
        self.pending_ask_card()
            .is_some_and(|card| !card.choices.is_empty())
    }

    /// Answer the pending approval (Spec R06's three-way card):
    /// `call_id` rides along so the worker ignores a verdict meant
    /// for a call it already resolved. A dead worker toasts — the
    /// task is ending anyway, so the card clears either way.
    pub(crate) fn decide(&mut self, verdict: ApprovalVerdict, toast: &mut Option<Toast>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if let Err(error) = self.session.approve(&pending.call_id, verdict) {
            *toast = Some(Toast::new(format!("{error}"), Instant::now()));
        }
        self.revision += 1;
    }

    /// The ask card awaiting an answer, if any — the choices and the
    /// highlighted selection live on the [`Entry::Ask`] itself.
    fn pending_ask_card(&self) -> Option<&AskCard> {
        let pending = self.pending_ask.as_ref()?;
        self.entries.iter().rev().find_map(|entry| match entry {
            Entry::Ask(card) if card.id == pending.call_id => Some(card),
            _ => None,
        })
    }

    /// The mutable half of [`pending_ask_card`].
    fn pending_ask_card_mut(&mut self) -> Option<&mut AskCard> {
        let pending = self.pending_ask.as_ref()?;
        self.entries.iter_mut().rev().find_map(|entry| match entry {
            Entry::Ask(card) if card.id == pending.call_id => Some(card),
            _ => None,
        })
    }

    /// ↑ on a choices ask — the highlight wraps to the tail from the
    /// top. A no-op with no card parked or nothing to choose.
    pub(crate) fn ask_prev(&mut self) {
        let Some(card) = self.pending_ask_card_mut() else {
            return;
        };
        if card.choices.is_empty() {
            return;
        }
        card.selected = card
            .selected
            .checked_sub(1)
            .unwrap_or(card.choices.len() - 1);
        self.revision += 1;
    }

    /// ↓ on a choices ask — the highlight wraps back to the top.
    pub(crate) fn ask_next(&mut self) {
        let Some(card) = self.pending_ask_card_mut() else {
            return;
        };
        if card.choices.is_empty() {
            return;
        }
        card.selected = (card.selected + 1) % card.choices.len();
        self.revision += 1;
    }

    /// Answer the pending ask (Spec R06 us-10): the highlighted
    /// choice, or — a free-text ask — the input line's text, sent on
    /// [`AgentSession::answer`] so it becomes the call's tool result.
    /// `call_id` rides along so the worker ignores an answer meant
    /// for a call it already resolved. An empty free-text line is a
    /// no-op — the card stays parked — and a dead worker toasts like
    /// every other send.
    pub(crate) fn submit_answer(&mut self, toast: &mut Option<Toast>) {
        let Some(pending) = self.pending_ask.as_ref() else {
            return;
        };
        let call_id = pending.call_id.clone();
        let answer = match self.pending_ask_card() {
            Some(card) if !card.choices.is_empty() => {
                card.choices.get(card.selected).cloned().unwrap_or_default()
            }
            _ => {
                let text = self.take_input().trim_end().to_string();
                if text.is_empty() {
                    return;
                }
                text
            }
        };
        self.pending_ask = None;
        match self.session.answer(&call_id, answer.clone()) {
            Ok(()) => {
                self.pinned = true;
                self.transcript.answer(&answer);
                if let Some(card) = self.entries.iter_mut().rev().find_map(|entry| match entry {
                    Entry::Ask(card) if card.id == call_id => Some(card),
                    _ => None,
                }) {
                    card.answer = Some(answer);
                }
            }
            Err(error) => *toast = Some(Toast::new(format!("{error}"), Instant::now())),
        }
        self.revision += 1;
    }

    /// The mutation counter `app`'s frame snapshot compares.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// The conversation entries, oldest first.
    #[allow(dead_code)] // the app tests and R06-T3's cards read this
    pub(crate) fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The input line's current text.
    #[cfg(test)]
    pub(crate) fn input(&self) -> &str {
        &self.input
    }

    /// Whether the view follows the conversation's tail — `false`
    /// after the user wheel-scrolls up.
    #[cfg(test)]
    pub(crate) fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// `Alt+A` on a live panel: open-but-unfocused refocuses;
    /// open-and-focused closes (focus dies with the split). The
    /// conversation — and any running task — survives a close.
    pub(crate) fn toggle(&mut self) {
        if !self.open {
            self.open = true;
            self.focused = true;
        } else if !self.focused {
            self.focused = true;
        } else {
            self.open = false;
            self.focused = false;
        }
        self.revision += 1;
    }

    /// Plain Esc while focused: a running task cancels (the terminal
    /// `Cancelled` event lands on the next drain and releases
    /// `running`); an idle one returns focus to the terminal while
    /// the split stays open.
    pub(crate) fn escape(&mut self) {
        if self.running {
            self.session.cancel();
        } else {
            self.focused = false;
        }
        self.revision += 1;
    }

    /// Append one typed character — plain and SHIFT-modified chars
    /// only reach here; everything else is swallowed upstream.
    pub(crate) fn push_input(&mut self, c: char) {
        self.input.push(c);
        self.revision += 1;
    }

    /// Backspace — pops one char, a no-op on the empty input.
    pub(crate) fn pop_input(&mut self) {
        self.input.pop();
        self.revision += 1;
    }

    /// Take the input line — the Enter path's consume-and-clear.
    pub(crate) fn take_input(&mut self) -> String {
        self.revision += 1;
        std::mem::take(&mut self.input)
    }

    /// Submit `request` to the agent loop: on the channel the panel
    /// becomes `running`, the task joins the conversation and the
    /// view pins to the tail; a dead worker toasts the error instead
    /// (the §6 degrade-gracefully path every send shares).
    pub(crate) fn submit(&mut self, request: AgentRequest, toast: &mut Option<Toast>) {
        let task = request.task.clone();
        match self.session.submit(request) {
            Ok(()) => {
                self.running = true;
                self.pinned = true;
                self.transcript.task(&task);
                self.push(Entry::Task(task));
            }
            Err(error) => *toast = Some(Toast::new(format!("{error}"), Instant::now())),
        }
    }

    /// Queue `message` as steering (Spec R06: frontend input during a
    /// run enqueues a user message the worker injects at the next
    /// model turn). Recorded in the conversation only once the send
    /// succeeded — a dead worker toasts instead.
    pub(crate) fn steer(&mut self, message: String, toast: &mut Option<Toast>) {
        match self.session.steer(message.clone()) {
            Ok(()) => {
                self.pinned = true;
                self.transcript.steering(&message);
                self.push(Entry::Steering(message));
            }
            Err(error) => *toast = Some(Toast::new(format!("{error}"), Instant::now())),
        }
    }

    /// Drain one tick of [`AgentEvent`]s — the seam `app` calls once
    /// per loop iteration; tests drive [`apply`](Self::apply)
    /// directly with staged events (the `SuggestionPin::apply`
    /// precedent).
    pub(crate) fn drain(&mut self) {
        while let Ok(event) = self.session.events().try_recv() {
            self.apply(event);
        }
    }

    /// One event applied: a `TextDelta` accumulates onto the newest
    /// `Text` entry (a turn after a task or steering entry gets its
    /// own); an `ApprovalRequest` opens an awaiting tool card and the
    /// approve-card state; a `ToolCall` opens a running card; a
    /// `ToolUpdate` transitions the card its `call_id` addresses and
    /// resolves a matching wait; every terminal event releases
    /// `running`, drops any pending approval and leaves a `Status`
    /// line the user can read against the stream above it. Every
    /// event also folds into the persisted-vocabulary mirror (Spec
    /// R08-T3) — staged `apply` tests exercise it too.
    pub(crate) fn apply(&mut self, event: AgentEvent) {
        self.transcript.apply(&event);
        match event {
            AgentEvent::TextDelta(text) => match self.entries.last_mut() {
                Some(Entry::Text(last)) => last.push_str(&text),
                _ => self.entries.push(Entry::Text(text)),
            },
            AgentEvent::ApprovalRequest { call, summary } => {
                self.entries.push(Entry::Tool(ToolCard {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    summary,
                    status: CardStatus::AwaitingApproval,
                    detail: None,
                }));
                self.pending = Some(PendingApproval {
                    call_id: call.id,
                    name: call.name,
                    denying: false,
                });
            }
            AgentEvent::ToolCall { call, summary } => {
                self.entries.push(Entry::Tool(ToolCard {
                    id: call.id,
                    name: call.name,
                    summary,
                    status: CardStatus::Running,
                    detail: None,
                }));
            }
            AgentEvent::AskRequest {
                call,
                question,
                choices,
            } => {
                self.entries.push(Entry::Ask(AskCard {
                    id: call.id.clone(),
                    question,
                    choices,
                    selected: 0,
                    answer: None,
                }));
                self.pending_ask = Some(PendingAsk { call_id: call.id });
            }
            AgentEvent::ToolUpdate {
                call_id,
                status,
                detail,
                ..
            } => {
                // The newest card with the id wins — call ids are
                // provider-unique but defensive all the same.
                if let Some(card) = self.entries.iter_mut().rev().find_map(|entry| match entry {
                    Entry::Tool(card) if card.id == call_id => Some(card),
                    _ => None,
                }) {
                    card.status = card_status(status);
                    card.detail = detail;
                }
                if self
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.call_id == call_id)
                {
                    self.pending = None;
                }
                if self
                    .pending_ask
                    .as_ref()
                    .is_some_and(|pending| pending.call_id == call_id)
                {
                    self.pending_ask = None;
                }
            }
            AgentEvent::PlanUpdated { todos } => {
                // The pinned block's whole content — replace
                // semantics (us-14): each call's snapshot IS the
                // plan; an empty list clears the block.
                self.plan = todos;
            }
            AgentEvent::Done => {
                self.running = false;
                self.pending = None;
                self.pending_ask = None;
                self.entries.push(Entry::Status("done".to_string()));
            }
            AgentEvent::Cancelled => {
                self.running = false;
                self.pending = None;
                self.pending_ask = None;
                self.entries.push(Entry::Status("cancelled".to_string()));
            }
            AgentEvent::BudgetReached => {
                self.running = false;
                self.pending = None;
                self.pending_ask = None;
                self.entries
                    .push(Entry::Status("step budget reached".to_string()));
            }
            AgentEvent::Error(message) => {
                self.running = false;
                self.pending = None;
                self.pending_ask = None;
                self.entries
                    .push(Entry::Status(format!("error: {message}")));
            }
        }
        self.revision += 1;
    }

    /// The wheel inside the panel's rect: scroll the conversation
    /// `lines` up into history (the view unpins) or back down toward
    /// the tail (landing on the bottom re-pins).
    pub(crate) fn scroll_up(&mut self, lines: usize) {
        self.top.set(self.painted.get().0.saturating_sub(lines));
        self.pinned = false;
        self.revision += 1;
    }

    /// Scroll the conversation `lines` toward the tail — reaching the
    /// bottom re-pins the view.
    pub(crate) fn scroll_down(&mut self, lines: usize) {
        let (top, max) = self.painted.get();
        let next = top.saturating_add(lines).min(max);
        if next == max {
            self.pinned = true;
        } else {
            self.top.set(next);
        }
        self.revision += 1;
    }

    /// The host cursor's screen position while the panel is focused:
    /// the end of the input line — the input row's tail-clip means it
    /// can sit at the row's right edge on a long entry.
    pub(crate) fn cursor_position(&self, area: Rect) -> (u16, u16) {
        let inner_x = area.x + 1;
        let inner_width = usize::from(area.width.saturating_sub(1));
        let room = inner_width.saturating_sub(PROMPT_WIDTH);
        let shown = tail(&self.input, room);
        let x = inner_x + PROMPT_WIDTH as u16 + shown.chars().count() as u16;
        (x.min(area.right().saturating_sub(1)), area.bottom() - 1)
    }

    /// Swap the panel to a stored conversation (Spec R08-T6, us-6):
    /// the scrollback rebuilds from the loaded transcript items, the
    /// pinned block restores the conversation's latest plan, and the
    /// transcript mirror adopts the items already-flushed — they ARE
    /// the store's rows, so nothing re-appends on the next drain and
    /// the next task's `prior` IS this transcript. The split opens
    /// focused: picking a conversation means picking it up. The
    /// conversation this replaces is already on disk — the swap
    /// loses nothing (us-12).
    pub(crate) fn restore(&mut self, items: Vec<TranscriptItem>, plan: Option<Vec<PlanTodo>>) {
        self.entries = entries_from(&items);
        self.transcript = Transcript::restored(items);
        self.plan = plan.unwrap_or_default();
        self.pending = None;
        self.pending_ask = None;
        self.running = false;
        self.pinned = true;
        self.top.set(0);
        self.open = true;
        self.focused = true;
        self.revision += 1;
    }

    /// Push one entry and bump the revision — the single write point
    /// the scrollback has.
    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.revision += 1;
    }
}

/// The rebuilt card's `detail` for an `interrupted` close (Spec R08
/// us-9): a call that never got its result is a run that died
/// mid-call — replay closes it with a synthetic "interrupted"
/// result, so its card closes the same way rather than waiting
/// forever.
const INTERRUPTED_DETAIL: &str = "interrupted — the run ended before this call produced a result";

/// A card's `detail` rebuilt from a stored `tool_result`'s full
/// text — the same first-line clip the live `ToolUpdate` carried
/// (the worker's `preview`, `session.rs`).
fn result_preview(text: &str) -> String {
    const PREVIEW_CHARS: usize = 120;
    let first = text.lines().next().unwrap_or_default();
    if first.chars().count() > PREVIEW_CHARS {
        format!("{}…", first.chars().take(PREVIEW_CHARS).collect::<String>())
    } else if text.lines().nth(1).is_some() {
        format!("{first} …")
    } else {
        first.to_string()
    }
}

/// `transcript_items` → panel `Entry`s — resume's scrollback
/// rebuild (Spec R08-T6): the inverse of [`Transcript::apply`]'s
/// fold. Item kinds map onto their entry shapes:
///
/// - `task`/`steering`/`assistant_text`/`status` → their direct
///   entries;
/// - `tool_call`/`tool_result` → a [`ToolCard`] opened `Running`
///   then transitioned by its result's status (the `AwaitingApproval`
///   distinction is frontend state the transcript never kept — a
///   call parked at exit reads interrupted, not awaiting);
/// - `ask`/`answer` → an [`AskCard`] plus its recorded answer — the
///   stored `ask` carries no call id (the card IS the record), so
///   the synthetic `ask_replay_N` id mirrors replay's convention;
/// - `summary` → the "conversation summarized" status line the
///   live event pushes (T7's marker);
/// - `plan` → no entry: the pinned block restores from the `plans`
///   row, and the calls' tool cards already carry the audit trail
///   (us-15).
///
/// A `tool_call` still `Running` at the end never got its result —
/// the run died mid-call — so it closes [`INTERRUPTED_DETAIL`]
/// rather than rendering `…` forever.
fn entries_from(items: &[TranscriptItem]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut asks = 0usize;
    for item in items {
        match item {
            TranscriptItem::Task { text } => entries.push(Entry::Task(text.clone())),
            TranscriptItem::Steering { text } => entries.push(Entry::Steering(text.clone())),
            TranscriptItem::AssistantText { text } => entries.push(Entry::Text(text.clone())),
            TranscriptItem::ToolCall {
                id, name, summary, ..
            } => entries.push(Entry::Tool(ToolCard {
                id: id.clone(),
                name: name.clone(),
                summary: summary.clone(),
                status: CardStatus::Running,
                detail: None,
            })),
            TranscriptItem::ToolResult { id, text, status } => {
                // The newest card with the id wins — the same rule
                // the live `ToolUpdate` applies.
                if let Some(card) = entries.iter_mut().rev().find_map(|entry| match entry {
                    Entry::Tool(card) if card.id == *id => Some(card),
                    _ => None,
                }) {
                    card.status = card_status(*status);
                    card.detail = Some(result_preview(text));
                }
            }
            TranscriptItem::Ask { question, choices } => {
                asks += 1;
                entries.push(Entry::Ask(AskCard {
                    id: format!("ask_replay_{asks}"),
                    question: question.clone(),
                    choices: choices.clone(),
                    selected: 0,
                    answer: None,
                }));
            }
            TranscriptItem::Answer { text } => {
                // The answer IS its ask's result — the newest
                // unresolved ask card takes it.
                if let Some(card) = entries.iter_mut().rev().find_map(|entry| match entry {
                    Entry::Ask(card) if card.answer.is_none() => Some(card),
                    _ => None,
                }) {
                    card.answer = Some(text.clone());
                }
            }
            TranscriptItem::Status { text } => entries.push(Entry::Status(text.clone())),
            TranscriptItem::Summary { .. } => {
                entries.push(Entry::Status("conversation summarized".to_string()));
            }
            TranscriptItem::Plan { .. } => {}
        }
    }
    for entry in &mut entries {
        if let Entry::Tool(card) = entry {
            if card.status == CardStatus::Running {
                card.status = CardStatus::Failed;
                card.detail = Some(INTERRUPTED_DETAIL.to_string());
            }
        }
    }
    entries
}

/// `ait_agent::ToolStatus` → [`CardStatus`] — the worker's
/// vocabulary mapped onto the card's (whose extra `AwaitingApproval`
/// state never comes through a `ToolUpdate`).
fn card_status(status: ToolStatus) -> CardStatus {
    match status {
        ToolStatus::Running => CardStatus::Running,
        ToolStatus::Done => CardStatus::Done,
        ToolStatus::Denied => CardStatus::Denied,
        ToolStatus::Failed => CardStatus::Failed,
    }
}

/// The `AgentContext` a submitted task carries (Spec R06 §5.6) — the
/// same answers the suggester's §5.4 assembly gives: the shared
/// `suggest::cwd_for`/`suggest::output_lines_for` seams, the spawned
/// program's basename and `std::env::consts::OS`.
pub(crate) fn context_for(tab: &Tab) -> AgentContext {
    AgentContext {
        cwd: suggest::cwd_for(tab),
        shell: tab.title().to_owned(),
        os: std::env::consts::OS.to_owned(),
        output_lines: suggest::output_lines_for(tab),
    }
}

/// Paint the panel into `area`: a `│` separator down the shared edge,
/// the title row (reversed while focused — the focus affordance the
/// ticket pins), the wrapped conversation between, and the input line
/// on the last row. All writes reset their span first, matching every
/// other painter's contract.
pub(crate) fn paint_agent_panel(buf: &mut Buffer, area: Rect, panel: &AgentPanel) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    for y in area.y..area.bottom() {
        let cell = &mut buf[(area.x, y)];
        cell.reset();
        cell.set_char('│');
    }
    let inner = Rect {
        x: area.x + 1,
        width: area.width.saturating_sub(1),
        ..area
    };
    if inner.width == 0 {
        return;
    }
    // Title row: the focus affordance — reversed while the panel owns
    // the keyboard — plus the in-flight hint while a task runs.
    reset_span(buf, inner.y, inner.x..inner.right());
    let title = if panel.running {
        " agent — working "
    } else {
        " agent "
    };
    let width = usize::from(inner.width);
    buf.set_stringn(inner.x, inner.y, title, width, Style::default());
    if panel.is_focused() {
        for x in inner.x..inner.right() {
            buf[(x, inner.y)].modifier |= Modifier::REVERSED;
        }
    }
    if inner.height < 2 {
        return;
    }
    // The input line owns the last row: `› ` plus the tail of the
    // buffer (long input clips from the left — the cursor end is the
    // half the user is editing). A parked card takes the row just
    // above — the approve card while the gate waits, the ask card's
    // prompt while a question waits — it is never hidden, and the
    // conversation loses that row for the duration.
    let input_y = inner.bottom() - 1;
    reset_span(buf, input_y, inner.x..inner.right());
    let room = width.saturating_sub(PROMPT_WIDTH);
    let shown = tail(&panel.input, room);
    buf.set_stringn(
        inner.x,
        input_y,
        format!("{INPUT_PROMPT}{shown}"),
        width,
        Style::default(),
    );
    let prompt_text = if let Some(pending) = &panel.pending {
        Some(if pending.denying {
            format!(" deny {} — type a reason · enter: deny ", pending.name)
        } else {
            format!(" approve {} — a: once · t: task · d: deny ", pending.name)
        })
    } else if panel.pending_ask.is_some() {
        Some(if panel.ask_has_choices() {
            " ask — ↑/↓ choose · enter: answer ".to_string()
        } else {
            " ask — type your answer · enter: send ".to_string()
        })
    } else {
        None
    };
    let prompt_rows = if let (Some(text), true) = (prompt_text, input_y > inner.y + 1) {
        let y = input_y - 1;
        reset_span(buf, y, inner.x..inner.right());
        buf.set_stringn(
            inner.x,
            y,
            text,
            width,
            Style::default()
                .fg(RtColor::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        1
    } else {
        0
    };
    if inner.height < 3 {
        return;
    }
    // The pinned plan block (Spec R08-T5, us-14): directly under the
    // title — each `update_plan` snapshot replaces its contents — and
    // the conversation scrolls beneath. The cap keeps a long plan
    // from crowding the scrollback out of the panel.
    let available = usize::from(input_y - (inner.y + 1)).saturating_sub(prompt_rows);
    let block = plan_lines(&panel.plan, PLAN_BLOCK_MAX.min(available.saturating_sub(1)));
    let plan_rows = block.len() as u16;
    for (index, (text, style)) in block.iter().enumerate() {
        let y = inner.y + 1 + index as u16;
        reset_span(buf, y, inner.x..inner.right());
        buf.set_stringn(inner.x, y, text, width, *style);
    }
    // The conversation between them, pinned to the tail unless the
    // user scrolled up — the paint pass owns the wrap, so it also
    // owns the scroll clamp: `top`/`painted` are what the wheel
    // handlers bound against.
    let conversation = display_lines(&panel.entries, width);
    let height = available - block.len();
    let max_top = conversation.len().saturating_sub(height);
    let top = if panel.pinned {
        max_top
    } else {
        panel.top.get().min(max_top)
    };
    panel.painted.set((top, max_top));
    for row in 0..height {
        let y = inner.y + 1 + plan_rows + row as u16;
        reset_span(buf, y, inner.x..inner.right());
        if let Some((text, style)) = conversation.get(top + row) {
            buf.set_stringn(inner.x, y, text, width, *style);
        }
    }
}

/// The conversation flattened into display lines at `width` columns:
/// entries wrap char-wise (never mid-UTF-8), separated by one blank
/// line, and each role carries its style — user entries bold under
/// `›`, assistant text plain, terminal statuses dimmed. The tool-card
/// ticket plugs its `Entry` variants in here.
fn display_lines(entries: &[Entry], width: usize) -> Vec<(String, Style)> {
    let mut lines = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            lines.push((String::new(), Style::default()));
        }
        let blocks: Vec<(String, Style)> = match entry {
            Entry::Task(text) | Entry::Steering(text) => vec![(
                format!("{INPUT_PROMPT}{text}"),
                Style::default().add_modifier(Modifier::BOLD),
            )],
            Entry::Text(text) => vec![(text.clone(), Style::default())],
            Entry::Tool(card) => tool_card_lines(card),
            Entry::Ask(card) => ask_card_lines(card),
            Entry::Status(text) => vec![(text.clone(), Style::default().fg(RtColor::DarkGray))],
        };
        for (text, style) in blocks {
            for line in wrap(&text, width) {
                lines.push((line, style));
            }
        }
    }
    lines
}

/// The pinned plan block's display rows (Spec R08-T5, us-14): a
/// `plan` header, then one row per todo — `✓` done, `▸` in progress,
/// `○` pending — and a trailing blank separating the block from the
/// scrollback. More rows than `cap` folds the tail into a `… N more`
/// note, so a long plan can never crowd the conversation out of the
/// panel; below a useful minimum (header + one todo + separator) the
/// rows are better spent on scrollback and the block sits out.
fn plan_lines(plan: &[PlanTodo], cap: usize) -> Vec<(String, Style)> {
    if plan.is_empty() || cap < 3 {
        return Vec::new();
    }
    let mut lines = vec![(
        String::from(" plan"),
        Style::default().fg(RtColor::DarkGray),
    )];
    // `cap - 2` todo rows at most — one for the header, one for the
    // separator/overflow row.
    let visible = (cap - 2).min(plan.len());
    for todo in &plan[..visible] {
        let (mark, style) = match todo.status {
            PlanTodoStatus::Done => ("✓", Style::default().fg(RtColor::DarkGray)),
            PlanTodoStatus::InProgress => ("▸", Style::default().add_modifier(Modifier::BOLD)),
            PlanTodoStatus::Pending => ("○", Style::default()),
        };
        lines.push((format!("{mark} {}", todo.content), style));
    }
    if plan.len() > visible {
        // The row this note replaces counted as hidden too.
        let hidden = plan.len() - visible + 1;
        lines.pop();
        lines.push((
            format!("    … {hidden} more"),
            Style::default().fg(RtColor::DarkGray),
        ));
    }
    lines.push((String::new(), Style::default()));
    lines
}

/// A tool card's display blocks (Spec R06 us-5): a status-marked
/// header — `✓` done, `…` running, `?` awaiting approval, `✗` denied,
/// `!` failed — carrying `name summary` inline; a multi-line summary
/// (`edit_files`' diff, us-22) instead explodes into one styled row
/// per line under a name-only header. The detail (result preview,
/// error or denial text) follows indented and dimmed when the
/// update carried one.
fn tool_card_lines(card: &ToolCard) -> Vec<(String, Style)> {
    let (mark, style) = match card.status {
        CardStatus::AwaitingApproval => (
            "?",
            Style::default()
                .fg(RtColor::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        CardStatus::Running => ("…", Style::default()),
        CardStatus::Done => ("✓", Style::default().fg(RtColor::Green)),
        CardStatus::Denied => ("✗", Style::default().fg(RtColor::DarkGray)),
        CardStatus::Failed => ("!", Style::default().fg(RtColor::Red)),
    };
    let mut lines = Vec::new();
    if card.summary.contains('\n') {
        // A multi-line summary — `edit_files`' unified diff
        // (Spec R06 us-22): the header carries the name alone and
        // each diff line paints its own row so `+`/`-` color as
        // additions/deletions instead of wrapping as one blob.
        lines.push((format!("{mark} {}", card.name), style));
        lines.extend(
            card.summary
                .lines()
                .map(|line| (format!("  {line}"), diff_line_style(line))),
        );
    } else {
        let summary = if card.summary.is_empty() {
            card.name.clone()
        } else {
            format!("{} {}", card.name, card.summary)
        };
        lines.push((format!("{mark} {summary}"), style));
    }
    if let Some(detail) = &card.detail {
        lines.push((
            format!("    {detail}"),
            Style::default().fg(RtColor::DarkGray),
        ));
    }
    lines
}

/// The ask card's display blocks (us-10): while it waits, `?` marks
/// the question and each choice lists beneath — `▸` on the
/// highlighted one; answered, `✓` marks the question and the answer
/// indents dimmed like a tool result's detail.
fn ask_card_lines(card: &AskCard) -> Vec<(String, Style)> {
    match &card.answer {
        Some(answer) => vec![
            (
                format!("✓ {}", card.question),
                Style::default().fg(RtColor::Green),
            ),
            (
                format!("    {answer}"),
                Style::default().fg(RtColor::DarkGray),
            ),
        ],
        None => {
            let mut lines = vec![(
                format!("? {}", card.question),
                Style::default()
                    .fg(RtColor::Yellow)
                    .add_modifier(Modifier::BOLD),
            )];
            for (index, choice) in card.choices.iter().enumerate() {
                lines.push(if index == card.selected {
                    (
                        format!("    ▸ {choice}"),
                        Style::default().add_modifier(Modifier::BOLD),
                    )
                } else {
                    (format!("      {choice}"), Style::default())
                });
            }
            lines
        }
    }
}

/// Per-line styling for a unified-diff summary: `+` green, `-` red,
/// `@@` cyan, the `\ No newline` marker dimmed. `---`/`+++` file
/// headers stay dim rather than deletion-red/addition-green —
/// they label files, not churn.
fn diff_line_style(line: &str) -> Style {
    if line.starts_with("---") || line.starts_with("+++") {
        Style::default().fg(RtColor::DarkGray)
    } else if line.starts_with('+') {
        Style::default().fg(RtColor::Green)
    } else if line.starts_with('-') {
        Style::default().fg(RtColor::Red)
    } else if line.starts_with('@') {
        Style::default().fg(RtColor::Cyan)
    } else if line.starts_with('\\') {
        Style::default().fg(RtColor::DarkGray)
    } else {
        Style::default()
    }
}

/// Char-wrap `text` to `width` columns honoring embedded newlines;
/// empty input still yields one (empty) line.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical in text.split('\n') {
        let mut line = String::new();
        let mut columns = 0;
        for c in logical.chars() {
            if columns == width {
                lines.push(std::mem::take(&mut line));
                columns = 0;
            }
            line.push(c);
            columns += 1;
        }
        lines.push(line);
    }
    lines
}

/// The trailing `max` characters of `text` — the input row's view of
/// a buffer longer than its width (the tail is the end being edited).
fn tail(text: &str, max: usize) -> &str {
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    text.char_indices()
        .nth(count - max)
        .map(|(index, _)| &text[index..])
        .unwrap_or("")
}

/// `paint_session`'s row-blanking primitive, duplicated locally —
/// `render` keeps it private; a `pub(crate)` share would be one more
/// cross-module surface for a three-line helper.
fn reset_span(buf: &mut Buffer, y: u16, xs: std::ops::Range<u16>) {
    for x in xs {
        buf[(x, y)].reset();
    }
}

/// One crossterm key edited into the panel — the half of the routing
/// `Tab::agent_key` does not need `self` for: chars append (plain and
/// SHIFT), Backspace pops, everything else is swallowed. Enter and
/// Esc are the caller's — they submit/steer/cancel against the tab's
/// state. `true` when the key edited the buffer.
pub(crate) fn edit_input(panel: &mut AgentPanel, key: &KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(c) if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => {
            panel.push_input(c);
            true
        }
        KeyCode::Backspace if key.modifiers.is_empty() => {
            panel.pop_input();
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    //! The panel's seams (Spec R06 / issue #85): the widget state
    //! driven through its own methods — no PTY, no model, no TTY —
    //! and the paint asserted on a ratatui `Buffer` the way
    //! `render`'s tests do.

    use super::*;
    use crate::config::NoSecrets;

    /// A panel over a session whose worker resolves nothing (default
    /// `Settings` hold no model) — the channel is real, the model is
    /// never reached.
    fn panel() -> AgentPanel {
        AgentPanel::with_session(AgentSession::with_secret_store(NoSecrets))
    }

    /// `Alt+A`'s three-state cycle on a live panel: unfocused →
    /// focused → closed — and a closed panel keeps its conversation.
    #[test]
    fn toggle_refocuses_then_closes_without_losing_the_conversation() {
        let mut panel = panel();
        panel.apply(AgentEvent::TextDelta("hello".into()));

        panel.toggle(); // focused → closed
        assert!(!panel.is_open());
        assert!(!panel.is_focused());
        assert_eq!(panel.entries().len(), 1, "the conversation survives");

        panel.toggle(); // closed → open + focused
        assert!(panel.is_open() && panel.is_focused());

        // The Esc-unfocused state the third branch serves.
        panel.escape();
        assert!(panel.is_open() && !panel.is_focused());
        panel.toggle();
        assert!(panel.is_focused(), "Alt+A refocuses rather than closing");
    }

    /// Esc is modal: it cancels a running task (the terminal event
    /// releases `running`) and, while idle, returns focus — the split
    /// itself stays open either way.
    #[test]
    fn escape_cancels_a_running_task_and_unfocuses_when_idle() {
        let mut panel = panel();
        panel.entries.push(Entry::Status("staged".into()));
        panel.running = true;

        panel.escape();
        assert!(
            panel.is_focused(),
            "cancelling keeps focus for the next task"
        );
        assert!(panel.running, "released only by the terminal event");

        panel.apply(AgentEvent::Cancelled);
        assert!(!panel.running);

        panel.escape();
        assert!(
            panel.is_open() && !panel.is_focused(),
            "an idle Esc returns focus, split stays open"
        );
    }

    /// `TextDelta`s accumulate onto the newest assistant entry; a
    /// task or steering entry between turns starts a fresh one — and
    /// every terminal event leaves its status line.
    #[test]
    fn events_accumulate_and_terminate() {
        let mut panel = panel();
        panel.entries.push(Entry::Task("fix it".into()));
        panel.apply(AgentEvent::TextDelta("hello".into()));
        panel.apply(AgentEvent::TextDelta(" world".into()));
        assert_eq!(
            panel.entries().last(),
            Some(&Entry::Text("hello world".into())),
            "deltas concat onto one entry"
        );

        panel.entries.push(Entry::Steering("and quickly".into()));
        panel.apply(AgentEvent::TextDelta("on it".into()));
        panel.apply(AgentEvent::BudgetReached);
        assert_eq!(
            panel.entries(),
            &[
                Entry::Task("fix it".into()),
                Entry::Text("hello world".into()),
                Entry::Steering("and quickly".into()),
                Entry::Text("on it".into()),
                Entry::Status("step budget reached".into()),
            ]
        );
        assert!(!panel.running);
    }

    /// The persisted vocabulary (Spec R08-T3): sends and applied
    /// events accumulate [`TranscriptItem`]s in order — a real submit
    /// records the `task`, deltas coalesce, a gated call and its
    /// verdict land as `tool_call` + `tool_result`, the terminal
    /// event as `status` — and `take` yields the batch once.
    #[test]
    fn sends_and_events_accumulate_transcript_items() {
        let mut panel = panel();
        let mut toast = None;
        panel.submit(
            AgentRequest {
                settings: ait_config::Settings::default(),
                profile: "default".to_string(),
                task: "fix it".to_string(),
                context: AgentContext {
                    cwd: "/tmp".into(),
                    shell: "sh".into(),
                    os: "test".into(),
                    output_lines: Vec::new(),
                },
                prior: Vec::new(),
                bridge: None,
            },
            &mut toast,
        );
        panel.apply(AgentEvent::TextDelta("on ".into()));
        panel.apply(AgentEvent::TextDelta("it".into()));
        panel.apply(AgentEvent::ApprovalRequest {
            call: ait_agent::ToolCall {
                id: "call_1".into(),
                name: "glob".into(),
                arguments: "{\"pattern\":\"x\"}".into(),
            },
            summary: "x".into(),
        });
        panel.apply(AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ait_agent::ToolStatus::Denied,
            detail: Some("no".into()),
            result: Some("no".into()),
        });
        panel.apply(AgentEvent::Cancelled);

        assert_eq!(
            panel.take_transcript_items(),
            vec![
                ait_agent::TranscriptItem::Task {
                    text: "fix it".into(),
                },
                ait_agent::TranscriptItem::AssistantText {
                    text: "on it".into(),
                },
                ait_agent::TranscriptItem::ToolCall {
                    id: "call_1".into(),
                    name: "glob".into(),
                    args: "{\"pattern\":\"x\"}".into(),
                    summary: "x".into(),
                },
                ait_agent::TranscriptItem::ToolResult {
                    id: "call_1".into(),
                    text: "no".into(),
                    status: ait_agent::ToolStatus::Denied,
                },
                ait_agent::TranscriptItem::Status {
                    text: "cancelled".into(),
                },
            ]
        );
        assert!(
            panel.take_transcript_items().is_empty(),
            "drained items don't repeat"
        );
    }

    /// A real submit round-trips the channel: the task lands in the
    /// conversation, `running` holds until the worker's terminal
    /// event (the unconfigured model's `Error`) releases it.
    #[test]
    fn submit_runs_until_the_workers_terminal_event() {
        let mut panel = panel();
        let mut toast = None;
        panel.submit(
            AgentRequest {
                settings: ait_config::Settings::default(),
                profile: "default".to_string(),
                task: "fix it".to_string(),
                context: AgentContext {
                    cwd: "/tmp".into(),
                    shell: "sh".into(),
                    os: "test".into(),
                    output_lines: Vec::new(),
                },
                prior: Vec::new(),
                bridge: None,
            },
            &mut toast,
        );

        assert!(panel.is_running());
        assert_eq!(panel.entries(), &[Entry::Task("fix it".into())]);
        assert!(toast.is_none());

        // The worker answers `Error` (no model configured) — the
        // drain lands it as a Status and releases `running`.
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while panel.is_running() {
            panel.drain();
            assert!(Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(
            matches!(panel.entries().last(), Some(Entry::Status(s)) if s.contains("coding_model")),
            "the worker's error became a status line: {:?}",
            panel.entries().last()
        );
    }

    /// Mid-run typing queues as a steering message on the channel
    /// AND appears in the conversation — the two halves of the
    /// ticket's steering criterion.
    #[test]
    fn steer_records_the_message_in_the_conversation() {
        let mut panel = panel();
        let mut toast = None;
        panel.entries.push(Entry::Task("fix it".into()));
        panel.running = true;

        panel.steer("use sudo".to_string(), &mut toast);

        assert_eq!(
            panel.entries().last(),
            Some(&Entry::Steering("use sudo".into()))
        );
        assert!(toast.is_none());
    }

    /// The editing half of the input line: chars append, SHIFT chars
    /// append already-shifted, Backspace pops, and keys the panel
    /// cannot use are swallowed without touching the buffer.
    #[test]
    fn input_editing() {
        use crossterm::event::KeyEvent as CtKeyEvent;
        let mut panel = panel();
        for c in "fix".chars() {
            assert!(edit_input(
                &mut panel,
                &CtKeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
            ));
        }
        assert_eq!(panel.input(), "fix");
        assert!(edit_input(
            &mut panel,
            &CtKeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT)
        ));
        assert_eq!(panel.input(), "fixX");
        assert!(edit_input(
            &mut panel,
            &CtKeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
        ));
        assert_eq!(panel.input(), "fix");
        // Control chords and navigation are swallowed, not edited.
        for key in [
            CtKeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
            CtKeyEvent::new(KeyCode::Left, KeyModifiers::NONE),
            CtKeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ] {
            assert!(!edit_input(&mut panel, &key));
        }
        assert_eq!(panel.input(), "fix");
        assert_eq!(panel.take_input(), "fix");
        assert_eq!(panel.input(), "");
    }

    /// Spec R08-T6: resume's scrollback rebuild — every item kind
    /// maps to its entry: the tool card transitions on its result,
    /// the ask card carries its recorded answer, statuses and the
    /// `summary` marker land as status lines, `plan` items stay out
    /// (the pinned block owns them).
    #[test]
    fn entries_from_rebuilds_the_scrollback() {
        let entries = entries_from(&[
            TranscriptItem::Task {
                text: "fix it".into(),
            },
            TranscriptItem::ToolCall {
                id: "call_1".into(),
                name: "glob".into(),
                args: "{}".into(),
                summary: "\"*.rs\"".into(),
            },
            TranscriptItem::ToolResult {
                id: "call_1".into(),
                text: "a.rs\nb.rs".into(),
                status: ait_agent::ToolStatus::Done,
            },
            TranscriptItem::Ask {
                question: "Ship it?".into(),
                choices: vec!["yes".into(), "no".into()],
            },
            TranscriptItem::Answer { text: "no".into() },
            TranscriptItem::AssistantText {
                text: "on it".into(),
            },
            TranscriptItem::Plan { todos: vec![] },
            TranscriptItem::Summary {
                text: "older half".into(),
                covers_seq: 3,
            },
            TranscriptItem::Status {
                text: "done".into(),
            },
        ]);

        assert_eq!(
            entries,
            vec![
                Entry::Task("fix it".into()),
                Entry::Tool(ToolCard {
                    id: "call_1".into(),
                    name: "glob".into(),
                    summary: "\"*.rs\"".into(),
                    status: CardStatus::Done,
                    detail: Some("a.rs …".into()),
                }),
                Entry::Ask(AskCard {
                    id: "ask_replay_1".into(),
                    question: "Ship it?".into(),
                    choices: vec!["yes".into(), "no".into()],
                    selected: 0,
                    answer: Some("no".into()),
                }),
                Entry::Text("on it".into()),
                Entry::Status("conversation summarized".into()),
                Entry::Status("done".into()),
            ],
            "no entry for the plan item — the pinned block owns it"
        );
    }

    /// us-9: a call whose run died before its result closes as
    /// interrupted — never an eternal `…` spinner or a live-looking
    /// awaiting card.
    #[test]
    fn an_unanswered_call_closes_interrupted() {
        let entries = entries_from(&[
            TranscriptItem::ToolCall {
                id: "call_1".into(),
                name: "run_command".into(),
                args: "{}".into(),
                summary: "\"ls\"".into(),
            },
            TranscriptItem::Status {
                text: "cancelled".into(),
            },
        ]);
        assert_eq!(
            entries[0],
            Entry::Tool(ToolCard {
                id: "call_1".into(),
                name: "run_command".into(),
                summary: "\"ls\"".into(),
                status: CardStatus::Failed,
                detail: Some(INTERRUPTED_DETAIL.to_string()),
            })
        );
    }

    /// Spec R08-T6/us-6: `restore` swaps the panel to the loaded
    /// conversation — entries rebuilt, plan pinned, transcript
    /// adopted already-flushed (nothing re-appends on the next
    /// drain; the next `prior` IS the restored transcript) — and
    /// the split opens focused.
    #[test]
    fn restore_swaps_the_panel_to_the_loaded_conversation() {
        let mut panel = panel();
        panel.entries.push(Entry::Task("old work".into()));
        panel.plan = vec![PlanTodo {
            content: "old".into(),
            status: PlanTodoStatus::Pending,
        }];
        panel.open = false;
        panel.focused = false;

        let plan = vec![PlanTodo {
            content: "verify".into(),
            status: PlanTodoStatus::Pending,
        }];
        panel.restore(
            vec![
                TranscriptItem::Task {
                    text: "fix the tests".into(),
                },
                TranscriptItem::AssistantText {
                    text: "on it".into(),
                },
            ],
            Some(plan.clone()),
        );

        assert_eq!(
            panel.entries(),
            &[
                Entry::Task("fix the tests".into()),
                Entry::Text("on it".into()),
            ]
        );
        assert_eq!(panel.plan(), plan.as_slice(), "the pinned block restores");
        assert!(panel.is_open() && panel.is_focused());
        assert!(
            panel.take_transcript_items().is_empty(),
            "the restored items are already persisted — nothing re-appends"
        );
        assert_eq!(
            panel.transcript_items().len(),
            2,
            "the next task's prior IS the restored transcript"
        );
    }

    /// `wrap` honors embedded newlines and hard-wraps long runs —
    /// the conversation's display-line math.
    #[test]
    fn wrap_splits_on_newlines_and_width() {
        assert_eq!(wrap("ab\ncd", 5), vec!["ab", "cd"]);
        assert_eq!(wrap("abcd", 2), vec!["ab", "cd"]);
        assert_eq!(wrap("", 2), vec![""]);
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
    }

    /// Paint on a small rect: the `│` separator, the title row
    /// (reversed while focused), the conversation in between and the
    /// input line at the bottom.
    #[test]
    fn paint_lays_out_separator_title_conversation_and_input() {
        let mut panel = panel();
        panel.apply(AgentEvent::TextDelta("reply".into()));
        panel.apply(AgentEvent::Done);
        panel.push_input('q');

        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 6));
        let area = Rect::new(10, 0, 10, 6);
        paint_agent_panel(&mut buf, area, &panel);

        // The separator fills the shared edge top to bottom.
        for y in 0..6 {
            assert_eq!(buf[(10, y)].symbol(), "│");
        }
        // Title text on row 0, reversed while focused.
        let title: String = (11..17).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(title, " agent");
        assert!(buf[(11, 0)].modifier.contains(Modifier::REVERSED));
        // The conversation fills its three rows: the reply, the
        // entry-separating blank and the `done` status.
        let row1: String = (11..20).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row1.trim_end(), "reply");
        let row3: String = (11..20).map(|x| buf[(x, 3)].symbol().to_string()).collect();
        assert_eq!(row3.trim_end(), "done");
        // The input line's `› ` prompt and text on the last row.
        let input_row: String = (11..14).map(|x| buf[(x, 5)].symbol().to_string()).collect();
        assert_eq!(input_row, "› q");
        // The cursor sits after the input text.
        assert_eq!(panel.cursor_position(area), (14, 5));
    }

    /// An unfocused panel paints its title row plain — the open-vs-
    /// focused distinction the ticket's render criterion names.
    #[test]
    fn unfocused_panel_paints_a_plain_title() {
        let mut panel = panel();
        panel.escape(); // idle → unfocused, still open

        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 4));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 12, 4), &panel);

        assert!(!buf[(1, 0)].modifier.contains(Modifier::REVERSED));
    }

    /// More lines than the view fits scroll the window: pinned shows
    /// the tail; a wheel-up holds the user's place while appends keep
    /// landing below; a wheel-down to the bottom re-pins.
    #[test]
    fn conversation_scrolls_and_re_pins() {
        let mut panel = panel();
        for n in 0..10 {
            panel.apply(AgentEvent::TextDelta(format!("line{n}\n")));
        }

        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 5));
        let area = Rect::new(0, 0, 12, 5); // 3 conversation rows
        paint_agent_panel(&mut buf, area, &panel);
        assert!(panel.is_pinned());
        // Rows 1..3 show the tail: lines 8, 9 and the trailing blank
        // of "line9\n".
        let tail_row: String = (1..6).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(tail_row.trim_end(), "line9");

        panel.scroll_up(2);
        paint_agent_panel(&mut buf, area, &panel);
        assert!(!panel.is_pinned());
        let top_row: String = (1..6).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(top_row.trim_end(), "line6", "scrolled two lines up");

        // Appends can't shift a scrolled view: the same lines stay.
        panel.apply(AgentEvent::TextDelta("more\n".into()));
        paint_agent_panel(&mut buf, area, &panel);
        let same_top: String = (1..6).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(same_top.trim_end(), "line6");

        panel.scroll_down(10);
        paint_agent_panel(&mut buf, area, &panel);
        assert!(panel.is_pinned(), "landing on the bottom re-pins");
    }

    /// A staged `ToolCall` for the card tests.
    fn call(id: &str, name: &str) -> ait_agent::ToolCall {
        ait_agent::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: String::new(),
        }
    }

    /// An `ApprovalRequest` opens an awaiting tool card and parks the
    /// gate; `decide` consumes the wait (the verdict travels the real
    /// channel — the worker's end is the session tests' seam).
    #[test]
    fn approval_request_opens_a_card_and_decide_answers_it() {
        let mut panel = panel();
        panel.apply(AgentEvent::ApprovalRequest {
            call: call("call_1", "read_files"),
            summary: "note.txt".into(),
        });
        assert!(panel.awaiting_verdict());
        assert_eq!(
            panel.entries().last(),
            Some(&Entry::Tool(ToolCard {
                id: "call_1".into(),
                name: "read_files".into(),
                summary: "note.txt".into(),
                status: CardStatus::AwaitingApproval,
                detail: None,
            }))
        );

        let mut toast = None;
        panel.decide(ApprovalVerdict::AllowOnce, &mut toast);
        assert!(!panel.awaiting_verdict());
        assert!(toast.is_none());
    }

    /// An ungated `ToolCall` opens a running card; `ToolUpdate`s
    /// transition it and a matching update resolves a parked gate.
    #[test]
    fn tool_cards_transition_on_updates() {
        let mut panel = panel();
        panel.apply(AgentEvent::ToolCall {
            call: call("call_1", "glob"),
            summary: "\"*.rs\"".into(),
        });
        panel.apply(AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ait_agent::ToolStatus::Done,
            detail: Some("a.rs".into()),
            result: Some("a.rs".into()),
        });
        assert_eq!(
            panel.entries().last(),
            Some(&Entry::Tool(ToolCard {
                id: "call_1".into(),
                name: "glob".into(),
                summary: "\"*.rs\"".into(),
                status: CardStatus::Done,
                detail: Some("a.rs".into()),
            }))
        );

        // A denial resolves the pending wait and marks the card.
        panel.apply(AgentEvent::ApprovalRequest {
            call: call("call_2", "read_files"),
            summary: "b.rs".into(),
        });
        assert!(panel.awaiting_verdict());
        panel.apply(AgentEvent::ToolUpdate {
            call_id: "call_2".into(),
            status: ait_agent::ToolStatus::Denied,
            detail: Some("not now".into()),
            result: Some("not now".into()),
        });
        assert!(!panel.awaiting_verdict());
        assert!(matches!(
            panel.entries().last(),
            Some(Entry::Tool(ToolCard {
                status: CardStatus::Denied,
                ..
            }))
        ));
    }

    /// A terminal event while the gate waits drops the approve card
    /// — it can never outlive its task.
    #[test]
    fn terminal_events_clear_a_pending_approval() {
        let mut panel = panel();
        panel.entries.push(Entry::Task("fix it".into()));
        panel.running = true;
        panel.apply(AgentEvent::ApprovalRequest {
            call: call("call_1", "grep"),
            summary: "\"x\"".into(),
        });
        assert!(panel.awaiting_verdict());
        panel.apply(AgentEvent::Cancelled);
        assert!(!panel.awaiting_verdict());
    }

    /// A staged `AskRequest` for the ask-card tests.
    fn ask(id: &str, question: &str, choices: &[&str]) -> AgentEvent {
        AgentEvent::AskRequest {
            call: ait_agent::ToolCall {
                id: id.into(),
                name: "ask_user_question".into(),
                arguments: String::new(),
            },
            question: question.into(),
            choices: choices.iter().map(|c| c.to_string()).collect(),
        }
    }

    /// Spec R08 us-13–15: a `PlanUpdated` pins the TODO block under
    /// the title — one styled row per todo — the call's ordinary
    /// card still sits in the scrollback, and the NEXT snapshot
    /// replaces the block's contents in place (never appends).
    #[test]
    fn plan_updates_pin_a_block_that_replaces_in_place() {
        use ait_agent::{PlanTodo, PlanTodoStatus};
        let todos = |items: &[(&str, PlanTodoStatus)]| {
            items
                .iter()
                .map(|(content, status)| PlanTodo {
                    content: (*content).into(),
                    status: *status,
                })
                .collect::<Vec<_>>()
        };

        let mut panel = panel();
        panel.apply(AgentEvent::ToolCall {
            call: call("call_1", "update_plan"),
            summary: "3 todos".into(),
        });
        panel.apply(AgentEvent::PlanUpdated {
            todos: todos(&[
                ("reproduce", PlanTodoStatus::Done),
                ("fix", PlanTodoStatus::InProgress),
                ("verify", PlanTodoStatus::Pending),
            ]),
        });
        panel.apply(AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ait_agent::ToolStatus::Done,
            detail: Some("plan updated — 3 todos".into()),
            result: Some("plan updated — 3 todos".into()),
        });

        // The call's ordinary card landed in the scrollback like
        // every other tool call.
        assert_eq!(
            panel.entries(),
            &[Entry::Tool(ToolCard {
                id: "call_1".into(),
                name: "update_plan".into(),
                summary: "3 todos".into(),
                status: CardStatus::Done,
                detail: Some("plan updated — 3 todos".into()),
            })]
        );

        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 12));
        let area = Rect::new(0, 0, 30, 12);
        paint_agent_panel(&mut buf, area, &panel);
        // Column 0 is the `│` separator — content starts at 1.
        fn row(buf: &Buffer, y: u16) -> String {
            (1..30).map(|x| buf[(x, y)].symbol().to_string()).collect()
        }
        // Row 0 is the title; the pinned block owns the next rows.
        assert_eq!(row(&buf, 1).trim_end(), " plan");
        assert_eq!(row(&buf, 2).trim_end(), "✓ reproduce");
        assert_eq!(row(&buf, 3).trim_end(), "▸ fix");
        assert_eq!(row(&buf, 4).trim_end(), "○ verify");
        // …and the scrollback conversation begins beneath it.
        assert_eq!(row(&buf, 5).trim_end(), "");
        assert_eq!(row(&buf, 6).trim_end(), "✓ update_plan 3 todos");

        // A second call replaces the block's contents — nothing of
        // the first list survives.
        panel.apply(AgentEvent::ToolCall {
            call: call("call_2", "update_plan"),
            summary: "1 todo".into(),
        });
        panel.apply(AgentEvent::PlanUpdated {
            todos: todos(&[("ship it", PlanTodoStatus::InProgress)]),
        });
        paint_agent_panel(&mut buf, area, &panel);
        assert_eq!(row(&buf, 2).trim_end(), "▸ ship it");
        assert_eq!(row(&buf, 3).trim_end(), "", "the old rows cleared");
        assert!(panel.plan().iter().all(|todo| todo.content != "verify"));
    }

    /// An empty snapshot clears the block — the newest plan IS
    /// "no plan".
    #[test]
    fn an_empty_plan_snapshot_clears_the_block() {
        let mut panel = panel();
        panel.apply(AgentEvent::PlanUpdated {
            todos: vec![ait_agent::PlanTodo {
                content: "step".into(),
                status: ait_agent::PlanTodoStatus::Pending,
            }],
        });
        assert_eq!(panel.plan().len(), 1);
        panel.apply(AgentEvent::PlanUpdated { todos: vec![] });
        assert!(panel.plan().is_empty());

        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 8));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 20, 8), &panel);
        let row: String = (1..20).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), "", "no block under the title");
    }

    /// An `AskRequest` opens its own card — question and choices in
    /// the scrollback — and `submit_answer` records the highlighted
    /// choice as the answer (the send travels the real channel; the
    /// worker's end is the session tests' seam).
    #[test]
    fn ask_request_opens_a_card_and_submit_answer_resolves_it() {
        let mut panel = panel();
        panel.apply(ask("call_1", "Which approach?", &["rewrite", "patch"]));
        assert!(panel.awaiting_answer());
        assert!(panel.ask_has_choices());
        assert_eq!(
            panel.entries().last(),
            Some(&Entry::Ask(AskCard {
                id: "call_1".into(),
                question: "Which approach?".into(),
                choices: vec!["rewrite".into(), "patch".into()],
                selected: 0,
                answer: None,
            }))
        );

        panel.ask_next();
        let mut toast = None;
        panel.submit_answer(&mut toast);
        assert!(!panel.awaiting_answer());
        assert!(toast.is_none());
        assert!(matches!(
            panel.entries().last(),
            Some(Entry::Ask(card)) if card.answer.as_deref() == Some("patch")
        ));
    }

    /// Navigation wraps at both ends of the choice list.
    #[test]
    fn ask_navigation_wraps() {
        let mut panel = panel();
        panel.apply(ask("call_1", "Pick", &["a", "b"]));

        panel.ask_prev();
        let Some(Entry::Ask(card)) = panel.entries().last() else {
            panic!("ask card")
        };
        assert_eq!(card.selected, 1, "prev from the top wraps to the tail");
        panel.ask_next();
        let Some(Entry::Ask(card)) = panel.entries().last() else {
            panic!("ask card")
        };
        assert_eq!(card.selected, 0, "next off the tail wraps to the top");
    }

    /// A free-text ask takes the input line: `submit_answer` consumes
    /// the typed text as the answer. An empty line answers nothing —
    /// the card stays parked.
    #[test]
    fn a_free_text_ask_answers_from_the_input_line() {
        let mut panel = panel();
        panel.apply(ask("call_1", "What should the command be?", &[]));
        assert!(panel.awaiting_answer());
        assert!(!panel.ask_has_choices());

        let mut toast = None;
        panel.submit_answer(&mut toast);
        assert!(
            panel.awaiting_answer(),
            "an empty free-text answer is a no-op"
        );

        for c in "cargo clippy".chars() {
            panel.push_input(c);
        }
        panel.submit_answer(&mut toast);
        assert!(!panel.awaiting_answer());
        assert_eq!(panel.input(), "");
        assert!(matches!(
            panel.entries().last(),
            Some(Entry::Ask(card)) if card.answer.as_deref() == Some("cargo clippy")
        ));
    }

    /// A terminal event while the ask waits drops the card's parked
    /// state — the Esc path: `Cancelled` releases `running` and
    /// leaves the card unanswered.
    #[test]
    fn terminal_events_clear_a_pending_ask() {
        let mut panel = panel();
        panel.entries.push(Entry::Task("set it up".into()));
        panel.running = true;
        panel.apply(ask("call_1", "Proceed?", &["yes", "no"]));
        assert!(panel.awaiting_answer());

        panel.apply(AgentEvent::Cancelled);
        assert!(!panel.awaiting_answer());
        assert!(!panel.running);
        assert!(panel
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Ask(card) if card.answer.is_none())));
    }

    /// The ask card paints question and choices in the conversation —
    /// the highlighted option marked — plus its prompt row above the
    /// input line; once answered it renders `✓` + the answer.
    #[test]
    fn paint_shows_the_ask_card_and_its_prompt_row() {
        let mut panel = panel();
        panel.apply(ask("call_1", "Which approach?", &["rewrite", "patch"]));

        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 8));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 60, 8), &panel);

        let row =
            |y: u16| -> String { (1..60).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert_eq!(row(1).trim_end(), "? Which approach?");
        assert_eq!(
            row(2).trim_end(),
            "    ▸ rewrite",
            "the first option is selected"
        );
        assert_eq!(row(3).trim_end(), "      patch");
        // The prompt row owns the line above the input row.
        assert!(row(6).contains("enter"), "{:?}", row(6));
        assert!(row(6).contains("choose"), "{:?}", row(6));

        // Answered: the card transitions to `✓` + the answer text.
        let mut toast = None;
        panel.submit_answer(&mut toast);
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 8));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 60, 8), &panel);
        let row =
            |y: u16| -> String { (1..60).map(|x| buf[(x, y)].symbol().to_string()).collect() };
        assert_eq!(row(1).trim_end(), "✓ Which approach?");
        assert_eq!(row(2).trim(), "rewrite");
        assert!(!row(6).contains("choose"), "the prompt row released");
    }

    /// The approve card paints its row above the input line while the
    /// gate waits — and the tool card renders mark + name + summary.
    #[test]
    fn paint_shows_tool_cards_and_the_approve_row() {
        let mut panel = panel();
        panel.apply(AgentEvent::ApprovalRequest {
            call: call("call_1", "read_files"),
            summary: "note.txt".into(),
        });

        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 7));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 60, 7), &panel);

        // The tool card on the first conversation row.
        let card_row: String = (1..25).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(card_row.trim_end(), "? read_files note.txt");
        // The approve card on the row above the input line.
        let approve_row: String = (1..60).map(|x| buf[(x, 5)].symbol().to_string()).collect();
        assert!(
            approve_row.contains("approve read_files"),
            "{approve_row:?}"
        );
        assert!(approve_row.contains("d: deny"), "{approve_row:?}");
        // The input line still owns the last row.
        let input_row: String = (1..3).map(|x| buf[(x, 6)].symbol().to_string()).collect();
        assert_eq!(input_row, "› ");

        // Once resolved the card transitions and the row returns.
        panel.apply(AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ait_agent::ToolStatus::Done,
            detail: Some("note.txt: contents".into()),
            result: Some("note.txt: contents".into()),
        });
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 7));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 60, 7), &panel);
        let card_row: String = (1..25).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(card_row.trim_end(), "✓ read_files note.txt");
        let detail_row: String = (1..25).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(detail_row.trim(), "note.txt: contents");
        let input_row_above: String = (1..20).map(|x| buf[(x, 5)].symbol().to_string()).collect();
        assert!(
            !input_row_above.contains("approve"),
            "the row released: {input_row_above:?}"
        );
    }

    /// An `edit_files` approve card renders its multi-line unified
    /// diff one styled row per line — `+` green, `-` red, `@@` cyan,
    /// file headers dim — under a name-only header (us-22).
    #[test]
    fn edit_files_card_renders_its_diff_per_line() {
        let summary = "--- a/note.txt\n+++ b/note.txt\n@@ -1,2 +1,2 @@\n old\n-gone\n+new\n\\ No newline at end of file";
        let card = ToolCard {
            id: "call_1".into(),
            name: "edit_files".into(),
            summary: summary.into(),
            status: CardStatus::AwaitingApproval,
            detail: None,
        };
        let lines = tool_card_lines(&card);
        assert_eq!(lines[0].0, "? edit_files");
        // Each diff line is its own display row with its own style.
        let rows: Vec<&str> = lines[1..].iter().map(|(t, _)| t.trim_start()).collect();
        assert_eq!(
            rows,
            vec![
                "--- a/note.txt",
                "+++ b/note.txt",
                "@@ -1,2 +1,2 @@",
                "old",
                "-gone",
                "+new",
                "\\ No newline at end of file"
            ]
        );
        assert_eq!(
            lines[3].1,
            Style::default().fg(RtColor::Cyan),
            "the @@ header"
        );
        assert_eq!(lines[5].1, Style::default().fg(RtColor::Red), "a - line");
        assert_eq!(lines[6].1, Style::default().fg(RtColor::Green), "a + line");
        assert_eq!(
            lines[1].1,
            Style::default().fg(RtColor::DarkGray),
            "--- stays dim, not deletion-red"
        );

        // …and the painted card shows the diff rows verbatim.
        let mut panel = panel();
        panel.apply(AgentEvent::ApprovalRequest {
            call: call("call_1", "edit_files"),
            summary: summary.into(),
        });
        // Twelve rows fit title + 8 card lines + approve row + input.
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
        paint_agent_panel(&mut buf, Rect::new(0, 0, 40, 12), &panel);
        let row: String = (1..40).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), "? edit_files");
        let row: String = (1..40).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), "  --- a/note.txt");
        let row: String = (1..40).map(|x| buf[(x, 7)].symbol().to_string()).collect();
        assert_eq!(row.trim_end(), "  +new");
        assert_eq!(buf[(3, 7)].fg, RtColor::Green);
    }
}
