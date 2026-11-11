//! e2e for §5.10.1 MCP + §5.10.2 skills (Spec R10, ticket R10-T7):
//! the round's end-to-end proof — the real `ait` binary, a fixture
//! `XDG_CONFIG_HOME` carrying `ait/mcp.json` and
//! `ait/skills/<name>/SKILL.md`, a project `.ait/mcp.json` under the
//! session's own cwd, and a scripted model driving `invoke_skill`,
//! `call_mcp_tool` and `read_mcp_resource` through the real
//! approve-card gate. No network anywhere (standard 11): the model is
//! `ait_llm::testing`'s loopback [`Fixture`] and the MCP server is
//! ait-agent's committed `mcp_fixture` example — a local child
//! process speaking NDJSON over stdio.
//!
//! Code-first for the `agent_panel.rs` reason: a scenario TOML's
//! `env` values are literal strings, so neither the per-test
//! `XDG_CONFIG_HOME` tempdir nor the fixture's ephemeral port can be
//! interpolated into one.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use ait_llm::testing::{assert_post, sse, Fixture};
use ait_testkit::{
    ait_bin, json_string, panel_has_line, wait_fresh_prompt, E2e, Size, SpawnConfig,
};

/// 140x30 — wider than the suite's usual 80x24 so the project-layer
/// annotation `(declared by the project .ait/mcp.json)` fits the
/// panel's two-fifths share without wrapping mid-probe. Row 0 is the
/// tab bar, row 29 the status line.
const SIZE: Size = Size {
    columns: 140,
    lines: 30,
};
const STATUS_ROW: usize = 29;

/// The committed fixture server's binary — `cargo test --workspace`
/// builds every package's examples into `target/debug/examples/`, and
/// this test's exe lives in `target/debug/deps/` (the same resolution
/// `ait-agent/tests/agent_mcp.rs` uses).
fn fixture_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let bin = exe
        .parent()
        .expect("deps dir")
        .parent()
        .expect("target dir")
        .join("examples")
        .join(format!("mcp_fixture{}", std::env::consts::EXE_SUFFIX));
    assert!(
        bin.exists(),
        "the mcp_fixture example must be built — `cargo test \
         --workspace` builds every example; under `-p ait` run \
         `cargo build -p ait-agent --example mcp_fixture` first"
    );
    bin
}

/// Write `contents` as `<xdg>/ait/settings.toml`, creating the `ait/`
/// directory first (the R03 pattern — the watcher needs the parent
/// directory to exist at spawn).
fn write_settings(xdg: &Path, contents: &str) {
    let dir = xdg.join("ait");
    fs::create_dir_all(&dir).expect("create the fixture config directory");
    fs::write(dir.join("settings.toml"), contents).expect("write fixture settings.toml");
}

/// A `settings.toml` whose `coding_model` binds one model at the
/// fixture's loopback `base_url` (the `agent_panel.rs` fixture
/// profile, verbatim). `read_files` — the level `invoke_skill`
/// rides — and `mcp_permissions` stay unset, so every call this task
/// makes parks on the approve card.
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

/// A `{"mcpServers": …}` document with one `name` → `command` entry —
/// the shape `ait_config::McpConfig` validates — written to
/// `path`, whose parent is created first.
fn write_mcp_json(path: &Path, name: &str, command: &Path) {
    fs::create_dir_all(path.parent().expect("a parent directory"))
        .expect("create the mcp.json directory");
    fs::write(
        path,
        format!(
            r#"{{"mcpServers":{{"{name}":{{"command":{}}}}}}}"#,
            json_string(&command.to_string_lossy())
        ),
    )
    .expect("write mcp.json");
}

/// A `SKILL.md` under `<xdg>/ait/skills/<name>/` — the user-layer
/// skills root — whose frontmatter satisfies the loader's
/// `name`/`description` contract and whose body's first line carries
/// `body_marker` (the card's result detail previews the first line).
fn write_skill(xdg: &Path, name: &str, body_marker: &str) {
    let dir = xdg.join("ait").join("skills").join(name);
    fs::create_dir_all(&dir).expect("create the skill directory");
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Exercises the skills loader.\n---\n\n{body_marker} — follow the checklist.\n"),
    )
    .expect("write SKILL.md");
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

/// `Alt+A` opens the split focused: the `│` separator renders and the
/// title row reads `│ agent`.
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

/// Spec R10's central e2e criterion: one scripted task exercises the
/// whole round — `invoke_skill` reads the user-layer `SKILL.md`,
/// `call_mcp_tool` reaches the PROJECT-layer fixture server (the
/// card's `(declared by the project .ait/mcp.json)` annotation is the
/// us-9 trust-model half, and the gated call's lazy connect is the
/// only way a project server ever spawns) and `read_mcp_resource`
/// reads the user-layer server the reconcile connected eagerly. Each
/// call parks on the approve card, `a` allows once, and every result
/// lands on screen and re-enters the next request body as an
/// ordinary tool item.
#[test]
fn invoke_skill_and_mcp_calls_run_through_the_approve_cards() {
    let fixture = Fixture::serve(vec![
        sse(&tool_turn("call_1", "invoke_skill", r#"{"name":"review"}"#)),
        sse(&tool_turn(
            "call_2",
            "call_mcp_tool",
            r#"{"server":"projectfixture","tool":"echo","arguments":{"text":"MCP-E2E-MARK"}}"#,
        )),
        sse(&tool_turn(
            "call_3",
            "read_mcp_resource",
            r#"{"server":"fixture","uri":"fixture://one"}"#,
        )),
        sse(&chat_text_turn("the skill and the server both answered")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_fixture(&fixture.base_url));
    // The user layer: an eagerly-connected server at
    // `<xdg>/ait/mcp.json` plus the `review` skill under
    // `<xdg>/ait/skills/`.
    write_mcp_json(
        &dir.path().join("ait").join("mcp.json"),
        "fixture",
        &fixture_bin(),
    );
    write_skill(dir.path(), "review", "REVIEW-SKILL-BODY");

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    wait_fresh_prompt(&mut app, "the first prompt");

    // The project layer: `.ait/mcp.json` inside the spawn's own cwd —
    // the tab reports no OSC 7 on the pinned `/bin/sh`, so the task's
    // `AgentContext.cwd` is this directory and the per-task reconcile
    // discovers `.ait/` here. Written post-spawn but pre-submit, when
    // the only `mcp.json` read has yet to happen.
    write_mcp_json(
        &app.cwd().join(".ait").join("mcp.json"),
        "projectfixture",
        &fixture_bin(),
    );

    open_panel(&mut app);
    submit_task(&mut app, "use the review skill then exercise the servers");

    // Beat 1: `invoke_skill` rides `read_files` — unset → the card
    // parks, naming the skill.
    app.wait_for_screen("the parked invoke_skill approve card", |app| {
        app.screen_text().contains("approve invoke_skill")
            && app.screen_text().contains("invoke_skill review")
    });
    app.type_text("a");

    // Beat 2: `call_mcp_tool` parks under `mcp_permissions` — and the
    // card carries the project-layer annotation: this server wants to
    // run code the repository shipped (us-9).
    app.wait_for_screen("the parked call_mcp_tool approve card", |app| {
        app.screen_text().contains("approve call_mcp_tool")
            && app.screen_text().contains("projectfixture:echo")
            && app
                .screen_text()
                .contains("(declared by the project .ait/mcp.json)")
    });
    app.type_text("a");

    // Beat 3: `read_mcp_resource` on the user-layer server — the
    // reconcile connected it eagerly at task start, so the resource
    // URI is on the enumerated surface the prompt already showed.
    app.wait_for_screen("the parked read_mcp_resource approve card", |app| {
        app.screen_text().contains("approve read_mcp_resource")
            && app.screen_text().contains("fixture fixture://one")
    });
    app.type_text("a");

    // All three cards settled `✓`, each carrying its result detail;
    // the task ends `done`.
    app.wait_for_screen("all three results and the task's done status", |app| {
        app.screen_text().contains("✓ invoke_skill")
            && app.screen_text().contains("✓ call_mcp_tool")
            && app.screen_text().contains("✓ read_mcp_resource")
            && app.screen_text().contains("REVIEW-SKILL-BODY")
            && app.screen_text().contains("echo:MCP-E2E-MARK")
            && app.screen_text().contains("FIXTURE-BODY")
            && panel_has_line(app, "done")
    });

    // Request 1's system prompt carried both catalogs: the user-tier
    // server enumerated `available` and the project one `declared`
    // (listed, never spawned), plus the "Available skills" section.
    let first = fixture.captured();
    assert_post(&first);
    assert!(first.body.contains("MCP servers"), "{}", first.body);
    assert!(first.body.contains("available"), "{}", first.body);
    assert!(first.body.contains("declared"), "{}", first.body);
    assert!(first.body.contains("Available skills"), "{}", first.body);
    assert!(first.body.contains("review"), "{}", first.body);

    // Requests 2–4 carry the tool calls and their results back to the
    // model — the skill's body, the MCP echo and the resource text
    // each re-enter the conversation verbatim.
    let second = fixture.captured();
    assert_post(&second);
    assert!(second.body.contains("invoke_skill"), "{}", second.body);
    assert!(
        second.body.contains("REVIEW-SKILL-BODY"),
        "the skill's body re-entered the conversation: {}",
        second.body
    );
    let third = fixture.captured();
    assert!(third.body.contains("call_mcp_tool"), "{}", third.body);
    assert!(
        third.body.contains("echo:MCP-E2E-MARK"),
        "the MCP tool's result re-entered the conversation: {}",
        third.body
    );
    let fourth = fixture.captured();
    assert!(fourth.body.contains("read_mcp_resource"), "{}", fourth.body);
    assert!(
        fourth.body.contains("FIXTURE-BODY"),
        "the resource's text re-entered the conversation: {}",
        fourth.body
    );
    fixture.assert_no_more_requests();
}
