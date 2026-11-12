//! The `# `+Enter and `Alt+Enter` triggers (Spec R04 — user stories
//! 1–2, 11–17, 29) plus the in-flight request state machine
//! ([`SuggestionPin`] — user stories 7–10, 14): the detection halves,
//! the context collection, and the machinery that pins a fired request
//! to its origin tab, streams sanitized `Delta`s into it under one
//! bracketed-paste envelope, and releases the pin on `Done`/`Error`/
//! Esc-cancel. The model call lives in `ait-agent` (CODING_STANDARDS
//! §9 — the TUI holds no business logic); `app` keeps only the thin
//! call-site glue — the keypress dispatch and the per-tick drain.
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
//!   echoed the request (user story 13). `Alt+Enter` runs the same
//!   check via [`InputShadow::verified_line`], minus the `# `
//!   requirement — the whole verified line is the request (us-11).
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
//! `/proc/<pid>/cwd` via [`LiveSession::child_pid`] on NON-INTEGRATED
//! tabs only — Spec R05, issue #76 — else the spawn cwd — us-15),
//! shell name and OS (us-17), and the output tail (us-16): block text
//! when the tab has blocks (Spec R05 us-31 — a "retry that with sudo"
//! request sees the failed command's real output), else the last
//! [`OUTPUT_LINE_BUDGET`] logical grid lines; credential redaction
//! runs inside `ait-agent` at assembly time either way (us-18). The
//! whole trigger is the testable seam (us-29): every case below is
//! exercised without a PTY via byte-fed [`ait_term::Session`]s.

use std::sync::mpsc::Receiver;
use std::time::Instant;

use ait_agent::{
    classify, CommandSuggester, InputClass, ProgramIndex, SuggestEvent, SuggestRequest,
    OUTPUT_LINE_BUDGET,
};
use ait_config::{ClassifierMode, Settings};
use ait_term::{Cursor, GridView, Key, KeyEvent, Keypad, Modifiers};

use crate::blocks::Block;
use crate::integration::IntegrationState;
use crate::selection::{extract, GridPoint, SelectionRange, PASTE_BEGIN, PASTE_END};
use crate::status::Toast;
use crate::tabs::{Tab, TabManager};

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

    /// Fold pasted text into the shadow — a clipboard paste IS user
    /// input the shadow can replay (unlike `ait ctl send`, whose
    /// bytes stay unverifiable — Spec R11 us-16): printable text
    /// appends — the shell echoes it onto the line, so
    /// [`verified_shadow`](Self::verified_shadow)'s echo-agreement
    /// still gates — while any control byte (a newline submits, a
    /// tab completes, an escape edits) is unreplayable and the
    /// shadow falls back to stale.
    pub fn update_pasted(&mut self, text: &str) {
        if text.chars().any(|c| c.is_control()) {
            self.mark_stale();
        } else {
            self.text.push_str(text);
        }
    }

    /// Clear the shadow back to the post-submission state — text and
    /// staleness both gone. The T5 Esc-cancel path wipes the input
    /// line with a `Ctrl+U` byte, which the shadow cannot replay as
    /// keystrokes; resetting (rather than marking stale) is honest —
    /// the line really is empty — and lets the next `# ` line trigger.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The shared verification tail of [`request_text`](Self::request_text)
    /// and [`verified_line`](Self::verified_line): the shadow's trimmed
    /// text, `Some` only when the shadow is in sync, non-empty, and
    /// `logical_line` (the grid's current logical input line, prompt
    /// included) ENDS WITH it — the echo-agreement check both triggers
    /// run (us-13). `ends_with` rather than equality because the logical
    /// line leads with the prompt (`$ `, `>>> `, …); trailing blanks are
    /// ignored on both sides — grid rows are space-padded, and the
    /// echoed text is what the user meant.
    fn verified_shadow(&self, logical_line: &str) -> Option<&str> {
        if self.is_stale() {
            return None;
        }
        let shadow = self.text().trim_end();
        if shadow.is_empty() {
            return None;
        }
        logical_line.trim_end().ends_with(shadow).then_some(shadow)
    }

    /// The verified request, or `None` — the pure trigger seam
    /// (us-29): `Some` only when the shadow verifies through
    /// [`verified_shadow`](Self::verified_shadow), starts with `# `,
    /// and carries a non-empty request. The returned text is the
    /// request verbatim — the shadow minus the prefix (us-19).
    pub fn request_text(&self, logical_line: &str) -> Option<String> {
        let request = self
            .verified_shadow(logical_line)?
            .strip_prefix(TRIGGER_PREFIX)?;
        if request.is_empty() {
            return None;
        }
        Some(request.to_owned())
    }

    /// The verified input line for the `Alt+Enter` trigger (us-11):
    /// `Some` only when the shadow verifies through
    /// [`verified_shadow`](Self::verified_shadow) — the same
    /// echo-agreement check [`request_text`](Self::request_text) runs,
    /// minus the `# ` prefix requirement. The returned text is the
    /// whole line — a `# ` line keeps its prefix, because here the
    /// line itself is the request (us-19). An empty, whitespace-only,
    /// stale or unverifiable line returns `None`: desync is inert.
    pub fn verified_line(&self, logical_line: &str) -> Option<String> {
        self.verified_shadow(logical_line).map(str::to_owned)
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

/// The §5.4 output tail for `tab` (Spec R05 user story 31 — the seam
/// Spec R04 declared): block text when the tab has blocks, so a
/// "retry that with sudo" request sees the failed command's real
/// output; the raw grid tail [`recent_output_lines`] when it has none
/// — a shell emitting nothing degrades exactly as before (§5.5's
/// graceful-degrade clause). Both sources stay
/// [`OUTPUT_LINE_BUDGET`]-bounded: "recent terminal output" is always
/// a last-N answer. `pub`: the agent panel's `AgentContext`
/// assembly asks the same question (Spec R06 §5.6).
pub fn output_lines_for(tab: &Tab) -> Vec<String> {
    let blocks = tab.blocks();
    if blocks.is_empty() {
        recent_output_lines(&tab.grid())
    } else {
        block_output_lines(&tab.grid(), &blocks)
    }
}

/// Block text as context lines, oldest first, capped at the newest
/// [`OUTPUT_LINE_BUDGET`]. Each block contributes its whole clamped
/// live range — the command echo row (prompt included, so the model
/// sees the command as typed) through its last output row — read
/// through [`extract`], the same machinery a block copy uses. Blocks
/// walk newest→oldest and stop once the bound is covered, so ancient
/// history costs nothing.
fn block_output_lines(grid: &GridView<'_>, blocks: &[Block]) -> Vec<String> {
    let scrollback = i64::try_from(grid.scrollback()).unwrap_or(i64::MAX);
    // Live scrollback-then-viewport index → `GridView::line` space.
    let to_grid_line = |line: usize| -> i32 {
        let line = i64::try_from(line).unwrap_or(i64::MAX) - scrollback;
        line.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    let mut lines: Vec<String> = Vec::new();
    for block in blocks.iter().rev() {
        let range = block.range();
        let text = extract(
            grid,
            SelectionRange {
                start: GridPoint::new(to_grid_line(*range.start()), 0),
                // `usize::MAX` caps at the row's content end — the same
                // full-line bound a block copy ends on.
                end: GridPoint::new(to_grid_line(*range.end()), usize::MAX),
            },
        );
        // Newest→oldest walk: prepend this block's lines.
        let mut older: Vec<String> = text.lines().map(str::to_owned).collect();
        older.append(&mut lines);
        lines = older;
        if lines.len() >= OUTPUT_LINE_BUDGET {
            break;
        }
    }
    let skip = lines.len().saturating_sub(OUTPUT_LINE_BUDGET);
    lines.drain(..skip);
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// The cwd a request reports (us-15): the OSC-7-reported directory
/// when the shell reports one — the only honest answer when the shell
/// could be a remote or container session. When OSC 7 never fired,
/// `/proc/<pid>/cwd` covers NON-INTEGRATED tabs (Spec R05 / issue #76
/// — an `Integrated` shell always reports OSC 7, so a still-unreported
/// cwd there means the report was lost, not missing, and `/proc` can
/// only describe the local child); the spawn cwd is the last resort on
/// hosts without `/proc` (non-Linux) or a child already gone.
/// `pub`: the agent panel reports the same cwd (Spec R06 §5.6).
pub fn cwd_for(tab: &Tab) -> String {
    if reads_proc_cwd(tab.cwd_reported(), tab.integration_state()) {
        if let Some(pid) = tab.session.child_pid() {
            if let Ok(path) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                return path.display().to_string();
            }
        }
    }
    tab.cwd().display().to_string()
}

/// Whether `cwd_for`'s `/proc/<pid>/cwd` fallback applies — the pure
/// gate Spec R05 revisits (issue #76): only when the shell never
/// reported a cwd AND the tab is not `Integrated`. Integrated shells
/// always emit OSC 7, so `/proc` can never be their missing report —
/// it would only ever leak the local child's directory into a remote
/// or container session's context.
fn reads_proc_cwd(cwd_reported: bool, integration: IntegrationState) -> bool {
    !cwd_reported && integration != IntegrationState::Integrated
}

/// The §5.4 context bundle around a verified `request` — the tail
/// [`request_for`] and [`line_request_for`] share (us-15–17, 23).
/// `profile` is the app's active execution profile (Spec R07 §5.7):
/// it rides the request beside the settings snapshot, so a switch
/// applies to the NEXT submission.
fn assemble(tab: &Tab, settings: &Settings, profile: &str, request: String) -> SuggestRequest {
    SuggestRequest {
        settings: settings.clone(),
        profile: profile.to_owned(),
        cwd: cwd_for(tab),
        // The spawned program's basename — the same source of truth
        // the tab title uses (us-17).
        shell: tab.title().to_owned(),
        os: std::env::consts::OS.to_owned(),
        output_lines: output_lines_for(tab),
        request,
    }
}

/// The verified `# ` request for `tab` with its §5.4 context, or
/// `None` — the trigger seam in one call (us-13, 29). On `Some` the
/// caller forwards Enter to the PTY normally (us-2) and sends the
/// returned [`SuggestRequest`] into the `CommandSuggester`; on `None`
/// nothing about the keypress changes.
pub fn request_for(tab: &Tab, settings: &Settings, profile: &str) -> Option<SuggestRequest> {
    let request = tab
        .shadow
        .request_text(&logical_line(&tab.grid(), tab.cursor()))?;
    Some(assemble(tab, settings, profile, request))
}

/// The verified `Alt+Enter` request for `tab` — the WHOLE input line
/// as the request (us-11; no `# ` prefix needed) — with the same §5.4
/// context [`request_for`] assembles. On `Some` the caller erases the
/// typed line (us-12) and runs the shared request → stream → insert
/// path; `None` (bare, empty, stale or unverifiable) changes nothing —
/// the desync-is-inert rule (us-13).
pub fn line_request_for(tab: &Tab, settings: &Settings, profile: &str) -> Option<SuggestRequest> {
    let request = tab
        .shadow
        .verified_line(&logical_line(&tab.grid(), tab.cursor()))?;
    Some(assemble(tab, settings, profile, request))
}

/// The verdict half of the §5.10.4 classifier trigger (Spec R11-T2):
/// `mode` armed AND `shell_name` a known shell AND `shadow` verifying
/// against `logical_line` — the same echo-agreement check both
/// explicit triggers run — hands the line to [`classify`].
/// `NaturalLanguage` returns the verified line for the
/// [`rewrite_as_comment`] path; every other outcome — `off`, an
/// unknown shell, a `Command` verdict, a stale shadow, echo
/// disagreement — is `None` and the submission stays ordinary: the
/// desync-is-inert rule, and the reason input that never passed
/// through keystrokes (`ait ctl send`, which marks the shadow stale)
/// is never classified (us-10/11/16). The known-shell gate is the
/// `\x15` wipe's exposure: a REPL or any other program that echoes
/// input without readline's kill-to-BOL would get `# <line>` APPENDED
/// to the un-wiped line — a mangled line executes — so the trigger
/// only arms where the wipe is honored (us-10). The pure half of the
/// trigger: byte-fed `Session`s and `with_path_entries` indexes drive
/// it without a PTY or a `$PATH` walk.
fn classified_line(
    shadow: &InputShadow,
    logical_line: &str,
    shell_name: &str,
    index: &ProgramIndex,
    mode: ClassifierMode,
) -> Option<String> {
    if mode == ClassifierMode::Off || !ait_shell::is_known_shell(shell_name) {
        return None;
    }
    let line = shadow.verified_line(logical_line)?;
    (classify(&line, index) == InputClass::NaturalLanguage).then_some(line)
}

/// The third submission trigger (§5.10.4, Spec R11-T2) — the check
/// `app`'s submission arm runs after the `# ` request refuses, while
/// the shadow still holds the line and the grid still echoes it. On
/// `Some` the caller swallows the Enter, arms the returned
/// [`SuggestRequest`] — the `Alt+Enter` whole-line form, so the
/// request lands on the model exactly as if the user had typed the
/// `# ` prefix — and submits [`rewrite_as_comment`]'s bytes instead.
/// `None` changes nothing: `Command` verdicts, `classifier = "off"`,
/// a non-shell program (`ait_shell::is_known_shell` on the tab
/// title — the `\x15` wipe's guard), stale shadows and echo
/// disagreement all forward verbatim (us-10/11/13). The index lives
/// on the tab — harvested once per session spawn — so the lookup
/// costs a set probe, never a `$PATH` walk (us-15).
pub fn classified_request(
    tab: &Tab,
    settings: &Settings,
    profile: &str,
) -> Option<(String, SuggestRequest)> {
    let line = classified_line(
        &tab.shadow,
        &logical_line(&tab.grid(), tab.cursor()),
        tab.title(),
        &tab.program_index,
        settings.input.classifier,
    )?;
    let request = assemble(tab, settings, profile, line.clone());
    Some((line, request))
}

/// The natural-language verdict's rewrite (Spec R11-T2, ADR-0014):
/// `Ctrl+U` (`\x15` — readline's kill-to-start-of-line, the same byte
/// [`SuggestionPin::cancel`] wipes with) erases the typed line, then
/// `# <line>` + `\r` submits the request through the shell as a
/// comment — it lands in history and scrollback exactly like an
/// explicit `# ` request (ADR-0007 semantics) and the fresh prompt
/// that follows is where the armed suggestion streams (us-2/3). The
/// shadow resets rather than going stale: the input line really is
/// empty, so the next line's shadow starts honest. Called only AFTER
/// `fire` accepted the request — a refused request leaves the typed
/// line alone and the Enter forwards verbatim: the erase-after-fire
/// ordering `suggest_from_line` established.
pub fn rewrite_as_comment(tab: &mut Tab, line: &str) {
    let _ = tab.session.send_bytes(b"\x15");
    let _ = tab
        .session
        .send_bytes(format!("{TRIGGER_PREFIX}{line}\r").as_bytes());
    tab.shadow.reset();
}

/// The in-flight suggestion state machine `app` delegates to (Spec R04
/// tickets T5/T6 — user stories 7–10, 14): `Some(InFlightSuggestion)`
/// inside while a request streams, `None` while idle. One request at a
/// time is the suggester's frontend contract, so a live pin also gates
/// the next trigger. `Delta` text streams into the pinned tab's session
/// — one `\x1b[200~`…`\x1b[201~` envelope when the child enabled mode
/// 2004 — and `Error`s land on the status line; Esc on the pinned tab
/// cancels (token + `Ctrl+U` wipe) while Enter is swallowed upstream in
/// `app`'s pass-through arm.
///
/// The pin resolves through the tab's stable [`id`](Tab::id) — a
/// `TabManager` counter immune both to the index shifts a mid-stream
/// tab close causes AND to pid-less platforms where `child_pid` is
/// `None`. A locator that could drift would type suggestion bytes into
/// an unrelated session (us-14), which is why identity — not position —
/// is what gets pinned.
#[derive(Default)]
pub struct SuggestionPin {
    /// The live pin; `None` while no request is in flight.
    inner: Option<InFlightSuggestion>,
}

/// An in-flight command suggestion, pinned to the tab that fired it
/// (Spec R04 user story 14).
struct InFlightSuggestion {
    /// The pinned tab's stable [`id`](Tab::id) — the identity the pin
    /// re-resolves through [`index_in`](Self::index_in); indices shift
    /// on close and child pids can be absent or recycled, so neither
    /// can substitute.
    id: u64,
    /// Whether a `\x1b[200~` envelope is currently open on the pinned
    /// session — opened lazily on the first `Delta` while mode 2004
    /// is on, closed on `Done`/`Error`/Esc-cancel so the whole stream
    /// is one undoable, non-executing insertion (user story 7).
    paste_open: bool,
    /// Esc-cancelled but not yet terminated: the pin stays until the
    /// worker's terminal event lands, so post-cancel `Delta`s drop
    /// instead of retyping after the `Ctrl+U` wipe, and the
    /// cancellation `Error` doesn't toast (user story 8). A
    /// cancelled-but-live pin also keeps the trigger gate shut until
    /// the stream is really over.
    cancelled: bool,
}

impl SuggestionPin {
    /// An idle pin — the state before the first request fires.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a request is in flight — the gate both triggers check
    /// (one request at a time is the suggester's frontend contract).
    pub fn is_some(&self) -> bool {
        self.inner.is_some()
    }

    /// Whether no request is in flight.
    pub fn is_none(&self) -> bool {
        self.inner.is_none()
    }

    /// Whether a live (non-cancelled) pin currently resolves to the tab
    /// at `index` — the in-flight key scope (us-8/9/10): Esc and Enter
    /// are intercepted only on the tab the request streams into; on
    /// every other tab both keys behave normally.
    pub fn is_live_for(&self, tabs: &TabManager, index: usize) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|pin| !pin.cancelled && pin.index_in(tabs) == Some(index))
    }

    /// Queue a verified request and take the in-flight pin on tab `id`
    /// — the shared tail of both triggers (the `# `+Enter path in
    /// `app`'s pass-through arm, `Alt+Enter` via
    /// [`suggest_from_line`](Self::suggest_from_line)). `true` when the
    /// suggester accepted the request and the pin was taken; a dead
    /// suggester surfaces as a §6 toast instead, and the caller must
    /// then leave the user's line — and its shadow — untouched.
    pub fn fire(
        &mut self,
        suggester: &CommandSuggester,
        id: u64,
        request: SuggestRequest,
        toast: &mut Option<Toast>,
    ) -> bool {
        match suggester.suggest(request) {
            Ok(()) => {
                self.inner = Some(InFlightSuggestion {
                    id,
                    paste_open: false,
                    cancelled: false,
                });
                true
            }
            Err(error) => {
                *toast = Some(Toast::new(format!("{error}"), Instant::now()));
                false
            }
        }
    }

    /// The `Alt+Enter` trigger's app half (Spec R04 us-11/12): verify
    /// the input line through [`line_request_for`] — the same
    /// shadow+grid echo agreement the `# ` path requires — then, ONLY
    /// once the request was accepted, erase the typed line with one DEL
    /// (`\x7f`) per shadowed character so the request text and the
    /// streamed command can never interleave, and reset the shadow
    /// (the line really is empty, so the streamed text lands on a
    /// fresh shadow, not a stale one). A live pin keeps the gate shut
    /// — one request in flight at a time, same rule as the `# `
    /// trigger — and an exited session drops the whole action like
    /// any PTY input. An empty, stale or unverifiable line does
    /// nothing at all: desync is inert (us-13). Erase-after-fire is
    /// the honest ordering: a `suggest()` failure (a dead worker)
    /// toasts the error and leaves the typed line exactly as the user
    /// left it, instead of erasing a line whose request never ran.
    pub fn suggest_from_line(
        &mut self,
        suggester: &CommandSuggester,
        settings: &Settings,
        profile: &str,
        tab: &mut Tab,
        toast: &mut Option<Toast>,
    ) {
        if self.is_some() || tab.exit_status().is_some() {
            return;
        }
        let Some(request) = line_request_for(tab, settings, profile) else {
            return;
        };
        if !self.fire(suggester, tab.id, request, toast) {
            return;
        }
        let erased = tab.shadow.text().chars().count();
        let _ = tab.session.send_bytes(&vec![0x7f; erased]);
        tab.shadow.reset();
    }

    /// Esc on the pinned tab (Spec R04 user story 8): cancel the stream
    /// at its `CancellationToken`, close the paste envelope if one is
    /// open, and send `Ctrl+U` (`\x15` — readline's
    /// kill-to-start-of-line) to wipe the partial suggestion. The pin
    /// is kept, marked `cancelled`: the worker's terminal event still
    /// has to release it, which keeps post-cancel `Delta`s from
    /// retyping onto the just-wiped line and makes the cancellation
    /// `Error` arrive silently rather than as a toast the user never
    /// asked about. `tab` is the pinned (== active) tab; writes to an
    /// exited session drop like all PTY input does.
    pub fn cancel(&mut self, suggester: &CommandSuggester, tab: &mut Tab) {
        let Some(pin) = self.inner.as_mut() else {
            return;
        };
        pin.cancelled = true;
        suggester.cancel();
        if tab.exit_status().is_none() {
            if pin.paste_open {
                let _ = tab.session.send_bytes(PASTE_END);
                pin.paste_open = false;
            }
            let _ = tab.session.send_bytes(b"\x15");
        }
        // `Ctrl+U` wiped the line through a side channel the shadow
        // cannot replay — the honest state is a fresh, empty one so
        // the next `# ` line can trigger.
        tab.shadow.reset();
    }

    /// One suggester event applied — the seam the app's per-tick drain
    /// shares with the tests, which inject `SuggestEvent`s directly
    /// instead of running a model (Spec R04's testable-seam rule).
    ///
    /// `Delta` text arrives pre-sanitized from `ait-agent` (no control
    /// byte can cross the channel — us-4/5), so it is typed verbatim
    /// into the PINNED tab's session via
    /// [`LiveSession::send_bytes`](ait_term::LiveSession::send_bytes):
    /// the stream stays with its origin tab across a tab switch
    /// (us-14), and deltas aimed at an exited session are dropped. When
    /// the child enabled bracketed paste (`Session::bracketed_paste`),
    /// the first `Delta` opens one `\x1b[200~`…`\x1b[201~` envelope the
    /// whole stream rides inside (us-7) — `Done`/`Error` close it via
    /// [`close_envelope`](Self::close_envelope).
    ///
    /// `Done` releases the pin; `Error` additionally surfaces the §6
    /// status-line toast — prefixed with the pinned tab's `N:title`
    /// label when it points at a background tab, the same attribution
    /// rule the notification toasts use — except an Esc-cancelled
    /// request, whose `Error` is the user's own doing and arrives
    /// silently.
    pub fn apply(&mut self, tabs: &TabManager, event: SuggestEvent, toast: &mut Option<Toast>) {
        match event {
            SuggestEvent::Delta(text) => {
                let Some(pin) = self.inner.as_mut() else {
                    return;
                };
                if pin.cancelled {
                    return;
                }
                let Some(tab) = pin.index_in(tabs).and_then(|index| tabs.tabs().get(index)) else {
                    return;
                };
                if tab.exit_status().is_some() {
                    return;
                }
                if !pin.paste_open && tab.session.bracketed_paste() {
                    let _ = tab.session.send_bytes(PASTE_BEGIN);
                    pin.paste_open = true;
                }
                let _ = tab.session.send_bytes(text.as_bytes());
            }
            SuggestEvent::Done => {
                self.close_envelope(tabs);
                self.inner = None;
            }
            SuggestEvent::Error(message) => {
                // Tab indices shift on close — resolve the pin through
                // its stable id (a gone tab just toasts unlabeled).
                let pin = self.inner.as_ref();
                let cancelled = pin.is_some_and(|pin| pin.cancelled);
                let source = pin.and_then(|pin| {
                    pin.index_in(tabs).and_then(|index| {
                        if index == tabs.active_index() {
                            None
                        } else {
                            tabs.tabs().get(index).map(|tab| tab.label(index))
                        }
                    })
                });
                self.close_envelope(tabs);
                self.inner = None;
                if !cancelled {
                    *toast = Some(match source {
                        Some(label) => Toast::new(format!("{label}: {message}"), Instant::now()),
                        None => Toast::new(message, Instant::now()),
                    });
                }
            }
        }
    }

    /// Drain one tick of suggester events (Spec R04) — every queued
    /// event through [`apply`](Self::apply). `try_recv` keeps the drain
    /// zero-cost inside the app's 8 ms loop.
    pub fn drain(
        &mut self,
        events: &Receiver<SuggestEvent>,
        tabs: &TabManager,
        toast: &mut Option<Toast>,
    ) {
        while let Ok(event) = events.try_recv() {
            self.apply(tabs, event, toast);
        }
    }

    /// Write `\x1b[201~` to the pinned session if an envelope is open —
    /// the close half of [`PASTE_BEGIN`]'s contract. Called on `Done`,
    /// `Error` and (inline, before `Ctrl+U`) Esc-cancel; a dead session
    /// makes the whole question moot, so writes there drop.
    fn close_envelope(&mut self, tabs: &TabManager) {
        let Some(pin) = self.inner.as_mut() else {
            return;
        };
        if !pin.paste_open {
            return;
        }
        pin.paste_open = false;
        if let Some(tab) = pin.index_in(tabs).and_then(|index| tabs.tabs().get(index)) {
            if tab.exit_status().is_none() {
                let _ = tab.session.send_bytes(PASTE_END);
            }
        }
    }

    /// The pinned state a fired request leaves behind — staged by a
    /// frontend's dispatch tests, which inject `SuggestEvent`s through
    /// [`apply`](Self::apply) instead of running a model.
    #[doc(hidden)]
    pub fn staged(id: u64) -> Self {
        Self {
            inner: Some(InFlightSuggestion {
                id,
                paste_open: false,
                cancelled: false,
            }),
        }
    }

    /// The pinned tab's stable id — what a frontend's dispatch tests
    /// assert the pin resolved to.
    #[doc(hidden)]
    pub fn pinned_id(&self) -> Option<u64> {
        self.inner.as_ref().map(|pin| pin.id)
    }

    /// Whether the live pin was Esc-cancelled — a frontend-test view.
    #[doc(hidden)]
    pub fn is_cancelled(&self) -> bool {
        self.inner.as_ref().is_some_and(|pin| pin.cancelled)
    }

    /// Whether a paste envelope is currently open — a frontend-test
    /// view.
    #[doc(hidden)]
    pub fn paste_is_open(&self) -> bool {
        self.inner.as_ref().is_some_and(|pin| pin.paste_open)
    }
}

impl InFlightSuggestion {
    /// The pinned suggestion's CURRENT position in the tab list, or
    /// `None` when its tab is gone. The stable [`id`](Tab::id) is the
    /// honest locator — tab indices shift when a tab closes mid-stream
    /// and `child_pid` can be `None` on pid-less platforms, so neither
    /// can substitute; a drifting locator would type suggestion bytes
    /// into an unrelated session (Spec R04 user story 14).
    fn index_in(&self, tabs: &TabManager) -> Option<usize> {
        tabs.tabs().iter().position(|tab| tab.id == self.id)
    }
}

#[cfg(test)]
mod tests {
    //! Trigger-detection tests at the agreed seams (us-29): the shadow
    //! updated key-by-key, real byte-fed `Session` grids for the
    //! logical-line walk — no PTY, shell or network anywhere.

    use super::*;
    use crate::blocks::BlockAssembler;
    use crate::tabs::PromptMark;
    use ait_term::{PromptMarkKind, Session, SessionOptions, Size};
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::{Duration, Instant};

    const MS: Duration = Duration::from_millis(1);

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

    /// Clipboard text is replayable input: a printable pasted line
    /// appends and fires the trigger exactly as if typed; a paste
    /// carrying control bytes (newline submits, tab completes) falls
    /// back to stale — the paste can never fake the echo.
    #[test]
    fn pasted_text_replays_or_stales() {
        let mut shadow = InputShadow::new();
        shadow.update_pasted("# list files");
        assert_eq!(
            shadow.request_text("user@box:~$ # list files"),
            Some("list files".to_string())
        );

        // Typing continues onto a pasted prefix.
        shadow.update(&char_key('s'));
        assert_eq!(
            shadow.request_text("user@box:~$ # list filess"),
            Some("list filess".to_string())
        );

        for control in ["# a\nb", "# a\tb", "# a\x07"] {
            let mut shadow = typed("# x");
            shadow.update_pasted(control);
            assert!(shadow.is_stale(), "{control:?} must stay stale");
        }
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

    /// The `Alt+Enter` half of the trigger seam (us-11): any verified
    /// input line is the request — no `# ` prefix needed, and a `# `
    /// line comes through VERBATIM (the whole line is the request).
    #[test]
    fn verified_line_fires_on_a_bare_line() {
        assert_eq!(
            typed("how do I undo a commit").verified_line("$ how do I undo a commit"),
            Some("how do I undo a commit".to_string())
        );
        // `# ` is NOT stripped on this path — the whole line asks.
        assert_eq!(
            typed("# list files").verified_line("$ # list files"),
            Some("# list files".to_string())
        );
    }

    /// Empty and whitespace-only lines, stale shadows, and grid
    /// disagreement all refuse — the desync-is-inert rule (us-12/13).
    #[test]
    fn verified_line_rejects_empty_stale_and_desynced() {
        assert_eq!(typed("").verified_line("$ "), None);
        assert_eq!(typed("   ").verified_line("$    "), None);
        let mut stale = typed("undo a commit");
        stale.update(&KeyEvent::new(Key::Up, Modifiers::NONE));
        assert_eq!(stale.verified_line("$ undo a commit"), None);
        // The echoed line must END WITH the shadow (us-13).
        assert_eq!(typed("undo a commit").verified_line("$ make install"), None);
        assert_eq!(typed("undo a commit").verified_line(">>> print(1)"), None);
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
        use crate::tabs::{SessionTemplate, TabManager};
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/sh")),
                    ..Default::default()
                },
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
        use crate::tabs::{SessionTemplate, TabManager};
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/sh")),
                    ..Default::default()
                },
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
        let request = request_for(tabs.active().unwrap(), &settings, "default").expect("must fire");

        assert_eq!(request.request, "list files");
        assert_eq!(request.profile, "default");
        // The armed name rides the request itself (Spec R07 us-26):
        // whatever profile was active at submission is pinned on the
        // snapshot — a later switch can never rewrite it.
        assert_eq!(
            request_for(tabs.active().unwrap(), &settings, "work")
                .expect("must fire")
                .profile,
            "work"
        );
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

    /// The `Alt+Enter` trigger bundle (us-11): a bare line echoed by a
    /// real `/bin/sh` produces a `SuggestRequest` whose request is the
    /// WHOLE line verbatim — the same §5.4 context `request_for`
    /// assembles.
    #[test]
    fn line_request_for_collects_the_whole_line() {
        use crate::tabs::{SessionTemplate, TabManager};
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/sh")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
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
            .send_bytes(b"undo the last commit")
            .unwrap();
        loop {
            tabs.drain_events();
            let tab = tabs.active().unwrap();
            if logical_line(&tab.grid(), tab.cursor()).ends_with("undo the last commit") {
                break;
            }
            assert!(Instant::now() < deadline, "echo never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Stage the shadow as `handle_event` would have.
        {
            let tab = tabs.active_mut().unwrap();
            for c in "undo the last commit".chars() {
                tab.shadow.update(&char_key(c));
            }
        }

        let settings = Settings::default();
        let request =
            line_request_for(tabs.active().unwrap(), &settings, "default").expect("must fire");

        assert_eq!(request.request, "undo the last commit");
        assert_eq!(request.shell, "sh");
        assert_eq!(request.os, std::env::consts::OS);
        assert!(!request.cwd.is_empty());
        assert_eq!(request.settings, settings);
    }

    /// A synthetic mark in record-time coordinates — the shape
    /// `TabManager::drain_events` hands the assembler (the same trick
    /// `blocks.rs`'s tests use).
    fn mark(kind: PromptMarkKind, line: usize, column: usize, at: Instant) -> PromptMark {
        PromptMark {
            kind,
            payload: None,
            line,
            column,
            at,
            wall: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// A `D` mark carrying an exit-code payload.
    fn end_mark(line: usize, at: Instant, payload: Option<&str>) -> PromptMark {
        PromptMark {
            kind: PromptMarkKind::CommandEnd,
            payload: payload.map(str::to_owned),
            line,
            column: 0,
            at,
            wall: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// `OSC 7700 ; <hex(json)>` as a line an interactive shell prints —
    /// how tests hand-emit hook events through a real `/bin/sh` (the
    /// trick `tabs.rs::hook_line` established for the drain seam).
    fn hook_line(json: &str) -> Vec<u8> {
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        format!("printf '\\033]7700;{hex}\\007'\n").into_bytes()
    }

    /// Spec R05 user story 31 / issue #76: block text as context — the
    /// command echo row AND the command's output come through, while a
    /// row that landed past the block's `D` line stays out. A byte-fed
    /// session plus synthetic marks — no PTY anywhere.
    #[test]
    fn block_output_lines_extract_the_block_range() {
        let session = session(
            40,
            b"$ sudo make install\r\npermission denied\r\n$ \r\ntail marker\r\n",
        );
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::PromptStart, 0, 0, t0), 0);
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 2, t0 + MS), 0);
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 1, 0, t0 + 2 * MS), 0);
        asm.observe_mark(&end_mark(2, t0 + 3 * MS, Some("1")), 0);

        let grid = session.grid();
        let extent = grid.scrollback() + grid.lines();
        let blocks: Vec<_> = asm
            .blocks()
            .filter_map(|entry| entry.resolve(&grid, 0, extent))
            .collect();
        assert_eq!(blocks.len(), 1);

        let lines = block_output_lines(&grid, &blocks);
        assert!(
            lines.iter().any(|line| line.contains("sudo make install")),
            "the command echo row is context: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("permission denied")),
            "the failed command's output is context: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("tail marker")),
            "past-D output is outside the block: {lines:?}"
        );
    }

    /// The last-N bound is the grid source's rule kept (us-16): block
    /// text is still capped at [`OUTPUT_LINE_BUDGET`], the newest
    /// lines winning.
    #[test]
    fn block_output_lines_keeps_the_budget_tail() {
        let mut output = Vec::new();
        for i in 0..150 {
            output.extend_from_slice(format!("line {i}\r\n").as_bytes());
        }
        let mut session = Session::new(SessionOptions {
            size: Size::new(40, 200),
            ..Default::default()
        });
        session.feed_bytes(&output);
        let t0 = Instant::now();
        let mut asm = BlockAssembler::new();
        asm.observe_mark(&mark(PromptMarkKind::CommandStart, 0, 0, t0), 0);
        asm.observe_mark(&mark(PromptMarkKind::OutputStart, 1, 0, t0 + MS), 0);
        asm.observe_mark(&end_mark(149, t0 + 2 * MS, Some("0")), 0);

        let grid = session.grid();
        let extent = grid.scrollback() + grid.lines();
        let blocks: Vec<_> = asm
            .blocks()
            .filter_map(|entry| entry.resolve(&grid, 0, extent))
            .collect();
        let lines = block_output_lines(&grid, &blocks);
        assert_eq!(lines.len(), OUTPUT_LINE_BUDGET);
        // `D` at column 0 ends the block on line 148 — the newest
        // budget slice runs line 49..=148.
        assert_eq!(lines.first().unwrap(), "line 49");
        assert_eq!(lines.last().unwrap(), "line 148");
    }

    /// Spec R05 us-31, the dispatch half: a tab WITH blocks draws its
    /// context tail from block text — a scripted `sh -c` emits one
    /// `A B C D` quadruple around a failing command, then a marker row
    /// past the block's `D` which the grid-lines source would carry
    /// and block text must not.
    #[test]
    fn output_lines_for_draws_on_blocks_when_present() {
        use crate::tabs::{SessionTemplate, TabManager};
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/sh")),
                    args: vec![
                        OsString::from("-c"),
                        OsString::from(
                            "printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
                             printf 'sudo make install'; sleep 0.2; \
                             printf '\\n\\033]133;C\\007'; sleep 0.2; \
                             printf 'permission denied\\n'; sleep 0.2; \
                             printf '\\033]133;D;1\\007'; sleep 0.2; \
                             printf '\\n'; sleep 0.2; \
                             printf 'outside any block\\n'; sleep 0.5",
                        ),
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            tabs.drain_events();
            let tab = tabs.active().unwrap();
            let assembled = !tab.blocks().is_empty();
            let tail_landed = recent_output_lines(&tab.grid())
                .iter()
                .any(|line| line.contains("outside any block"));
            if assembled && tail_landed {
                break;
            }
            assert!(Instant::now() < deadline, "block or tail never landed");
            std::thread::sleep(Duration::from_millis(5));
        }

        let tab = tabs.active().unwrap();
        let lines = output_lines_for(tab);
        assert!(
            lines.iter().any(|line| line.contains("sudo make install")),
            "the failed command's echo is context: {lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("permission denied")),
            "the failed command's output is context: {lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("outside any block")),
            "the post-block row stays out of block context: {lines:?}"
        );
        assert!(
            recent_output_lines(&tab.grid())
                .iter()
                .any(|line| line.contains("outside any block")),
            "…which the raw grid source does carry"
        );
    }

    /// The fallback half of us-31: a tab with NO blocks keeps the
    /// Spec-R04 grid-lines source — same answer, same bound.
    #[test]
    fn output_lines_for_falls_back_to_grid_lines_without_blocks() {
        use crate::tabs::{SessionTemplate, TabManager};
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/sh")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
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
            .send_bytes(b"echo grid-fallback-marker\n")
            .unwrap();
        loop {
            tabs.drain_events();
            let tab = tabs.active().unwrap();
            if recent_output_lines(&tab.grid())
                .iter()
                .any(|line| line.contains("grid-fallback-marker"))
            {
                break;
            }
            assert!(Instant::now() < deadline, "echo never landed");
            std::thread::sleep(Duration::from_millis(5));
        }

        let tab = tabs.active().unwrap();
        assert!(tab.blocks().is_empty(), "no marks → no blocks");
        let lines = output_lines_for(tab);
        assert_eq!(lines, recent_output_lines(&tab.grid()));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("grid-fallback-marker")),
            "the grid tail still answers: {lines:?}"
        );
    }

    /// Spec R05 / issue #76: `/proc/<pid>/cwd` may answer only when
    /// OSC 7 never reported AND the tab isn't `Integrated` — the pure
    /// gate `cwd_for` applies before touching the filesystem.
    #[test]
    fn proc_cwd_only_when_unreported_and_not_integrated() {
        for integration in [
            IntegrationState::Absent,
            IntegrationState::MarksOnly,
            IntegrationState::Integrated,
        ] {
            assert!(!reads_proc_cwd(true, integration), "{integration:?}");
        }
        assert!(reads_proc_cwd(false, IntegrationState::Absent));
        assert!(reads_proc_cwd(false, IntegrationState::MarksOnly));
        assert!(!reads_proc_cwd(false, IntegrationState::Integrated));
    }

    /// Spec R05 / issue #76: an `Integrated` tab whose OSC 7 report
    /// never arrived still answers its TRACKED cwd — `/proc` is not
    /// consulted, and a real `cd /` is what proves the read would have
    /// answered differently. Real `/bin/sh`, no TTY needed.
    #[test]
    fn integrated_tabs_skip_the_proc_cwd_read() {
        use crate::tabs::{SessionTemplate, TabManager};
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/sh")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
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

        // init_shell integrates the tab — the hook's echoed
        // `session_id` must equal the allocated id (review fix: only a
        // vetted stream promotes); no OSC 7 ever arrives.
        tabs.active_mut().unwrap().session_id = Some("0".to_owned());
        tabs.active()
            .unwrap()
            .session
            .send_bytes(&hook_line(
                r#"{"hook":"init_shell","value":{"session_id":0,"shell":"sh"}}"#,
            ))
            .unwrap();
        loop {
            tabs.drain_events();
            if tabs.active().unwrap().integration_state() == IntegrationState::Integrated {
                break;
            }
            assert!(Instant::now() < deadline, "integration never landed");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Move the child's real cwd — where `/proc` exists it then
        // disagrees with the tracked cwd, which is what makes skipping
        // the read observable.
        tabs.active()
            .unwrap()
            .session
            .send_bytes(b"cd /\n")
            .unwrap();
        let proc_deadline = Instant::now() + Duration::from_secs(5);
        let mut proc_reports_root = false;
        while Instant::now() < proc_deadline {
            tabs.drain_events();
            if let Some(pid) = tabs.active().unwrap().session.child_pid() {
                if std::fs::read_link(format!("/proc/{pid}/cwd"))
                    .is_ok_and(|path| path == Path::new("/"))
                {
                    proc_reports_root = true;
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        let tab = tabs.active().unwrap();
        let tracked = tab.cwd().display().to_string();
        assert_ne!(
            tracked, "/",
            "the test discriminates only if the tracked cwd isn't /"
        );
        assert_eq!(tab.integration_state(), IntegrationState::Integrated);
        assert!(!tab.cwd_reported(), "plain sh never reports OSC 7");
        assert_eq!(
            cwd_for(tab),
            tracked,
            "the tracked cwd answers, not /proc's"
        );
        if proc_reports_root {
            assert_ne!(
                cwd_for(tab),
                "/",
                "/proc would have answered `/` — the read was skipped"
            );
        }
    }

    // ---- Spec R11-T2 (§5.10.4): the classifier trigger ----
    //
    // Same seam as the `# `/`Alt+Enter` triggers: the shadow updated
    // key-by-key, byte-fed `Session` grids for the logical line, and a
    // `with_path_entries` index standing in for the spawn-time
    // `$PATH` harvest — no PTY, network or real filesystem walk.

    /// The verdict half: an armed `heuristic` mode plus a verified
    /// line the classifier reads as natural language returns the line
    /// verbatim — what the rewrite submits as `# <line>` (us-1/2).
    #[test]
    fn classified_line_returns_the_verified_nl_line() {
        let index = ProgramIndex::with_path_entries("sh", ["ls", "echo"]);
        assert_eq!(
            classified_line(
                &typed("what is the largest file"),
                "$ what is the largest file",
                "sh",
                &index,
                ClassifierMode::Heuristic,
            ),
            Some("what is the largest file".to_string())
        );
    }

    /// Lines the heuristic reads as commands — a builtin or `$PATH`
    /// first word, shell syntax, a single token, or unknown words
    /// without sentence evidence — return `None`: Enter submits them
    /// verbatim and the trigger stays invisible (us-5/6/7/8).
    #[test]
    fn classified_line_stays_none_for_commands() {
        let index = ProgramIndex::with_path_entries("sh", ["echo", "mybin"]);
        for line in [
            "echo hi there",      // a builtin first word
            "mybin --verbose go", // a $PATH binary plus a flag token
            "foo | bar baz",      // a pipe is unquoted shell syntax
            "justoneword",        // single tokens are always commands
            "foo bar",            // unknown words, no positive evidence
        ] {
            assert_eq!(
                classified_line(
                    &typed(line),
                    &format!("$ {line}"),
                    "sh",
                    &index,
                    ClassifierMode::Heuristic,
                ),
                None,
                "{line:?} submits verbatim"
            );
        }
    }

    /// The `\x15` wipe's exposure (us-10): the trigger only arms when
    /// the tab's program is a known shell — `rewrite_as_comment`
    /// erases the typed line with readline's kill-to-BOL, and a REPL
    /// or any other program that echoes input without honoring `Ctrl+U`
    /// would get `# <line>` appended onto the un-wiped text. Non-shell
    /// prompts degrade to the same verbatim path every other
    /// unverifiable line takes.
    #[test]
    fn classified_line_is_none_for_a_non_shell_program() {
        let index = ProgramIndex::with_path_entries("python3", Vec::<&str>::new());
        let line = "what is the largest file";
        let logical = ">>> what is the largest file";
        // A verified NL line under a REPL prompt is verbatim.
        assert_eq!(
            classified_line(
                &typed(line),
                logical,
                "python3",
                &index,
                ClassifierMode::Heuristic
            ),
            None
        );
        // …as is every non-shell basename — the gate is a known-set,
        // not a denialist.
        for shell in ["cat", "irb", "node", "gdb"] {
            assert_eq!(
                classified_line(
                    &typed(line),
                    logical,
                    shell,
                    &index,
                    ClassifierMode::Heuristic
                ),
                None,
                "{shell} is not a shell the rewrite can wipe"
            );
        }
        // The POSIX/readline family still arms.
        for shell in ["bash", "zsh", "fish", "sh", "dash"] {
            assert_eq!(
                classified_line(
                    &typed(line),
                    &format!("$ {line}"),
                    shell,
                    &index,
                    ClassifierMode::Heuristic
                ),
                Some(line.to_string()),
                "{shell} arms the trigger"
            );
        }
    }

    /// `off`, a stale shadow and grid-echo disagreement all return
    /// `None` — an ordinary submission, the desync-is-inert rule
    /// (us-10/11/13). The stale case is what `ait ctl send` leaves
    /// behind — the same `mark_stale` pastes use — so input that
    /// never passed through keystrokes is never classified (us-16).
    #[test]
    fn classified_line_is_none_when_off_stale_or_desynced() {
        let index = ProgramIndex::with_path_entries("sh", ["ls"]);
        let line = "what is the largest file";
        let logical = "$ what is the largest file";
        // `classifier = "off"` keeps every submission verbatim.
        assert_eq!(
            classified_line(&typed(line), logical, "sh", &index, ClassifierMode::Off),
            None
        );
        // A stale shadow never verifies — `mark_stale` is the same
        // call pastes and `ait ctl send` make.
        let mut stale = typed(line);
        stale.mark_stale();
        assert_eq!(
            classified_line(&stale, logical, "sh", &index, ClassifierMode::Heuristic),
            None
        );
        // Echo disagreement (history recall, a REPL, a heredoc): the
        // line on the grid is not the shadow's line.
        assert_eq!(
            classified_line(
                &typed(line),
                "$ make install",
                "sh",
                &index,
                ClassifierMode::Heuristic
            ),
            None
        );
    }

    /// The request half on a real `/bin/sh`: a verified natural-
    /// language line produces the verified line AND a `SuggestRequest`
    /// in the `Alt+Enter` whole-line form — the request text IS the
    /// line, bundled with the same §5.4 context `request_for`
    /// assembles (us-15–17). Real PTY spawn, no TTY needed.
    #[test]
    fn classified_request_bundles_the_verified_line() {
        use crate::tabs::{SessionTemplate, TabManager};
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/sh")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
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
            .send_bytes(b"what is the largest file")
            .unwrap();
        loop {
            tabs.drain_events();
            let tab = tabs.active().unwrap();
            if logical_line(&tab.grid(), tab.cursor()).ends_with("what is the largest file") {
                break;
            }
            assert!(Instant::now() < deadline, "echo never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Stage the shadow as `handle_event` would have.
        {
            let tab = tabs.active_mut().unwrap();
            for c in "what is the largest file".chars() {
                tab.shadow.update(&char_key(c));
            }
        }

        let settings = Settings::default();
        let (line, request) =
            classified_request(tabs.active().unwrap(), &settings, "default").expect("NL fires");

        assert_eq!(line, "what is the largest file");
        assert_eq!(request.request, "what is the largest file");
        assert_eq!(request.shell, "sh");
        assert_eq!(request.profile, "default");
        assert_eq!(request.settings, settings);
    }

    /// The known-shell gate on the request half (us-10): a tab whose
    /// program merely echoes its input — `/bin/cat` — verifies the
    /// shadow exactly like a shell prompt would, but the title fails
    /// [`ait_shell::is_known_shell`], so the natural-language verdict
    /// degrades to a verbatim submission instead of a `Ctrl+U` wipe
    /// the program may not honor. Same seam as the `sh` test above.
    #[test]
    fn classified_request_stays_verbatim_on_a_non_shell() {
        use crate::tabs::{SessionTemplate, TabManager};
        use ait_term::LiveSessionOptions;

        let mut tabs = TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(OsString::from("/bin/cat")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        // `cat` prints no prompt — the PTY's own echo is what lands
        // the line on the grid, which is exactly the exposure: echo
        // agreement without a shell underneath.
        tabs.active()
            .unwrap()
            .session
            .send_bytes(b"what is the largest file")
            .unwrap();
        loop {
            tabs.drain_events();
            let tab = tabs.active().unwrap();
            if logical_line(&tab.grid(), tab.cursor()).ends_with("what is the largest file") {
                break;
            }
            assert!(Instant::now() < deadline, "echo never arrived");
            std::thread::sleep(Duration::from_millis(5));
        }
        {
            let tab = tabs.active_mut().unwrap();
            for c in "what is the largest file".chars() {
                tab.shadow.update(&char_key(c));
            }
        }

        let settings = Settings::default();
        assert!(
            classified_request(tabs.active().unwrap(), &settings, "default").is_none(),
            "a non-shell program submits verbatim"
        );
    }
}
