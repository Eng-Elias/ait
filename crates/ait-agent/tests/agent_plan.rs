//! `update_plan` integration tests at the `AgentSession` channel
//! seam (Spec R08 us-13–17 / issue #109): the gate-exempt TODO tool
//! — a scripted call emits an ordinary ungated [`AgentEvent::ToolCall`]
//! card plus an [`AgentEvent::PlanUpdated`] snapshot carrying the
//! whole list, under every profile including an `agent_decides`-
//! heavy one and the missing-profile fail-safe, and NEVER an
//! `ApprovalRequest` — proven on the in-process `tiny_http` SSE
//! fixture's captured request bodies (standard 11). The pinned
//! block's paint and the `plans` upsert live frontend-side
//! (`crates/ait`).

use std::time::Duration;

use ait_agent::{
    AgentContext, AgentEvent, AgentRequest, AgentSession, PlanTodo, PlanTodoStatus, ToolStatus,
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

fn session() -> AgentSession {
    AgentSession::with_secret_store(NoSecrets)
}

/// A `settings.toml` snapshot binding `default`'s `coding_model` to
/// the fixture endpoint, with `profile_fields` appended to the
/// `[agents.execution_profiles.default]` table.
fn settings(base_url: &str, profile_fields: &str) -> Settings {
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
{profile_fields}
"#
    ))
    .expect("test settings must parse")
}

/// Every §5.7 field `agent_decides` — the heaviest gating stance a
/// profile can declare; `update_plan` must still run gate-free.
const AGENT_DECIDES_EVERYTHING: &str = r#"
read_files = "agent_decides"
write_to_pty = "agent_decides"
execute_commands = "agent_decides"
apply_code_diffs = "agent_decides"
ask_user_question = "agent_decides"
mcp_permissions = "agent_decides"
"#;

/// A `settings.toml` with NO `execution_profiles` table — the
/// missing-profile fail-safe asks every OTHER call; `update_plan`
/// must still pass.
fn settings_without_profiles(base_url: &str) -> Settings {
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
"#
    ))
    .expect("test settings must parse")
}

/// A task bound to `cwd`.
fn task(settings: Settings, cwd: &str) -> AgentRequest {
    AgentRequest {
        settings,
        profile: String::from("default"),
        task: String::from("plan the work"),
        context: AgentContext {
            cwd: cwd.to_owned(),
            shell: String::from("bash"),
            os: String::from("linux"),
            output_lines: Vec::new(),
        },
        bridge: None,
        prior: Vec::new(),
    }
}

/// A `chat.completion.chunk` `data:` line carrying a tool-call
/// fragment — `id`/`name` on the announce fragment, bare `arguments`
/// fragments afterwards.
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

/// A chat-completions transcript calling `name` once with `args`
/// (the JSON *text* the `arguments` string carries).
fn chat_tool_turn(index: u32, id: &str, name: &str, args: &str) -> String {
    let mut body = sse_tool_fragment(index, Some((id, name)), "");
    let (first, second) = args.split_at(args.len() / 2);
    body.push_str(&sse_tool_fragment(index, None, first));
    body.push_str(&sse_tool_fragment(index, None, second));
    body.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    );
    body.push_str("data: [DONE]\n\n");
    body
}

/// A plain end-turn chat transcript.
fn chat_text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        serde_json::to_string(text).expect("json literal")
    )
}

/// The next event, or panic — every wait carries the bound.
fn next(session: &AgentSession) -> AgentEvent {
    session
        .events()
        .recv_timeout(BOUND)
        .expect("an event within the bound")
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

/// The `PlanUpdated` snapshots in `events`, in order.
fn plan_updates(events: &[AgentEvent]) -> Vec<&[PlanTodo]> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::PlanUpdated { todos } => Some(todos.as_slice()),
            _ => None,
        })
        .collect()
}

/// Spec R08 us-13/16: a scripted `update_plan` call under an
/// `agent_decides`-everything profile emits the ungated tool card,
/// the `PlanUpdated` snapshot and a `Done` update — and NO
/// `ApprovalRequest` wraps any of it. The result text re-enters the
/// conversation on the next request, and the wire declaration
/// carried no `safety` argument.
#[test]
fn a_plan_call_emits_its_snapshot_gate_free_under_agent_decides() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "update_plan",
            r#"{"todos":[
                {"content":"reproduce","status":"done"},
                {"content":"fix","status":"in_progress"},
                {"content":"verify","status":"pending"}
            ]}"#,
        )),
        sse(&chat_text_turn("planned")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, AGENT_DECIDES_EVERYTHING),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalRequest { .. })),
        "update_plan never gates: {events:?}"
    );
    // The ordinary card still landed — name + summary — and closed
    // `Done` like every executed call (us-15).
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolCall { call, .. } if call.id == "call_1" && call.name == "update_plan"
        )),
        "{events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Done,
                ..
            } if call_id == "call_1"
        )),
        "{events:?}"
    );
    // The snapshot carries the whole list, verbatim.
    assert_eq!(
        plan_updates(&events),
        vec![vec![
            PlanTodo {
                content: "reproduce".into(),
                status: PlanTodoStatus::Done,
            },
            PlanTodo {
                content: "fix".into(),
                status: PlanTodoStatus::InProgress,
            },
            PlanTodo {
                content: "verify".into(),
                status: PlanTodoStatus::Pending,
            },
        ]
        .as_slice()],
        "{events:?}"
    );

    // Request one: `update_plan` declared with no `safety` argument —
    // the field exists for nothing it could map to (us-16).
    let first = fixture.captured();
    assert_post(&first);
    let body: serde_json::Value = serde_json::from_str(&first.body).expect("request json");
    let declaration = body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .find(|tool| tool["function"]["name"] == "update_plan")
        .expect("update_plan declared");
    assert_eq!(
        declaration["function"]["parameters"]["properties"]["safety"],
        serde_json::Value::Null,
        "{}",
        first.body
    );
    // …and a governed neighbour did gain it, proving the profile bit.
    let governed = body["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .find(|tool| tool["function"]["name"] == "read_files")
        .expect("read_files declared");
    assert_eq!(
        governed["function"]["parameters"]["properties"]["safety"]["enum"],
        serde_json::json!(["safe", "unsafe"]),
        "{}",
        first.body
    );
    // Request two: the result text re-entered as the call's result.
    let second = fixture.captured();
    assert!(
        second.body.contains("\"tool_call_id\":\"call_1\""),
        "{}",
        second.body
    );
    assert!(second.body.contains("plan updated"), "{}", second.body);
    fixture.assert_no_more_requests();
}

/// Replace-whole-list semantics ride the channel verbatim: two calls
/// in one task emit two snapshots, the second carrying its own list
/// — the panel's block and the `plans` row both keep only the last.
#[test]
fn each_call_emits_a_whole_new_snapshot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "update_plan",
            r#"{"todos":[
                {"content":"reproduce","status":"in_progress"},
                {"content":"fix","status":"pending"}
            ]}"#,
        )),
        sse(&chat_tool_turn(
            0,
            "call_2",
            "update_plan",
            r#"{"todos":[
                {"content":"reproduce","status":"done"},
                {"content":"fix","status":"done"}
            ]}"#,
        )),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    let snapshots = plan_updates(&events);
    assert_eq!(snapshots.len(), 2, "{events:?}");
    assert_eq!(
        snapshots[0]
            .iter()
            .map(|todo| todo.status)
            .collect::<Vec<_>>(),
        [PlanTodoStatus::InProgress, PlanTodoStatus::Pending]
    );
    assert_eq!(
        snapshots[1]
            .iter()
            .map(|todo| todo.status)
            .collect::<Vec<_>>(),
        [PlanTodoStatus::Done, PlanTodoStatus::Done]
    );
}

/// An empty `todos` clears the plan — the snapshot event still
/// carries it (an empty list IS the newest plan), and the result
/// text says so.
#[test]
fn an_empty_list_clears_the_plan() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "update_plan",
            r#"{"todos":[]}"#,
        )),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(
        plan_updates(&events),
        vec![&[] as &[PlanTodo]],
        "{events:?}"
    );
    let second = {
        fixture.captured();
        fixture.captured()
    };
    assert!(second.body.contains("plan cleared"), "{}", second.body);
}

/// The missing-profile fail-safe asks every OTHER call — `update_plan`
/// passes it gate-free, exemption outranking the interim rule.
#[test]
fn a_plan_call_runs_gate_free_with_no_profile_at_all() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "update_plan",
            r#"{"todos":[{"content":"only step","status":"in_progress"}]}"#,
        )),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings_without_profiles(&fixture.base_url),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalRequest { .. })),
        "the fail-safe must not gate it: {events:?}"
    );
    assert_eq!(plan_updates(&events).len(), 1, "{events:?}");
}

/// A call the model garbled — a `status` outside the three-value
/// vocabulary — is a tool error, not a wedge: no snapshot leaves,
/// the error becomes the call's result and the loop carries on.
#[test]
fn a_plan_call_with_bad_arguments_is_a_tool_error_not_a_snapshot() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "update_plan",
            r#"{"todos":[{"content":"x","status":"bogus"}]}"#,
        )),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Failed,
                ..
            } if call_id == "call_1"
        )),
        "{events:?}"
    );
    assert!(
        plan_updates(&events).is_empty(),
        "a malformed call emits no snapshot: {events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// Events stay ordered card → snapshot → result — the frontend folds
/// them in that order into the card, the pinned block and the
/// transcript (`tool_call`, `plan`, `tool_result`).
#[test]
fn the_snapshot_lands_between_its_card_and_its_result() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "update_plan",
            r#"{"todos":[{"content":"step","status":"pending"}]}"#,
        )),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // Drain in order: ToolCall first, then the snapshot, then the
    // terminal update.
    match next(&session) {
        AgentEvent::ToolCall { call, .. } if call.name == "update_plan" => {}
        other => panic!("expected the ungated card first, got {other:?}"),
    }
    match next(&session) {
        AgentEvent::PlanUpdated { todos } => assert_eq!(todos.len(), 1),
        other => panic!("expected the snapshot next, got {other:?}"),
    }
    match next(&session) {
        AgentEvent::ToolUpdate {
            status: ToolStatus::Done,
            ..
        } => {}
        other => panic!("expected the Done update, got {other:?}"),
    }
    match next(&session) {
        AgentEvent::TextDelta(text) => assert_eq!(text, "done"),
        other => panic!("expected the follow-up turn's text, got {other:?}"),
    }
    assert_eq!(next(&session), AgentEvent::Done);
}
