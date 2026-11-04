//! `ask_user_question` integration tests at the `AgentSession`
//! channel seam (Spec R06 us-10 / issue #91): the gate-exempt
//! question tool whose card IS the ask — an [`AgentEvent::AskRequest`]
//! out carrying the question and its choices-or-free-text, the user's
//! answer back on [`AgentSession::answer`], and the answer re-entering
//! the conversation as the call's tool result — proven on the
//! in-process `tiny_http` SSE fixture's captured request bodies
//! (standard 11).

use std::time::Duration;

use ait_agent::{AgentContext, AgentEvent, AgentRequest, AgentSession, ToolStatus};
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

/// The ask wait shares the gate's `recv_timeout` poll cadence
/// (session.rs `GATE_POLL`) — the no-stray-answer test waits well
/// past it.
const ASK_GRACE: Duration = Duration::from_millis(400);

fn session() -> AgentSession {
    AgentSession::with_secret_store(NoSecrets)
}

/// A `settings.toml` snapshot binding `coding_model` to the fixture.
fn settings(base_url: &str) -> Settings {
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
"#
    ))
    .expect("test settings must parse")
}

/// A task bound to `cwd`.
fn task(settings: Settings, cwd: &str) -> AgentRequest {
    AgentRequest {
        settings,
        profile: String::from("default"),
        task: String::from("set things up"),
        context: AgentContext {
            cwd: cwd.to_owned(),
            shell: String::from("bash"),
            os: String::from("linux"),
            output_lines: Vec::new(),
        },
        bridge: None,
    }
}

/// A `chat.completion.chunk` `data:` line carrying a tool-call
/// fragment — `id`/`name` on the first fragment of a call, bare
/// `arguments` fragments afterwards.
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

/// A chat-completions transcript: one tool turn for `(index, id,
/// name, arguments)` with `arguments` split into two fragments so
/// accumulation is exercised.
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

/// The next `AskRequest` — fails on anything else so the event
/// ordering shows in the panic.
fn next_ask(session: &AgentSession) -> (String, String, Vec<String>) {
    match next(session) {
        AgentEvent::AskRequest {
            call,
            question,
            choices,
        } => (call.id, question, choices),
        other => panic!("expected an AskRequest, got {other:?}"),
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

/// A scripted `ask_user_question` call emits its own event — the
/// question and its choices — and NO `ApprovalRequest`: the card is
/// the ask, exempt from the gate by design (us-10). The chosen
/// answer re-enters as the call's tool result in the next request.
#[test]
fn a_multiple_choice_question_parks_on_its_own_card_and_the_answer_reenters() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "ask_user_question",
            r#"{"question":"Which approach?","choices":["rewrite","patch"]}"#,
        )),
        sse(&chat_text_turn("on it")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, question, choices) = next_ask(&session);
    assert_eq!(id, "call_1");
    assert_eq!(question, "Which approach?");
    assert_eq!(choices, vec!["rewrite", "patch"]);

    session.answer(&id, "patch".to_string()).expect("answer");

    let events = collect(&session);
    // The card resolves `Done` with the answer previewed — and no
    // `ApprovalRequest` ever wrapped the call.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Done,
                detail: Some(detail),
                ..
            } if call_id == "call_1" && detail == "patch"
        )),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ApprovalRequest { .. })),
        "the ask card must not produce an approve card: {events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The answer IS the tool result — proven on the wire.
    fixture.captured();
    let second = fixture.captured();
    assert_post(&second);
    assert!(
        second.body.contains("\"tool_call_id\":\"call_1\""),
        "{}",
        second.body
    );
    assert!(second.body.contains("patch"), "{}", second.body);
    fixture.assert_no_more_requests();
}

/// A call with no `choices` asks a free-text question — `choices`
/// arrives empty and the typed answer travels the same channel.
#[test]
fn a_question_without_choices_takes_a_free_text_answer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "ask_user_question",
            r#"{"question":"What should the command be?"}"#,
        )),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, question, choices) = next_ask(&session);
    assert_eq!(question, "What should the command be?");
    assert!(choices.is_empty(), "free-text asks carry no choices");

    session
        .answer(&id, "cargo clippy --fix".to_string())
        .expect("answer");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("cargo clippy --fix"),
        "{}",
        second.body
    );
}

/// A call the model garbled — valid JSON but no string `question` —
/// is a tool error, not a wedge: the card never parks, the error
/// becomes the call's result and the loop carries on.
#[test]
fn an_ask_without_a_question_is_a_tool_error_not_a_park() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "ask_user_question",
            r#"{"choices":["a","b"]}"#,
        )),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url),
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
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::AskRequest { .. } | AgentEvent::ApprovalRequest { .. }
        )),
        "a malformed ask parks on nothing: {events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("question"), "{}", second.body);
}

/// Esc resolves an ask wait exactly like a gate wait: `cancel()`
/// while the card is parked ends the task with `Cancelled` (us-9 /
/// the ticket's "ignoring the card blocks nothing else" — the user
/// can always walk away).
#[test]
fn esc_cancels_while_the_ask_card_waits() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![sse(&chat_tool_turn(
        0,
        "call_1",
        "ask_user_question",
        r#"{"question":"Proceed?"}"#,
    ))]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let _ = next_ask(&session);
    session.cancel();
    match session.events().recv_timeout(BOUND) {
        Ok(AgentEvent::Cancelled) => {}
        other => panic!("expected Cancelled, got {other:?}"),
    }
}

/// An answer addressed to a different call is dropped — a stale
/// answer can never resolve the ask the loop is parked on (the
/// stale-verdict rule, verbatim).
#[test]
fn a_stale_answer_cannot_resolve_the_ask() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(
            0,
            "call_1",
            "ask_user_question",
            r#"{"question":"Proceed?"}"#,
        )),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let _ = next_ask(&session);
    session
        .answer("call_other", "yes".to_string())
        .expect("stale answer");
    // Well past the poll cadence — still parked.
    assert!(
        session.events().recv_timeout(ASK_GRACE * 3).is_err(),
        "a stray answer resolved the ask"
    );

    session.answer("call_1", "yes".to_string()).expect("answer");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("yes"), "{}", second.body);
}
