//! e2e for §5.6 agent mode — the full user-visible loop against the
//! real `ait` binary (Spec R06 testing decisions, ticket R06-T9):
//! `Alt+A` opens the panel, a typed task produces an approve card,
//! the verdict drives `run_command` in the live shell and the task
//! ends on a `done` status — plus the deny, Esc-cancel, steering and
//! `write_to_pty` paths.
//!
//! The model is `ait_llm::testing`'s loopback [`Fixture`] — a real
//! socket scripting one `openai_chat_completions` SSE response per
//! request, so nothing touches the network. Code-first for the same
//! reason the R03/R04 tests are: a scenario TOML's `env` values are
//! literal strings, so neither `XDG_CONFIG_HOME` (a per-test tempdir)
//! nor the fixture's ephemeral port can be interpolated into one.
//!
//! The harness pins `SHELL=/bin/sh` — dash takes no injection (the
//! planner returns `None` for unknown basenames), so every tab here
//! is `Absent` and `run_command` uses the typed-sentinel tier: the
//! `cmd; printf '__AIT_END_<id>=%d\n' $?` line the executor types is
//! the visible trace the tests assert on (us-14/16). Panel-side and
//! terminal-side text are told apart by the `│` separator: rows are
//! `<terminal cols>│<panel cols>`, so a probe on a row's left half can
//! only be the real grid, never a card's summary.

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
/// directory first (the R03 pattern — the watcher needs the parent
/// directory to exist at spawn).
fn write_settings(xdg: &Path, contents: &str) {
    let dir = xdg.join("ait");
    fs::create_dir_all(&dir).expect("create the fixture config directory");
    fs::write(dir.join("settings.toml"), contents).expect("write fixture settings.toml");
}

/// A `settings.toml` whose `coding_model` — the role the agent loop
/// resolves (§4.2, first consumer) — binds one model at the fixture's
/// loopback `base_url`. No `api_key_env` and a host keyring miss mean
/// the requests go out unauthenticated (the R04 tests' precedent).
fn settings_with_fixture(base_url: &str) -> String {
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

[agents.execution_profiles.default]
coding_model = "coding-key"
"#
    )
}

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
/// unrendered prompt (the `-sh` race the R05 tests name).
fn wait_fresh_prompt(app: &mut E2e, description: &str) {
    app.wait_for_screen(description, |app| {
        app.screen_text()
            .lines()
            .any(|line| line.ends_with('$') || line.ends_with('%') || line.ends_with('>'))
    });
}

/// `Alt+A` opens the split focused: the `│` separator renders and the
/// title row reads `│ agent` (rows are `trim_end`'ed, so the probe
/// carries no trailing space).
fn open_panel(app: &mut E2e) {
    app.press("Alt+A");
    app.wait_for_screen("the open, focused agent panel", |app| {
        app.screen_text().contains('│') && app.screen_text().contains(" agent")
    });
}

/// Type `task` on the panel's input line and submit it — the bytes
/// order the keystrokes, so the gate on the rendered input is
/// belt-and-braces rather than load-bearing.
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

/// A JSON string literal (`"…"`) for hand-built chunk bodies — the
/// same escaping the R04 e2e transcripts use.
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
/// one complete tool call (`id`/`name` on the announce fragment, the
/// arguments as a string fragment after it, `finish_reason:
/// "tool_calls"` closing the turn). `args` is the JSON *text* the
/// `arguments` string carries.
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

/// Manual PR-evidence generator (the `pr_evidence_transcript`
/// precedent in `e2e.rs`): runs the central approve→`run_command`
/// scenario and prints the screen at each beat. Excluded from the
/// suite — run it on demand:
///
/// ```text
/// cargo test -p ait --test agent_panel pr_evidence -- --ignored --nocapture
/// ```
#[test]
#[ignore = "manual evidence generator for PR bodies"]
fn pr_evidence_agent_loop_transcript() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "run_command",
            r#"{"command":"echo E2E-MARK"}"#,
        )),
        sse(&chat_text_turn("the marker is on screen")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");

    app.press("Alt+A");
    app.wait_for_screen("the open, focused agent panel", |app| {
        app.screen_text().contains('│') && app.screen_text().contains(" agent")
    });
    println!("--- after Alt+A ---\n{}", app.screen_text());

    submit_task(&mut app, "run my marker command");
    app.wait_for_screen("the parked approve card", |app| {
        app.screen_text().contains("approve run_command")
    });
    println!("--- approve card parked ---\n{}", app.screen_text());

    app.type_text("a");
    app.wait_for_screen("the task's done status", |app| panel_has_line(app, "done"));
    println!("--- after approve: trace + done ---\n{}", app.screen_text());
}

/// Spec R06's central e2e criterion (issue #92): the full loop —
/// panel → task → approve card → `a` → `run_command`'s trace in the
/// real grid → `done` — against a scripted endpoint. On the pinned
/// `Absent` shell the trace is the executor's typed sentinel line
/// (us-14/16): before the verdict the grid shows nothing, after it
/// the wrapped command echoes, runs and prints its `__AIT_END_`
/// sentinel — and the merged output plus exit code re-enter the
/// conversation as the schema's own `role:"tool"` item, which the
/// fixture's captured request body proves.
#[test]
fn approve_runs_run_command_in_the_shell_and_leaves_the_trace() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "run_command",
            r#"{"command":"echo E2E-MARK"}"#,
        )),
        sse(&chat_text_turn("the marker is on screen")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");

    open_panel(&mut app);
    submit_task(&mut app, "run my marker command");

    // The approve card parks the gate (us-5/6): the card names the
    // tool and command — and NOTHING has reached the shell yet; the
    // terminal half of every row is still marker-free.
    app.wait_for_screen("the parked approve card", |app| {
        app.screen_text().contains("approve run_command") && app.screen_text().contains("E2E-MARK")
    });
    assert!(
        !app.screen_text()
            .lines()
            .any(|row| terminal_half(row).contains("E2E-MARK")),
        "no request bytes before the verdict: {}",
        app.screen_text()
    );

    app.type_text("a"); // allow once

    // The executor typed `echo E2E-MARK; printf '__AIT_END_…=%d\n'
    // $?` at the prompt — the whole line is the visible trace — and
    // the shell printed the marker plus the sentinel's result.
    app.wait_for_screen("the typed sentinel trace in the grid", |app| {
        app.screen_text().contains("__AIT_END_")
            && app
                .screen_text()
                .lines()
                .any(|row| terminal_half(row).contains("E2E-MARK"))
    });
    app.wait_for_screen("the task's done status", |app| {
        panel_has_line(app, "done") && app.screen_text().contains("the marker is on screen")
    });

    // Request 2 is the schema's own tool-result turn: the run's
    // output and exit code went back to the model.
    let first = fixture.captured();
    assert_post(&first);
    let second = fixture.captured();
    assert_post(&second);
    assert!(
        second.body.contains("\"role\":\"tool\""),
        "the tool-result item in the schema's IR: {}",
        second.body
    );
    assert!(
        second.body.contains("E2E-MARK"),
        "the run's merged output re-entered the conversation: {}",
        second.body
    );
    assert!(
        second.body.contains("exit code"),
        "the exit code came back model-readable: {}",
        second.body
    );
    fixture.assert_no_more_requests();
}

/// Spec R06 us-8: *deny* never types a byte into the user's shell —
/// the grid stays clean — and the denial text is the tool result the
/// model reads on the next request. The card's `✗` status is the
/// user-visible half; the captured request body is the model-visible
/// half. `d` opens the card's denial-reason mode — the reason is
/// typed there (where the verdict letters are ordinary input) and
/// Enter sends the `Deny`.
#[test]
fn denied_call_never_reaches_the_shell_and_the_reason_reaches_the_model() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "run_command",
            r#"{"command":"rm -rf TARGET"}"#,
        )),
        sse(&chat_text_turn("understood, nothing removed")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");

    open_panel(&mut app);
    submit_task(&mut app, "remove the target");

    app.wait_for_screen("the parked approve card", |app| {
        app.screen_text().contains("approve run_command")
    });
    app.type_text("d"); // open the denial-reason mode
    app.type_text("nope"); // the reason — verdict letters type like any other
    app.press("Enter"); // deny with the typed reason

    app.wait_for_screen("the denied card and the task's done status", |app| {
        app.screen_text().contains('✗') && panel_has_line(app, "done")
    });

    // The terminal half never echoed `rm -rf` — a denied call offered
    // nothing to the shell (us-8/16).
    assert!(
        !app.screen_text()
            .lines()
            .any(|row| terminal_half(row).contains("rm -rf")),
        "a denied call never typed into the shell: {}",
        app.screen_text()
    );

    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("nope"),
        "the denial text IS the tool result the model read: {}",
        second.body
    );
    fixture.assert_no_more_requests();
}

/// Spec R06 us-9: an approve card waits indefinitely — never timing
/// out into a default verdict — and Esc is the way out: it cancels
/// the task, the panel keeps focus for the next one, and no second
/// request ever fires.
#[test]
fn escape_cancels_a_task_parked_on_the_approve_card() {
    let fixture = Fixture::serve(vec![sse(&tool_turn(
        "call_1",
        "run_command",
        r#"{"command":"echo NEVER-RAN"}"#,
    ))]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");

    open_panel(&mut app);
    submit_task(&mut app, "run something for me");
    app.wait_for_screen("the parked approve card", |app| {
        app.screen_text().contains("approve run_command")
    });

    app.press("Esc");
    app.wait_for_screen("the cancelled status line", |app| {
        panel_has_line(app, "cancelled")
    });

    // The cancelled call never typed — and the worker asked nothing
    // further of the model.
    assert!(
        !app.screen_text()
            .lines()
            .any(|row| terminal_half(row).contains("NEVER-RAN")),
        "a cancelled call never reached the shell: {}",
        app.screen_text()
    );
    fixture.captured(); // the task's first (only) request
    fixture.assert_no_more_requests();
}

/// Spec R06's steering path: input typed while a task runs enqueues a
/// user message the worker injects at the next model turn — here,
/// queued *while the gate is parked* and riding the post-approval
/// request beside the tool result. The panel records it as its own
/// `›` entry; the captured request body proves it reached the model.
/// `hurry` — like `nope` above — contains no `a`/`t`/`d` verdict
/// keys, so it survives typing against the parked card.
#[test]
fn steering_queued_on_the_parked_gate_rides_the_next_request() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "run_command",
            r#"{"command":"echo GO"}"#,
        )),
        sse(&chat_text_turn("wrapped up")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");

    open_panel(&mut app);
    submit_task(&mut app, "run the go command");
    app.wait_for_screen("the parked approve card", |app| {
        app.screen_text().contains("approve run_command")
    });

    // Steering while the gate waits: Enter queues the input as a user
    // message (the task is still running), rendered as its own entry.
    app.type_text("hurry");
    app.press("Enter");
    app.wait_for_screen("the steering entry in the conversation", |app| {
        app.screen_text().contains("hurry")
    });

    app.type_text("a"); // allow once — the queued steering rides along
    app.wait_for_screen("the task's done status", |app| panel_has_line(app, "done"));

    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("hurry"),
        "the steering message reached the model as a user turn: {}",
        second.body
    );
    assert!(
        second.body.contains("\"role\":\"tool\""),
        "the tool result rode the same request: {}",
        second.body
    );
    fixture.assert_no_more_requests();
}

/// Spec R06 us-17–20: `write_to_pty` is a gated tool like every
/// other — the card shows the bytes to send — and an allow spawns the
/// agent-owned PTY lazily, writes the input and returns the output it
/// produced. The captured second request is the proof the PTY really
/// ran `echo`: the session's own output re-entered the conversation
/// as the tool result.
#[test]
fn write_to_pty_card_drives_the_agent_owned_pty() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "write_to_pty",
            r#"{"input":"echo PTY-MARK\n"}"#,
        )),
        sse(&chat_text_turn("the pty answered")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_fixture(&fixture.base_url));

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");

    open_panel(&mut app);
    submit_task(&mut app, "use your own terminal");
    app.wait_for_screen("the parked write_to_pty approve card", |app| {
        app.screen_text().contains("approve write_to_pty") && app.screen_text().contains("PTY-MARK")
    });
    app.type_text("a"); // allow once — the PTY spawns on this call

    app.wait_for_screen("the task's done status", |app| {
        panel_has_line(app, "done") && app.screen_text().contains("the pty answered")
    });

    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("PTY-MARK"),
        "the agent PTY's own output re-entered as the tool result: {}",
        second.body
    );
    fixture.assert_no_more_requests();
}
