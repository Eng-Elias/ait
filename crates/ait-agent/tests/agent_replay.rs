//! Transcript-replay integration tests at the `AgentSession` channel
//! seam (Spec R08 §5.8, ticket R08-T4): an [`AgentRequest`] carrying
//! `prior` — the accumulated [`TranscriptItem`]s — replays them into
//! the resolved endpoint's IR, so a second task in a tab continues
//! the model context where the first left off. Captured fixture
//! bodies are the assertion: the prior items appear in the request
//! in that schema's own shape.
//!
//! The fixture server speaks whichever schema the test scripts;
//! secrets resolve through a stub `SecretStore` (`NoSecrets`).

use std::time::Duration;

use ait_agent::{AgentContext, AgentEvent, AgentRequest, AgentSession, ToolStatus, TranscriptItem};
use ait_config::{SecretStore, Settings};
use ait_llm::testing::{sse, Fixture};

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

/// A `settings.toml` snapshot binding `coding_model` to one endpoint
/// at the fixture URL, with `read_files` ungated so scripted calls
/// execute without the approval dance.
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
read_files = "always_allow"
"#
    ))
    .expect("test settings must parse")
}

/// A task on `cwd` carrying `prior` — empty is the old fresh
/// conversation, the tab's accumulated items is the R08 continuity
/// record.
fn task(settings: Settings, cwd: &str, text: &str, prior: Vec<TranscriptItem>) -> AgentRequest {
    AgentRequest {
        settings,
        profile: String::from("default"),
        task: text.to_owned(),
        prior,
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
/// call, bare `arguments` fragments afterwards (agent_tools.rs's
/// builder, duplicated per-file).
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

/// A chat-completions transcript: one tool turn for `call`.
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
    let mut body = sse_delta(text);
    body.push_str("data: [DONE]\n\n");
    body
}

/// A plain `anthropic_messages` end turn.
fn anthropic_text_turn(text: &str) -> String {
    format!(
        "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        json_string(text)
    )
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

/// The ticket's central criterion: a second task in one tab sends a
/// request whose body carries the FIRST task's transcript — task
/// text, assistant reply, tool call and result — in the resolved
/// schema's own shape. The model context is continuous; the panel's
/// scrollback is untouched (model-side only).
#[test]
fn a_second_task_continues_the_first_tasks_context() {
    let dir = project();
    let cwd = dir.path().to_str().expect("utf-8 tempdir");
    // Task 1: one tool turn then an end turn. Task 2: one end turn.
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
        )),
        sse(&chat_text_turn("all read")),
        sse(&chat_text_turn("second answer")),
    ]);
    let session = session();
    let settings = settings(&fixture.base_url, "openai_chat_completions");
    session
        .submit(task(settings.clone(), cwd, "first task", Vec::new()))
        .expect("submit task 1");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    fixture.captured();
    let second_request = fixture.captured();
    // Task 1's own second request already proves the live turn
    // landed; task 2's `prior` must replay the same items.
    assert!(second_request.body.contains("NOTE-CONTENTS"));

    // The items the frontend's transcript mirror records for task 1
    // — task, call, result, assistant text, terminal status.
    let prior = vec![
        TranscriptItem::Task {
            text: "first task".into(),
        },
        TranscriptItem::ToolCall {
            id: "call_1".into(),
            name: "read_files".into(),
            args: r#"{"paths":["note.txt"]}"#.into(),
            summary: "note.txt".into(),
        },
        TranscriptItem::ToolResult {
            id: "call_1".into(),
            text: "note.txt:\nNOTE-CONTENTS".into(),
            status: ToolStatus::Done,
        },
        TranscriptItem::AssistantText {
            text: "all read".into(),
        },
        TranscriptItem::Status {
            text: "done".into(),
        },
    ];
    session
        .submit(task(settings, cwd, "second task", prior))
        .expect("submit task 2");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let third = fixture.captured();
    let body = &third.body;
    // The whole first task, in chat-completions shape: the task as a
    // user message, the call inside an assistant `tool_calls` item,
    // the result as a `tool` message, the reply as assistant text.
    assert!(body.contains("first task"), "{body}");
    assert!(body.contains("\"tool_calls\""), "{body}");
    assert!(body.contains("\"id\":\"call_1\""), "{body}");
    assert!(body.contains("\"name\":\"read_files\""), "{body}");
    assert!(body.contains("\"role\":\"tool\""), "{body}");
    assert!(body.contains("\"tool_call_id\":\"call_1\""), "{body}");
    assert!(body.contains("NOTE-CONTENTS"), "{body}");
    assert!(body.contains("all read"), "{body}");
    assert!(body.contains("second task"), "{body}");
    fixture.assert_no_more_requests();
}

/// The neutral transcript replays into whatever schema the resolved
/// `coding_model` speaks — resuming onto an anthropic endpoint packs
/// the same items as `tool_use`/`tool_result` blocks, pairing
/// included.
#[test]
fn a_transcript_replays_into_a_different_schema() {
    let dir = project();
    let cwd = dir.path().to_str().expect("utf-8 tempdir");
    let fixture = Fixture::serve(vec![sse(&anthropic_text_turn("resumed"))]);
    let session = session();
    let prior = vec![
        TranscriptItem::Task {
            text: "first task".into(),
        },
        TranscriptItem::AssistantText {
            text: "let me look".into(),
        },
        TranscriptItem::ToolCall {
            id: "call_7".into(),
            name: "read_files".into(),
            args: r#"{"paths":["note.txt"]}"#.into(),
            summary: "note.txt".into(),
        },
        TranscriptItem::ToolResult {
            id: "call_7".into(),
            text: "note.txt:\nNOTE-CONTENTS".into(),
            status: ToolStatus::Done,
        },
        TranscriptItem::AssistantText {
            text: "all read".into(),
        },
    ];
    session
        .submit(task(
            settings(&fixture.base_url, "anthropic_messages"),
            cwd,
            "second task",
            prior,
        ))
        .expect("submit");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let captured = fixture.captured();
    let body = &captured.body;
    // Anthropic's own shapes: `tool_use` in an assistant message with
    // `input` decoded, `tool_result` blocks answering it, the system
    // prompt top-level.
    assert!(body.contains("\"type\":\"tool_use\""), "{body}");
    assert!(body.contains("\"id\":\"call_7\""), "{body}");
    assert!(body.contains("\"type\":\"tool_result\""), "{body}");
    assert!(body.contains("\"tool_use_id\":\"call_7\""), "{body}");
    assert!(body.contains("NOTE-CONTENTS"), "{body}");
    assert!(body.contains("first task"), "{body}");
    assert!(body.contains("second task"), "{body}");
    assert_eq!(captured.url, "/messages");
}

/// A `tool_call` with no recorded result — a crash mid-run or an
/// approve card parked at exit — reaches the model paired with the
/// synthetic interrupted result: no `tool_use` dangles.
#[test]
fn an_unanswered_prior_call_arrives_paired_with_interrupted() {
    let dir = project();
    let cwd = dir.path().to_str().expect("utf-8 tempdir");
    let fixture = Fixture::serve(vec![sse(&anthropic_text_turn("resumed"))]);
    let session = session();
    let prior = vec![
        TranscriptItem::Task {
            text: "first task".into(),
        },
        TranscriptItem::ToolCall {
            id: "call_9".into(),
            name: "run_command".into(),
            args: r#"{"command":"make"}"#.into(),
            summary: "\"make\"".into(),
        },
        TranscriptItem::Status {
            text: "cancelled".into(),
        },
    ];
    session
        .submit(task(
            settings(&fixture.base_url, "anthropic_messages"),
            cwd,
            "pick up where we stopped",
            prior,
        ))
        .expect("submit");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let body = fixture.captured().body;
    assert!(body.contains("\"tool_use_id\":\"call_9\""), "{body}");
    assert!(body.contains("interrupted"), "{body}");
}

/// The empty-`prior` invariant: a task with no transcript behaves
/// exactly like today's fresh conversation — task two's request
/// carries nothing of task one.
#[test]
fn an_empty_prior_is_a_fresh_conversation() {
    let dir = project();
    let cwd = dir.path().to_str().expect("utf-8 tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_text_turn("first reply")),
        sse(&chat_text_turn("second reply")),
    ]);
    let session = session();
    let settings = settings(&fixture.base_url, "openai_chat_completions");
    session
        .submit(task(settings.clone(), cwd, "FIRST-TASK-MARKER", Vec::new()))
        .expect("submit task 1");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    session
        .submit(task(settings, cwd, "SECOND-TASK-MARKER", Vec::new()))
        .expect("submit task 2");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("SECOND-TASK-MARKER"));
    assert!(
        !second.body.contains("FIRST-TASK-MARKER"),
        "empty prior replays nothing: {}",
        second.body
    );
    assert!(
        !second.body.contains("first reply"),
        "no prior ⇒ the first turn's reply never entered: {}",
        second.body
    );
}
