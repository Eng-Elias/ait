//! `ait-testkit` self-tests (Spec E01): the `E2e` harness driving plain
//! `/bin/sh` programs — spawn, chord keys, typed text, raw bytes, SGR mouse
//! bytes, resize, bounded screen waits and exit status are all proven here
//! without `ait` involved, so harness bugs can't hide behind the app.

use std::fs;

use ait_testkit::{dump_root, E2e, MouseButton, MouseEvent, Size, SpawnConfig};

const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};

/// `/bin/sh -c <script>` in the harness's pinned environment.
fn sh(script: &str) -> E2e {
    E2e::spawn(SpawnConfig::new("/bin/sh", &["-c", script], SIZE)).expect("spawn /bin/sh")
}

/// `sh` stays alive until `read` gets a line, so output is observable
/// before the exit tests touch `wait_for_exit`.
#[test]
fn spawned_program_output_lands_on_screen() {
    let mut app = sh("printf e2e-marker; read x");

    app.wait_for_screen("the marker text", |app| {
        app.screen_text().contains("e2e-marker")
    });
}

/// `type_text` + `press("Enter")` are the read-a-line input path.
#[test]
fn typed_text_and_enter_reach_the_program() {
    let mut app = sh("read line; echo \"saw:$line\"");

    app.type_text("hi");
    app.press("Enter");

    app.wait_for_screen("the echo", |app| app.screen_text().contains("saw:hi"));
}

/// The raw-byte escape hatch reaches the program unmodified.
#[test]
fn send_bytes_reaches_the_program() {
    let mut app = sh("read line; echo \"saw:$line\"");

    app.send_bytes(b"bytes-here\n");

    app.wait_for_screen("the echo", |app| {
        app.screen_text().contains("saw:bytes-here")
    });
}

/// A named chord encodes to the bytes a real terminal would send: `Alt+x`
/// is `ESC x` (`1b 78`) — read raw off the PTY via `dd`/`od`.
#[test]
fn press_encodes_named_chords_into_pty_bytes() {
    // `-icanon` so `dd` sees bytes without a newline; `printf READY`
    // guarantees `stty` ran before we send (no race).
    let mut app = sh("stty -icanon -echo; printf READY; dd bs=1 count=2 2>/dev/null | od -An -tx1");
    app.wait_for_screen("the ready marker", |app| {
        app.screen_text().contains("READY")
    });

    app.press("Alt+x");

    app.wait_for_screen("the two bytes in hex", |app| {
        app.screen_text().contains("1b 78")
    });
}

/// `send_mouse` writes SGR (1006) sequences: `Down(Left)` at (10,5) is
/// `ESC [ < 0 ; 1 0 ; 5 M` — ten bytes, hex-dumped by `od`.
#[test]
fn send_mouse_writes_sgr_button_sequences() {
    let mut app =
        sh("stty -icanon -echo; printf READY; dd bs=1 count=10 2>/dev/null | od -An -tx1");
    app.wait_for_screen("the ready marker", |app| {
        app.screen_text().contains("READY")
    });

    app.send_mouse(MouseEvent::Down(MouseButton::Left), 10, 5);

    app.wait_for_screen("the ten bytes in hex", |app| {
        app.screen_text().contains("1b 5b 3c 30 3b 31 30 3b 35 4d")
    });
}

/// `resize` forwards the new winsize to the PTY — `stty size` reads it
/// back as `lines columns`.
#[test]
fn resize_reaches_the_pty_winsize() {
    let mut app = sh("read x; stty size");

    app.resize(Size::new(100, 40));
    app.send_bytes(b"\n");

    app.wait_for_screen("the new winsize", |app| {
        app.screen_text().contains("40 100")
    });
}

/// `wait_for_exit` surfaces the child's status once the event stream
/// reports `Exited`.
#[test]
fn wait_for_exit_reports_the_child_status() {
    let mut app = sh("exit 7");

    let status = app.wait_for_exit();

    assert_eq!(status.code, 7);
    assert!(!status.success());
}

/// The pinned environment: `SHELL` points at the known shell and the
/// program runs in the harness's fresh temp dir.
#[test]
fn spawn_environment_and_cwd_are_pinned() {
    let mut app = sh("echo \"shell=$SHELL\"; pwd");

    app.wait_for_screen("the pinned SHELL", |app| {
        app.screen_text().contains("shell=/bin/sh")
    });
    let cwd = app.cwd().to_string_lossy().into_owned();
    app.wait_for_screen("the fresh temp cwd", |app| app.screen_text().contains(&cwd));
}

/// The screen accessors a test relies on: `line(n)` is one trimmed row,
/// `cursor()` tracks the app's renderable cursor.
#[test]
fn screen_accessors_read_the_grid() {
    let mut app = sh("printf 'one\\ntwo'; read x");
    app.wait_for_screen("both lines", |app| app.line(1) == "two");

    assert_eq!(app.line(0), "one");
    assert_eq!(app.line(1), "two");
    let cursor = app.cursor();
    assert_eq!((cursor.line, cursor.column), (1, 3), "after `two`");
    assert_eq!(app.size(), SIZE);
}

/// `pty_tail()` reports `LiveSession`'s raw output tail: the bytes the
/// session fed into the grid, escapes and all (Spec E01, ticket E01-T5).
#[test]
fn pty_tail_reports_the_raw_output_the_session_consumed() {
    let mut app = sh("printf tail-marker; read y");
    app.wait_for_screen("the marker", |app| {
        app.screen_text().contains("tail-marker")
    });

    let tail = app.pty_tail();
    assert!(
        String::from_utf8_lossy(tail).contains("tail-marker"),
        "the tail should hold the raw output: {}",
        String::from_utf8_lossy(tail)
    );
}

/// Spec E01's failure evidence: a failed step dumps the final screen,
/// the raw PTY tail and the ordered action log under
/// `target/e2e-failures/<test>/` *before* the panic propagates —
/// verified by catching the panic and reading the dump back, so the
/// suite itself stays green. The dump dir is named after the panicking
/// test's thread (this test's own name).
#[test]
fn a_failed_step_leaves_a_failure_dump() {
    let dir = dump_root().join("a_failed_step_leaves_a_failure_dump");
    let _ = fs::remove_dir_all(&dir);

    let outcome = std::panic::catch_unwind(|| {
        let mut app = sh("printf doomed; exit 0");
        app.wait_for_screen("a marker that never appears", |app| {
            app.screen_text().contains("never-appears")
        });
    });
    assert!(outcome.is_err(), "the failing wait must panic");

    let screen = fs::read_to_string(dir.join("screen.txt")).expect("screen.txt written");
    assert!(screen.contains("doomed"), "final screen text in the dump");
    let tail = fs::read(dir.join("pty-tail.bin")).expect("pty-tail.bin written");
    assert!(
        String::from_utf8_lossy(&tail).contains("doomed"),
        "raw PTY tail in the dump"
    );
    let actions = fs::read_to_string(dir.join("actions.log")).expect("actions.log written");
    assert!(
        actions.contains("wait_for_screen(\"a marker that never appears\")"),
        "ordered action log in the dump:\n{actions}"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// `E2e::dump_evidence(name)` writes the same dump under an explicit
/// name — the hook the scenario runner (E01-T4) uses per failing
/// scenario, where no test-thread name exists to derive one from.
#[test]
fn dump_evidence_writes_the_dump_under_an_explicit_name() {
    let mut app = sh("printf evidence; read y");
    app.wait_for_screen("the marker", |app| app.screen_text().contains("evidence"));

    let dir = app
        .dump_evidence("selftest-explicit-dump")
        .expect("dump writes");

    assert_eq!(dir, dump_root().join("selftest-explicit-dump"));
    assert!(fs::read_to_string(dir.join("screen.txt"))
        .unwrap()
        .contains("evidence"));
    assert!(
        String::from_utf8_lossy(&fs::read(dir.join("pty-tail.bin")).unwrap()).contains("evidence")
    );
    assert!(fs::read_to_string(dir.join("actions.log"))
        .unwrap()
        .contains("wait_for_screen"));

    fs::remove_dir_all(&dir).unwrap();
}

/// `SpawnConfig.headed` attaches a live output mirror (`/dev/tty` when
/// present, stderr otherwise) — either way the session must keep
/// working normally: the tap is observability, not data path.
#[test]
fn headed_mode_does_not_disturb_the_session() {
    let mut config = SpawnConfig::new("/bin/sh", &["-c", "printf headed-ok"], SIZE);
    config.headed = true;
    let mut app = E2e::spawn(config).expect("spawn /bin/sh");

    app.wait_for_screen("the marker text", |app| {
        app.screen_text().contains("headed-ok")
    });
}
