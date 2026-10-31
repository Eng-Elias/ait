//! `term-demo` — Round 1's runnable, TUI-free smoke check (Spec R01 user
//! story 20; REQUIREMENTS.md §5.1, §6).
//!
//! `cargo run -p ait-term --example term-demo` spawns `/bin/sh` in a real
//! PTY through [`LiveSession`] and drives a scripted session end to end,
//! exercising every layer built this round:
//!
//! - **PTY spawn / lifecycle** — a live `sh` session; `Exited` carries the
//!   child's status, which the demo itself exits with (the script ends in
//!   `exit 3`, so `echo $?` shows `3` — the non-zero exit is the point,
//!   not a failure);
//! - **key encoding** — every command line is typed through
//!   [`LiveSession::send_key`]: one `Key::Char` press per character,
//!   `Key::Enter` per line, plus a `Ctrl+C` on a doomed line;
//! - **VT parsing / styled grid** — `printf` SGR escapes and
//!   `ls --color=always` land styled cells in the grid, dumped below as
//!   plain text plus a styled-runs report;
//! - **OSC events** — the script emits OSC 7 (`CwdChanged`), OSC 133
//!   (`PromptMark`), OSC 9 (`Notification`), OSC 52 (`ClipboardWrite` —
//!   the demo opts in via `SessionOptions::osc52`) and OSC 7700 (the
//!   AIT-private hook channel → `Private`), printed as they arrive.
//!
//! Determinism: a fixed `/bin/sh`, a fixed `PS1` and `printf`-generated
//! escape sequences only — the same dump on any Linux box.

use std::ffi::OsString;
use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use ait_term::{
    Cell, CellKind, Color, ExitStatus, Key, KeyEvent, LiveEvent, LiveSession, LiveSessionOptions,
    Modifiers, Row, SessionEvent, SessionOptions, Size, Style, Underline,
};

/// The PTY size the demo runs at — fixed so the dump is stable.
const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};

/// No event activity for this long ends a step's collection window.
const STEP_QUIET: Duration = Duration::from_millis(250);
/// Cap on each collection window (and on the initial prompt wait).
const STEP_DEADLINE: Duration = Duration::from_secs(5);
/// How long to wait for `exit` to surface after the script ends.
const EXIT_DEADLINE: Duration = Duration::from_secs(10);
/// Poll interval while pumping the session.
const POLL: Duration = Duration::from_millis(10);

/// The scripted session: one command line per entry, POSIX `sh` plus
/// `ls`. Every OSC sequence is emitted with `printf`, so the result is
/// identical on any machine — no shell integration required.
const SCRIPT: &[&str] = &[
    // OSC 7 — report the working directory as a file:// URI.
    "cd /tmp",
    "printf '\\033]7;file://%s\\007\\n' \"$PWD\"",
    // OSC 133 — prompt-start and command-start marks.
    "printf '\\033]133;A\\007\\033]133;B\\007\\n'",
    // SGR styling — bold red, underlined cyan, inverse video.
    "printf '\\033[1;31mBOLD-RED\\033[0m \\033[4;36mUNDER-CYAN\\033[0m \\033[7mINVERSE\\033[0m\\n'",
    // Colored output from a real program.
    "ls --color=always -d /etc /tmp",
    // OSC 133 — output-start, then command-end carrying the exit code.
    "printf '\\033]133;C\\007\\n'",
    "printf '\\033]133;D;0\\007\\n'",
    // OSC 9 — a desktop-style notification.
    "printf '\\033]9;demo notification\\007\\n'",
    // OSC 52 — a clipboard write request (surfaced only because the demo
    // opts in via `osc52`; it is off by default).
    "printf '\\033]52;c;aGVsbG8=\\007\\n'",
    // OSC 7700 — the AIT-private hook channel surfaces as Private.
    "printf '\\033]7700;ZGVtby1ob29r\\007\\n'",
    // Non-zero on purpose: proves the demo exits with the child's status.
    "exit 3",
];

fn main() -> ExitCode {
    println!(
        "term-demo: spawning /bin/sh in a {}x{} PTY\n",
        SIZE.columns, SIZE.lines
    );

    let mut session = match LiveSession::spawn(LiveSessionOptions {
        session: SessionOptions {
            size: SIZE,
            // Opt in so the scripted OSC 52 surfaces as ClipboardWrite.
            osc52: true,
            ..Default::default()
        },
        program: Some(OsString::from("/bin/sh")),
        // A fixed, boring prompt keeps the dump readable on any machine.
        env: vec![(OsString::from("PS1"), OsString::from("ait> "))],
        ..Default::default()
    }) {
        Ok(session) => session,
        Err(e) => {
            eprintln!("term-demo: spawn failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "term-demo: session running, child pid {:?}\n",
        session.child_pid()
    );

    // Wait out the first prompt, then run the script, then wait for exit.
    // On any failure path `session` is dropped, which kills the child and
    // joins the worker threads — no orphan is left behind.
    let mut status = pump_until_quiet(&mut session, STEP_QUIET, STEP_DEADLINE);
    if status.is_none() {
        status = run_script(&mut session);
    }
    if status.is_none() {
        status = wait_for_exit(&mut session, EXIT_DEADLINE);
    }

    dump_grid(&session);
    drop(session);

    match status {
        Some(status) => {
            println!(
                "\nterm-demo: exiting with the child's status ({}) — \
                 `cargo run` will report a non-zero exit; that is the point.",
                describe_status(&status)
            );
            ExitCode::from(u8::try_from(status.code).unwrap_or(u8::MAX))
        }
        None => {
            eprintln!("term-demo: no exit observed within {EXIT_DEADLINE:?}; child was killed on teardown");
            ExitCode::FAILURE
        }
    }
}

/// The scripted part: a `Ctrl+C` interlude through the same `Key`
/// encoder, then every line of [`SCRIPT`] typed character by character.
/// Returns early with the exit status if the child dies mid-script.
fn run_script(session: &mut LiveSession) -> Option<ExitStatus> {
    println!(">>> cancel-this-line<C-c>");
    if let Err(e) = type_chars(session, "cancel-this-line")
        .and_then(|_| session.send_key(KeyEvent::new(Key::Char('c'), Modifiers::CONTROL)))
    {
        eprintln!("term-demo: input failed: {e}");
        return None;
    }
    if let Some(status) = pump_until_quiet(session, STEP_QUIET, STEP_DEADLINE) {
        return Some(status);
    }

    for line in SCRIPT {
        println!(">>> {line}");
        if let Err(e) = type_line(session, line) {
            eprintln!("term-demo: input failed: {e}");
            return None;
        }
        if let Some(status) = pump_until_quiet(session, STEP_QUIET, STEP_DEADLINE) {
            return Some(status);
        }
    }
    None
}

/// Type text through the `Key` encoder — one `Key::Char` per character.
fn type_chars(session: &LiveSession, text: &str) -> io::Result<()> {
    for key in text.chars().map(Key::Char) {
        session.send_key(KeyEvent::new(key, Modifiers::NONE))?;
    }
    Ok(())
}

/// Type one command line: characters plus `Key::Enter`.
fn type_line(session: &LiveSession, line: &str) -> io::Result<()> {
    type_chars(session, line)?;
    session.send_key(KeyEvent::new(Key::Enter, Modifiers::NONE))
}

/// Drain events once: print the observed OSC events, report whether
/// anything happened, and capture the exit status if it surfaced.
fn drain_and_report(session: &mut LiveSession) -> (bool, Option<ExitStatus>) {
    let mut activity = false;
    let mut status = None;
    for event in session.drain_events() {
        activity = true;
        match event {
            LiveEvent::Osc(event) => println!("    event: {}", describe_event(&event)),
            LiveEvent::Exited(exit) => {
                println!("    child exited: {}", describe_status(&exit));
                status = Some(exit);
            }
            // OutputReady is a repaint hint; the grid dump shows the rest.
            _ => {}
        }
    }
    (activity, status)
}

/// Pump the session until the event stream has been quiet for `quiet`
/// (bounded by `deadline`), printing events as they arrive. Returns the
/// exit status if the child died during the window.
fn pump_until_quiet(
    session: &mut LiveSession,
    quiet: Duration,
    deadline: Duration,
) -> Option<ExitStatus> {
    let start = Instant::now();
    let mut quiet_since = Instant::now();
    loop {
        let (activity, status) = drain_and_report(session);
        if status.is_some() {
            return status;
        }
        if activity {
            quiet_since = Instant::now();
        }
        if quiet_since.elapsed() >= quiet || start.elapsed() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// Pump until `Exited` surfaces or the deadline passes.
fn wait_for_exit(session: &mut LiveSession, deadline: Duration) -> Option<ExitStatus> {
    let start = Instant::now();
    loop {
        if let (_, Some(status)) = drain_and_report(session) {
            return Some(status);
        }
        if start.elapsed() >= deadline {
            return None;
        }
        std::thread::sleep(POLL);
    }
}

/// Plain-text dump of scrollback + viewport — what a TUI would paint,
/// minus color. Followed by the styled-runs report, proving styled
/// output landed as styled cells and not just text.
fn dump_grid(session: &LiveSession) {
    let grid = session.grid();
    println!(
        "\n--- final grid: {}x{} viewport, {} scrollback line(s) ---",
        grid.columns(),
        grid.lines(),
        grid.scrollback()
    );
    for index in -(grid.scrollback() as i32)..grid.lines() as i32 {
        if let Some(row) = grid.line(index) {
            println!("{index:>4}|{}", row.text().trim_end());
        }
    }
    let cursor = session.cursor();
    println!(
        "cursor: line {} column {} {:?}; session state: {:?}",
        cursor.line,
        cursor.column,
        cursor.shape,
        session.state()
    );

    println!("\n--- styled runs (cells with non-default fg/bg/style) ---");
    let mut any = false;
    for index in -(grid.scrollback() as i32)..grid.lines() as i32 {
        let Some(row) = grid.line(index) else {
            continue;
        };
        for run in styled_runs(&row) {
            if run.is_default() {
                continue;
            }
            any = true;
            println!(
                "  line {index} cols {}..{}: {:?} [{}]",
                run.start,
                run.end,
                run.text.trim_end(),
                describe_style(&run.cell)
            );
        }
    }
    if !any {
        println!("  (none)");
    }
}

/// A maximal run of same-appearance cells in one row.
struct StyledRun {
    /// First column of the run.
    start: usize,
    /// One past the last column of the run.
    end: usize,
    /// A representative cell (all cells in the run share its appearance).
    cell: Cell,
    /// The run's text.
    text: String,
}

impl StyledRun {
    fn is_default(&self) -> bool {
        self.cell.fg == Color::Default
            && self.cell.bg == Color::Default
            && self.cell.style == Style::default()
    }
}

/// Group a row's cells into [`StyledRun`]s.
fn styled_runs(row: &Row<'_>) -> Vec<StyledRun> {
    let mut runs = Vec::new();
    let mut start = 0;
    let mut prev: Option<Cell> = None;
    let mut text = String::new();
    for (col, cell) in row.cells().enumerate() {
        let same = prev.is_some_and(|p| (p.fg, p.bg, p.style) == (cell.fg, cell.bg, cell.style));
        if !same {
            if let Some(cell) = prev {
                runs.push(StyledRun {
                    start,
                    end: col,
                    cell,
                    text: std::mem::take(&mut text),
                });
            }
            start = col;
            prev = Some(cell);
        }
        if cell.kind != CellKind::WideSpacer {
            text.push(cell.character);
        }
    }
    if let Some(cell) = prev {
        runs.push(StyledRun {
            start,
            end: row.len(),
            cell,
            text,
        });
    }
    runs
}

/// One readable line per [`SessionEvent`].
fn describe_event(event: &SessionEvent) -> String {
    match event {
        SessionEvent::CwdChanged(path) => format!("cwd changed → {}", path.display()),
        SessionEvent::PromptMark { kind, payload } => match payload {
            Some(payload) => format!("prompt mark {kind:?} (payload: {payload:?})"),
            None => format!("prompt mark {kind:?}"),
        },
        SessionEvent::Notification { title, message } => match title {
            Some(title) => format!("notification: {title:?} — {message:?}"),
            None => format!("notification: {message:?}"),
        },
        SessionEvent::ClipboardWrite {
            selection,
            contents,
        } => format!("clipboard write ({selection:?}): {contents:?}"),
        SessionEvent::Private { code, params } => format!(
            "private OSC {} ({} param(s): {:?})",
            String::from_utf8_lossy(code),
            params.len(),
            params
                .iter()
                .map(|p| String::from_utf8_lossy(p).into_owned())
                .collect::<Vec<_>>()
        ),
    }
}

/// `code 3`, or `code 1 (Terminated by SIGHUP)` when signaled.
fn describe_status(status: &ExitStatus) -> String {
    match &status.signal {
        Some(signal) => format!("code {} ({signal})", status.code),
        None => format!("code {}", status.code),
    }
}

/// Compact appearance description for a styled-runs report entry.
fn describe_style(cell: &Cell) -> String {
    let mut parts = Vec::new();
    if cell.fg != Color::Default {
        parts.push(format!("fg={:?}", cell.fg));
    }
    if cell.bg != Color::Default {
        parts.push(format!("bg={:?}", cell.bg));
    }
    let style = cell.style;
    if style.bold {
        parts.push("bold".to_string());
    }
    if style.dim {
        parts.push("dim".to_string());
    }
    if style.italic {
        parts.push("italic".to_string());
    }
    if style.underline != Underline::None {
        parts.push(format!("underline={:?}", style.underline));
    }
    if style.inverse {
        parts.push("inverse".to_string());
    }
    if style.hidden {
        parts.push("hidden".to_string());
    }
    if style.strikeout {
        parts.push("strikeout".to_string());
    }
    parts.join(" ")
}
