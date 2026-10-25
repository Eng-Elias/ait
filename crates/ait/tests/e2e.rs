//! e2e tests for the `ait` binary (Spec E01, ticket E01-T1): the real
//! binary in a real PTY via `ait-testkit`'s [`E2e`] harness. The harness
//! pins `SHELL=/bin/sh`, a fresh temp cwd and `TERM=xterm-256color`, so
//! the spawned app's chrome — tab bar (`N:sh` titles) and status line
//! (`cwd · sh · — · running`) — reproduces identically on every machine.

use ait_testkit::{ait_bin, E2e, Size, SpawnConfig};

/// 80x24: row 0 is the tab bar, row 23 the status line.
const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};
const STATUS_ROW: usize = 23;

fn spawn_ait() -> E2e {
    E2e::spawn(SpawnConfig::new(ait_bin!(), &[], SIZE)).expect("spawn the ait binary")
}

/// Wait until the app's chrome has rendered: ` 1:sh ` in the tab bar,
/// the `… · sh · — · running` summary on the status line.
fn wait_for_chrome(app: &mut E2e) {
    app.wait_for_screen("the tab bar and status line", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh · — · running")
    });
}

/// Spawning `ait` shows a `1:sh` tab and the cwd/shell/model/state
/// status line — the §5.2 chrome a user sees first.
#[test]
fn spawn_renders_the_tab_bar_and_status_line() {
    let mut app = spawn_ait();

    wait_for_chrome(&mut app);

    assert_eq!(app.line(0), " 1:sh");
}

/// `Alt+T` opens a second tab and the bar gains `2:sh`.
#[test]
fn alt_t_opens_a_second_tab_in_the_bar() {
    let mut app = spawn_ait();
    wait_for_chrome(&mut app);

    app.press("Alt+T");

    app.wait_for_screen("a second tab in the bar", |app| {
        app.line(0).contains("2:sh")
    });
    assert!(app.line(0).contains("1:sh"), "the first tab stays");
}

/// `Alt+Q` quits: the process exits cleanly with code 0.
#[test]
fn alt_q_quits_with_exit_code_zero() {
    let mut app = spawn_ait();
    wait_for_chrome(&mut app);

    app.press("Alt+Q");

    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");
}

/// Manual PR-evidence generator (Spec E01, ticket E01-T5: "the PR
/// description includes a transcript of an e2e run"). Excluded from the
/// suite — run it on demand:
///
/// ```text
/// cargo test -p ait --test e2e pr_evidence_transcript -- --ignored --nocapture
/// ```
///
/// It prints a spawn → keystroke → screen assertion → exit transcript
/// and leaves the full evidence dump under
/// `target/e2e-failures/pr-evidence/`.
#[test]
#[ignore = "manual evidence generator for PR bodies"]
fn pr_evidence_transcript() {
    let mut app = spawn_ait();
    wait_for_chrome(&mut app);
    println!("--- screen after spawn ---\n{}", app.screen_text());

    app.press("Alt+T");
    app.wait_for_screen("a second tab in the bar", |app| {
        app.line(0).contains("2:sh")
    });
    println!("--- tab bar after Alt+T ---\n{}", app.line(0));

    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");
    println!("--- exited: {status:?} ---");

    let dir = app.dump_evidence("pr-evidence").expect("evidence dump");
    println!("--- evidence dump: {} ---", dir.display());
}

/// One text snapshot of the initial screen (Spec E01: plain-text diffs
/// for visual regressions). The pinned dash prompt is `$ `; the temp cwd
/// is redacted so the snapshot is machine-independent.
#[test]
fn initial_screen_snapshot() {
    let mut app = spawn_ait();
    wait_for_chrome(&mut app);
    app.wait_for_screen("the shell prompt", |app| app.line(1) == "$");

    let cwd = app.cwd().to_string_lossy().into_owned();
    let screen = app.screen_text().replace(&cwd, "[cwd]");
    insta::assert_snapshot!(screen);
}
