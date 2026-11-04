//! e2e for §5.7 execution profiles (Spec R07, ticket R07-T7): the
//! round's user-visible surface proven against the real `ait` binary —
//! a permissive profile runs `read_files` with no approve card, a
//! restrictive one parks `run_command` and an approve runs it, a
//! denylisted command renders `Denied` without asking, `Alt+P` updates
//! the status line's profile slot and the next task evaluates the new
//! stance, and a hot reload that deletes the armed profile toasts and
//! re-arms the ask-everything fail-safe.
//!
//! Same seam as the R06 agent scenarios: `ait_llm::testing`'s loopback
//! [`Fixture`] scripts one `openai_chat_completions` SSE response per
//! request (no network), and `SpawnConfig::env` pins `XDG_CONFIG_HOME`
//! to a per-test tempdir holding the fixture `settings.toml` — code-
//! first because a scenario TOML's `env` values cannot interpolate a
//! tempdir path or the fixture's ephemeral port.
//!
//! The harness pins `SHELL=/bin/sh` — dash takes no injection, so
//! `run_command` rides the `Absent` tier whose typed
//! `cmd; printf '__AIT_END_<id>=%d\n' $?` sentinel line is the trace
//! the run_command asserts watch for. Panel-side and terminal-side
//! text stay told apart by the `│` separator.

use std::ffi::OsString;
use std::fs;
use std::path::Path;

use ait_llm::testing::{assert_post, sse, Fixture};
use ait_testkit::{ait_bin, E2e, Size, SpawnConfig};

/// 80x24: row 0 is the tab bar, row 23 the status line; an open panel
/// takes two fifths of the content band — the terminal keeps 48
/// columns, the panel 32.
const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};
const STATUS_ROW: usize = 23;

/// Write `contents` as `<xdg>/ait/settings.toml`, creating the `ait/`
/// directory first — the watcher attaches to the file's parent, so it
/// must exist before spawn for the reload scenario to fire.
fn write_settings(xdg: &Path, contents: &str) {
    let dir = xdg.join("ait");
    fs::create_dir_all(&dir).expect("create the fixture config directory");
    fs::write(dir.join("settings.toml"), contents).expect("write fixture settings.toml");
}

/// A `settings.toml` whose models all live at the fixture's loopback
/// `base_url`, plus whatever `[agents.execution_profiles.*]` tables
/// `profiles` declares. `coding-key` is the `config_key` every model
/// role references; no `api_key_env` and a host keyring miss mean the
/// requests go out unauthenticated.
fn settings_with_fixture(base_url: &str, profiles: &str) -> String {
    format!(
        r#"
[agents.custom_endpoints.local]
name     = "Fixture"
base_url = "{base_url}"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "fixture-model"
alias      = "Fixture Model"
config_key = "coding-key"
{profiles}"#
    )
}

/// The `default` profile with `read_files` at `always_allow` — the
/// permissive stance.
const PERMISSIVE_READS: &str = r#"
[agents.execution_profiles.default]
coding_model = "coding-key"
read_files   = "always_allow"
"#;

/// The `default` profile with `read_files` allowed but
/// `execute_commands` at `always_ask` — the restrictive stance.
const READS_FREE_COMMANDS_ASK: &str = r#"
[agents.execution_profiles.default]
coding_model     = "coding-key"
read_files       = "always_allow"
execute_commands = "always_ask"
"#;

/// The `default` profile allowing every command EXCEPT the denylisted
/// shape — `always_allow` would run anything the lists let through, so
/// a denial can only have come from the `^rm` floor.
const DENYLISTED_RM: &str = r#"
[agents.execution_profiles.default]
coding_model      = "coding-key"
execute_commands  = "always_allow"
command_denylist  = ['^rm']
"#;

/// Two declared profiles — the `Alt+P` direct-cycle case. `default`
/// leaves `read_files` unset (it would ask); `permissive` allows the
/// reads and carries the `base_model` the status slot resolves.
const TWO_PROFILES: &str = r#"
[agents.execution_profiles.default]
coding_model = "coding-key"

[agents.execution_profiles.permissive]
coding_model = "coding-key"
base_model   = "coding-key"
read_files   = "always_allow"
"#;

/// Spawn `ait` with `XDG_CONFIG_HOME` pointing at `xdg` — applied
/// after the harness's pinned env, so it overrides the missing-dir
/// default.
fn spawn_ait_with_xdg(xdg: &Path) -> E2e {
    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config.env.push((
        OsString::from("XDG_CONFIG_HOME"),
        xdg.as_os_str().to_os_string(),
    ));
    E2e::spawn(config).expect("spawn the ait binary")
}

/// Wait until the app's chrome has rendered: ` 1:sh ` in the tab bar,
/// the `… · sh · — · running` summary on the status line.
fn wait_for_chrome(app: &mut E2e) {
    app.wait_for_screen("the tab bar and status line", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh · — · running")
    });
}

/// A line *ending* in `$`/`%`/`>` is a fresh, empty prompt — the
/// gate before any typing, so keystrokes can never interleave with an
/// unrendered prompt.
fn wait_fresh_prompt(app: &mut E2e, description: &str) {
    app.wait_for_screen(description, |app| {
        app.screen_text()
            .lines()
            .any(|line| line.ends_with('$') || line.ends_with('%') || line.ends_with('>'))
    });
}

/// `Alt+A` opens the split focused: the `│` separator renders and the
/// title row reads `│ agent`.
fn open_panel(app: &mut E2e) {
    app.press("Alt+A");
    app.wait_for_screen("the open, focused agent panel", |app| {
        app.screen_text().contains('│') && app.screen_text().contains(" agent")
    });
}

/// Type `task` on the panel's input line and submit it.
fn submit_task(app: &mut E2e, task: &str) {
    app.type_text(task);
    app.wait_for_screen("the task on the input line", |app| {
        app.screen_text().contains(task)
    });
    app.press("Enter");
}

/// The terminal half of one screen row — everything left of the
/// panel's `│` separator. The panel never reaches these columns, so
/// probes here can only be the session's own grid.
fn terminal_half(row: &str) -> &str {
    row.split('│').next().unwrap_or(row)
}

/// `true` when `row`'s terminal half carries a sentinel *result*
/// line — `__AIT_END_<id>=<exit>` — which only the run's `printf`
/// produces. The echoed type-in line carries the same `__AIT_END_`
/// prefix verbatim but `=%d`, never `=<digit>`, so a bare
/// `contains("__AIT_END_")` would pass on echo alone — the exact
/// wait CODING_STANDARDS §15 forbids.
fn sentinel_result(row: &str) -> bool {
    let half = terminal_half(row);
    let Some(start) = half.find("__AIT_END_") else {
        return false;
    };
    half[start..]
        .split('=')
        .nth(1)
        .is_some_and(|tail| tail.starts_with(|ch: char| ch.is_ascii_digit()))
}

/// Whether any screen row's *panel* half trims to exactly `text` —
/// how a conversation status line (`done`, `cancelled`) is found
/// without colliding with terminal-side text.
fn panel_has_line(app: &E2e, text: &str) -> bool {
    app.screen_text().lines().any(|row| {
        row.split('│')
            .nth(1)
            .is_some_and(|half| half.trim() == text)
    })
}

/// A JSON string literal (`"…"`) for hand-built chunk bodies.
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// One `chat.completion.chunk` `data:` frame carrying a content
/// delta — a plain end turn the loop terminates on.
fn chat_text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        json_string(text)
    )
}

/// A `chat.completion.chunk` turn calling `name` once with `args` —
/// the wire shape the `openai_chat_completions` adapter decodes into
/// one complete tool call. `args` is the JSON *text* the `arguments`
/// string carries.
fn tool_turn(call_id: &str, name: &str, args: &str) -> String {
    let mut body = format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":{},\"type\":\"function\",\"function\":{{\"name\":{},\"arguments\":\"\"}}}}],\"finish_reason\":null}}}}]}}\n\n",
        json_string(call_id),
        json_string(name),
    );
    body.push_str(&format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":{}}}}}]}}}}]}}\n\n",
        json_string(args)
    ));
    body.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    );
    body.push_str("data: [DONE]\n\n");
    body
}

/// Spec R07 e2e criterion 1: a profile whose `read_files` level is
/// `always_allow` runs the call without ever parking — the card opens
/// `Running` and lands `✓`, and the file's contents re-enter the
/// conversation as the tool result (the captured request body is the
/// proof the read really happened — a parked gate would have left the
/// second request unfired).
#[test]
fn a_permissive_profile_runs_read_files_with_no_approve_card() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )),
        sse(&chat_text_turn("marker read")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(
        dir.path(),
        &settings_with_fixture(&fixture.base_url, PERMISSIVE_READS),
    );

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");
    // `read_files` is confined to the session's cwd — the spawn's
    // tempdir — so the seeded file lives inside it.
    fs::write(app.cwd().join("note.txt"), "READ-MARKER-7f3a\n").expect("seed note.txt");

    open_panel(&mut app);
    submit_task(&mut app, "read the seeded file");

    app.wait_for_screen("the done card and status", |app| {
        app.screen_text().contains('✓')
            && app.screen_text().contains("read_files")
            && panel_has_line(app, "done")
    });
    assert!(
        !app.screen_text().contains("approve"),
        "the approve row never rendered: {}",
        app.screen_text()
    );

    fixture.captured(); // the task's first request
    let second = fixture.captured();
    assert_post(&second);
    assert!(
        second.body.contains("READ-MARKER-7f3a"),
        "the file's contents re-entered as the tool result: {}",
        second.body
    );
    fixture.assert_no_more_requests();
}

/// Spec R07 e2e criterion 2: the same profile allows `read_files` and
/// parks `run_command` — per-tool levels in one task. That the flow
/// reaches the `approve run_command` card at all proves the read ran
/// ungated (the missing-profile fail-safe would have parked it first,
/// waiting forever on a verdict this test never sends); the `a`
/// verdict then types the command into the live shell, where the
/// Absent-tier sentinel trace lands in the grid.
#[test]
fn a_restrictive_profile_parks_run_command_and_an_approve_runs_it() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )),
        sse(&tool_turn(
            "call_2",
            "run_command",
            r#"{"command":"echo E2E-MARK"}"#,
        )),
        sse(&chat_text_turn("the command ran")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(
        dir.path(),
        &settings_with_fixture(&fixture.base_url, READS_FREE_COMMANDS_ASK),
    );

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");
    fs::write(app.cwd().join("note.txt"), "READ-MARKER-7f3a\n").expect("seed note.txt");

    open_panel(&mut app);
    submit_task(&mut app, "read, then run the marker");

    // The read's `✓` card is already down and the command's card is
    // parked — and NOTHING has reached the shell yet: the terminal
    // half of every row is still marker-free.
    app.wait_for_screen("the read done and the command parked", |app| {
        app.screen_text().contains("approve run_command")
            && app.screen_text().contains("read_files")
    });
    assert!(
        !app.screen_text()
            .lines()
            .any(|row| terminal_half(row).contains("E2E-MARK")),
        "no request bytes before the verdict: {}",
        app.screen_text()
    );

    app.type_text("a"); // allow once

    // Gate on output only execution produces (standard 15): the
    // sentinel's `__AIT_END_<id>=<exit>` RESULT line — the echoed
    // type-in carries `=%d`, never `=<digit>` — and a trimmed
    // `E2E-MARK` row, the echo's output rather than its input.
    app.wait_for_screen("the sentinel result and marker in the grid", |app| {
        app.screen_text().lines().any(sentinel_result)
            && app
                .screen_text()
                .lines()
                .any(|row| terminal_half(row).trim() == "E2E-MARK")
    });
    app.wait_for_screen("the task's done status", |app| {
        panel_has_line(app, "done") && app.screen_text().contains("the command ran")
    });

    // Request 3 is the schema's own tool-result turn: the run's
    // output and exit code went back to the model.
    fixture.captured();
    fixture.captured();
    let third = fixture.captured();
    assert_post(&third);
    assert!(
        third.body.contains("\"role\":\"tool\"") && third.body.contains("E2E-MARK"),
        "the run's merged output re-entered the conversation: {}",
        third.body
    );
    fixture.assert_no_more_requests();
}

/// Spec R07 e2e criterion 3: the `command_denylist` is a hard floor
/// under every level — `execute_commands = "always_allow"` still
/// refuses the `^rm`-matched call outright, so the denial can only
/// have come from the list. The card renders `Denied` (`✗` plus the
/// refusal detail) with the approve row never painting, nothing is
/// typed into the shell, and the profile-named refusal is the tool
/// result the model reads on its next request.
#[test]
fn a_denylisted_run_command_renders_denied_without_asking() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "run_command",
            r#"{"command":"rm -rf TARGET"}"#,
        )),
        sse(&chat_text_turn("rerouting around the refusal")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(
        dir.path(),
        &settings_with_fixture(&fixture.base_url, DENYLISTED_RM),
    );

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");

    open_panel(&mut app);
    submit_task(&mut app, "remove the target");

    app.wait_for_screen("the denied card", |app| {
        app.screen_text().contains('✗')
            && app.screen_text().contains("run_command")
            && app.screen_text().contains("denied by execution")
    });
    assert!(
        !app.screen_text().contains("approve"),
        "a profile denial never parks the approve card: {}",
        app.screen_text()
    );
    assert!(
        !app.screen_text()
            .lines()
            .any(|row| terminal_half(row).contains("rm -rf")),
        "a denied call never typed into the shell: {}",
        app.screen_text()
    );

    app.wait_for_screen("the task's done status", |app| panel_has_line(app, "done"));
    fixture.captured(); // the task's first request
    let second = fixture.captured();
    assert!(
        second.body.contains("denied by execution profile default"),
        "the refusal text IS the tool result the model read: {}",
        second.body
    );
    fixture.assert_no_more_requests();
}

/// Spec R07 e2e criterion 4: `Alt+P` arms the other profile directly
/// when exactly two are declared — the status line gains the
/// `permissive` segment and the model slot re-resolves that profile's
/// `base_model` — and the NEXT task evaluates the new stance: the
/// `read_files` call `default`'s unset level would have parked runs
/// ungated under `permissive`. (Reaching `done` without a verdict is
/// the proof — a stale `default` would have parked on `approve
/// read_files` and waited forever.)
#[test]
fn alt_p_arms_the_other_profile_and_the_next_task_uses_it() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )),
        sse(&chat_text_turn("marker read")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(
        dir.path(),
        &settings_with_fixture(&fixture.base_url, TWO_PROFILES),
    );

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");
    fs::write(app.cwd().join("note.txt"), "READ-MARKER-7f3a\n").expect("seed note.txt");

    app.press("Alt+P"); // two declared — cycle straight to `permissive`
    app.wait_for_screen("the armed profile on the status line", |app| {
        app.line(STATUS_ROW).contains("· permissive ·")
            && app.line(STATUS_ROW).contains("Fixture Model")
    });

    open_panel(&mut app);
    submit_task(&mut app, "read the seeded file");
    app.wait_for_screen("the done card and status", |app| {
        app.screen_text().contains('✓')
            && app.screen_text().contains("read_files")
            && panel_has_line(app, "done")
    });
    assert!(
        !app.screen_text().contains("approve"),
        "the armed profile's always_allow ran ungated: {}",
        app.screen_text()
    );

    fixture.captured(); // the task's first request
    let second = fixture.captured();
    assert!(
        second.body.contains("READ-MARKER-7f3a"),
        "the file's contents re-entered as the tool result: {}",
        second.body
    );
    fixture.assert_no_more_requests();
}

/// Spec R07 e2e criterion 5: a hot reload that deletes the ARMED
/// profile toasts its name and re-arms the ask-everything fail-safe —
/// the rewritten file declares no profiles at all, so the worker's
/// missing-profile path (`permissive` resolves to nothing) parks even
/// the `read_files` call the vanished profile allowed. The `a` verdict
/// still runs it, proving the fail-safe is the same approve card as
/// ever, not a lock-out.
#[test]
fn a_reload_deleting_the_active_profile_toasts_and_re_arms_ask_everything() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )),
        sse(&chat_text_turn("marker read")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(
        dir.path(),
        &settings_with_fixture(&fixture.base_url, TWO_PROFILES),
    );

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");
    fs::write(app.cwd().join("note.txt"), "READ-MARKER-7f3a\n").expect("seed note.txt");

    app.press("Alt+P");
    app.wait_for_screen("the armed profile on the status line", |app| {
        app.line(STATUS_ROW).contains("· permissive ·")
    });

    // The reload drops the whole `execution_profiles` table: the armed
    // `permissive` name survives (the selection is never rewritten),
    // the toast names it, and the zero-profiles file resolves the one
    // configured model implicitly — so the task below still reaches
    // the model and its calls all evaluate the fail-safe.
    write_settings(dir.path(), &settings_with_fixture(&fixture.base_url, ""));
    app.wait_for_screen("the vanished-profile toast", |app| {
        app.line(STATUS_ROW)
            .contains("execution profile \"permissive\" no longer declared")
    });

    open_panel(&mut app);
    submit_task(&mut app, "read the seeded file");
    app.wait_for_screen("the fail-safe parked on the approve card", |app| {
        app.screen_text().contains("approve read_files")
    });
    // Parked means parked: the task's first request is in, but no
    // tool-result request can fire until a verdict arrives.
    fixture.captured();
    fixture.assert_no_more_requests();

    app.type_text("a"); // allow once — the fail-safe is a gate, not a refusal
    app.wait_for_screen("the task's done status", |app| {
        app.screen_text().contains('✓') && panel_has_line(app, "done")
    });

    let second = fixture.captured(); // request 1 was drained mid-park
    assert!(
        second.body.contains("READ-MARKER-7f3a"),
        "the approved read re-entered as the tool result: {}",
        second.body
    );
    fixture.assert_no_more_requests();
}
