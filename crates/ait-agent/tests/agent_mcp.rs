//! MCP integration tests at the `AgentSession` channel seam (Spec
//! R10, §5.10.1): a fixture stdio MCP server — the committed
//! `examples/mcp_fixture` NDJSON binary, spawned by the worker's
//! `McpPool`, never by the test — driven end-to-end through real
//! tasks against the in-process `tiny_http` SSE fixture
//! (`ait_llm::testing`, `test-fixtures`). No network anywhere:
//! the model is the scripted loopback fixture and the MCP server is
//! a local child process (standard 11).
//!
//! The user-tier `mcp.json` lives at `$XDG_CONFIG_HOME/ait/mcp.json`
//! — the tests serialize on `XDG` and point the variable at a
//! per-test tempdir, the same env-lock pattern ait-config's watch
//! tests use.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ait_agent::{
    AgentContext, AgentEvent, AgentRequest, AgentSession, ApprovalVerdict, ToolStatus,
};
use ait_config::{SecretStore, Settings};
use ait_llm::testing::{assert_post, sse, Fixture};

/// Tests never touch a real keyring — a missing secret is `None`, so
/// fixture requests go out with no `Authorization` header.
struct NoSecrets;

impl SecretStore for NoSecrets {
    fn get(&self, _service: &str, _account: &str) -> Option<String> {
        None
    }
}

/// Bound on "the worker answered" — loopback plus a local child
/// spawn is fast; a hang is a failure, not a slow pass.
const BOUND: Duration = Duration::from_secs(15);

/// The env lock: `XDG_CONFIG_HOME` is process-global, so every test
/// that plants a user-tier `mcp.json` — or wants the user's real one
/// out of the picture — holds this for its whole body. Poisoned
/// guards still guard: an earlier test's panic must not let two
/// writers race the variable.
static XDG: Mutex<()> = Mutex::new(());

/// Take the env lock, surviving a poisoned guard.
fn xdg_lock() -> std::sync::MutexGuard<'static, ()> {
    XDG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn session() -> AgentSession {
    AgentSession::with_secret_store(NoSecrets)
}

/// A `settings.toml` snapshot binding `coding_model` to the chat
/// fixture, with `profile_line` appended to profile `default` (the
/// MCP level/list keys under test).
fn settings(base_url: &str, profile_line: &str) -> Settings {
    Settings::from_toml(&format!(
        r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "{base_url}"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "coding-key"

[agents.execution_profiles.default]
coding_model = "coding-key"
{profile_line}
"#
    ))
    .expect("test settings must parse")
}

/// The committed fixture server's binary — `cargo test` builds the
/// package's examples into `target/debug/examples/`, and the test
/// exe lives in `target/debug/deps/`.
fn fixture_bin() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    exe.parent()
        .expect("deps dir")
        .parent()
        .expect("target dir")
        .join("examples")
        .join(format!("mcp_fixture{}", std::env::consts::EXE_SUFFIX))
}

/// A `{"mcpServers": …}` document with one `name` → `command`+args
/// entry, written to `<xdg>/ait/mcp.json`.
fn write_mcp_json(xdg: &Path, name: &str, command: &Path, args: &[&str]) {
    let ait_dir = xdg.join("ait");
    std::fs::create_dir_all(&ait_dir).expect("ait config dir");
    let args = args
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("arg"))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        ait_dir.join("mcp.json"),
        format!(
            r#"{{"mcpServers":{{"{name}":{{"command":{},"args":[{args}]}}}}}}"#,
            serde_json::to_string(&command.to_string_lossy()).expect("path")
        ),
    )
    .expect("mcp.json");
}

/// A `{"mcpServers": …}` document with one `name` → `command`+args
/// entry, written to `<cwd>/.ait/mcp.json` — the project-local
/// layer (§5.10.1) `project_ait_dir` discovers from the task cwd.
fn write_project_mcp_json(cwd: &Path, name: &str, command: &Path, args: &[&str]) {
    let ait_dir = cwd.join(".ait");
    std::fs::create_dir_all(&ait_dir).expect(".ait dir");
    let args = args
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("arg"))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        ait_dir.join("mcp.json"),
        format!(
            r#"{{"mcpServers":{{"{name}":{{"command":{},"args":[{args}]}}}}}}"#,
            serde_json::to_string(&command.to_string_lossy()).expect("path")
        ),
    )
    .expect("project mcp.json");
}

/// A task bound to `cwd`.
fn task(settings: Settings, cwd: &str) -> AgentRequest {
    AgentRequest {
        settings,
        profile: String::from("default"),
        task: String::from("use the fixture server"),
        context: AgentContext {
            cwd: cwd.to_owned(),
            shell: String::from("bash"),
            os: String::from("linux"),
            output_lines: Vec::new(),
        },
        prior: Vec::new(),
        bridge: None,
        skills: ait_agent::SkillsCatalog::default(),
    }
}

/// A `chat.completion.chunk` `data:` line carrying a tool-call
/// fragment — `id`/`name` on the announce fragment, `arguments`
/// afterwards.
fn sse_tool_fragment(index: u32, announce: Option<(&str, &str)>, args: &str) -> String {
    let tool_call = match announce {
        Some((id, name)) => serde_json::json!({
            "index": index,
            "id": id,
            "type": "function",
            "function": {"name": name, "arguments": args}
        }),
        None => serde_json::json!({
            "index": index,
            "function": {"arguments": args}
        }),
    };
    let chunk = serde_json::json!({
        "choices": [{"index": 0, "delta": {"tool_calls": [tool_call]}, "finish_reason": null}]
    });
    format!("data: {chunk}\n\n")
}

/// A `chat.completion.chunk` `data:` line carrying a content delta.
fn sse_delta(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(text).expect("json literal")
    )
}

/// A chat-completions transcript: one tool turn per `(index, id,
/// name, arguments)` in `calls`, with `arguments` split into two
/// fragments so accumulation is exercised.
fn chat_tool_turn(calls: &[(u32, &str, &str, &str)]) -> String {
    let mut body = String::new();
    for (index, id, name, args) in calls {
        body.push_str(&sse_tool_fragment(*index, Some((id, name)), ""));
        let (first, second) = args.split_at(args.len() / 2);
        body.push_str(&sse_tool_fragment(*index, None, first));
        body.push_str(&sse_tool_fragment(*index, None, second));
    }
    body.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    );
    body.push_str("data: [DONE]\n\n");
    body
}

/// A plain end-turn chat transcript.
fn chat_text_turn(text: &str) -> String {
    let mut body = sse_delta(text);
    body.push_str("data: [DONE]\n\n");
    body
}

/// The next event, or panic — every wait carries the bound.
fn next(session: &AgentSession) -> AgentEvent {
    session
        .events()
        .recv_timeout(BOUND)
        .expect("an event within the bound")
}

/// Drain events until the next `ApprovalRequest`.
fn until_approval(session: &AgentSession) -> (String, String, String) {
    let mut drained = Vec::new();
    until_approval_collecting(session, &mut drained)
}

/// [`until_approval`], keeping every skipped event in `drained` so
/// completed calls' `ToolUpdate`s stay assertable.
fn until_approval_collecting(
    session: &AgentSession,
    drained: &mut Vec<AgentEvent>,
) -> (String, String, String) {
    loop {
        match next(session) {
            AgentEvent::ApprovalRequest { call, summary } => return (call.id, call.name, summary),
            event @ (AgentEvent::Done
            | AgentEvent::Cancelled
            | AgentEvent::BudgetReached
            | AgentEvent::Error(_)) => panic!("expected an ApprovalRequest, got {event:?}"),
            event => drained.push(event),
        }
    }
}

/// Drain events until a terminal `Done`/`Cancelled`/`BudgetReached`/
/// `Error` or the bound.
fn collect(session: &AgentSession) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    loop {
        match session.events().recv_timeout(BOUND) {
            Ok(
                event @ (AgentEvent::Done
                | AgentEvent::Cancelled
                | AgentEvent::BudgetReached
                | AgentEvent::Error(_)),
            ) => {
                events.push(event);
                return events;
            }
            Ok(event) => events.push(event),
            Err(_) => panic!("timed out waiting for a terminal event; got {events:?}"),
        }
    }
}

/// The full result text of call `id`'s terminal `ToolUpdate`.
fn result_of<'a>(events: &'a [AgentEvent], id: &str) -> &'a str {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolUpdate {
                call_id,
                result: Some(text),
                ..
            } if call_id == id => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no terminal ToolUpdate for {id}: {events:?}"))
}

/// The status of call `id`'s terminal `ToolUpdate`.
fn status_of(events: &[AgentEvent], id: &str) -> ToolStatus {
    events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolUpdate {
                call_id,
                status,
                result: Some(_),
                ..
            } if call_id == id => Some(*status),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no terminal ToolUpdate for {id}: {events:?}"))
}

/// End to end (Spec R10 us-6/10/11/14/17): the fixture stdio server
/// is spawned by the reconcile, the "MCP servers" prompt section
/// names it and its enumerated surface, `call_mcp_tool`'s card
/// renders `server:tool <args>` and its result — credential-bearing,
/// so the redacted text proves §5.10.3's tail applies to MCP results
/// too — and `read_mcp_resource` returns the resource's text; both
/// ride the approve card, and call + result land in the next
/// request's body as ordinary tool items.
#[test]
fn mcp_tools_call_and_read_end_to_end_through_the_gate() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    write_mcp_json(xdg.path(), "fixture", &fixture_bin(), &[]);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"AKIAIOSFODNN7EXAMPLE"}}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "read_mcp_resource",
            r#"{"server":"fixture","uri":"fixture://one"}"#,
        )])),
        sse(&chat_text_turn("all done")),
    ]);
    let session = session();
    // `mcp_permissions` unset → the gate asks, exercising the
    // ordinary approve card.
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // The tool call: the card names the call in `server:tool` form.
    let mut events = Vec::new();
    let (id, name, summary) = until_approval_collecting(&session, &mut events);
    assert_eq!(name, "call_mcp_tool");
    assert_eq!(id, "call_1");
    assert_eq!(summary, r#"fixture:echo {"text":"AKIAIOSFODNN7EXAMPLE"}"#);
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // The resource read: the card renders `server <uri>`.
    let (id, name, summary) = until_approval_collecting(&session, &mut events);
    assert_eq!(name, "read_mcp_resource");
    assert_eq!(id, "call_2");
    assert_eq!(summary, "fixture fixture://one");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    events.extend(collect(&session));
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    // The credential in the tool's output never survives the
    // redact tail — the card result and the model's view agree.
    assert_eq!(
        result_of(&events, "call_1"),
        "echo:[REDACTED]",
        "{events:?}"
    );
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Done);
    assert_eq!(result_of(&events, "call_2"), "FIXTURE-BODY");
    assert_eq!(status_of(&events, "call_2"), ToolStatus::Done);

    // Request 1's system prompt carries the catalog: the server is
    // `available` and its enumerated tool is named.
    let first = fixture.captured();
    assert_post(&first);
    assert!(first.body.contains("MCP servers"), "{}", first.body);
    assert!(first.body.contains("fixture"), "{}", first.body);
    assert!(first.body.contains("available"), "{}", first.body);
    assert!(first.body.contains("echo"), "{}", first.body);

    // Requests 2 and 3 carry the MCP call + result as ordinary
    // transcript tool items (us-17) — redacted where the content
    // was a credential.
    let second = fixture.captured();
    assert!(second.body.contains("call_mcp_tool"), "{}", second.body);
    assert!(second.body.contains("echo:[REDACTED]"), "{}", second.body);
    // The model's own call arguments replay verbatim — they came
    // FROM the model; the credential in the tool RESULT is what
    // §5.10.3 masks.
    assert!(
        !second.body.contains("echo:AKIAIOSFODNN7EXAMPLE"),
        "the unredacted result never re-enters: {}",
        second.body
    );
    let third = fixture.captured();
    assert!(third.body.contains("read_mcp_resource"), "{}", third.body);
    assert!(third.body.contains("FIXTURE-BODY"), "{}", third.body);
    fixture.assert_no_more_requests();
}

/// `mcp_permissions = "always_allow"` skips the card entirely —
/// `ToolCall` events open `Running`-bound cards and the results
/// land with no verdicts sent.
#[test]
fn always_allow_runs_mcp_calls_ungated() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    write_mcp_json(xdg.path(), "fixture", &fixture_bin(), &[]);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalRequest { .. })),
        "always_allow never asks: {events:?}"
    );
    assert_eq!(result_of(&events, "call_1"), "echo:hi");
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// `mcp_permissions = "never"` denies outright — the profile-denial
/// text IS the tool result and no approve card ever parks
/// (Spec R10 us-12).
#[test]
fn never_denies_mcp_calls_with_the_profile_text() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    write_mcp_json(xdg.path(), "fixture", &fixture_bin(), &[]);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("understood")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"never\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(
        result_of(&events, "call_1"),
        "denied by execution profile default"
    );
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Denied);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("denied by execution profile default"),
        "{}",
        second.body
    );
    assert!(
        !second.body.contains("echo:"),
        "the denied call never ran: {}",
        second.body
    );
}

/// `mcp_denylist` beats the level before the card: a `server:tool`
/// literal refuses exactly that pair while a different tool on the
/// same server runs (Spec R10 us-13, deny beats allow).
#[test]
fn mcp_denylist_literal_refuses_one_server_tool_pair() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    write_mcp_json(xdg.path(), "fixture", &fixture_bin(), &[]);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("understood")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(
                &fixture.base_url,
                "mcp_permissions = \"always_allow\"\nmcp_denylist = [\"fixture:echo\"]",
            ),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(
        result_of(&events, "call_1"),
        "denied by execution profile default"
    );
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Denied);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// A server that cannot initialize degrades to "unavailable" in the
/// prompt's catalog — the task still runs — and a call against it
/// is a tool error the model reads, never a wedge (Spec R10 us-7).
#[test]
fn an_uninitializable_server_degrades_to_unavailable() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    write_mcp_json(
        xdg.path(),
        "broken",
        Path::new("/nonexistent/ait-mcp-nope"),
        &[],
    );
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"broken","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("moving on")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    let result = result_of(&events, "call_1");
    assert!(result.contains("unavailable"), "{result}");
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Failed);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let first = fixture.captured();
    assert!(first.body.contains("unavailable"), "{}", first.body);
    let second = fixture.captured();
    assert!(second.body.contains("unavailable"), "{}", second.body);
}

/// A server that crashes mid-call answers a tool error — the
/// transport close is `ServiceError::TransportClosed` on the client
/// side — and the task continues (Spec R10 us-18). The fixture's
/// `--exit-on tools/call` leg survives initialization and dies on
/// the first call.
#[test]
fn a_crash_mid_call_surfaces_as_a_tool_error() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    write_mcp_json(
        xdg.path(),
        "fixture",
        &fixture_bin(),
        &["--exit-on", "tools/call"],
    );
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("noted")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Failed);
    let result = result_of(&events, "call_1");
    assert!(result.contains("failed"), "{result}");
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("failed"), "{}", second.body);
    assert!(
        !second.body.contains("echo:hi"),
        "the crashed call never produced output: {}",
        second.body
    );
}

/// The project-local trust boundary end to end (Spec R10 us-8/9):
/// a `.ait/mcp.json` server lists in the catalog as
/// declared-by-the-project and is NOT spawned at task start — the
/// prompt names no enumerated surface — while the first gated call
/// connects lazily: the approve card carries the `project .ait/`
/// origin annotation, and allowing it spawns + initializes +
/// dispatches under the ordinary timeouts. A second call in the
/// same task rides the now-live connection, and the NEXT task's
/// reconcile keeps it — its catalog line upgrades to `available`
/// and §5.10.3 redaction applies to its results, exactly like a
/// user-tier server.
#[test]
fn the_first_allowed_call_connects_a_project_server_lazily() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    // No user-tier mcp.json — only the project layer declares.
    let cwd = tempfile::tempdir().expect("cwd");
    write_project_mcp_json(cwd.path(), "proj", &fixture_bin(), &[]);

    let fixture = Fixture::serve(vec![
        // Task 1: an allowed call, then a resource read on the
        // same connection.
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"proj","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "read_mcp_resource",
            r#"{"server":"proj","uri":"fixture://one"}"#,
        )])),
        sse(&chat_text_turn("first task done")),
        // Task 2: the same server, called again — the connection
        // reconcile kept — with a credential in the result path.
        sse(&chat_tool_turn(&[(
            0,
            "call_3",
            "call_mcp_tool",
            r#"{"server":"proj","tool":"echo","arguments":{"text":"AKIAIOSFODNN7EXAMPLE"}}"#,
        )])),
        sse(&chat_text_turn("second task done")),
    ]);
    let session = session();
    // `mcp_permissions` unset → the gate asks — the approve card
    // IS the consent surface for the project-shipped process.
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let mut events = Vec::new();
    let (id, name, summary) = until_approval_collecting(&session, &mut events);
    assert_eq!(name, "call_mcp_tool");
    assert_eq!(id, "call_1");
    // The card names the declaring layer (us-9): this process came
    // from the repository, not the user's own config.
    assert_eq!(
        summary,
        "proj:echo {\"text\":\"hi\"}\n(declared by the project .ait/mcp.json)"
    );
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let (id, name, _) = until_approval_collecting(&session, &mut events);
    assert_eq!(name, "read_mcp_resource");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    events.extend(collect(&session));
    assert_eq!(result_of(&events, "call_1"), "echo:hi");
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Done);
    assert_eq!(result_of(&events, "call_2"), "FIXTURE-BODY");
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The task-start catalog — captured in request 1 — listed the
    // server as project-declared with NO enumerated surface: proof
    // nothing spawned before consent.
    let first = fixture.captured();
    assert!(
        first.body.contains("proj (project .ait/)"),
        "{}",
        first.body
    );
    assert!(first.body.contains("declared"), "{}", first.body);
    assert!(
        !first.body.contains("tools: echo"),
        "the unconnected server enumerated nothing: {}",
        first.body
    );

    // The second task's reconcile keeps the connected slot — the
    // catalog upgrades it to `available` with the enumerated
    // surface, and the call's credential-bearing result is redacted
    // like any user-tier server's (us-9/§5.10.3).
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");
    let mut events = Vec::new();
    let (id, _, _) = until_approval_collecting(&session, &mut events);
    assert_eq!(id, "call_3");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    events.extend(collect(&session));
    assert_eq!(result_of(&events, "call_3"), "echo:[REDACTED]");
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // Requests 2/3 close task 1; request 4 opens task 2 with the
    // upgraded catalog.
    fixture.captured();
    fixture.captured();
    let fourth = fixture.captured();
    assert!(
        fourth.body.contains("proj (project .ait/): available"),
        "{}",
        fourth.body.replace("\\n", "\n")
    );
    assert!(fourth.body.contains("tools: echo"), "{}", fourth.body);
    let fifth = fixture.captured();
    assert!(
        !fifth.body.contains("echo:AKIAIOSFODNN7EXAMPLE"),
        "the unredacted project-server result never re-enters: {}",
        fifth.body
    );
    fixture.assert_no_more_requests();
}

/// A denied call never spawns the project process (Spec R10 us-8 —
/// the round's one hard guarantee): the `sh -c 'touch MARKER; exec
/// fixture'` wrapper makes the spawn observable — after a deny the
/// marker is absent, and the next call's allow still takes the lazy
/// path and produces it.
#[test]
fn a_denied_project_call_never_spawns_the_process() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    let cwd = tempfile::tempdir().expect("cwd");
    let marker = cwd.path().join("spawned");
    write_project_mcp_json(
        cwd.path(),
        "proj",
        Path::new("sh"),
        &[
            "-c",
            &format!("touch {:?}; exec {:?}", marker, fixture_bin()),
        ],
    );

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"proj","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        // The model retries after the denial — this call's allow is
        // where the spawn finally happens.
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "call_mcp_tool",
            r#"{"server":"proj","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let mut events = Vec::new();
    let (id, _, summary) = until_approval_collecting(&session, &mut events);
    assert_eq!(id, "call_1");
    assert!(summary.contains("(declared by the project .ait/mcp.json)"));
    session
        .approve(&id, ApprovalVerdict::Deny("I don't trust this repo".into()))
        .expect("deny");

    // The denial is the tool result, and the process provably never
    // spawned — the gate refused before `execute` could connect.
    let (id, _, _) = until_approval_collecting(&session, &mut events);
    assert_eq!(id, "call_2");
    assert!(
        !marker.exists(),
        "the denied call must never spawn the project process"
    );
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    events.extend(collect(&session));
    assert_eq!(result_of(&events, "call_1"), "I don't trust this repo");
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Denied);
    assert_eq!(result_of(&events, "call_2"), "echo:hi");
    assert!(
        marker.exists(),
        "the allowed call lazy-connected and spawned"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// `mcp_permissions = "always_allow"` skips the ask but NOT the
/// lazy connect (Spec R10 us-8): the project server still spawns
/// only when the first call reaches it — the task-start catalog
/// lists it declared with no surface — and its ungated card still
/// carries the `.ait/` origin annotation.
#[test]
fn always_allow_connects_project_servers_lazily() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    let cwd = tempfile::tempdir().expect("cwd");
    write_project_mcp_json(cwd.path(), "proj", &fixture_bin(), &[]);

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"proj","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalRequest { .. })),
        "always_allow never asks: {events:?}"
    );
    // The ungated card's summary annotates the declaring layer too.
    let summary = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::ToolCall { call, summary } if call.id == "call_1" => Some(summary.as_str()),
            _ => None,
        })
        .expect("call_1's ToolCall card");
    assert!(
        summary.contains("(declared by the project .ait/mcp.json)"),
        "{summary}"
    );
    assert_eq!(result_of(&events, "call_1"), "echo:hi");
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Done);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // Still lazy: the task-start catalog listed no enumerated
    // surface — the connect happened inside the call.
    let first = fixture.captured();
    assert!(first.body.contains("declared"), "{}", first.body);
    assert!(
        !first.body.contains("tools: echo"),
        "spawned only on the call, not at task start: {}",
        first.body
    );
}

/// A project server shadows a same-named user server (§5.10.1's
/// nearest-layer merge): the effective entry is the project's — it
/// lists as project-declared, stays unconnected at task start, and
/// the gated call runs the PROJECT command (the user declaration's
/// spawn would have failed).
#[test]
fn a_project_server_shadows_the_same_named_user_server() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    // The user layer declares `shared` as an unspawnable command;
    // the project layer shadows it with the fixture.
    write_mcp_json(
        xdg.path(),
        "shared",
        Path::new("/nonexistent/ait-mcp-nope"),
        &[],
    );
    let cwd = tempfile::tempdir().expect("cwd");
    write_project_mcp_json(cwd.path(), "shared", &fixture_bin(), &[]);

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"shared","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    // `echo:hi` is the project entry's answer — the user's
    // /nonexistent command could never produce it.
    assert_eq!(result_of(&events, "call_1"), "echo:hi");
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Done);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let first = fixture.captured();
    assert!(
        first.body.contains("shared (project .ait/)"),
        "{}",
        first.body
    );
    assert!(
        !first.body.contains("shared (user config)"),
        "the project layer shadowed the user entry: {}",
        first.body
    );
}

/// A lazily-connected project server degrades exactly like a
/// user-tier one (Spec R10 us-18): a crash mid-call is a tool
/// error, never a task wedge.
#[test]
fn a_project_server_crash_surfaces_as_a_tool_error() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    let cwd = tempfile::tempdir().expect("cwd");
    write_project_mcp_json(
        cwd.path(),
        "proj",
        &fixture_bin(),
        &["--exit-on", "tools/call"],
    );

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"proj","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("noted")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Failed);
    assert!(result_of(&events, "call_1").contains("failed"));
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// `agent_decides` works unchanged (Spec R10 us-12): the flag rides
/// the call's own arguments — `"safe"` runs without a card while a
/// flagless call asks.
#[test]
fn agent_decides_honors_the_safety_flag_on_mcp_calls() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    write_mcp_json(xdg.path(), "fixture", &fixture_bin(), &[]);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"hi"},"safety":"safe"}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"again"},"safety":"unsafe"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"agent_decides\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // call_1 flagged "safe" — runs with no card; keep every drained
    // event so call_1's updates stay assertable.
    let mut saw_first = false;
    let mut events = Vec::new();
    let call_2_ask = loop {
        match next(&session) {
            AgentEvent::ToolCall { ref call, .. } => {
                assert_eq!(call.id, "call_1");
                saw_first = true;
            }
            AgentEvent::ApprovalRequest { call, summary } => break (call.id, summary),
            event @ (AgentEvent::Done | AgentEvent::Error(_)) => {
                panic!("expected the call_2 ask, got {event:?}")
            }
            event => events.push(event),
        }
    };
    assert!(saw_first, "call_1 ran ungated: {events:?}");
    // call_2's "unsafe" flag parks the approve card.
    assert_eq!(call_2_ask.0, "call_2");
    session
        .approve("call_2", ApprovalVerdict::AllowOnce)
        .expect("approve");
    events.extend(collect(&session));
    assert_eq!(result_of(&events, "call_1"), "echo:hi");
    assert_eq!(result_of(&events, "call_2"), "echo:again");
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The declaration carried the injected `safety` argument.
    let first = fixture.captured();
    assert!(first.body.contains("safety"), "{}", first.body);
}

/// ADR-0006 keep-last-good (Spec R10 us-4's "the terminal keeps
/// working"): a malformed `mcp.json` saved mid-session must not
/// drain healthy connections — the reconcile keeps the last working
/// pool, the server stays callable on its live transport, and the
/// `ConfigError` still surfaces: the catalog's `load_error` line
/// rides the next task's prompt (the frontend toasts the same error
/// at its submit seam).
#[test]
fn a_malformed_mcp_json_keeps_the_healthy_connections() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    write_mcp_json(xdg.path(), "fixture", &fixture_bin(), &[]);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        // Task 1: one call on the eager connection.
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("first task done")),
        // Task 2 — submitted after the file went bad: the same call
        // must still run on the kept connection.
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"still up"}}"#,
        )])),
        sse(&chat_text_turn("second task done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");
    let events = collect(&session);
    assert_eq!(result_of(&events, "call_1"), "echo:hi");
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // Break the file mid-session — the next task's reconcile fails
    // `load_mcp`.
    std::fs::write(xdg.path().join("ait").join("mcp.json"), "{not json").expect("corrupt mcp.json");

    session
        .submit(task(
            settings(&fixture.base_url, "mcp_permissions = \"always_allow\""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");
    let events = collect(&session);
    // Keep-last-good: the pool was NOT drained — the call answers
    // on the live transport rather than degrading to "no MCP server
    // named fixture".
    assert_eq!(result_of(&events, "call_2"), "echo:still up");
    assert_eq!(status_of(&events, "call_2"), ToolStatus::Done);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The error still surfaces — task 2's opening request (the
    // third captured) carries the catalog's load-error line — and
    // the kept server still lists its live surface.
    fixture.captured();
    fixture.captured();
    let third = fixture.captured();
    assert!(
        third.body.contains("mcp.json failed to load"),
        "the malformed file must surface in the prompt: {}",
        third.body
    );
    assert!(
        third.body.contains("fixture (user config): available"),
        "the kept connection stays listed: {}",
        third.body
    );
    // Task 2's closing turn is the fourth and last request.
    fixture.captured();
    fixture.assert_no_more_requests();
}

/// The deny verdict's text reaches the model as the tool result —
/// the existing approve-card flow, unchanged for MCP calls
/// (Spec R10 us-8/12).
#[test]
fn a_user_denial_is_the_mcp_calls_tool_result() {
    let _lock = xdg_lock();
    let xdg = tempfile::tempdir().expect("xdg");
    std::env::set_var("XDG_CONFIG_HOME", xdg.path());
    write_mcp_json(xdg.path(), "fixture", &fixture_bin(), &[]);
    let cwd = tempfile::tempdir().expect("cwd");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "call_mcp_tool",
            r#"{"server":"fixture","tool":"echo","arguments":{"text":"hi"}}"#,
        )])),
        sse(&chat_text_turn("understood")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            cwd.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = until_approval(&session);
    session
        .approve(&id, ApprovalVerdict::Deny("not that server".into()))
        .expect("deny");
    let events = collect(&session);
    assert_eq!(result_of(&events, "call_1"), "not that server");
    assert_eq!(status_of(&events, "call_1"), ToolStatus::Denied);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("not that server"), "{}", second.body);
    assert!(
        !second.body.contains("echo:hi"),
        "the denied call never ran: {}",
        second.body
    );
}
