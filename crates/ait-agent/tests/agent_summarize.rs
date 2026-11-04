//! Summarization integration tests at the `AgentSession` channel
//! seam (Spec R08 §5.8, ticket R08-T7): when the estimate crosses
//! `SUMMARIZE_AT` of the effective limit, the worker makes one
//! summarize call to the task's coding endpoint, emits
//! [`AgentEvent::Summarized`], and the NEXT model request carries
//! the summary marker's text in place of the covered span. With no
//! limit configured, nothing ever fires.
//!
//! The fixture server speaks `openai_chat_completions`; secrets
//! resolve through a stub `SecretStore` (`NoSecrets`).

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

/// A `settings.toml` snapshot binding `coding_model` to the fixture
/// endpoint, with `read_files` ungated so the scripted call executes
/// without the approval dance. `extra_profile` carries the
/// `context_window_limit` line when the test wants one.
fn settings(base_url: &str, extra_profile: &str) -> Settings {
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
read_files = "always_allow"
{extra_profile}
"#
    ))
    .expect("test settings must parse")
}

/// A task on `cwd` with `prior` — the tab's accumulated transcript.
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

/// A chat-completions transcript: one tool turn for the call,
/// optionally closed by a usage chunk before `[DONE]`.
fn chat_tool_turn(id: &str, name: &str, args: &str, usage: Option<(u64, u64, u64)>) -> String {
    let mut body = sse_tool_fragment(0, Some((id, name)), "");
    let (first, second) = args.split_at(args.len() / 2);
    body.push_str(&sse_tool_fragment(0, None, first));
    body.push_str(&sse_tool_fragment(0, None, second));
    body.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    );
    if let Some((prompt, completion, total)) = usage {
        body.push_str(&format!(
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt},\"completion_tokens\":{completion},\"total_tokens\":{total}}}}}\n\n"
        ));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

/// A plain end-turn chat transcript.
fn chat_text_turn(text: &str) -> String {
    let mut body = sse_delta(text);
    body.push_str("data: [DONE]\n\n");
    body
}

/// Drain events until a terminal `Done`/`Cancelled`/`BudgetReached`/
/// `Error` or the bound — returning everything seen.
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

/// A tempdir holding one file big enough to push the estimate over
/// the test limit once its read lands — bound as the task cwd.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("note.txt"), "NOTE-CONTENT ".repeat(60)).expect("note");
    dir
}

/// The first task's transcript — old enough that the oldest-half cut
/// covers it whole once the estimate crosses.
fn prior() -> Vec<TranscriptItem> {
    vec![
        TranscriptItem::Task {
            text: "OLD-TASK-MARKER".into(),
        },
        TranscriptItem::AssistantText {
            text: "OLD-REPLY-MARKER".into(),
        },
        TranscriptItem::Status {
            text: "done".into(),
        },
    ]
}

/// The ticket's central criterion: the estimate crossing
/// `SUMMARIZE_AT` × the profile's `context_window_limit` fires one
/// summarize call — captured as a request of its own carrying the
/// rendered span — emits `AgentEvent::Summarized` mid-run, and the
/// next model request carries the summary text where the covered
/// items used to be.
#[test]
fn a_crossing_estimate_summarizes_the_oldest_span() {
    let dir = project();
    let cwd = dir.path().to_str().expect("utf-8 tempdir");
    // Request 1 = the scripted tool turn; request 2 = the summarize
    // call; request 3 = the next model turn on the compacted
    // conversation.
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
            None,
        )),
        sse(&chat_text_turn("SUMMARY-MARKER summary body")),
        sse(&chat_text_turn("final answer")),
    ]);
    let session = session();
    // 200-token limit → the 0.8 threshold trips once turn 1's call
    // and its big result land (~160 tokens), but not on the bare
    // transcript at the first request.
    session
        .submit(task(
            settings(&fixture.base_url, "context_window_limit = 200"),
            cwd,
            "the new task",
            prior(),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    let summarized = events.iter().find_map(|event| match event {
        AgentEvent::Summarized { covers_seq, text } => Some((*covers_seq, text.clone())),
        _ => None,
    });
    let (covers_seq, text) = summarized.expect("a Summarized event fired mid-run");
    // The covered span is exactly the prior transcript's three
    // items — the persisted seq domain.
    assert_eq!(covers_seq, 3, "{events:?}");
    assert!(text.contains("SUMMARY-MARKER"), "{text}");

    // Request 1: the original tool turn.
    let first = fixture.captured();
    assert!(first.body.contains("the new task"), "{}", first.body);
    assert!(first.body.contains("OLD-TASK-MARKER"), "{}", first.body);

    // Request 2: the summarize call — the rendered span rides the
    // prompt, and the call carries no tools.
    let summarize_call = fixture.captured();
    assert!(
        summarize_call.body.contains("<transcript>"),
        "{}",
        summarize_call.body
    );
    assert!(
        summarize_call.body.contains("OLD-TASK-MARKER"),
        "{}",
        summarize_call.body
    );

    // Request 3: the compacted conversation — the summary marker's
    // text replaces the covered items; the kept tail (new task, the
    // call and its result) is verbatim.
    let compacted = fixture.captured();
    assert!(
        compacted.body.contains("SUMMARY-MARKER"),
        "{}",
        compacted.body
    );
    assert!(
        compacted
            .body
            .contains("the earlier conversation, summarized"),
        "{}",
        compacted.body
    );
    assert!(
        !compacted.body.contains("OLD-TASK-MARKER"),
        "{}",
        compacted.body
    );
    assert!(
        !compacted.body.contains("OLD-REPLY-MARKER"),
        "{}",
        compacted.body
    );
    assert!(
        compacted.body.contains("the new task"),
        "{}",
        compacted.body
    );
    assert!(
        compacted.body.contains("NOTE-CONTENT"),
        "{}",
        compacted.body
    );
    fixture.assert_no_more_requests();
}

/// Spec us-18 + the usage rule: a stream reporting `usage` feeds the
/// estimate directly — here the chars/4 heuristic is nowhere near the
/// threshold but the reported 910 tokens cross it.
#[test]
fn provider_usage_drives_the_estimate_when_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("note.txt"), "small").expect("note");
    let cwd = dir.path().to_str().expect("utf-8 tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
            Some((880, 30, 910)),
        )),
        sse(&chat_text_turn("SUMMARY-MARKER")),
        sse(&chat_text_turn("final answer")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "context_window_limit = 1000"),
            cwd,
            "the new task",
            prior(),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Summarized { .. })),
        "reported usage past 0.8 × 1000 must fire: {events:?}"
    );
    // request 1 = tool turn, request 2 = summarize call, request 3 =
    // the compacted turn.
    fixture.captured();
    fixture.captured();
    let compacted = fixture.captured();
    assert!(
        compacted.body.contains("SUMMARY-MARKER"),
        "{}",
        compacted.body
    );
    fixture.assert_no_more_requests();
}

/// Spec us-19: with neither `context_window_limit` on the profile
/// nor `context_window` on the model, summarization is off — no
/// `Summarized` event, no extra request, however big the items get.
#[test]
fn with_neither_limit_set_nothing_ever_fires() {
    let dir = project();
    let cwd = dir.path().to_str().expect("utf-8 tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"]}"#,
            None,
        )),
        sse(&chat_text_turn("final answer")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            cwd,
            "the new task",
            prior(),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::Summarized { .. })),
        "no limit means no rewrite: {events:?}"
    );
    fixture.captured();
    let second = fixture.captured();
    assert!(!second.body.contains("<transcript>"), "{}", second.body);
    fixture.assert_no_more_requests();
}

/// The marker the frontend records replays correctly on the NEXT
/// task: a `summary` item plus the kept tail rebuild the context
/// — the continuity record and the live rewrite stay identical by
/// construction.
#[test]
fn the_next_task_replays_the_marker_and_kept_tail() {
    let fixture = Fixture::serve(vec![sse(&chat_text_turn("answer"))]);
    let session = session();
    let mut prior = prior();
    // What the frontend transcript looks like after a rewrite: the
    // old items still present, the marker after them.
    prior.push(TranscriptItem::Summary {
        text: "SUMMARY-MARKER".into(),
        covers_seq: 3,
    });
    prior.push(TranscriptItem::ToolCall {
        id: "call_9".into(),
        name: "read_files".into(),
        args: "{}".into(),
        summary: "note.txt".into(),
    });
    prior.push(TranscriptItem::ToolResult {
        id: "call_9".into(),
        text: "KEPT-RESULT-MARKER".into(),
        status: ToolStatus::Done,
    });
    session
        .submit(task(
            settings(&fixture.base_url, ""),
            "/home/u/proj",
            "next task",
            prior,
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    let body = fixture.captured().body;
    assert!(body.contains("SUMMARY-MARKER"), "{body}");
    assert!(body.contains("KEPT-RESULT-MARKER"), "{body}");
    assert!(!body.contains("OLD-TASK-MARKER"), "{body}");
}
