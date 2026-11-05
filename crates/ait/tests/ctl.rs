//! e2e for `ait ctl` (Spec R11 §5.10.5, ticket R11-T6): a real `ait`
//! instance running in a PTY driven by real `ait ctl` subprocesses —
//! `send`/`last-block`/`open-tab` round-trips, the outside-session
//! single-socket resolution and the inside-session `AIT_CTL_SOCKET`
//! path.
//!
//! `XDG_RUNTIME_DIR` pins the socket dir to a per-test tempdir: the
//! outside resolution scans it for exactly one `*.sock`, so parallel
//! test instances can never cross-talk. `SHELL=/bin/bash` makes the
//! injected `--rcfile` snippet live — its OSC 133 marks seal the
//! blocks `last-block` reads (`/bin/sh` is `Absent`: no marks, no
//! blocks, and `last-block` would answer "no sealed blocks"). As root
//! the bash prompt ends `#`, so a *verbatim* submitted row already
//! reads `# <line>` and a classifier rewrite would read `# # <line>`
//! — the doubled hash is the rewrite's tell.

#![cfg(unix)]

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use ait_testkit::{ait_bin, wait_fresh_prompt, E2e, Size, SpawnConfig};

/// 80x24: row 0 is the tab bar, row 23 the status line.
const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};
const STATUS_ROW: usize = 23;

/// Bound on the one wait with no screen evidence — a `send` into a
/// non-active tab's block seal.
const BOUND: Duration = Duration::from_secs(15);

/// Spawn `ait` with bash tabs (the injected snippet's marks seal
/// blocks) and `XDG_RUNTIME_DIR` pinned to `runtime` — the control
/// socket lands at `runtime/ait/<pid>.sock` and every spawned session
/// inherits the same dir.
fn spawn_ait(runtime: &Path) -> E2e {
    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config
        .env
        .push((OsString::from("SHELL"), OsString::from("/bin/bash")));
    config.env.push((
        OsString::from("XDG_RUNTIME_DIR"),
        runtime.as_os_str().to_os_string(),
    ));
    E2e::spawn(config).expect("spawn the ait binary")
}

/// Wait for the app's chrome and an interactive bash prompt: the tab
/// bar's `1:bash`, the `· bash ·` status slot, then a row ending in a
/// prompt char (`#` under uid 0).
fn wait_ready(app: &mut E2e) {
    app.wait_for_screen("the bash tab and status line", |app| {
        app.line(0).contains("1:bash") && app.line(STATUS_ROW).contains("· bash ·")
    });
    wait_fresh_prompt(app, "the first bash prompt");
}

/// `ait ctl <args>` resolving the instance outside-session: no
/// `AIT_CTL_SOCKET`, so the `*.sock` scan under `runtime/ait/` must
/// find exactly this one (us-32).
fn ctl(runtime: &Path, args: &[&str]) -> Output {
    Command::new(ait_bin!())
        .arg("ctl")
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .env_remove("AIT_CTL_SOCKET")
        .output()
        .expect("spawn the ait binary")
}

/// `ctl` retried until it succeeds or [`BOUND`] elapses — for the one
/// round-trip whose seal evidence renders on a non-active tab (no
/// screen to gate on). Still a bounded wait, never a fixed sleep: the
/// poll exits on the first success.
fn ctl_until_ok(runtime: &Path, args: &[&str]) -> Output {
    let deadline = Instant::now() + BOUND;
    loop {
        let output = ctl(runtime, args);
        if output.status.success() {
            return output;
        }
        assert!(
            Instant::now() < deadline,
            "ctl {args:?} never succeeded: {}",
            stderr(&output)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// us-32/34/37/38/41: the outside-session round trip — `send` writes
/// into the active tab, the command's `D` mark seals the block (`✓ 0`
/// on the status line) and `last-block` reads its output, command and
/// exit code back; a tab that never ran a command answers non-zero
/// naming the empty case (us-39 — not an empty result).
#[test]
fn send_then_last_block_round_trips_outside_a_session() {
    if !Path::new("/bin/bash").exists() {
        return; // bash isn't installed everywhere tests run
    }
    let runtime = tempfile::tempdir().expect("runtime tempdir");
    let mut app = spawn_ait(runtime.path());
    wait_ready(&mut app);

    let sent = ctl(runtime.path(), &["send", "echo R11T6_OUTSIDE"]);
    assert!(sent.status.success(), "send stderr: {}", stderr(&sent));
    // `send`'s stdout is the JSON response — us-41 composes with `jq`.
    assert_eq!(stdout(&sent), "{\"ok\":true}\n");

    // `✓ 0` lands with the sealing `D` mark — the block is readable.
    app.wait_for_screen("the sent command sealed", |app| {
        app.screen_text().contains("R11T6_OUTSIDE")
            && app.line(STATUS_ROW).contains("✓ 0")
    });

    let output = ctl(runtime.path(), &["last-block"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output).trim_end(), "R11T6_OUTSIDE");
    let command = ctl(runtime.path(), &["last-block", "--command"]);
    assert!(command.status.success(), "stderr: {}", stderr(&command));
    assert_eq!(stdout(&command), "echo R11T6_OUTSIDE\n");
    let exit = ctl(runtime.path(), &["last-block", "--exit"]);
    assert!(exit.status.success(), "stderr: {}", stderr(&exit));
    assert_eq!(stdout(&exit), "0\n");

    // us-39: a fresh tab with no sealed blocks fails clearly rather
    // than answering an empty readout — "no result yet" is
    // distinguishable from an empty result.
    let opened = ctl(runtime.path(), &["open-tab"]);
    assert!(opened.status.success(), "stderr: {}", stderr(&opened));
    let no_blocks = ctl(runtime.path(), &["last-block", "--tab", "1"]);
    assert!(!no_blocks.status.success());
    assert!(
        stderr(&no_blocks).contains("no sealed blocks"),
        "stderr: {}",
        stderr(&no_blocks)
    );
}

/// us-33/36: `open-tab` allocates a tab in the running instance and
/// answers its `AIT_SESSION_ID`; `--tab <id>` on `send`/`last-block`
/// addresses a tab other than the active one — stable identities, not
/// positions.
#[test]
fn open_tab_allocates_a_tab_send_and_last_block_target() {
    if !Path::new("/bin/bash").exists() {
        return; // bash isn't installed everywhere tests run
    }
    let runtime = tempfile::tempdir().expect("runtime tempdir");
    let mut app = spawn_ait(runtime.path());
    wait_ready(&mut app);

    let opened = ctl(runtime.path(), &["open-tab"]);
    assert!(opened.status.success(), "stderr: {}", stderr(&opened));
    // Tab ids allocate upward from 0 — the first tab is "0", so the
    // opened one is "1" (us-33's printed session id).
    assert_eq!(stdout(&opened), "{\"ok\":true,\"session_id\":\"1\"}\n");
    app.wait_for_screen("the opened tab active", |app| {
        app.line(0).contains("2:bash")
    });
    wait_fresh_prompt(&mut app, "the new tab's prompt");

    // `--tab 0` reaches the BACKGROUND tab: tab 1 is active, yet the
    // sent command runs in tab 0 and its sealed block reads back under
    // the same id — the round trip's seal has no screen evidence (the
    // grid shows tab 1), so `ctl_until_ok` polls the readout.
    let sent = ctl(runtime.path(), &["send", "--tab", "0", "echo R11T6_TAB0"]);
    assert!(sent.status.success(), "stderr: {}", stderr(&sent));
    let command = ctl_until_ok(runtime.path(), &["last-block", "--tab", "0", "--command"]);
    assert_eq!(stdout(&command), "echo R11T6_TAB0\n");

    // The active tab takes the same verbs — its `✓ 0` seal renders on
    // the status line, so the screen gates this half.
    let sent = ctl(runtime.path(), &["send", "--tab", "1", "echo R11T6_TAB1"]);
    assert!(sent.status.success(), "stderr: {}", stderr(&sent));
    app.wait_for_screen("the targeted command sealed", |app| {
        app.screen_text().contains("R11T6_TAB1") && app.line(STATUS_ROW).contains("✓ 0")
    });
    let command = ctl_until_ok(runtime.path(), &["last-block", "--tab", "1", "--command"]);
    assert_eq!(stdout(&command), "echo R11T6_TAB1\n");
}

/// us-31/41: `ait ctl` typed inside a session drives its own instance —
/// `AIT_CTL_SOCKET` is exported into the spawned shell's env, so the
/// client never needs the runtime-dir scan. The `send` bytes land in
/// this tab's own PTY and `last-block` reads the sealed block back
/// onto the grid.
#[test]
fn ctl_inside_a_session_drives_its_own_instance() {
    if !Path::new("/bin/bash").exists() {
        return; // bash isn't installed everywhere tests run
    }
    let runtime = tempfile::tempdir().expect("runtime tempdir");
    let mut app = spawn_ait(runtime.path());
    wait_ready(&mut app);

    // The exported socket path — the inside-session resolution seam.
    app.type_text("echo \"SOCK=$AIT_CTL_SOCKET\"\n");
    let socket_dir = runtime.path().join("ait");
    app.wait_for_screen("the exported control socket path", |app| {
        app.screen_text()
            .contains(&format!("SOCK={}/", socket_dir.display()))
            && app.screen_text().contains(".sock")
    });
    wait_fresh_prompt(&mut app, "the prompt after the env echo");

    // `send` inside: `{"ok":true}` answers on the grid, then the sent
    // line itself runs in this same tab.
    let bin = ait_bin!().to_string_lossy().into_owned();
    app.type_text(&format!("{bin} ctl send echo R11T6_INSIDE\n"));
    app.wait_for_screen("the inside send answered and ran", |app| {
        app.screen_text().lines().any(|line| line == "{\"ok\":true}")
            && app
                .screen_text()
                .lines()
                .any(|line| line == "R11T6_INSIDE")
    });
    wait_fresh_prompt(&mut app, "the prompt after the sent command");

    // `last-block` inside: the newest sealed block is the sent
    // `echo R11T6_INSIDE` — its command, output and exit read back.
    app.type_text(&format!("{bin} ctl last-block --command\n"));
    app.wait_for_screen("the inside command readout", |app| {
        app.screen_text()
            .lines()
            .any(|line| line == "echo R11T6_INSIDE")
    });
    wait_fresh_prompt(&mut app, "the prompt after --command");

    app.type_text(&format!("{bin} ctl last-block --exit\n"));
    app.wait_for_screen("the inside exit readout", |app| {
        app.screen_text().lines().any(|line| line == "0")
    });
}

/// us-16/44: `send` input reaches the shell verbatim — ctl bytes are
/// not keystrokes (the dispatch marks the tab's input shadow stale),
/// so a natural-language sentence is never rewritten through the
/// classifier path. bash runs it and answers `command not found` —
/// under the root `#` prompt a verbatim row already reads `# <line>`,
/// so the never-rewritten tell is the missing doubled hash.
#[test]
fn sent_input_is_never_classified() {
    if !Path::new("/bin/bash").exists() {
        return; // bash isn't installed everywhere tests run
    }
    let runtime = tempfile::tempdir().expect("runtime tempdir");
    let mut app = spawn_ait(runtime.path());
    wait_ready(&mut app);

    let sent = ctl(runtime.path(), &["send", "what is the largest file"]);
    assert!(sent.status.success(), "stderr: {}", stderr(&sent));

    // The shell ran the line itself — bash's `command not found` and
    // the `✗ 127` seal — and no `# # <line>` rewrite row exists.
    app.wait_for_screen("the verbatim failure sealed", |app| {
        app.screen_text().contains("command not found")
            && app.line(STATUS_ROW).contains("✗ 127")
    });
    assert!(
        !app
            .screen_text()
            .contains("# # what is the largest file"),
        "the sent line was never rewritten: {}",
        app.screen_text()
    );

    let exit = ctl(runtime.path(), &["last-block", "--exit"]);
    assert!(exit.status.success(), "stderr: {}", stderr(&exit));
    assert_eq!(stdout(&exit), "127\n");
}
