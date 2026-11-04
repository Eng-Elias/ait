//! Tool-pipeline integration tests at the `AgentSession` channel seam
//! (Spec R06 testing decisions / issue #86): an [`AgentRequest`] in on
//! one channel, [`AgentEvent`]s out on the other, verdicts back on
//! [`AgentSession::approve`] — against the in-process `tiny_http` SSE
//! fixture (`ait_llm::testing`, `test-fixtures` feature), so the full
//! HTTP+SSE path runs with zero external network (standard 11).
//!
//! The fixture's captured request bodies are the assertion for
//! "tool results re-entered in the schema's own IR"; file-tool
//! effects assert against `tempfile` trees.

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

/// Bound on "the worker answered" — loopback is fast; a hang is a
/// failure, not a slow pass.
const BOUND: Duration = Duration::from_secs(10);

/// The gate's `recv_timeout` poll cadence (session.rs `GATE_POLL`) is
/// 100 ms — the no-timeout test waits well past it, and the
/// cancel-while-gated test allows one full poll for `Cancelled`.
const GATE_GRACE: Duration = Duration::from_millis(400);

fn session() -> AgentSession {
    AgentSession::with_secret_store(NoSecrets)
}

/// A `settings.toml` snapshot binding `coding_model` to one endpoint
/// at the fixture URL.
fn settings(base_url: &str, schema: &str) -> Settings {
    Settings::from_toml(&format!(
        r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "{base_url}"
schema = "{schema}"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "coding-key"

[agents.execution_profiles.default]
coding_model = "coding-key"
"#
    ))
    .expect("test settings must parse")
}

/// A task bound to `cwd` (a tempdir in most tests — the file tools
/// confine to it).
fn task(settings: Settings, cwd: &str) -> AgentRequest {
    AgentRequest {
        settings,
        profile: String::from("default"),
        task: String::from("look around"),
        context: AgentContext {
            cwd: cwd.to_owned(),
            shell: String::from("bash"),
            os: String::from("linux"),
            output_lines: Vec::new(),
        },
        bridge: None,
    }
}

/// A JSON string literal (`"…"`) for hand-built chunk bodies.
fn json_string(text: &str) -> String {
    serde_json::to_string(text).expect("json literal")
}

/// A `chat.completion.chunk` `data:` line carrying a tool-call
/// fragment — `id`/`name` on the first (`announce`) fragment of a
/// call, bare `arguments` fragments afterwards.
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
        json_string(text)
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

/// An `openai_responses` transcript announcing one `function_call`
/// item then streaming its arguments.
fn responses_tool_turn(item_id: &str, call_id: &str, name: &str, args: &str) -> String {
    let mut body = format!(
        "event: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"id\":{item},\"type\":\"function_call\",\"call_id\":{call},\"name\":{name},\"arguments\":\"\",\"status\":\"in_progress\"}}}}\n\n",
        item = json_string(item_id),
        call = json_string(call_id),
        name = json_string(name),
    );
    body.push_str(&format!(
        "event: response.function_call_arguments.delta\ndata: {{\"type\":\"response.function_call_arguments.delta\",\"item_id\":{},\"delta\":{}}}\n\n",
        json_string(item_id),
        json_string(args)
    ));
    body.push_str(
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    body
}

/// A plain `openai_responses` end turn.
fn responses_text_turn(text: &str) -> String {
    let mut body = format!(
        "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":{}}}\n\n",
        json_string(text)
    );
    body.push_str(
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    body
}

/// An `anthropic_messages` transcript opening one `tool_use` block
/// and streaming `partial_json` fragments.
fn anthropic_tool_turn(index: u32, id: &str, name: &str, args: &str) -> String {
    let mut body = format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{{\"type\":\"tool_use\",\"id\":{id},\"name\":{name},\"input\":{{}}}}}}\n\n",
        id = json_string(id),
        name = json_string(name),
    );
    body.push_str(&format!(
        "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{}}}}}\n\n",
        json_string(args)
    ));
    body.push_str(&format!(
        "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{index}}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    ));
    body
}

/// A plain `anthropic_messages` end turn.
fn anthropic_text_turn(text: &str) -> String {
    format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        json_string(text)
    )
}

/// The next event, or panic — every wait carries the bound.
fn next(session: &AgentSession) -> AgentEvent {
    session
        .events()
        .recv_timeout(BOUND)
        .expect("an event within the bound")
}

/// The next `ApprovalRequest` — fails on anything else so the event
/// ordering shows in the panic.
fn next_approval(session: &AgentSession) -> (String, String, String) {
    match next(session) {
        AgentEvent::ApprovalRequest { call, summary } => (call.id, call.name, summary),
        other => panic!("expected an ApprovalRequest, got {other:?}"),
    }
}

/// Drain card transitions until the next ungated `ToolCall` — the
/// read for allow-for-task repeats, whose updates interleave with
/// the previous call's `Running`/`Done`.
fn until_tool_call(session: &AgentSession) -> String {
    loop {
        match next(session) {
            AgentEvent::ToolCall { call, .. } => return call.id,
            event @ (AgentEvent::Done
            | AgentEvent::Cancelled
            | AgentEvent::BudgetReached
            | AgentEvent::Error(_)) => panic!("expected a ToolCall, got {event:?}"),
            _ => {}
        }
    }
}

/// Drain card transitions until the next `ApprovalRequest` — the
/// read for tests where completed calls' updates interleave with the
/// next gate.
fn until_approval(session: &AgentSession) -> (String, String, String) {
    loop {
        match next(session) {
            AgentEvent::ApprovalRequest { call, summary } => return (call.id, call.name, summary),
            event @ (AgentEvent::Done
            | AgentEvent::Cancelled
            | AgentEvent::BudgetReached
            | AgentEvent::Error(_)) => panic!("expected an ApprovalRequest, got {event:?}"),
            _ => {}
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

/// A tempdir holding one readable file, bound as the task cwd.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("note.txt"), "NOTE-CONTENTS").expect("note");
    dir
}

/// The gate asks before anything runs; *allow once* executes and the
/// result re-enters as the schema's own tool-result item in the next
/// request (Spec R06 — the ticket's central criterion).
#[test]
fn approval_gates_execution_and_result_reenters_as_tool_item() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )])),
        sse(&chat_text_turn("all read")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, name, summary) = next_approval(&session);
    assert_eq!(name, "read_files");
    assert_eq!(summary, "note.txt");

    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let events = collect(&session);
    // Running → Done transitions on the card, then the end turn.
    assert_eq!(
        events[..2],
        [
            AgentEvent::ToolUpdate {
                call_id: "call_1".into(),
                status: ToolStatus::Running,
                detail: None,
                result: None,
            },
            AgentEvent::ToolUpdate {
                call_id: "call_1".into(),
                status: ToolStatus::Done,
                detail: Some("note.txt: …".into()),
                result: Some("note.txt:\nNOTE-CONTENTS".into()),
            },
        ],
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // Request 2 carries the schema's own tool items: an assistant
    // message with the call, then a `tool` message with the result.
    fixture.captured();
    let second = fixture.captured();
    assert_post(&second);
    let body = &second.body;
    assert!(body.contains("\"tool_calls\""), "{body}");
    assert!(body.contains("\"role\":\"tool\""), "{body}");
    assert!(body.contains("\"tool_call_id\":\"call_1\""), "{body}");
    assert!(body.contains("NOTE-CONTENTS"), "{body}");
    // …and the request advertised the registry (all seven v1 tools).
    assert!(body.contains("\"name\":\"read_files\""), "{body}");
    assert!(body.contains("\"name\":\"run_command\""), "{body}");
    fixture.assert_no_more_requests();
}

/// *Deny* sends the denial text to the model as the tool's result —
/// and the call never executed: no card ever went `Running`.
#[test]
fn denied_call_feeds_the_denial_text_back_as_its_result() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )])),
        sse(&chat_text_turn("understood")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::Deny("not that file".into()))
        .expect("deny");

    let events = collect(&session);
    assert_eq!(
        events[0],
        AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ToolStatus::Denied,
            detail: Some("not that file".into()),
            result: Some("not that file".into()),
        }
    );
    assert!(!events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolUpdate {
            status: ToolStatus::Running,
            ..
        }
    )));
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("not that file"), "{}", second.body);
    assert!(
        !second.body.contains("NOTE-CONTENTS"),
        "the denied call never ran: {}",
        second.body
    );
}

/// *Allow for task* suppresses later prompts for THAT tool only —
/// the same tool runs ungated while another still asks (us-7).
#[test]
fn allow_for_task_suppresses_that_tools_prompts_only() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_3",
            "glob",
            r#"{"pattern":"*.txt"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowForTask)
        .expect("approve");

    // Turn 2's read_files runs WITHOUT asking: a `ToolCall` event
    // (card straight to running), no `ApprovalRequest`, and it
    // completes on its own — no verdict is ever sent for call_2.
    assert_eq!(until_tool_call(&session), "call_2");
    // The next prompt is turn 3's glob — a different tool still asks.
    let (id, name, _) = until_approval(&session);
    assert_eq!(name, "glob");
    assert_eq!(id, "call_3");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    fixture.captured();
    fixture.captured();
    fixture.captured();
    let fourth = fixture.captured();
    assert!(fourth.body.contains("note.txt"), "{}", fourth.body);
}

/// Two calls in one turn gate and execute sequentially, in model
/// order — the second prompt only arrives after the first resolved.
#[test]
fn calls_in_one_turn_execute_sequentially_in_model_order() {
    let dir = project();
    std::fs::write(dir.path().join("other.txt"), "OTHER-CONTENTS").expect("other");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[
            (0, "call_1", "read_files", r#"{"paths":["note.txt"]}"#),
            (1, "call_2", "read_files", r#"{"paths":["other.txt"]}"#),
        ])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    assert_eq!(id, "call_1", "the first call asks first");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // The first call resolves before the second prompt appears.
    match next(&session) {
        AgentEvent::ToolUpdate {
            call_id,
            status: ToolStatus::Running,
            ..
        } => assert_eq!(call_id, "call_1"),
        other => panic!("expected call_1 running, got {other:?}"),
    }
    match next(&session) {
        AgentEvent::ToolUpdate {
            call_id,
            status: ToolStatus::Done,
            ..
        } => assert_eq!(call_id, "call_1"),
        other => panic!("expected call_1 done, got {other:?}"),
    }
    let (id, _, _) = next_approval(&session);
    assert_eq!(id, "call_2", "the second call asks second");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    fixture.captured();
    let second = fixture.captured();
    // Results are appended in call order.
    let first = second.body.find("NOTE-CONTENTS").expect("first result");
    let other = second.body.find("OTHER-CONTENTS").expect("second result");
    assert!(first < other, "results kept model order: {}", second.body);
}

/// `read_files`' guards reach the model as tool results: escapes,
/// oversized and binary reads are errors in the next request body —
/// never a panic, never a read outside the tree.
#[test]
fn read_guard_errors_arrive_as_tool_results() {
    let dir = project();
    std::fs::write(dir.path().join("bin.dat"), b"\x00binary").expect("binary");
    std::fs::write(dir.path().join("../escape.txt"), "SECRET-OUTSIDE").expect("outside");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "read_files",
            r#"{"paths":["../escape.txt","bin.dat","missing.txt","note.txt"]}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    let body = &second.body;
    assert!(body.contains("outside the working directory"), "{body}");
    assert!(body.contains("binary"), "{body}");
    assert!(body.contains("cannot stat"), "{body}");
    assert!(body.contains("NOTE-CONTENTS"), "{body}");
    assert!(!body.contains("SECRET-OUTSIDE"), "{body}");
}

/// `grep` and `glob` run confined to the same subtree — their
/// results land in the next request as match lines and paths.
#[test]
fn grep_and_glob_results_reenter_confined_to_the_tree() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("src")).expect("mkdir");
    std::fs::write(dir.path().join("src/a.rs"), "// MARK-HERE\nfn a() {}\n").expect("a");
    std::fs::write(dir.path().join("../outside.rs"), "// MARK-HERE\n").expect("outside");

    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "grep",
            r#"{"pattern":"MARK-HERE"}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "glob",
            r#"{"pattern":"**/*.rs"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let (id, _, _) = until_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("src/a.rs:1: // MARK-HERE"),
        "{}",
        second.body
    );
    let third = fixture.captured();
    assert!(third.body.contains("src/a.rs"), "{}", third.body);
    assert!(!third.body.contains("outside.rs"), "{}", third.body);
}

/// A credential-looking string inside a tool result never reaches the
/// next provider request — §5.10.3's rule extends to everything the
/// agent touches.
#[test]
fn tool_results_are_redacted_before_reentering() {
    let dir = project();
    std::fs::write(dir.path().join("key.txt"), "key = AKIAIOSFODNN7EXAMPLE\n").expect("key");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "read_files",
            r#"{"paths":["key.txt"]}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(
        !second.body.contains("AKIAIOSFODNN7EXAMPLE"),
        "{}",
        second.body
    );
    assert!(second.body.contains("[REDACTED]"), "{}", second.body);
}

/// A runaway model that keeps requesting tool calls terminates at
/// the 50-turn step budget (Spec R06 — the budget's first end-to-end
/// exercise).
#[test]
fn a_tool_call_loop_terminates_at_the_step_budget() {
    let dir = project();
    let turn = chat_tool_turn(&[(0, "call_n", "read_files", r#"{"paths":["note.txt"]}"#)]);
    let fixture = Fixture::serve((0..50).map(|_| sse(&turn)).collect());
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // Allow `read_files` once for the task — 50 calls run ungated,
    // each emitting its card events.
    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowForTask)
        .expect("approve");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::BudgetReached));
    // Only the first call ever asked — every later `read_files` ran
    // on the allow-for-task grant (the ApprovalRequest itself was
    // consumed by `next_approval` above).
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalRequest { .. })),
        "{events:?}"
    );
    assert_eq!(fixture.captured_count(), 50);
}

/// Approval prompts wait indefinitely — no timeout into a default
/// verdict (us-9): well past the gate's poll cadence nothing has
/// moved, and the task proceeds only once the verdict lands.
#[test]
fn approval_never_times_out() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    // Well past GATE_POLL — the loop must still be parked on the card.
    assert!(
        session.events().recv_timeout(GATE_GRACE * 3).is_err(),
        "the gate answered itself — prompts must wait indefinitely"
    );
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// Esc still cancels the whole task while the loop is parked on an
/// approve card.
#[test]
fn esc_cancels_while_the_gate_waits() {
    let dir = project();
    let fixture = Fixture::serve(vec![sse(&chat_tool_turn(&[(
        0,
        "call_1",
        "read_files",
        r#"{"paths":["note.txt"]}"#,
    )]))]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let _ = next_approval(&session);
    session.cancel();
    match session.events().recv_timeout(BOUND) {
        Ok(AgentEvent::Cancelled) => {}
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

/// `openai_responses`: streamed `item_id`-addressed fragments gate
/// the same way, and the result re-enters as a
/// `function_call_output` item answering `call_id` (§5.3 — the
/// schema's own IR).
#[test]
fn responses_schema_tool_result_items() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&responses_tool_turn(
            "fc_1",
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )),
        sse(&responses_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_responses"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, name, _) = next_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_1", "read_files"));
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert_eq!(second.url, "/responses");
    assert!(
        second.body.contains("\"type\":\"function_call\""),
        "{}",
        second.body
    );
    assert!(
        second.body.contains("\"type\":\"function_call_output\""),
        "{}",
        second.body
    );
    assert!(
        second.body.contains("\"call_id\":\"call_1\""),
        "{}",
        second.body
    );
    assert!(second.body.contains("NOTE-CONTENTS"), "{}", second.body);
}

/// `anthropic_messages`: block-index-addressed fragments gate the
/// same way; the result re-enters as a `tool_result` block inside a
/// `user` message answering `tool_use_id` (§5.3).
#[test]
fn anthropic_schema_tool_result_items() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&anthropic_tool_turn(
            0,
            "toolu_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )),
        sse(&anthropic_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "anthropic_messages"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, name, _) = next_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("toolu_1", "read_files"));
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert_eq!(second.url, "/messages");
    assert!(
        second.body.contains("\"type\":\"tool_use\""),
        "{}",
        second.body
    );
    assert!(
        second.body.contains("\"type\":\"tool_result\""),
        "{}",
        second.body
    );
    assert!(
        second.body.contains("\"tool_use_id\":\"toolu_1\""),
        "{}",
        second.body
    );
    assert!(second.body.contains("NOTE-CONTENTS"), "{}", second.body);
}

/// `edit_files`' approve card carries the unified diff — `a`/`b`
/// labels, `-`/`+` lines — and an allow writes `content`
/// byte-for-byte (Spec R06 us-22: the rendered diff IS the change).
#[test]
fn edit_files_approve_writes_exactly_what_the_diff_showed() {
    let dir = project(); // note.txt holds "NOTE-CONTENTS"
    let content = "REWRITTEN line\n";
    let args = format!(
        r#"{{"files":[{{"path":"note.txt","content":{}}}]}}"#,
        json_string(content)
    );
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(0, "call_1", "edit_files", &args)])),
        sse(&chat_text_turn("edited")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, name, summary) = next_approval(&session);
    assert_eq!(name, "edit_files");
    assert!(summary.contains("--- a/note.txt"), "{summary}");
    assert!(summary.contains("+++ b/note.txt"), "{summary}");
    assert!(summary.contains("-NOTE-CONTENTS"), "{summary}");
    assert!(summary.contains("+REWRITTEN line"), "{summary}");

    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert_eq!(
        std::fs::read(dir.path().join("note.txt")).expect("read"),
        content.as_bytes(),
        "what the diff showed is byte-for-byte what landed"
    );

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("wrote note.txt"), "{}", second.body);
}

/// A denied `edit_files` call writes nothing — the file keeps its
/// bytes and the denial text is the tool result the model reads.
#[test]
fn edit_files_deny_writes_nothing() {
    let dir = project();
    let args = r#"{"files":[{"path":"note.txt","content":"SHOULD NOT LAND"}]}"#;
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(0, "call_1", "edit_files", args)])),
        sse(&chat_text_turn("ok")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::Deny("leave it alone".into()))
        .expect("deny");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).expect("read"),
        "NOTE-CONTENTS"
    );

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("leave it alone"), "{}", second.body);
}

/// A new file previews `--- /dev/null` and lands on approve; a
/// missing parent directory and a `..` escape are tool errors the
/// model reads — never silent mkdirs or outside writes (us-23/24).
#[test]
fn edit_files_new_file_missing_parent_and_escape() {
    let dir = project();
    let new_args = r#"{"files":[{"path":"sub/new.txt","content":"FRESH\n"}]}"#;
    std::fs::create_dir_all(dir.path().join("sub")).expect("sub");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(0, "call_1", "edit_files", new_args)])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "edit_files",
            r#"{"files":[{"path":"nope/new.txt","content":"x"}]}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_3",
            "edit_files",
            &format!(
                r#"{{"files":[{{"path":"../escape-{}.txt","content":"x"}}]}}"#,
                std::process::id()
            ),
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // New file: /dev/null diff on the card, file written on allow.
    let (id, _, summary) = next_approval(&session);
    assert!(summary.contains("--- /dev/null"), "{summary}");
    assert!(summary.contains("+++ b/sub/new.txt"), "{summary}");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // Missing parent: the card carries the error and allowing it
    // fails as a tool result — no directory was created. Its
    // prompt arriving also proves call_1's write landed.
    let (id, _, summary) = until_approval(&session);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("sub/new.txt")).expect("read"),
        "FRESH\n"
    );
    assert!(summary.contains("parent"), "{summary}");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    assert!(!dir.path().join("nope").exists());

    // Escape: same shape — the error asks, the allow fails.
    let (id, _, summary) = until_approval(&session);
    assert!(
        summary.contains("outside the working directory"),
        "{summary}"
    );
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    // The escaped path got no write — assert on a unique name, the
    // tempdir parent is shared across tests.
    assert!(!dir
        .path()
        .join(format!("../escape-{}.txt", std::process::id()))
        .exists());

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    // The failures re-entered as Failed card transitions and tool
    // results the model reads in later request bodies.
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::ToolUpdate {
            status: ToolStatus::Failed,
            ..
        }
    )));
    fixture.captured();
    fixture.captured();
    let third = fixture.captured();
    assert!(third.body.contains("parent"), "{}", third.body);
    let fourth = fixture.captured();
    assert!(
        fourth.body.contains("outside the working directory"),
        "{}",
        fourth.body
    );
}

/// The interim gate treats `edit_files` like every other tool —
/// *allow for task* suppresses its later prompts while the writes
/// still land ungated (us-7).
#[test]
fn edit_files_allow_for_task_suppresses_later_prompts() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "edit_files",
            r#"{"files":[{"path":"note.txt","content":"ONE\n"}]}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "edit_files",
            r#"{"files":[{"path":"note.txt","content":"TWO\n"}]}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowForTask)
        .expect("approve");

    // call_2 runs WITHOUT asking — a ToolCall event, and its write
    // lands with no verdict ever sent.
    assert_eq!(until_tool_call(&session), "call_2");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalRequest { .. })),
        "{events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("note.txt")).expect("read"),
        "TWO\n",
        "the ungated repeat still applied its plan"
    );
}

/// `write_to_pty` gates like every call — the approve card's summary
/// is the input about to be sent — and on approval the agent's own
/// PTY round-trips through call↔read: a `read` builtin parks the
/// first call, the second call's bytes produce the response the
/// model then sees in its context (Spec R06 us-17/18/20, issue #90).
/// The worker resolves the user's shell — `/bin/sh` fallback or an
/// injected bash/zsh/fish; `read`/`echo` are builtins on all of
/// them, so the scenario needs no external program.
#[test]
fn write_to_pty_gates_then_round_trips_through_the_agent_pty() {
    // Pin the worker's resolved shell to an injected one where it
    // exists: the `OSC 133` prompt marks give the settle its
    // deterministic path (a `133;A` after the write means the command
    // finished — no output can straddle a quiet window). `/bin/bash`
    // is the portable pick; this test binary spawns no other PTYs.
    if std::path::Path::new("/bin/bash").exists() {
        // Env mutation is process-global but safe here — no other
        // test in this binary spawns a PTY.
        std::env::set_var("SHELL", "/bin/bash");
    }
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "write_to_pty",
            r#"{"input":"read v; echo READ-$v\n"}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "write_to_pty",
            r#"{"input":"hello\n"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // The first call parks on the approve card — nothing is written
    // (or spawned) until the verdict lands. The card's summary is
    // the bytes queued for the PTY (us-20).
    let (id, name, summary) = next_approval(&session);
    assert_eq!(name, "write_to_pty");
    assert!(summary.contains("read v"), "{summary}");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // Call 2's card appears only after call 1's write+settle ran.
    let (id, name, summary) = until_approval(&session);
    assert_eq!(name, "write_to_pty");
    assert!(summary.contains("hello"), "{summary}");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    let events = collect(&session);
    // Both cards ran to Done; the last line of the PTY output rode
    // call_2's card detail (us-20's output tail).
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Done,
                detail: Some(_),
                ..
            } if call_id == "call_2"
        )),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    fixture.captured();
    // `READ-hello` is the shell's answer to the second call's input,
    // not the echoed input line — proof the bytes reached a live
    // interactive session and its output re-entered the conversation
    // as the schema's tool-result item.
    let third = fixture.captured();
    assert!(third.body.contains("READ-hello"), "{}", third.body);
    fixture.assert_no_more_requests();
}

/// A denied `write_to_pty` never reaches the PTY — the card goes
/// `Denied` without ever running and the denial text is the tool's
/// result (issue #90: approval resolves before any bytes are
/// written).
#[test]
fn denied_write_to_pty_never_writes() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "write_to_pty",
            r#"{"input":"cat\n"}"#,
        )])),
        sse(&chat_text_turn("understood")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_chat_completions"),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::Deny("keep hands off".into()))
        .expect("deny");

    let events = collect(&session);
    assert_eq!(
        events[0],
        AgentEvent::ToolUpdate {
            call_id: "call_1".into(),
            status: ToolStatus::Denied,
            detail: Some("keep hands off".into()),
            result: Some("keep hands off".into()),
        }
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                status: ToolStatus::Running,
                ..
            }
        )),
        "a denied call never ran: {events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("keep hands off"), "{}", second.body);
}
