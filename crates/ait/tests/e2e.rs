//! e2e tests for the `ait` binary (Spec E01, ticket E01-T1): the real
//! binary in a real PTY via `ait-testkit`'s [`E2e`] harness. The harness
//! pins `SHELL=/bin/sh`, a fresh temp cwd and `TERM=xterm-256color`, so
//! the spawned app's chrome — tab bar (`N:sh` titles) and status line
//! (`cwd · sh · — · running`) — reproduces identically on every machine.
//!
//! The Round-3 config tests below are code-first rather than TOML
//! scenarios (Spec R03, ticket R03-T8): a scenario's `env` values are
//! literal strings, so `XDG_CONFIG_HOME` cannot be interpolated to a
//! per-test tempdir holding a fixture `settings.toml` — a testkit gap.
//! Here each test writes its own settings file under a `tempfile` dir
//! and passes the path through [`SpawnConfig::env`], which applies after
//! the harness's pinned `XDG_CONFIG_HOME` and therefore overrides it.

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
/// for visual regressions). The pinned dash prompt is `$ `;
/// [`E2e::snapshot`] redacts the temp cwd so the snapshot is
/// machine-independent.
#[test]
fn initial_screen_snapshot() {
    let mut app = spawn_ait();
    wait_for_chrome(&mut app);
    app.wait_for_screen("the shell prompt", |app| app.line(1) == "$");

    insta::assert_snapshot!(app.snapshot());
}

// ---------------------------------------------------------------------------
// Spec R03: settings.toml driving the live app (ticket R03-T8).
// ---------------------------------------------------------------------------

/// Write `contents` as `<xdg>/ait/settings.toml`, creating the `ait/`
/// directory first — the app's hot-reload watcher attaches to the
/// settings file's *parent* directory at startup, so the directory must
/// exist before spawn for reload coverage.
fn write_settings(xdg: &Path, contents: &str) {
    let dir = xdg.join("ait");
    fs::create_dir_all(&dir).expect("create the fixture config directory");
    fs::write(dir.join("settings.toml"), contents).expect("write fixture settings.toml");
}

/// Spawn `ait` with `XDG_CONFIG_HOME` pointing at `xdg` — applied after
/// the harness's pinned env, so it overrides the missing-dir default.
fn spawn_ait_with_xdg(xdg: &Path) -> E2e {
    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config.env.push((
        OsString::from("XDG_CONFIG_HOME"),
        xdg.as_os_str().to_os_string(),
    ));
    E2e::spawn(config).expect("spawn the ait binary")
}

/// A valid `settings.toml`: one local endpoint whose model carries
/// `alias`, and the `default` profile's `base_model` resolving to it.
fn settings_with_model(alias: &str) -> String {
    format!(
        r#"
[agents.custom_endpoints.local]
name     = "Local"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "wire-name"
alias      = "{alias}"
config_key = "key-1"

[agents.execution_profiles.default]
base_model = "key-1"
"#
    )
}

/// Stories 1 + 24: a `settings.toml` under `$XDG_CONFIG_HOME/ait/` is
/// picked up at startup and the status line's model slot shows the
/// resolved `base_model`'s alias instead of the `—` placeholder.
#[test]
fn settings_file_sets_the_status_line_model() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_model("Qwen Coder"));

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the resolved model alias on the status line", |app| {
        app.line(STATUS_ROW).contains("· sh · Qwen Coder · running")
    });
    assert_eq!(app.line(0), " 1:sh");
}

/// Stories 7–8 + strict unknown-key rejection (ADR-0006): an invalid
/// `settings.toml` never costs the terminal — `ait` boots on built-in
/// defaults, toasts the `ConfigError` (`config error: <key-path>: …`),
/// and reverts to the normal `—` status summary once the toast expires.
#[test]
fn invalid_settings_still_boots_with_a_config_error_toast() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    // An unknown key is rejected by `deny_unknown_fields` — the error
    // names the offending key path.
    write_settings(dir.path(), "[agents]\nbogus_key = true\n");

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the config-error toast on the status line", |app| {
        app.line(STATUS_ROW).contains("config error:")
    });
    assert!(
        app.line(STATUS_ROW).contains("bogus_key"),
        "the toast names the offending key: {}",
        app.line(STATUS_ROW)
    );
    // Boots anyway — past the ~3 s toast the default summary returns.
    app.wait_for_screen("the default status summary after the toast", |app| {
        app.line(STATUS_ROW).contains("· sh · — · running")
    });
    assert_eq!(app.line(0), " 1:sh");
}

/// Story 12: `[terminal] shell` overrides the program NEW sessions
/// spawn — the tab title and the status line's shell slot are the
/// spawned program's basename, so `cat` shows up as `1:cat` / `· cat ·`.
#[test]
fn terminal_shell_spawns_the_configured_program() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), "[terminal]\nshell = \"cat\"\n");

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the configured shell in the chrome", |app| {
        app.line(0).contains("1:cat") && app.line(STATUS_ROW).contains("· cat · — · running")
    });
}

/// Story 13: `[terminal] scrollback_lines` bounds the scrollback a new
/// session keeps — with a 5-line capacity the `seq 1 100` command echo
/// falls out of retained history entirely (a default-capacity session
/// would still show it at the scroll top, per scrollback-keys.toml).
#[test]
fn terminal_scrollback_lines_bounds_retained_history() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), "[terminal]\nscrollback_lines = 5\n");

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("the shell prompt", |app| app.line(1) == "$");

    app.type_text("seq 1 100\n");
    // Gate on the fresh prompt at the content band's bottom row, so the
    // scroll accounting below is settled (output finished, prompt back).
    app.wait_for_screen("seq's output and a fresh prompt", |app| app.line(22) == "$");

    app.press("Shift+Home");
    // At the scroll top only the last ~5 scrolled-off lines survive:
    // the top row is a seq number near the end, and the command echo —
    // line 1 of the session's history — is gone entirely.
    app.wait_for_screen("a truncated scroll top", |app| {
        !app.screen_text().contains("seq 1 100") && app.line(1).chars().all(|c| c.is_ascii_digit())
    });
    let oldest: u32 = app
        .line(1)
        .parse()
        .expect("the scroll top is a seq output line");
    assert!(
        oldest >= 50,
        "5-line scrollback keeps only the tail of 100 lines, got line {oldest}"
    );
}

/// Stories 10–11 + 14: edits to `settings.toml` hot-reload live —
/// the model slot re-renders, and reloaded `[terminal]` options apply
/// to NEW sessions only (the live `sh` tab keeps its spawn-time
/// program; a post-reload `Alt+T` tab gets the configured `cat`).
/// A broken save keeps the last-good settings and toasts the error.
#[test]
fn hot_reload_swaps_the_model_and_keeps_last_good() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let settings_path = dir.path().join("ait").join("settings.toml");
    write_settings(dir.path(), &settings_with_model("First Model"));

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("the first model on the status line", |app| {
        app.line(STATUS_ROW)
            .contains("· sh · First Model · running")
    });

    // A good edit swaps the live settings: the model slot re-renders on
    // the existing tab, and the new `[terminal]` becomes the template
    // for sessions spawned from now on.
    let reloaded = format!(
        "[terminal]\nshell = \"cat\"\n{}",
        settings_with_model("Second Model")
    );
    fs::write(&settings_path, reloaded).expect("rewrite settings.toml");
    app.wait_for_screen("the reloaded model on the status line", |app| {
        app.line(STATUS_ROW)
            .contains("· sh · Second Model · running")
    });
    app.press("Alt+T");
    app.wait_for_screen("a new tab on the reloaded shell", |app| {
        app.line(0).contains("2:cat")
            && app
                .line(STATUS_ROW)
                .contains("· cat · Second Model · running")
    });

    // A broken save keeps last-good: the error toasts, then the summary
    // returns with the previous model still resolved.
    fs::write(&settings_path, "[agents]\nbogus_key = true\n").expect("break settings.toml");
    app.wait_for_screen("the reload-error toast", |app| {
        app.line(STATUS_ROW).contains("config error:")
    });
    app.wait_for_screen("last-good settings after the toast", |app| {
        app.line(STATUS_ROW)
            .contains("· cat · Second Model · running")
    });
}

/// Story 24's fallback chain, middle step: a model entry with an EMPTY
/// `alias` renders its wire `name` in the status line's model slot
/// (`alias → wire name → —`). `alias` is a required field, so `""` —
/// not an absent key — is what drives the fallback.
#[test]
fn model_slot_falls_back_to_wire_name() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(
        dir.path(),
        r#"
[agents.custom_endpoints.local]
name     = "Local"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "qwen2.5-coder:14b"
alias      = ""
config_key = "key-1"

[agents.execution_profiles.default]
base_model = "key-1"
"#,
    );

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the wire name on the status line", |app| {
        app.line(STATUS_ROW)
            .contains("· sh · qwen2.5-coder:14b · running")
    });
}

/// Endpoints declared but no `default` profile (or no `base_model` on
/// it): `resolve_model` yields `None` and the model slot renders `—` —
/// a quiet empty state, not a config error.
#[test]
fn model_slot_is_dash_without_a_default_profile() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(
        dir.path(),
        r#"
[agents.custom_endpoints.local]
name     = "Local"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "wire-name"
config_key = "key-1"
"#,
    );

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the dash model slot", |app| {
        app.line(STATUS_ROW).contains("· sh · — · running")
    });
    // A toast renders on the same tick the status does — by the time the
    // summary is up, an error would already be on screen.
    assert!(
        !app.line(STATUS_ROW).contains("config error:"),
        "no error toast for a merely-unconfigured profile: {}",
        app.line(STATUS_ROW)
    );
}

/// Story 10's creation case: the watcher attaches to the settings
/// file's parent directory — creating it on the first-run path where
/// `$XDG_CONFIG_HOME/ait/` doesn't exist yet — so the user's first-ever
/// `settings.toml` save flips `—` to the resolved model without a
/// restart.
#[test]
fn settings_created_after_spawn_hot_loads() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    // No `ait/` directory at all — the true first-run state.

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("defaults before any settings file", |app| {
        app.line(STATUS_ROW).contains("· sh · — · running")
    });

    write_settings(dir.path(), &settings_with_model("Late Model"));

    app.wait_for_screen("the hot-loaded model on the status line", |app| {
        app.line(STATUS_ROW).contains("· sh · Late Model · running")
    });
}

/// Deleting `settings.toml` mid-session reloads to built-in defaults —
/// `load()` treats a missing file as defaults and the watcher reports
/// the deletion, so the model slot returns to `—`.
#[test]
fn settings_deleted_reverts_to_defaults() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let settings_path = dir.path().join("ait").join("settings.toml");
    write_settings(dir.path(), &settings_with_model("Going Away"));

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("the model before deletion", |app| {
        app.line(STATUS_ROW).contains("· sh · Going Away · running")
    });

    fs::remove_file(&settings_path).expect("delete settings.toml");

    app.wait_for_screen("defaults after the file is deleted", |app| {
        app.line(STATUS_ROW).contains("· sh · — · running")
    });
}

/// Story 14's scrollback half: a reloaded `scrollback_lines` bounds NEW
/// sessions only — the pre-reload tab keeps its full spawn-time
/// history (the `seq 1 100` echo survives at its scroll top), while a
/// tab spawned after the reload is capped at 5 lines.
#[test]
fn existing_tabs_keep_spawn_time_scrollback_after_reload() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let settings_path = dir.path().join("ait").join("settings.toml");
    // No [terminal] — the spawn-time scrollback is the default 10k.
    write_settings(dir.path(), &settings_with_model("First Model"));

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("the first model and a prompt", |app| {
        app.line(STATUS_ROW).contains("First Model") && app.line(1) == "$"
    });
    app.type_text("seq 1 100\n");
    app.wait_for_screen("seq's output and a fresh prompt", |app| app.line(22) == "$");

    // Reload: a 5-line scrollback cap plus a new model — the model swap
    // on the status line gates that the reload landed.
    let reloaded = format!(
        "[terminal]\nscrollback_lines = 5\n{}",
        settings_with_model("Second Model")
    );
    fs::write(&settings_path, reloaded).expect("rewrite settings.toml");
    app.wait_for_screen("the reloaded model on the status line", |app| {
        app.line(STATUS_ROW)
            .contains("· sh · Second Model · running")
    });

    // The live tab keeps its spawn-time capacity: Shift+Home reaches
    // the command echo at the very top of its history.
    app.press("Shift+Home");
    app.wait_for_screen("the echo still at the old tab's scroll top", |app| {
        app.screen_text().contains("seq 1 100")
    });

    // A post-reload tab gets the 5-line cap — the bounded-history
    // assertion from terminal_scrollback_lines_bounds_retained_history.
    app.press("Alt+T");
    app.wait_for_screen("a second tab in the bar", |app| {
        app.line(0).contains("2:sh")
    });
    app.wait_for_screen("the new tab's prompt", |app| app.line(1) == "$");
    app.type_text("seq 1 100\n");
    app.wait_for_screen("seq's output and a fresh prompt", |app| app.line(22) == "$");
    app.press("Shift+Home");
    app.wait_for_screen("a truncated scroll top on the new tab", |app| {
        !app.screen_text().contains("seq 1 100") && app.line(1).chars().all(|c| c.is_ascii_digit())
    });
    let oldest: u32 = app
        .line(1)
        .parse()
        .expect("the scroll top is a seq output line");
    assert!(
        oldest >= 50,
        "the reloaded 5-line cap bounds only the new tab, got line {oldest}"
    );
}
