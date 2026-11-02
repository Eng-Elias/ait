//! e2e for §5.5 shell integration — the assertions a TOML scenario
//! cannot express (Spec R05, ticket R05-T9):
//!
//! - `Alt+B`'s copy rides the OSC-52 path like a mouse release, but the
//!   copied block's first and last rows carry the *host-dependent*
//!   prompt text — `expect_clipboard`'s exact match can't spell it.
//! - `__ait_run`'s response must be *absent* from the grid — the verbs
//!   have no negative assert, so the invisibility check lives here.
//! - `[terminal] shell_integration = false` needs a `settings.toml`
//!   fixture under a per-test tempdir — a scenario `env` value is a
//!   literal string and can't interpolate one (the R03 tests' gap).
//!
//! The prompt text is equally host-dependent (the user's real rc files
//! set PS1), so every gate waits on marker output, never on `$ `.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use ait_testkit::{ait_bin, E2e, Size, SpawnConfig};

/// 80x24: row 0 is the tab bar, row 23 the status line.
const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};
const STATUS_ROW: usize = 23;

/// Spawn `ait` with `SHELL` pointed at `shell` — the spawned app's tabs
/// resolve it, so the injection planner sees its basename.
fn spawn_ait_with_shell(shell: &str) -> E2e {
    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config
        .env
        .push((OsString::from("SHELL"), OsString::from(shell)));
    E2e::spawn(config).expect("spawn the ait binary")
}

/// Wait for a fresh, empty prompt before typing: an interactive
/// bash/zsh/fish prompt ends `$`/`%`/`>` (host-dependent text, prompt
/// shape is the constant) and a line *ending* in such a char is a
/// prompt with nothing typed yet — screen rows are trim_end'ed, and a
/// prompt carrying text ends in the command instead. Without this
/// gate a `type_text` can beat the prompt's render and interleave
/// echo, prompt and output on one row (the `-sh` scenario race).
fn wait_fresh_prompt(app: &mut E2e, description: &str) {
    app.wait_for_screen(description, |app| {
        app.screen_text()
            .lines()
            .any(|line| line.ends_with('$') || line.ends_with('%') || line.ends_with('>'))
    });
}

/// Type `command` + Enter and wait out its execution: first its own
/// ECHO must render — until it does, the previous bare prompt still
/// satisfies [`wait_fresh_prompt`] and a prompt wait could pass on the
/// pre-type screen — then a fresh bare prompt, which draws only after
/// the command finished. Without the pair, a fast `type_text` beats
/// the next prompt's render and interleaves echo, prompt and output
/// on one row (the `-sh` scenario race). `echo_probe` is a substring
/// of the echo that fits on one row — a long command wraps and a
/// full-text probe would never match.
fn run_line(app: &mut E2e, command: &str, echo_probe: &str) {
    app.type_text(&format!("{command}\n"));
    app.wait_for_screen(&format!("the echo of `{command}`"), |app| {
        app.screen_text()
            .lines()
            .any(|line| line.contains(echo_probe))
    });
    wait_fresh_prompt(app, "the prompt back");
}

/// The first installed path for each supported basename — the set of
/// shells this machine can actually drive through the snippet flow.
/// Empty entries mean the shell isn't installed here (CI and dev hosts
/// differ); bash presence is asserted by the caller.
fn installed_supported_shells() -> Vec<(&'static str, &'static str)> {
    let candidates: &[(&str, &[&str])] = &[
        ("bash", &["/bin/bash", "/usr/bin/bash"]),
        ("zsh", &["/bin/zsh", "/usr/bin/zsh", "/usr/local/bin/zsh"]),
        (
            "fish",
            &["/bin/fish", "/usr/bin/fish", "/usr/local/bin/fish"],
        ),
    ];
    let mut found = Vec::new();
    for (name, paths) in candidates {
        if let Some(path) = paths.iter().find(|path| Path::new(path).exists()) {
            found.push((*name, *path));
        }
    }
    found
}

/// Spec R05 us-1/6/7/22–24 — the full integrated journey on real bash:
/// init_shell lands (blocks forming proves the snippet's marks flow,
/// which only exists post-snippet), `Alt+Up` walks to a block start,
/// `Alt+B` copies command+output through the OSC-52 path, and a typed
/// `__ait_run` round-trips — its merged output and exit code return
/// over `7701` frames that never render while the request line stays
/// visible.
#[test]
fn bash_blocks_copy_and_inband_roundtrip() {
    if !Path::new("/bin/bash").exists() {
        return; // bash isn't installed everywhere tests run
    }
    let mut app = spawn_ait_with_shell("/bin/bash");
    app.wait_for_screen("the bash chrome", |app| {
        app.line(0).contains("1:bash") && app.line(STATUS_ROW).contains("· bash ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");

    // `true` closes a block with exit 0 — the `✓ 0` status segment is
    // the screen-visible proof integration went live (the marks and
    // hooks only flow once the snippet is sourced).
    run_line(&mut app, "true", "true");
    app.wait_for_screen("the first block's status segment", |app| {
        app.line(STATUS_ROW).contains("✓ 0")
    });

    // §5.5.1: the user-visible `__ait_run` trace (us-23) executes the
    // snippet's function — its `7701` reply is decoded by ait but never
    // rendered (us-24). `6563686f…` is hex("echo R05INBAND"); a marker
    // command afterwards proves the run finished before we assert.
    run_line(
        &mut app,
        "__ait_run 7 6563686f20523035494e42414e44",
        "__ait_run 7",
    );
    run_line(&mut app, "echo AFTER-INBAND", "echo AFTER-INBAND");
    let screen = app.screen_text();
    assert!(screen.contains("__ait_run 7"), "trace line: {screen}");
    assert!(
        !screen.contains("R05INBAND"),
        "the 7701 payload never renders: {screen}"
    );
    assert!(
        !screen.contains("command not found"),
        "the snippet defines __ait_run: {screen}"
    );

    // Fill scrollback so Alt+Up has somewhere to land, then jump to the
    // seq block's start (the scenario TOML asserts the same walk).
    run_line(&mut app, "seq 1 40", "seq 1 40");
    run_line(&mut app, "echo TAILMARKER", "echo TAILMARKER");
    app.press("Alt+Up");
    app.wait_for_screen("the seq block start topping the view", |app| {
        app.line(1).contains("seq 1 40")
    });

    // `Alt+B` selects the block the view sits in — the seq block's
    // whole echo+output range — and copies it via the shared OSC-52
    // path (us-17/18). The clipboard text is host-dependent at the
    // prompt edge, so assert it spans the command AND its output.
    app.press("Alt+B");
    app.wait_for_screen("the block copy reaching the clipboard", |app| {
        !app.clipboard_writes().is_empty()
    });
    let copied = app.clipboard_writes().last().unwrap();
    assert!(
        copied.contains("seq 1 40"),
        "the copy includes the command echo: {copied:?}"
    );
    assert!(
        copied.trim_end().ends_with("\n40"),
        "the copy ends at the last output line: {copied:?}"
    );
    assert!(
        !copied.contains("echo TAILMARKER"),
        "the next command's echo stays out of the copy: {copied:?}"
    );
}

/// Spec R05's per-shell matrix at e2e level: every installed supported
/// shell spawns integrated — its snippet emits `init_shell` and the
/// OSC 133 marks a finished `true` needs to render `✓ 0`. Shells that
/// aren't installed are skipped; the assertion that *some* shell ran
/// keeps the test honest everywhere.
#[test]
fn installed_supported_shells_integrate() {
    let mut tested = 0;
    for (name, path) in installed_supported_shells() {
        let mut app = spawn_ait_with_shell(path);
        app.wait_for_screen("the shell's chrome", |app| {
            app.line(STATUS_ROW).contains(&format!("· {name} ·"))
        });
        // The prompt renders only once rc + snippet processing is done
        // — the integration-ready gate before typing.
        wait_fresh_prompt(&mut app, "the shell's first prompt");
        run_line(&mut app, "true", "true");
        app.wait_for_screen("the first block's status segment", |app| {
            app.line(STATUS_ROW).contains("✓ 0")
        });
        tested += 1;
    }
    assert!(
        tested >= 1,
        "no supported shell (bash/zsh/fish) is installed on this host"
    );
}

/// Spec R05 us-3: `[terminal] shell_integration = false` leaves bash
/// uninjected — commands run and output renders, but no marks means no
/// block segment and no errors, the §5.5 degrade contract.
#[test]
fn shell_integration_optout_leaves_a_plain_terminal() {
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let ait_dir = dir.path().join("ait");
    fs::create_dir_all(&ait_dir).expect("create the fixture config dir");
    fs::write(
        ait_dir.join("settings.toml"),
        "[terminal]\nshell_integration = false\n",
    )
    .expect("write fixture settings.toml");

    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config.env.push((
        OsString::from("XDG_CONFIG_HOME"),
        dir.path().as_os_str().to_os_string(),
    ));
    config
        .env
        .push((OsString::from("SHELL"), OsString::from("/bin/bash")));
    let mut app = E2e::spawn(config).expect("spawn the ait binary");

    app.wait_for_screen("the bash chrome", |app| {
        app.line(STATUS_ROW).contains("· bash ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");
    run_line(&mut app, "false", "false");
    run_line(&mut app, "echo OPTOUT-MARKER", "echo OPTOUT-MARKER");
    // `false` failing yet no `✗` appearing is exactly "no blocks, no
    // errors" — the summary line keeps its unintegrated shape.
    assert!(
        app.line(STATUS_ROW).contains("· bash · — · running"),
        "no block segment without integration: {}",
        app.line(STATUS_ROW)
    );
}
