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
//!
//! The paint lives in the frontend — each paints `entries()`, the
//! pinned `plan()` and `input()` through its own toolkit (the TUI's
//! `paint_agent_panel` in `ait`; the GUI its egui painter) and
//! publishes what it showed through `set_painted_view` so
//! `scroll_up`/`scroll_down` wheel against real bounds.

use std::cell::Cell;
use std::time::Instant;

use ait_agent::{
    ask_replay_id, result_preview, AgentContext, AgentEvent, AgentRequest, AgentSession,
    ApprovalVerdict, PlanTodo, ToolStatus, TranscriptItem, INTERRUPTED_RESULT, STATUS_BUDGET,
    STATUS_CANCELLED, STATUS_DONE,
};
use ait_term::{Key, KeyEvent, Modifiers};

use crate::status::Toast;
use crate::suggest;
use crate::tabs::Tab;
use crate::transcript::Transcript;

/// One conversation entry — what the panel's scrollback is made of.
/// Each variant owns its display shape in [`display_lines`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
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
pub enum CardStatus {
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
pub struct ToolCard {
    /// The provider call id `ToolUpdate`s address.
    pub id: String,
    /// The registry name.
    pub name: String,
    /// The renderable argument summary.
    pub summary: String,
    /// Where the call stands.
    pub status: CardStatus,
    /// The one-line result preview or error/denial text.
    pub detail: Option<String>,
}

/// The `ask_user_question` card — built from an
/// [`AgentEvent::AskRequest`] (question plus choices-or-free-text,
/// already parsed worker-side), resolved by
/// [`submit_answer`](AgentPanel::submit_answer) which records the
/// user's reply in `answer`. `selected` highlights the option the
/// ↑/↓ keys move over while the card waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskCard {
    /// The provider call id `ToolUpdate`s address.
    pub id: String,
    /// The question the model asked.
    pub question: String,
    /// The offered choices; empty means a free-text answer.
    pub choices: Vec<String>,
    /// The highlighted choice while the card waits.
    pub selected: usize,
    /// The user's answer — `Some` once the card is resolved.
    pub answer: Option<String>,
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
pub struct AgentPanel {
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
    pub fn new() -> Self {
        Self::with_session(AgentSession::new())
    }

    /// The transcript items accumulated since the last take — the
    /// store's per-drain batch (Spec R08-T3; the
    /// [`take_persisted_blocks`](Tab::take_persisted_blocks)
    /// precedent: the take drains regardless of a live store).
    pub fn take_transcript_items(&mut self) -> Vec<ait_agent::TranscriptItem> {
        self.transcript.take_new()
    }

    /// The whole accumulated transcript — the items a resumed tab's
    /// next `AgentRequest.prior` hands the worker for in-tab
    /// continuity (R08-T4).
    pub fn transcript_items(&self) -> &[ait_agent::TranscriptItem] {
        self.transcript.items()
    }

    /// [`new`](Self::new) over a caller's session — the seam tests
    /// inject a `with_secret_store` handle through (the
    /// `CommandSuggester::with_secret_store` precedent).
    pub fn with_session(session: AgentSession) -> Self {
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
    /// The newest `PlanUpdated` snapshot — the pinned block's rows
    /// under the title; the newest plan replaces the previous in
    /// place, an empty snapshot clears it.
    pub fn plan(&self) -> &[PlanTodo] {
        &self.plan
    }

    /// Whether the split is open — drives the layout and the tab's
    /// winsize derivation.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether keystrokes route to the input line — implies `open`.
    /// The `suggestion_here`-style gate `app`'s pass-through arm
    /// checks before PTY input.
    pub fn is_focused(&self) -> bool {
        self.open && self.focused
    }

    /// Whether a task is in flight — Enter steers vs. submits, Esc
    /// cancels vs. unfocuses, and the title row's hint reads it.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Whether the gate is parked on a call — `a`/`t`/`d` verdicts
    /// apply while `true` (the approve card shows), and the paint
    /// reserves its row.
    pub fn awaiting_verdict(&self) -> bool {
        self.pending.is_some()
    }

    /// Whether the parked approve card is in denial-reason mode —
    /// every key edits the input (verdict letters type like any
    /// other) and Enter answers `Deny` (us-8). Set by `d` on an
    /// empty input ([`begin_deny_reason`](Self::begin_deny_reason)).
    pub fn deny_reason_active(&self) -> bool {
        self.pending.as_ref().is_some_and(|p| p.denying)
    }

    /// `d` on the approve card with an empty input: the input row
    /// becomes the denial-reason prompt — the verdict keys stop
    /// firing so the reason itself can contain them.
    pub fn begin_deny_reason(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.denying = true;
            self.revision += 1;
        }
    }

    /// Whether the input line is empty — the verdict keys only fire
    /// on an empty buffer, so typed text can never be mistaken for a
    /// verdict and a verdict never swallows a draft.
    pub fn input_is_empty(&self) -> bool {
        self.input.is_empty()
    }

    /// Whether an `ask_user_question` card waits on an answer — the
    /// ask keys (↑/↓ + Enter for choices, typed input for free text)
    /// apply while `true`, and the paint reserves its prompt row.
    pub fn awaiting_answer(&self) -> bool {
        self.pending_ask.is_some()
    }

    /// Whether the pending ask offers choices — `false` means the
    /// card takes a free-text answer from the input line.
    pub fn ask_has_choices(&self) -> bool {
        self.pending_ask_card()
            .is_some_and(|card| !card.choices.is_empty())
    }

    /// Answer the pending approval (Spec R06's three-way card):
    /// `call_id` rides along so the worker ignores a verdict meant
    /// for a call it already resolved. A dead worker toasts — the
    /// task is ending anyway, so the card clears either way.
    pub fn decide(&mut self, verdict: ApprovalVerdict, toast: &mut Option<Toast>) {
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
    pub fn ask_prev(&mut self) {
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
    pub fn ask_next(&mut self) {
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
    pub fn submit_answer(&mut self, toast: &mut Option<Toast>) {
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
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The conversation entries, oldest first.
    /// The conversation scrollback — entries the paint lays out
    /// between the title row and the input line.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The input line's current text — what the paint's input row
    /// tail-clips and the panel cursor trails.
    pub fn input(&self) -> &str {
        &self.input
    }

    /// Whether the view follows the conversation's tail — `false`
    /// after the user wheel-scrolls up; the paint pins to the tail
    /// while `true`, clamps `view_top` otherwise.
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// The top display line while unpinned — the paint clamps it
    /// against each frame's line count.
    pub fn view_top(&self) -> usize {
        self.top.get()
    }

    /// Set the unpinned view's top line — the paint's clamp writes
    /// back through here, same as the wheel handlers'.
    pub fn set_view_top(&self, top: usize) {
        self.top.set(top);
    }

    /// What the last paint actually showed — `(top line, max legal
    /// top)` — the wheel handlers' bounds.
    pub fn painted_view(&self) -> (usize, usize) {
        self.painted.get()
    }

    /// Record what a paint showed — `(top, max_top)`: the bounds
    /// [`scroll_up`](Self::scroll_up)/[`scroll_down`](Self::scroll_down)
    /// wheel against. Every frontend's paint publishes it after its
    /// layout pass.
    pub fn set_painted_view(&self, painted: (usize, usize)) {
        self.painted.set(painted);
    }

    /// The parked approve card's `(tool name, deny-reason mode)` —
    /// what the prompt row above the input line reads while the gate
    /// waits. `None` with no gate parked.
    pub fn pending_approval(&self) -> Option<(&str, bool)> {
        self.pending
            .as_ref()
            .map(|pending| (pending.name.as_str(), pending.denying))
    }

    /// `Alt+A` on a live panel: open-but-unfocused refocuses;
    /// open-and-focused closes (focus dies with the split). The
    /// conversation — and any running task — survives a close.
    pub fn toggle(&mut self) {
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
    pub fn escape(&mut self) {
        if self.running {
            self.session.cancel();
        } else {
            self.focused = false;
        }
        self.revision += 1;
    }

    /// Append one typed character — plain and SHIFT-modified chars
    /// only reach here; everything else is swallowed upstream.
    pub fn push_input(&mut self, c: char) {
        self.input.push(c);
        self.revision += 1;
    }

    /// Backspace — pops one char, a no-op on the empty input.
    pub fn pop_input(&mut self) {
        self.input.pop();
        self.revision += 1;
    }

    /// Take the input line — the Enter path's consume-and-clear.
    pub fn take_input(&mut self) -> String {
        self.revision += 1;
        std::mem::take(&mut self.input)
    }

    /// Submit `request` to the agent loop: on the channel the panel
    /// becomes `running`, the task joins the conversation and the
    /// view pins to the tail; a dead worker toasts the error instead
    /// (the §6 degrade-gracefully path every send shares).
    pub fn submit(&mut self, request: AgentRequest, toast: &mut Option<Toast>) {
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
    pub fn steer(&mut self, message: String, toast: &mut Option<Toast>) {
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
    pub fn drain(&mut self) {
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
    pub fn apply(&mut self, event: AgentEvent) {
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
            AgentEvent::Summarized { .. } => {
                // The worker rewrote the conversation's oldest half
                // into a summary marker (us-22): the status entry
                // makes the rewrite visible — never silent. The
                // transcript fold already recorded the `summary`
                // item; the task keeps running.
                self.entries
                    .push(Entry::Status("conversation summarized".to_string()));
            }
            AgentEvent::Done => {
                self.running = false;
                self.pending = None;
                self.pending_ask = None;
                self.entries.push(Entry::Status(STATUS_DONE.to_string()));
            }
            AgentEvent::Cancelled => {
                self.running = false;
                self.pending = None;
                self.pending_ask = None;
                self.entries
                    .push(Entry::Status(STATUS_CANCELLED.to_string()));
            }
            AgentEvent::BudgetReached => {
                self.running = false;
                self.pending = None;
                self.pending_ask = None;
                self.entries.push(Entry::Status(STATUS_BUDGET.to_string()));
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
    pub fn scroll_up(&mut self, lines: usize) {
        self.top.set(self.painted.get().0.saturating_sub(lines));
        self.pinned = false;
        self.revision += 1;
    }

    /// Scroll the conversation `lines` toward the tail — reaching the
    /// bottom re-pins the view.
    pub fn scroll_down(&mut self, lines: usize) {
        let (top, max) = self.painted.get();
        let next = top.saturating_add(lines).min(max);
        if next == max {
            self.pinned = true;
        } else {
            self.top.set(next);
        }
        self.revision += 1;
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
    pub fn restore(&mut self, items: Vec<TranscriptItem>, plan: Option<Vec<PlanTodo>>) {
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

/// `new()` IS the default: a fresh, focused panel over a fresh
/// session.
impl Default for AgentPanel {
    fn default() -> Self {
        Self::new()
    }
}

// `ait_agent::INTERRUPTED_RESULT` is the rebuilt card's `detail`
// for an `interrupted` close (Spec R08 us-9): a call that never got
// its result is a run that died mid-call — replay closes it with a
// synthetic "interrupted" result, so its card closes the same way
// rather than waiting forever. `ait_agent::result_preview` clips a
// stored `tool_result`'s full text the same way the live
// `ToolUpdate` did (the worker's own preview fn).

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
/// the run died mid-call — so it closes `INTERRUPTED_RESULT`
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
                    id: ask_replay_id(asks),
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
                card.detail = Some(INTERRUPTED_RESULT.to_string());
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
pub fn context_for(tab: &Tab) -> AgentContext {
    AgentContext {
        cwd: suggest::cwd_for(tab),
        shell: tab.title().to_owned(),
        os: std::env::consts::OS.to_owned(),
        output_lines: suggest::output_lines_for(tab),
    }
}

/// One key edited into the panel — the half of the routing
/// `Tab::agent_key` does not need `self` for: chars append (plain and
/// SHIFT), Backspace pops, everything else is swallowed. Enter and
/// Esc are the caller's — they submit/steer/cancel against the tab's
/// state. `true` when the key edited the buffer.
pub fn edit_input(panel: &mut AgentPanel, key: &KeyEvent) -> bool {
    match key.key {
        Key::Char(c) if key.modifiers.is_empty() || key.modifiers == Modifiers::SHIFT => {
            panel.push_input(c);
            true
        }
        Key::Backspace if key.modifiers.is_empty() => {
            panel.pop_input();
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    //! The panel's seams (Spec R06 / issue #85): the widget state
    //! driven through its own methods — no PTY, no model, no TTY.
    //! The paint assertions live beside the frontend paint in `ait`.

    use super::*;
    use crate::config::NoSecrets;
    use ait_agent::PlanTodoStatus;

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
                skills: ait_agent::SkillsCatalog::default(),
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
                skills: ait_agent::SkillsCatalog::default(),
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
        let mut panel = panel();
        for c in "fix".chars() {
            assert!(edit_input(
                &mut panel,
                &KeyEvent::new(Key::Char(c), Modifiers::NONE)
            ));
        }
        assert_eq!(panel.input(), "fix");
        assert!(edit_input(
            &mut panel,
            &KeyEvent::new(Key::Char('X'), Modifiers::SHIFT)
        ));
        assert_eq!(panel.input(), "fixX");
        assert!(edit_input(
            &mut panel,
            &KeyEvent::new(Key::Backspace, Modifiers::NONE)
        ));
        assert_eq!(panel.input(), "fix");
        // Control chords and navigation are swallowed, not edited.
        for key in [
            KeyEvent::new(Key::Char('a'), Modifiers::CONTROL),
            KeyEvent::new(Key::Left, Modifiers::NONE),
            KeyEvent::new(Key::Escape, Modifiers::NONE),
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
                detail: Some(INTERRUPTED_RESULT.to_string()),
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

    /// Spec R08 us-22: a `Summarized` event folds into a `summary`
    /// transcript item AND leaves a visible status entry — the
    /// rewrite is never silent — while the task keeps running.
    #[test]
    fn a_summarized_event_shows_a_status_entry_and_records_the_marker() {
        let mut panel = panel();
        panel.running = true; // staged — a submit would set it
        panel.apply(AgentEvent::TextDelta("on it".into()));
        panel.apply(AgentEvent::Summarized {
            covers_seq: 4,
            text: "the old half".into(),
        });

        assert!(
            matches!(
                panel.entries().last(),
                Some(Entry::Status(text)) if text == "conversation summarized"
            ),
            "{:?}",
            panel.entries()
        );
        assert_eq!(
            panel.transcript_items().last(),
            Some(&ait_agent::TranscriptItem::Summary {
                text: "the old half".into(),
                covers_seq: 4,
            })
        );
        // Non-terminal: the task still runs until Done.
        assert!(panel.running);
        panel.apply(AgentEvent::Done);
        assert!(matches!(
            panel.entries().last(),
            Some(Entry::Status(text)) if text == "done"
        ));
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
}
