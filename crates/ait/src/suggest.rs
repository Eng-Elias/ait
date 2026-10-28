//! The `# `+Enter trigger (Spec R04 — user stories 1–2, 13–17, 29):
//! detection and context-collection glue only. The model call lives in
//! `ait-agent` (CODING_STANDARDS §9 — the TUI holds no business logic);
//! streaming the reply into the input line is T5.
//!
//! Detection has two halves:
//!
//! - [`InputShadow`] — a per-tab shadow of the printable input since
//!   the last line submission, updated at the one place every
//!   keystroke becomes PTY bytes (`app`'s pass-through arm, before
//!   [`LiveSession::send_key`](ait_term::LiveSession::send_key));
//! - the grid check on submission — [`InputShadow::request_text`]
//!   fires only when the shadow starts with `# ` AND the grid's
//!   current logical line (the cursor row walked over soft-wrap
//!   boundaries, [`logical_line`]) ENDS WITH it: the shell really
//!   echoed the request (user story 13).
//!
//! Honesty rules: anything the shadow cannot replay — arrows (history
//! recall, mid-line editing), control characters, editing keys,
//! pastes — marks it *stale*, and a stale shadow never fires; the next
//! submission clears it. Enter is forwarded to the PTY either way:
//! `# …` is a comment no-op in bash/zsh/fish, lands in history and
//! costs nothing when the request fails (us-2). A desynced shadow that
//! slipped past the staleness flag (a REPL or heredoc whose echo just
//! happens to differ) still fails `ends_with` — the failure mode is
//! "no trigger", never a wrong trigger.
//!
//! [`request_for`] bundles the verified request with the §5.4 context
//! the model may see: cwd (OSC 7 when the shell reports it, else
//! `/proc/<pid>/cwd` via [`LiveSession::child_pid`], else the spawn
//! cwd — us-15), shell name and OS (us-17), and the last
//! [`OUTPUT_LINE_BUDGET`] logical grid lines (us-16 — credential
//! redaction runs inside `ait-agent` at assembly time, us-18). The
//! whole trigger is the testable seam (us-29): every case below is
//! exercised without a PTY via byte-fed [`ait_term::Session`]s.

use ait_agent::{SuggestRequest, OUTPUT_LINE_BUDGET};
use ait_config::Settings;
use ait_term::{Cursor, GridView, Key, KeyEvent, Keypad, Modifiers};

use crate::tabs::Tab;

/// The request prefix: a submitted input line starting `# ` is a
/// natural-language command request — while remaining a perfectly
/// ordinary comment to the shell (us-1/2).
const TRIGGER_PREFIX: &str = "# ";

/// A per-tab shadow of the printable input typed since the last line
/// submission (us-29's seam). It exists because the terminal cannot
/// ask the shell what its line buffer holds — it can only watch the
/// keystrokes it forwards and check the result against what the shell
/// echoed into the grid.
///
/// The shadow is deliberately pessimistic: it only tracks input it can
/// replay faithfully (printable characters appended, Backspace
/// popping). Any key that moves the readline cursor or edits through
/// side channels — arrows, Home/End, Delete, Tab completion, control
/// characters, Alt-modified input — marks it **stale**, because the
/// linear text can no longer be trusted to mirror the line. Stale is
/// sticky until the next submission: a user who arrowed mid-line to
/// edit degrades to no-trigger rather than a wrong-trigger.
///
/// [`InputShadow::update`] runs on every PTY-bound key, BEFORE the key
/// is sent — ordering with [`request_text`](Self::request_text) is the
/// caller's: verify first, then let the submission reset.
#[derive(Debug, Default)]
pub struct InputShadow {
    /// The input as typed — printable characters appended, Backspace
    /// popping. Meaningless once `stale`.
    text: String,
    /// Keys arrived that a linear text cannot replay (arrows, control
    /// input, edits, paste). Sticky until the next submission.
    stale: bool,
}

impl InputShadow {
    /// An empty, in-sync shadow — the state right after a submission.
    pub fn new() -> Self {
        Self::default()
    }

    /// The shadowed input line — empty while nothing printable has
    /// arrived since the last submission.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether keystrokes the shadow cannot replay have desynced it
    /// from the shell's line. A stale shadow never fires
    /// ([`request_text`](Self::request_text) returns `None`); the next
    /// submission clears it.
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Record one PTY-bound key.
    ///
    /// Update rules, in order:
    ///
    /// - a submission ([`is_submission`]: any Enter but `Alt+Enter`)
    ///   resets — the line just left the buffer;
    /// - a plain or SHIFT-modified [`Key::Char`] appends — crossterm
    ///   reports the already-shifted character, so `SHIFT` is honest;
    /// - a plain [`Key::Backspace`] pops one character;
    /// - EVERYTHING ELSE marks the shadow stale: arrows and navigation
    ///   move the cursor where a linear text cannot follow (history
    ///   recall is just `Up` to the shadow), control characters edit
    ///   through readline, `Delete` removes ahead of the cursor, and
    ///   `Alt+Enter` does not submit (`ESC CR`) so it poisons rather
    ///   than resets.
    pub fn update(&mut self, event: &KeyEvent) {
        if is_submission(event) {
            self.text.clear();
            self.stale = false;
            return;
        }
        match event.key {
            Key::Char(c) if event.modifiers.is_empty() || event.modifiers == Modifiers::SHIFT => {
                self.text.push(c);
            }
            Key::Backspace if event.modifiers.is_empty() => {
                self.text.pop();
            }
            _ => self.mark_stale(),
        }
    }

    /// Mark the shadow stale without a key — a `Shift+Insert` paste
    /// (arbitrary bytes, newlines included) or any other input path
    /// that bypasses [`update`](Self::update).
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// The verified request, or `None` — the pure trigger seam
    /// (us-29): `Some` only when the shadow is in sync, starts with
    /// `# `, carries a non-empty request, and `logical_line` (the
    /// grid's current logical input line, prompt included) ENDS WITH
    /// it. The returned text is the request verbatim — the shadow
    /// minus the prefix (us-19).
    ///
    /// `ends_with` rather than equality because the logical line leads
    /// with the prompt (`$ `, `>>> `, …). Trailing blanks are ignored
    /// on both sides — grid rows are space-padded, and the echoed text
    /// is what the user meant.
    pub fn request_text(&self, logical_line: &str) -> Option<String> {
        if self.is_stale() {
            return None;
        }
        let shadow = self.text().trim_end();
        let request = shadow.strip_prefix(TRIGGER_PREFIX)?;
        if request.is_empty() {
            return None;
        }
        logical_line
            .trim_end()
            .ends_with(shadow)
            .then(|| request.to_owned())
    }
}

/// Whether `event` submits the input line: an Enter key whose encoding
/// the shell accepts as newline. The core encodes `Key::Enter` and
/// `Keypad::Enter` to `\r` (or SS3) for every modifier set EXCEPT ALT —
/// `Alt+Enter` sends `ESC CR`, which readline does not submit by
/// default (and the T6 `Alt+Enter` binding resolves before pass-through
/// anyway), so it is not a submission here.
pub fn is_submission(event: &KeyEvent) -> bool {
    matches!(event.key, Key::Enter | Key::Keypad(Keypad::Enter))
        && !event.modifiers.contains(Modifiers::ALT)
}

/// The grid's current logical input line: the cursor row joined with
/// any soft-wrapped rows above and below it, each row end-trimmed.
/// A logical line can start in scrollback (the walk allows negative
/// row indices) and the cursor can sit mid-line after editing, so the
/// walk extends in both directions over [`Row::wrapped`](ait_term::Row)
/// rows.
pub fn logical_line(grid: &GridView, cursor: Cursor) -> String {
    let cursor_line = cursor.line as i32;
    // Walk up while the row above soft-wraps into its successor.
    let mut start = cursor_line;
    while grid.line(start - 1).is_some_and(|row| row.wrapped()) {
        start -= 1;
    }
    // Walk down while the row itself wraps into the next.
    let mut end = cursor_line;
    while grid.line(end).is_some_and(|row| row.wrapped()) {
        end += 1;
    }
    let mut line = String::new();
    for index in start..=end {
        if let Some(row) = grid.line(index) {
            line.push_str(row.text().trim_end());
        }
    }
    line
}

/// The last [`OUTPUT_LINE_BUDGET`] logical rows of grid output —
/// scrollback then viewport, oldest first — with trailing blank rows
/// dropped (the live cursor sits above mostly-blank rows). These are
/// the RAW lines the request may carry (us-16); redaction is
/// `ait-agent`'s job inside `assemble_request` (us-18).
pub fn recent_output_lines(grid: &GridView) -> Vec<String> {
    let mut lines: Vec<String> = grid
        .scrollback_rows()
        .chain(grid.visible_rows())
        .map(|row| row.text().trim_end().to_owned())
        .collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    let skip = lines.len().saturating_sub(OUTPUT_LINE_BUDGET);
    lines.drain(..skip);
    lines
}

/// The cwd a request reports (us-15): the OSC-7-reported directory
/// when the shell reports one — the only honest answer when the shell
/// could be a remote or container session. When OSC 7 never fired,
/// `/proc/<pid>/cwd` covers vanilla shells (a `cd` without shell
/// integration still shows up there); the spawn cwd is the last
/// resort on hosts without `/proc` (non-Linux) or a child already gone.
fn cwd_for(tab: &Tab) -> String {
    if !tab.cwd_reported() {
        if let Some(pid) = tab.session.child_pid() {
            if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                return path.display().to_string();
            }
        }
    }
    tab.cwd().display().to_string()
}

/// The verified `# ` request for `tab` with its §5.4 context, or
/// `None` — the trigger seam in one call (us-13, 29). On `Some` the
/// caller forwards Enter to the PTY normally (us-2) and sends the
/// returned [`SuggestRequest`] into the `CommandSuggester`; on `None`
/// nothing about the keypress changes.
pub fn request_for(tab: &Tab, settings: &Settings) -> Option<SuggestRequest> {
    let grid = tab.grid();
    let request = tab
        .shadow
        .request_text(&logical_line(&grid, tab.cursor()))?;
    Some(SuggestRequest {
        settings: settings.clone(),
        cwd: cwd_for(tab),
        // The spawned program's basename — the same source of truth
        // the tab title uses (us-17).
        shell: tab.title().to_owned(),
        os: std::env::consts::OS.to_owned(),
        output_lines: recent_output_lines(&grid),
        request,
    })
}

#[cfg(test)]
mod tests {
    //! Trigger-detection tests at the agreed seams (us-29): the shadow
    //! updated key-by-key, real byte-fed `Session` grids for the
    //! logical-line walk — no PTY, shell or network anywhere.

    use super::*;
    use ait_term::{Session, SessionOptions, Size};
    use std::ffi::OsString;
    use std::time::{Duration, Instant};

    fn char_key(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), Modifiers::NONE)
    }

    /// A shadow that saw `text` typed character by character.
    fn typed(text: &str) -> InputShadow {
        let mut shadow = InputShadow::new();
        for c in text.chars() {
            shadow.update(&char_key(c));
        }
        shadow
    }

    /// A byte-fed session showing `output` — canned grid states for
    /// the verification seam without a live shell.
    fn session(columns: usize, output: &[u8]) -> Session {
        let mut session = Session::new(SessionOptions {
            size: Size::new(columns, 10),
            ..Default::default()
        });
        session.feed_bytes(output);
        session
    }

    /// The logical input line at `session`'s cursor.
    fn logical(session: &Session) -> String {
        logical_line(&session.grid(), session.cursor())
    }

    /// Printable characters accumulate — including SHIFT-modified
    /// ones, which crossterm reports already-shifted.
    #[test]
    fn printable_chars_accumulate() {
        let mut shadow = typed("# a");
        shadow.update(&KeyEvent::new(Key::Char('B'), Modifiers::SHIFT));
        assert_eq!(shadow.text(), "# aB");
        assert!(!shadow.is_stale());
    }

    /// Backspace pops one character; on an empty shadow it is a no-op.
    #[test]
    fn backspace_pops() {
        let mut shadow = typed("# lsz");
        shadow.update(&KeyEvent::new(Key::Backspace, Modifiers::NONE));
        assert_eq!(shadow.text(), "# ls");
        for _ in 0..5 {
            shadow.update(&KeyEvent::new(Key::Backspace, Modifiers::NONE));
        }
        assert_eq!(shadow.text(), "");
    }

    /// Enter resets: the next line starts a fresh shadow — for every
    /// Enter variant whose encoding submits (`Ctrl+Enter` is `\r` too).
    /// `Alt+Enter` is `ESC CR` and poisons instead.
    #[test]
    fn enter_resets_the_shadow() {
        for modifiers in [Modifiers::NONE, Modifiers::SHIFT, Modifiers::CONTROL] {
            for key in [Key::Enter, Key::Keypad(Keypad::Enter)] {
                let mut shadow = typed("# ls");
                shadow.update(&KeyEvent::new(key, modifiers));
                assert_eq!(shadow.text(), "", "{key:?} + {modifiers:?}");
                assert!(!shadow.is_stale());
            }
        }
        let mut shadow = typed("# ls");
        shadow.update(&KeyEvent::new(Key::Enter, Modifiers::ALT));
        assert!(shadow.is_stale(), "Alt+Enter does not submit");
    }

    /// Anything the shadow cannot honestly replay — arrows (history
    /// recall, mid-line editing), control characters, Tab, Delete —
    /// marks it stale (us-13).
    #[test]
    fn unreplayable_keys_mark_stale() {
        for event in [
            KeyEvent::new(Key::Up, Modifiers::NONE),
            KeyEvent::new(Key::Down, Modifiers::NONE),
            KeyEvent::new(Key::Left, Modifiers::NONE),
            KeyEvent::new(Key::Right, Modifiers::NONE),
            KeyEvent::new(Key::Home, Modifiers::NONE),
            KeyEvent::new(Key::End, Modifiers::NONE),
            KeyEvent::new(Key::Delete, Modifiers::NONE),
            KeyEvent::new(Key::Tab, Modifiers::NONE),
            KeyEvent::new(Key::Escape, Modifiers::NONE),
            KeyEvent::new(Key::Char('c'), Modifiers::CONTROL),
            KeyEvent::new(Key::Char('x'), Modifiers::ALT),
        ] {
            let mut shadow = typed("# ls");
            shadow.update(&event);
            assert!(shadow.is_stale(), "{event:?} should desync the shadow");
        }
        // Once stale, further typing does not resurrect it — a
        // mid-line edit degrades to no-trigger, never a wrong trigger.
        let mut shadow = typed("# l");
        shadow.update(&KeyEvent::new(Key::Up, Modifiers::NONE));
        shadow.update(&char_key('s'));
        assert!(shadow.is_stale());

        // And the next submission clears it.
        shadow.update(&KeyEvent::new(Key::Enter, Modifiers::NONE));
        assert!(!shadow.is_stale());
        assert_eq!(shadow.text(), "");
    }

    /// The pure trigger check on (shadow, logical line): an agreeing
    /// `# ` line fires its request verbatim, prompt prefix ignored.
    #[test]
    fn agreeing_hash_line_fires() {
        assert_eq!(
            typed("# list files").request_text("user@box:~$ # list files"),
            Some("list files".to_string())
        );
    }

    /// No `# ` prefix, an empty request, a stale shadow, or a grid
    /// line that does not end with the shadow — no trigger (us-13).
    #[test]
    fn only_a_verified_hash_line_fires() {
        // Plain commands never trigger.
        assert_eq!(typed("ls -la").request_text("$ ls -la"), None);
        // `#` alone is not a request.
        assert_eq!(typed("#").request_text("$ #"), None);
        assert_eq!(typed("# ").request_text("$ # "), None);
        // A stale shadow never fires — not even on an agreeing line.
        let mut stale = typed("# list files");
        stale.update(&KeyEvent::new(Key::Left, Modifiers::NONE));
        assert_eq!(stale.request_text("$ # list files"), None);
        // History recall / heredoc / REPL desyncs: the line on the
        // grid is not the shadow's line.
        assert_eq!(typed("# list files").request_text("$ make install"), None);
        assert_eq!(typed("# list files").request_text(">>> print(1)"), None);
        assert_eq!(typed("# list files").request_text("> some body"), None);
        // Trailing edits desync: "# list filesX" does not end with
        // "# list files".
        assert_eq!(typed("# list files").request_text("$ # list filesX"), None);
    }

    /// The grid side of the check: a prompt + echoed `# ` line at the
    /// cursor fires through `logical_line` + `request_text` (us-1).
    #[test]
    fn grid_echo_agreement_fires() {
        let session = session(40, b"$ # list files");
        assert_eq!(logical(&session), "$ # list files");
        assert_eq!(
            typed("# list files").request_text(&logical(&session)),
            Some("list files".to_string())
        );
    }

    /// A request wider than the grid soft-wraps; the logical line
    /// joins the wrapped rows and still ends with the shadow.
    #[test]
    fn wrapped_input_line_still_fires() {
        let session = session(10, b"$ # list all files here");
        assert_eq!(logical(&session), "$ # list all files here");
        assert_eq!(
            typed("# list all files here").request_text(&logical(&session)),
            Some("list all files here".to_string())
        );
    }

    /// History recall: the `Up` already marked the shadow stale, and
    /// the recalled line bears no relation to it — either guard alone
    /// would suffice; together they make the wrong trigger impossible
    /// (us-13).
    #[test]
    fn history_recall_desync_does_not_fire() {
        let mut shadow = typed("# list files");
        shadow.update(&KeyEvent::new(Key::Up, Modifiers::NONE));
        let session = session(40, b"$ make install");
        assert_eq!(shadow.request_text(&logical(&session)), None);
    }

    /// A REPL or heredoc where the echoed continuation differs from
    /// the shadow: `ends_with` refuses (us-13).
    #[test]
    fn continuation_desync_does_not_fire() {
        let session = session(40, b"$ cat <<EOF\r\n> some typed body");
        assert_eq!(
            typed("# some typed body").request_text(&logical(&session)),
            None
        );
    }

    /// Context lines: scrollback + viewport, end-trimmed, trailing
    /// blanks dropped, capped at [`OUTPUT_LINE_BUDGET`] (us-16).
    #[test]
    fn recent_output_lines_keeps_the_tail() {
        let mut output = Vec::new();
        for i in 0..120 {
            output.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        output.extend_from_slice(b"$ ");
        let session = session(40, &output);
        let lines = recent_output_lines(&session.grid());
        assert_eq!(lines.len(), OUTPUT_LINE_BUDGET);
        assert_eq!(lines.first().unwrap(), "line 21");
        assert_eq!(lines.last().unwrap(), "$");
    }

    /// A fresh grid carries no output context.
    #[test]
    fn recent_output_lines_on_a_fresh_grid_is_empty() {
        let session = session(40, b"");
        assert!(recent_output_lines(&session.grid()).is_empty());
    }

    /// Only Enter variants that encode a newline submit — `Alt+Enter`
    /// is `ESC CR` and must not reset or trigger.
    #[test]
    fn enter_variants_that_send_cr_submit() {
        assert!(is_submission(&KeyEvent::new(Key::Enter, Modifiers::NONE)));
        assert!(is_submission(&KeyEvent::new(Key::Enter, Modifiers::SHIFT)));
        assert!(is_submission(&KeyEvent::new(
            Key::Enter,
            Modifiers::CONTROL
        )));
        assert!(is_submission(&KeyEvent::new(
            Key::Keypad(Keypad::Enter),
            Modifiers::NONE
        )));
        assert!(!is_submission(&KeyEvent::new(Key::Enter, Modifiers::ALT)));
        assert!(!is_submission(&KeyEvent::new(
            Key::Char('x'),
            Modifiers::NONE
        )));
    }

    /// us-15: a vanilla `/bin/sh` reports no OSC 7, so `/proc/<pid>/cwd`
    /// answers where it exists (the spawn cwd otherwise); once the
    /// shell reports OSC 7 the reported path wins outright. Real PTY
    /// spawn — needs no host TTY (same allowance as the tabs tests).
    #[test]
    fn cwd_falls_back_to_proc_then_prefers_osc7() {
        use crate::tabs::TabManager;
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            LiveSessionOptions {
                program: Some(OsString::from("/bin/sh")),
                ..Default::default()
            },
        )
        .unwrap();

        // No OSC 7 yet: `/proc/<pid>/cwd` where the host has /proc,
        // else the spawn cwd — but never an empty answer.
        {
            let tab = tabs.active().unwrap();
            assert!(!tab.cwd_reported());
            let cwd = cwd_for(tab);
            let expected = tab
                .session
                .child_pid()
                .and_then(|pid| std::fs::read_link(format!("/proc/{pid}/cwd")).ok())
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| tab.cwd().display().to_string());
            assert_eq!(cwd, expected);
            assert!(!cwd.is_empty());
        }

        // Emit OSC 7 through the child — the reported path then wins.
        tabs.active()
            .unwrap()
            .session
            .send_bytes(b"printf '\\033]7;file://h/tmp/osc7\\a'\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            tabs.drain_events();
            if tabs.active().unwrap().cwd_reported() {
                break;
            }
            assert!(Instant::now() < deadline, "OSC 7 never surfaced");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(cwd_for(tabs.active().unwrap()), "/tmp/osc7");
    }

    /// The full trigger bundle (us-15–17): a `# ` line echoed by a
    /// real `/bin/sh` produces a `SuggestRequest` carrying the request
    /// verbatim, the tab's shell name, the OS, the cwd and the output
    /// tail — plus the caller's `Settings` snapshot (us-23).
    #[test]
    fn request_for_collects_context() {
        use crate::tabs::TabManager;
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            LiveSessionOptions {
                program: Some(OsString::from("/bin/sh")),
                ..Default::default()
            },
        )
        .unwrap();
        // `/bin/sh` on a PTY is interactive: wait for its prompt
        // BEFORE writing the request, or the kernel echo can land
        // ahead of the prompt and bury the text mid-line.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            tabs.drain_events();
            let tab = tabs.active().unwrap();
            if !logical_line(&tab.grid(), tab.cursor()).is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "prompt never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        tabs.active()
            .unwrap()
            .session
            .send_bytes(b"# list files")
            .unwrap();
        loop {
            tabs.drain_events();
            let tab = tabs.active().unwrap();
            if logical_line(&tab.grid(), tab.cursor()).ends_with("# list files") {
                break;
            }
            assert!(Instant::now() < deadline, "echo never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Stage the shadow as `handle_event` would have.
        {
            let tab = tabs.active_mut().unwrap();
            for c in "# list files".chars() {
                tab.shadow.update(&char_key(c));
            }
        }

        let settings = Settings::default();
        let request = request_for(tabs.active().unwrap(), &settings).expect("must fire");

        assert_eq!(request.request, "list files");
        assert_eq!(request.shell, "sh");
        assert_eq!(request.os, std::env::consts::OS);
        assert!(!request.cwd.is_empty());
        assert_eq!(request.settings, settings);
        assert!(
            request
                .output_lines
                .iter()
                .any(|line| line.ends_with("# list files")),
            "the echoed request line is part of the output tail"
        );
    }
}
