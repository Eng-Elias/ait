//! e2e for §5.8 persistence — terminal-session + block metadata
//! (Spec R08, ticket R08-T2). Code-first for the same reason the R03
//! fixtures are: a TOML scenario's `env` values are literal strings,
//! so `XDG_DATA_HOME` cannot be interpolated to a per-test tempdir —
//! and the store assertions live outside the screen anyway: the rows
//! are read back through [`ait_store::Store`] against the database
//! file the spawned app left in the fixture dir.
//!
//! The app runs real bash so shell integration emits the OSC 133
//! marks + `preexec` hooks a `blocks` row needs (the
//! `shell_integration.rs` precedent: `SHELL` selects the spawned
//! shell).

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use ait_store::Store;
use ait_testkit::{ait_bin, wait_fresh_prompt, E2e, Size, SpawnConfig};

/// 80x24: row 0 is the tab bar, row 23 the status line.
const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};
const STATUS_ROW: usize = 23;

/// Spawn `ait` on bash with `XDG_DATA_HOME` pinned to `xdg` — appended
/// after the harness's pinned env, so it applies. `xdg` is a fresh
/// tempdir; the app's `store.db` lands at `xdg/ait/store.db`.
fn spawn_ait_with_data(xdg: &Path) -> E2e {
    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config.env.push((
        OsString::from("XDG_DATA_HOME"),
        xdg.as_os_str().to_os_string(),
    ));
    config
        .env
        .push((OsString::from("SHELL"), OsString::from("/bin/bash")));
    E2e::spawn(config).expect("spawn the ait binary")
}

/// Type `command` + Enter and wait out its execution: its own echo
/// must render first — until it does, the previous bare prompt still
/// satisfies [`wait_fresh_prompt`] — then a fresh bare prompt, which
/// draws only after the command finished and its `D` mark sealed the
/// block (the `run_line` recipe from `shell_integration.rs`).
fn run_line(app: &mut E2e, command: &str, echo_probe: &str) {
    app.type_text(&format!("{command}\n"));
    app.wait_for_screen(&format!("the echo of `{command}`"), |app| {
        app.screen_text()
            .lines()
            .any(|line| line.contains(echo_probe))
    });
    wait_fresh_prompt(app, "the prompt back");
}

/// The `Store` the app left behind in the fixture dir — opened the
/// same way a second `ait` instance would (WAL makes the rows
/// visible).
fn open_store(xdg: &Path) -> Store {
    Store::open(xdg.join("ait").join("store.db")).expect("open the app's store.db")
}

/// Spec R08 us-23 + ticket: running commands in a session leaves one
/// `terminal_sessions` row per spawned tab — `ended_at` stamped on tab
/// close (`Alt+W`) and on app exit (`Alt+Q`) — and one `blocks` row per
/// sealed block, carrying the command, exit code, duration and
/// wall-clock bounds.
#[test]
fn commands_leave_session_and_block_rows() {
    if !Path::new("/bin/bash").exists() {
        return; // bash isn't installed everywhere tests run
    }
    let xdg = tempfile::tempdir().expect("fixture tempdir");
    let mut app = spawn_ait_with_data(xdg.path());
    app.wait_for_screen("the bash chrome", |app| {
        app.line(0).contains("1:bash") && app.line(STATUS_ROW).contains("· bash ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");
    let spawn_cwd = app.cwd().to_path_buf();

    run_line(&mut app, "echo R08MARKER", "echo R08MARKER");
    run_line(&mut app, "false", "false");
    // `seq`'s output — `9000`, `9001`, `9002` — exists only as output;
    // its middle line is the no-output-text sentinel asserted below.
    run_line(&mut app, "seq 9000 9002", "seq 9000 9002");

    // A second tab gets its own session row; `Alt+W` closes it —
    // `ended_at` on tab close, before app exit.
    app.press("Alt+T");
    app.wait_for_screen("the second tab's chrome", |app| {
        app.line(0).contains("2:bash")
    });
    wait_fresh_prompt(&mut app, "the new tab's prompt");
    run_line(&mut app, "echo TAB2", "echo TAB2");
    app.press("Alt+W");
    app.wait_for_screen("the first tab active again", |app| {
        app.line(0).contains("1:bash") && !app.line(0).contains("2:bash")
    });

    // The block writes are synchronous per drain, so the rows are
    // durable the moment `Alt+Q` finishes app exit.
    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");

    let store = open_store(xdg.path());
    let sessions = store.list_terminal_sessions().expect("list sessions");
    assert_eq!(sessions.len(), 2, "one row per spawned tab");
    let (first, second) = (&sessions[0], &sessions[1]);
    assert_eq!(first.shell, "bash");
    assert_eq!(first.cwd, spawn_cwd.to_string_lossy(), "cwd at spawn");
    for session in [&first, &second] {
        let ended = session
            .ended_at
            .expect("ended_at stamped — Alt+W or app exit");
        assert!(session.started_at <= ended, "bounds are ordered");
    }

    // Tab 1's blocks: the two commands, in order.
    let blocks = store.list_blocks(first.id).expect("list blocks");
    let commands: Vec<&str> = blocks.iter().map(|b| b.command.as_str()).collect();
    let marker = commands
        .iter()
        .position(|c| *c == "echo R08MARKER")
        .expect("the echo block persisted");
    assert_eq!(blocks[marker].exit, Some(0));
    assert_eq!(
        commands[marker + 1],
        "false",
        "the next sealed block is the second command"
    );
    assert_eq!(blocks[marker + 1].exit, Some(1), "false's exit code");
    for block in &blocks[marker..=marker + 1] {
        assert!(block.duration.is_some(), "D−C duration stored");
        let (started, ended) = (
            block.started_at.expect("started_at stored"),
            block.ended_at.expect("ended_at stored"),
        );
        assert!(started <= ended, "the wall-clock bounds are ordered");
    }

    // Tab 2's blocks live against its own session row.
    let second_blocks = store.list_blocks(second.id).expect("list tab-2 blocks");
    assert!(
        second_blocks.iter().any(|b| b.command == "echo TAB2"),
        "tab 2's command persisted under its session: {second_blocks:?}"
    );

    // Metadata only: no stored command ever carries output text —
    // `seq`'s middle row only ever existed as output, never as a
    // typed line.
    assert!(
        commands.iter().all(|c| !c.contains("9001")),
        "output text never lands in a block row: {commands:?}"
    );
}

/// Spec R08 us-25 + ticket: a store that can't open degrades to a
/// toast and a fully functional terminal — never a crash, never a
/// render stall. `XDG_DATA_HOME` pointed at a regular FILE makes
/// `create_dir_all` fail on every spawn.
#[test]
fn an_unopenable_store_toasts_and_the_terminal_still_works() {
    let xdg = tempfile::tempdir().expect("fixture tempdir");
    let blocker = xdg.path().join("blocked");
    fs::write(&blocker, b"not a directory").expect("write the blocker file");

    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config.env.push((
        OsString::from("XDG_DATA_HOME"),
        blocker.as_os_str().to_os_string(),
    ));
    let mut app = E2e::spawn(config).expect("spawn the ait binary");

    app.wait_for_screen("the store-unavailable toast", |app| {
        app.line(STATUS_ROW).contains("store unavailable")
    });

    // The app is a terminal first: commands still run after the toast.
    wait_fresh_prompt(&mut app, "the first prompt");
    run_line(&mut app, "echo STILL-WORKS", "echo STILL-WORKS");

    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(
        status.success(),
        "the unpersisted run still exits cleanly, got {status:?}"
    );
}
