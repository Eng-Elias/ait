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
//!   newest assistant entry; `Done`/`Cancelled`/`BudgetReached`/
//!   `Error` are terminal and leave a status line. The variant list
//!   is the seam the tool-card ticket extends: each `Entry` owns its
//!   display shape in [`display_lines`].
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

use ait_agent::{AgentContext, AgentEvent, AgentRequest, AgentSession};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier, Style};

use crate::status::Toast;
use crate::suggest;
use crate::tabs::Tab;

/// The input line's prompt marker — user text (a submitted task and a
/// mid-run steering message alike) carries the same marker in the
/// scrollback.
const INPUT_PROMPT: &str = "› ";

/// [`INPUT_PROMPT`]'s cell width — `›` is three bytes but one column;
/// input-line math works in cells, never `str::len`.
const PROMPT_WIDTH: usize = 2;

/// One conversation entry — what the panel's scrollback is made of.
/// The tool-card ticket extends this enum with the variants its new
/// `AgentEvent`s carry (tool-call status, approval requests,
/// `ask_user_question` prompts); each variant owns its display shape
/// in [`display_lines`].
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
    /// A terminal outcome line: the task finished, the user cancelled,
    /// the step budget tripped, or a failure message.
    Status(String),
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
}

impl AgentPanel {
    /// A fresh panel, opened and focused — what `Alt+A` leaves behind
    /// on a tab that had none.
    pub(crate) fn new() -> Self {
        Self::with_session(AgentSession::new())
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
            pinned: true,
            top: Cell::new(0),
            painted: Cell::new((0, 0)),
            revision: 0,
        }
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
    /// own), and every terminal event releases `running` and leaves a
    /// `Status` line the user can read against the stream above it.
    pub(crate) fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(text) => match self.entries.last_mut() {
                Some(Entry::Text(last)) => last.push_str(&text),
                _ => self.entries.push(Entry::Text(text)),
            },
            AgentEvent::Done => {
                self.running = false;
                self.entries.push(Entry::Status("done".to_string()));
            }
            AgentEvent::Cancelled => {
                self.running = false;
                self.entries.push(Entry::Status("cancelled".to_string()));
            }
            AgentEvent::BudgetReached => {
                self.running = false;
                self.entries
                    .push(Entry::Status("step budget reached".to_string()));
            }
            AgentEvent::Error(message) => {
                self.running = false;
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

    /// Push one entry and bump the revision — the single write point
    /// the scrollback has.
    fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
        self.revision += 1;
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
    // half the user is editing).
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
    if inner.height < 3 {
        return;
    }
    // The conversation between them, pinned to the tail unless the
    // user scrolled up — the paint pass owns the wrap, so it also
    // owns the scroll clamp: `top`/`painted` are what the wheel
    // handlers bound against.
    let conversation = display_lines(&panel.entries, width);
    let height = usize::from(input_y - (inner.y + 1));
    let max_top = conversation.len().saturating_sub(height);
    let top = if panel.pinned {
        max_top
    } else {
        panel.top.get().min(max_top)
    };
    panel.painted.set((top, max_top));
    for row in 0..height {
        let y = inner.y + 1 + row as u16;
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
        let (text, style) = match entry {
            Entry::Task(text) | Entry::Steering(text) => (
                format!("{INPUT_PROMPT}{text}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Entry::Text(text) => (text.clone(), Style::default()),
            Entry::Status(text) => (text.clone(), Style::default().fg(RtColor::DarkGray)),
        };
        for line in wrap(&text, width) {
            lines.push((line, style));
        }
    }
    lines
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
                task: "fix it".to_string(),
                context: AgentContext {
                    cwd: "/tmp".into(),
                    shell: "sh".into(),
                    os: "test".into(),
                    output_lines: Vec::new(),
                },
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
}
