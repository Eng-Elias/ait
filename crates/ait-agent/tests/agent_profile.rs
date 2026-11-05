//! The §5.7 permission engine at the `AgentSession` channel seam
//! (Spec R07 testing decisions / issue #96): scripted transcripts
//! emit tool calls under an execution profile and the assertions are
//! the emitted [`AgentEvent`]s plus the fixture's captured request
//! bodies — an `always_allow` call produces no `ApprovalRequest`,
//! a `never` call's result item carries the profile's denial text,
//! and a missing profile falls back to the Round 6 ask-everything
//! gate — all against the in-process `tiny_http` SSE fixture
//! (`ait_llm::testing`, `test-fixtures` feature; standard 11).

use std::time::Duration;

use ait_agent::{
    AgentContext, AgentEvent, AgentRequest, AgentSession, ApprovalVerdict, ToolStatus,
};
use ait_config::{SecretStore, Settings};
use ait_llm::testing::{assert_post, sse, Fixture, Script};

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
/// `[agents.execution_profiles.default]` table — the T1 knob the
/// tests turn.
fn settings(base_url: &str, profile_fields: &str) -> Settings {
    settings_on_schema(base_url, "openai_chat_completions", profile_fields)
}

/// [`settings`] against an explicit wire schema — the `safety`
/// injection must hold under all three endpoint IRs (R07-T2).
fn settings_on_schema(base_url: &str, schema: &str, profile_fields: &str) -> Settings {
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
{profile_fields}
"#
    ))
    .expect("test settings must parse")
}

/// A `settings.toml` snapshot with NO `execution_profiles` table —
/// `resolve_model`'s implicit-default rule still binds the sole
/// model, so the task runs while the engine sees no profile at all.
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
        prior: Vec::new(),
        bridge: None,
        skills: ait_agent::SkillsCatalog::default(),
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
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        json_string(text)
    )
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

/// One tool turn followed by a text end turn, in `schema`'s wire
/// dialect — the two-request script the `safety`-schema tests run
/// per endpoint IR.
fn tool_then_text(schema: &str, call: (u32, &str, &str, &str), text: &str) -> Vec<Script> {
    let (index, id, name, args) = call;
    match schema {
        "openai_responses" => vec![
            sse(&responses_tool_turn("fc_1", id, name, args)),
            sse(&responses_text_turn(text)),
        ],
        "anthropic_messages" => vec![
            sse(&anthropic_tool_turn(index, id, name, args)),
            sse(&anthropic_text_turn(text)),
        ],
        _ => vec![
            sse(&chat_tool_turn(&[(index, id, name, args)])),
            sse(&chat_text_turn(text)),
        ],
    }
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

/// The next `AskRequest` — fails on anything else.
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

/// Drain card transitions until the next `ApprovalRequest` — the
/// read for tests where completed calls' updates interleave with
/// the next gate.
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

/// Drain card transitions until the next ungated `ToolCall` — the
/// read for profile-allowed calls, whose updates interleave with
/// the previous call's transitions.
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

/// `true` when no event in `events` is an `ApprovalRequest`.
fn no_approval(events: &[AgentEvent]) -> bool {
    !events
        .iter()
        .any(|e| matches!(e, AgentEvent::ApprovalRequest { .. }))
}

/// A tempdir holding one readable file, bound as the task cwd.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("note.txt"), "NOTE-CONTENTS").expect("note");
    dir
}

/// `read_files = "always_allow"` runs `read_files`, `grep` and
/// `glob` with no `ApprovalRequest` — the shared `read_files` level
/// governs all three read-only tools — and each result re-enters as
/// the schema's own tool-result item.
#[test]
fn always_allow_runs_the_read_tools_without_asking() {
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
            "grep",
            r#"{"pattern":"NOTE","path":"."}"#,
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
            settings(&fixture.base_url, "read_files = \"always_allow\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // Every call opens an already-running card — no approve card
    // ever appears, and no verdict is ever sent.
    assert_eq!(until_tool_call(&session), "call_1");
    assert_eq!(until_tool_call(&session), "call_2");
    assert_eq!(until_tool_call(&session), "call_3");
    let events = collect(&session);
    assert!(
        no_approval(&events),
        "always_allow must never ask: {events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // Each result reached the model in the next request body.
    fixture.captured();
    let second = fixture.captured();
    assert_post(&second);
    assert!(second.body.contains("NOTE-CONTENTS"), "{}", second.body);
    let third = fixture.captured();
    assert!(third.body.contains("note.txt"), "{}", third.body);
    fixture.captured();
    fixture.assert_no_more_requests();
}

/// An explicit `always_ask` parks on the unchanged three-option
/// approve card — the profile asks exactly what the interim gate
/// asked.
#[test]
fn always_ask_parks_on_the_approve_card() {
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
            settings(&fixture.base_url, "read_files = \"always_ask\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, name, summary) = next_approval(&session);
    assert_eq!(
        (name.as_str(), summary.as_str()),
        ("read_files", "note.txt")
    );
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("NOTE-CONTENTS"), "{}", second.body);
}

/// `never` refuses the call outright: the tool stays declared, the
/// card opens then flips `Denied`, no `Running` update ever lands,
/// and the `"denied by execution profile <name>"` text reaches the
/// model as the call's tool result.
#[test]
fn never_denies_the_call_and_the_denial_reaches_the_model() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "run_command",
            r#"{"command":"rm -rf note.txt"}"#,
        )])),
        sse(&chat_text_turn("understood, switching approach")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "execute_commands = \"never\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // No approve card: the profile's verdict is `Deny`, so the call
    // opens a plain card…
    match next(&session) {
        AgentEvent::ToolCall { call, .. } => {
            assert_eq!(
                (call.id.as_str(), call.name.as_str()),
                ("call_1", "run_command")
            )
        }
        other => panic!("expected the denied call's card, got {other:?}"),
    }
    let events = collect(&session);
    // …which the `Denied` update flips immediately — and nothing
    // ever ran (no `Running` update, no approval was awaited).
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Denied,
                detail: Some(detail),
                ..
            } if call_id == "call_1" && detail == "denied by execution profile default"
        )),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                status: ToolStatus::Running,
                ..
            }
        )),
        "a denied call never runs: {events:?}"
    );
    assert!(no_approval(&events));
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The refusal text IS the tool result in the next request — and
    // `run_command` stayed in the declared tool list so the model
    // reads why it was refused.
    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("denied by execution profile default"),
        "{}",
        second.body
    );
    assert!(
        second.body.contains("\"name\":\"run_command\""),
        "never keeps the tool declared: {}",
        second.body
    );
    assert!(
        !second.body.contains("no live shell"),
        "the call never reached the executor: {}",
        second.body
    );
}

/// `ask_user_question` at `never` returns the profile denial like
/// every other tool — its ask card never appears.
#[test]
fn never_on_ask_user_question_returns_the_profile_denial() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "ask_user_question",
            r#"{"question":"Proceed?"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "ask_user_question = \"never\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Denied,
                detail: Some(detail),
                ..
            } if call_id == "call_1" && detail == "denied by execution profile default"
        )),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::AskRequest { .. })),
        "a never ask never parks on its card: {events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("denied by execution profile default"),
        "{}",
        second.body
    );
}

/// `ask_user_question` at a non-`never` level behaves exactly as it
/// did under the interim gate — its own card IS the ask — even when
/// the profile names an explicit level for it.
#[test]
fn ask_user_question_keeps_its_own_card_at_a_non_never_level() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "ask_user_question",
            r#"{"question":"Which approach?","choices":["rewrite","patch"]}"#,
        )])),
        sse(&chat_text_turn("on it")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "ask_user_question = \"always_ask\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, question, choices) = next_ask(&session);
    assert_eq!(id, "call_1");
    assert_eq!(question, "Which approach?");
    assert_eq!(choices, vec!["rewrite", "patch"]);

    session.answer(&id, "patch".to_string()).expect("answer");
    let events = collect(&session);
    assert!(no_approval(&events));
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("patch"), "{}", second.body);
}

/// `agent_decides` is real now (T2): a `"safe"`-flagged `read_files`
/// runs with no approve card at all, while `ask_on_first_write`
/// still degrades to `always_ask` — its first-write memory lands in
/// T5, so the `write_to_pty` call parks.
#[test]
fn agent_decides_safe_runs_while_ask_on_first_write_still_asks() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"],"safety":"safe"}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "write_to_pty",
            r#"{"input":"q"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(
                &fixture.base_url,
                "read_files = \"agent_decides\"\nwrite_to_pty = \"ask_on_first_write\"",
            ),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // The self-flagged "safe" call runs — a `ToolCall` event (card
    // straight to running), never an `ApprovalRequest`.
    assert_eq!(until_tool_call(&session), "call_1");

    // `ask_on_first_write` asks — deny it so no PTY spawns.
    let (id, name, _) = until_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_2", "write_to_pty"));
    session
        .approve(&id, ApprovalVerdict::Deny("not now".into()))
        .expect("deny");

    let events = collect(&session);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Denied,
                ..
            } if call_id == "call_2"
        )),
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The flag rode the call's own arguments verbatim into the
    // recorded assistant-call item — the self-assessment is auditable
    // in the transcript (us-10).
    fixture.captured();
    let second = fixture.captured();
    let body: serde_json::Value = serde_json::from_str(&second.body).expect("request body");
    let arguments = body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find_map(|m| {
            serde_json::from_str::<serde_json::Value>(
                m["tool_calls"][0]["function"]["arguments"].as_str()?,
            )
            .ok()
        })
        .expect("the recorded read_files call");
    assert_eq!(arguments["safety"], "safe");
    assert_eq!(arguments["paths"][0], "note.txt");
}

/// `agent_decides`'s flag is opt-in, never opt-out: `"unsafe"`, an
/// absent flag and a non-string flag all park on the unchanged
/// three-option approve card.
#[test]
fn agent_decides_unsafe_absent_and_malformed_flags_ask() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "read_files",
            r#"{"paths":["note.txt"],"safety":"unsafe"}"#,
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
            "read_files",
            r#"{"paths":["note.txt"],"safety":true}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "read_files = \"agent_decides\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    for call_id in ["call_1", "call_2", "call_3"] {
        let (id, name, _) = until_approval(&session);
        assert_eq!((id.as_str(), name.as_str()), (call_id, "read_files"));
        session
            .approve(&id, ApprovalVerdict::AllowOnce)
            .expect("approve");
    }
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// `agent_decides` works on every field the level maps to: a
/// `"safe"`-flagged `edit_files` writes ungated under
/// `apply_code_diffs`, and a `"safe"`-flagged `run_command` under
/// `execute_commands` reaches the executor without asking — the
/// detached tier answers its clean "no live shell" tool error,
/// proving the call ran rather than parking.
#[test]
fn agent_decides_governs_every_mapped_field() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "edit_files",
            r#"{"files":[{"path":"made.txt","content":"MADE"}],"safety":"safe"}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "run_command",
            r#"{"command":"echo hi","safety":"safe"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(
                &fixture.base_url,
                "apply_code_diffs = \"agent_decides\"\nexecute_commands = \"agent_decides\"",
            ),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    assert_eq!(until_tool_call(&session), "call_1");
    assert_eq!(until_tool_call(&session), "call_2");
    let events = collect(&session);
    assert!(
        no_approval(&events),
        "safe-flagged calls never ask: {events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The edit actually landed — ungated.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("made.txt")).expect("made.txt"),
        "MADE"
    );
    // And `run_command` reached the (detached) executor — its clean
    // "no live shell" error re-entered as the tool result.
    fixture.captured();
    fixture.captured();
    let third = fixture.captured();
    assert!(third.body.contains("no live shell"), "{}", third.body);
}

/// The injected `safety` schema rides only `agent_decides` tools —
/// under all three wire schemas the governed tools declare it
/// required while the others ship unchanged — and the flag the model
/// sends back is recorded verbatim in the assistant-call item.
#[test]
fn safety_schema_ships_only_on_agent_decides_tools_per_schema() {
    let governed = ["read_files", "grep", "glob"];
    for schema in [
        "openai_chat_completions",
        "openai_responses",
        "anthropic_messages",
    ] {
        let dir = project();
        let fixture = Fixture::serve(tool_then_text(
            schema,
            (
                0,
                "call_1",
                "read_files",
                r#"{"paths":["note.txt"],"safety":"safe"}"#,
            ),
            "done",
        ));
        let session = session();
        session
            .submit(task(
                settings_on_schema(&fixture.base_url, schema, "read_files = \"agent_decides\""),
                dir.path().to_str().expect("utf-8 tempdir"),
            ))
            .expect("submit");

        // The safe-flagged call runs ungated under every wire schema.
        assert_eq!(until_tool_call(&session), "call_1", "{schema}");
        let events = collect(&session);
        assert_eq!(events.last(), Some(&AgentEvent::Done), "{schema}");

        // Request 1: the declared tool schemas — `safety` is a
        // required enum on exactly the governed tools.
        let first = fixture.captured();
        let body: serde_json::Value = serde_json::from_str(&first.body).expect("request body");
        for tool in body["tools"].as_array().expect("tools") {
            // `openai_chat_completions` nests name/description/
            // parameters under `function`; `anthropic_messages`
            // calls the parameter schema `input_schema`.
            let (name, parameters) = match tool.get("function") {
                Some(function) => (
                    function["name"].as_str().expect("name"),
                    &function["parameters"],
                ),
                None => (
                    tool["name"].as_str().expect("name"),
                    tool.get("input_schema").unwrap_or(&tool["parameters"]),
                ),
            };
            if governed.contains(&name) {
                assert_eq!(
                    parameters["properties"]["safety"]["enum"],
                    serde_json::json!(["safe", "unsafe"]),
                    "{schema}/{name}"
                );
                assert!(
                    parameters["required"]
                        .as_array()
                        .expect("required")
                        .contains(&serde_json::json!("safety")),
                    "{schema}/{name}"
                );
            } else {
                assert_eq!(
                    parameters["properties"]["safety"],
                    serde_json::Value::Null,
                    "{schema}/{name} must not gain the flag"
                );
            }
        }

        // Request 2: the recorded assistant-call item carries the
        // flag verbatim inside the call's own arguments — auditable
        // in the transcript (us-10).
        let second = fixture.captured();
        let recorded = recorded_safety(schema, &second.body);
        assert_eq!(recorded.as_deref(), Some("safe"), "{schema}");
        fixture.assert_no_more_requests();
    }
}

/// The `safety` flag inside the recorded assistant-call item, per
/// wire schema — `arguments` is a nested JSON string on the OpenAI
/// IRs, a decoded `input` object on `anthropic_messages`.
fn recorded_safety(schema: &str, body: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(body).expect("request body");
    let flag = |arguments: &serde_json::Value| arguments["safety"].as_str().map(str::to_owned);
    match schema {
        "openai_responses" => body["input"].as_array()?.iter().find_map(|item| {
            if item["type"] != "function_call" {
                return None;
            }
            let arguments =
                serde_json::from_str::<serde_json::Value>(item["arguments"].as_str()?).ok()?;
            flag(&arguments)
        }),
        "anthropic_messages" => body["messages"].as_array()?.iter().find_map(|message| {
            message["content"].as_array()?.iter().find_map(|block| {
                if block["type"] != "tool_use" {
                    return None;
                }
                flag(&block["input"])
            })
        }),
        _ => body["messages"].as_array()?.iter().find_map(|message| {
            message["tool_calls"].as_array()?.iter().find_map(|call| {
                let arguments = serde_json::from_str::<serde_json::Value>(
                    call["function"]["arguments"].as_str()?,
                )
                .ok()?;
                flag(&arguments)
            })
        }),
    }
}

/// A profile with no `agent_decides` fields ships the same
/// declarations the registry always wrote — no tool gains a
/// `safety` property or the contract sentence.
#[test]
fn a_profile_without_agent_decides_ships_identical_declarations() {
    let dir = project();
    let fixture = Fixture::serve(vec![sse(&chat_text_turn("done"))]);
    let session = session();
    session
        .submit(task(
            settings(
                &fixture.base_url,
                "read_files = \"always_allow\"\nexecute_commands = \"always_ask\"",
            ),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let captured = fixture.captured();
    let body: serde_json::Value = serde_json::from_str(&captured.body).expect("request body");
    for tool in body["tools"].as_array().expect("tools") {
        let function = &tool["function"];
        assert_eq!(
            function["parameters"]["properties"]["safety"],
            serde_json::Value::Null,
            "{}",
            function["name"]
        );
        assert!(
            !function["description"]
                .as_str()
                .expect("description")
                .contains("self-assessment"),
            "{}",
            function["name"]
        );
    }
}

/// Zero declared profiles — a `default` that was never written or
/// `write_to_pty = "ask_on_first_write"` — the §5.7 headline (us-11):
/// the task's first write parks on the approve card; *allow once* is
/// the interactive session's consent, so the second write runs with
/// a `Running` card and no ask — and the bytes really reach the live
/// PTY the first write spawned.
#[test]
fn write_to_pty_first_write_asks_then_the_session_runs_free() {
    // Same shell pin as the R06 round-trip test: bash's `OSC 133`
    // marks give the settle its deterministic path.
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
            settings(&fixture.base_url, "write_to_pty = \"ask_on_first_write\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // The first write parks — nothing spawns until consent lands.
    let (id, name, _) = next_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_1", "write_to_pty"));
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // The second write opens an already-running card — no ask.
    assert_eq!(until_tool_call(&session), "call_2");
    let events = collect(&session);
    assert!(
        no_approval(&events),
        "one consent covers the session: {events:?}"
    );
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

    // `READ-hello` is the shell's answer to the second call's input —
    // the session the first write consented to really kept running.
    fixture.captured();
    fixture.captured();
    let third = fixture.captured();
    assert!(third.body.contains("READ-hello"), "{}", third.body);
}

/// A deny on the first write answers that call only — the next write
/// asks again (us-12: one refusal doesn't lock the model out), and
/// the allow that lands then covers the rest of the task.
#[test]
fn a_denied_first_write_asks_again_on_the_next_call() {
    if std::path::Path::new("/bin/bash").exists() {
        std::env::set_var("SHELL", "/bin/bash");
    }
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[
            (0, "call_1", "write_to_pty", r#"{"input":"rm x\n"}"#),
            (1, "call_2", "write_to_pty", r#"{"input":"echo HI\n"}"#),
        ])),
        sse(&chat_tool_turn(&[(
            0,
            "call_3",
            "write_to_pty",
            r#"{"input":"echo MORE\n"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "write_to_pty = \"ask_on_first_write\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // The first write asks and is refused — that call only.
    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::Deny("not that".into()))
        .expect("deny");

    // The very next write asks again — a deny is not a lockout. On
    // the way to the re-ask, the refused call's card flips `Denied`
    // with the user's reason.
    let mut denied_card = false;
    let (id, name) = loop {
        match next(&session) {
            AgentEvent::ApprovalRequest { call, .. } => break (call.id, call.name),
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Denied,
                detail,
                ..
            } => {
                assert_eq!(call_id, "call_1");
                assert_eq!(detail.as_deref(), Some("not that"));
                denied_card = true;
            }
            event @ (AgentEvent::Done
            | AgentEvent::Cancelled
            | AgentEvent::BudgetReached
            | AgentEvent::Error(_)) => panic!("expected an ApprovalRequest, got {event:?}"),
            _ => {}
        }
    };
    assert!(denied_card, "call_1's card rendered its Denied update");
    assert_eq!((id.as_str(), name.as_str()), ("call_2", "write_to_pty"));
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // …and the allow covers the rest of the task — call_3 runs free.
    assert_eq!(until_tool_call(&session), "call_3");
    let events = collect(&session);
    assert!(no_approval(&events), "{events:?}");
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The denial text reached the model beside the allowed call's
    // result in the same turn's results.
    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("not that"), "{}", second.body);
}

/// The first-write semantics are uniform across fields (us-12) — not
/// write-specific: on `read_files`, the field's first `read_files`
/// call asks and its allow covers `read_files`, while `grep` — a
/// different tool on the same field — asks its own first ask. The
/// memory stays per tool name, never per field, never blanket.
#[test]
fn ask_on_first_write_is_uniform_across_fields() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[
            (0, "call_1", "read_files", r#"{"paths":["note.txt"]}"#),
            (1, "call_2", "read_files", r#"{"paths":["note.txt"]}"#),
        ])),
        sse(&chat_tool_turn(&[(
            0,
            "call_3",
            "grep",
            r#"{"pattern":"NOTE","path":"."}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "read_files = \"ask_on_first_write\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // First gated read_files call asks; *allow once* covers it…
    let (id, name, _) = next_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_1", "read_files"));
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // …so the second read_files call — same tool, same turn — runs
    // without asking.
    assert_eq!(until_tool_call(&session), "call_2");

    // `grep` rides the same profile field but is a different tool —
    // its first call asks its own first-write ask.
    let (id, name, _) = until_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_3", "grep"));
    session
        .approve(&id, ApprovalVerdict::AllowForTask)
        .expect("approve");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// The first-write memory is per task (us-11): a new task asks on
/// first write again — consent never leaks across task boundaries.
#[test]
fn a_new_task_asks_on_first_write_again() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        // Task one: first write asks and is allowed, then it ends.
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "write_to_pty",
            r#"{"input":"q"}"#,
        )])),
        sse(&chat_text_turn("task one done")),
        // Task two: the first write asks again — deny it so no more
        // writes run.
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "write_to_pty",
            r#"{"input":"q"}"#,
        )])),
        sse(&chat_text_turn("task two done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "write_to_pty = \"ask_on_first_write\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowForTask)
        .expect("approve");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // A fresh task on the same session gets a fresh memory.
    session
        .submit(task(
            settings(&fixture.base_url, "write_to_pty = \"ask_on_first_write\""),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");
    let (id, name, _) = next_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_2", "write_to_pty"));
    session
        .approve(&id, ApprovalVerdict::Deny("not now".into()))
        .expect("deny");
    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// The deny floor composes with the flag: in one profile,
/// `execute_commands = "never"` refuses its call outright — card
/// `Denied`, no approve card, floor first in the decision order —
/// while `write_to_pty`'s first-write ask still parks.
#[test]
fn the_deny_floor_still_refuses_alongside_a_first_write_ask() {
    if std::path::Path::new("/bin/bash").exists() {
        std::env::set_var("SHELL", "/bin/bash");
    }
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[
            (0, "call_1", "run_command", r#"{"command":"rm note.txt"}"#),
            (1, "call_2", "write_to_pty", r#"{"input":"echo HI\n"}"#),
        ])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(
                &fixture.base_url,
                "execute_commands = \"never\"\nwrite_to_pty = \"ask_on_first_write\"",
            ),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // The `never` call never reaches an approve card: its card opens
    // then flips `Denied` with the profile's refusal.
    match next(&session) {
        AgentEvent::ToolCall { call, .. } => {
            assert_eq!(
                (call.id.as_str(), call.name.as_str()),
                ("call_1", "run_command")
            )
        }
        other => panic!("expected the denied call's card, got {other:?}"),
    }
    match next(&session) {
        AgentEvent::ToolUpdate {
            call_id,
            status: ToolStatus::Denied,
            detail: Some(detail),
            ..
        } => {
            assert_eq!(call_id, "call_1");
            assert_eq!(detail, "denied by execution profile default");
        }
        other => panic!("expected the Denied update, got {other:?}"),
    }

    // The first write still asks — the floor only touches `never`.
    let (id, name, _) = next_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_2", "write_to_pty"));
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    let events = collect(&session);
    assert!(no_approval(&events), "{events:?}");
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// was removed — evaluates every tool as `always_ask`: the Round 6
/// interim gate verbatim, fail-safe never silent.
#[test]
fn a_missing_profile_asks_everything() {
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
            "glob",
            r#"{"pattern":"*.txt"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings_without_profiles(&fixture.base_url),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // Every tool parks — even one that would be `always_allow`
    // under a stricter reading.
    let (id, name, _) = next_approval(&session);
    assert_eq!(name, "read_files");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    let (id, name, _) = until_approval(&session);
    assert_eq!(name, "glob");
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    fixture.captured();
    fixture.captured();
    let third = fixture.captured();
    assert!(third.body.contains("note.txt"), "{}", third.body);
}

/// A renamed `default` — other profiles exist but the name the
/// session resolves does not — fails closed at model resolution
/// (the existing `resolve_model` behavior), never silently.
#[test]
fn a_renamed_default_profile_errors_before_any_call() {
    let dir = project();
    let fixture = Fixture::serve(vec![sse(&chat_text_turn("unreachable"))]);
    let session = session();
    let renamed = Settings::from_toml(&format!(
        r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "{}"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "coding-key"

[agents.execution_profiles.work]
coding_model = "coding-key"
"#,
        fixture.base_url
    ))
    .expect("test settings must parse");
    session
        .submit(task(renamed, dir.path().to_str().expect("utf-8 tempdir")))
        .expect("submit");

    match session.events().recv_timeout(BOUND) {
        Ok(AgentEvent::Error(message)) => assert!(
            message.contains("unknown execution profile"),
            "unexpected error {message:?}"
        ),
        other => panic!("expected a terminal Error, got {other:?}"),
    }
    fixture.assert_no_more_requests();
}

/// A `command_denylist` hit refuses `run_command` exactly like a
/// `never` level — even at `always_allow` (Spec R07's order:
/// `never` → denylist → allowlist → level). The card opens then
/// flips `Denied`, nothing reaches the executor, and the
/// `"denied by execution profile <name>"` text is the tool result
/// the model reads in the next request.
#[test]
fn a_denylisted_command_denies_even_at_always_allow() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "run_command",
            r#"{"command":"rm -rf note.txt"}"#,
        )])),
        sse(&chat_text_turn("understood, using read_files instead")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(
                &fixture.base_url,
                "execute_commands = \"always_allow\"\ncommand_denylist = ['^rm']",
            ),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // No approve card: the profile's verdict is `Deny`, so the call
    // opens a plain card…
    match next(&session) {
        AgentEvent::ToolCall { call, .. } => {
            assert_eq!(
                (call.id.as_str(), call.name.as_str()),
                ("call_1", "run_command")
            )
        }
        other => panic!("expected the denied call's card, got {other:?}"),
    }
    let events = collect(&session);
    // …which the `Denied` update flips immediately — and nothing
    // ever ran.
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Denied,
                detail: Some(detail),
                ..
            } if call_id == "call_1" && detail == "denied by execution profile default"
        )),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                status: ToolStatus::Running,
                ..
            }
        )),
        "a denied call never runs: {events:?}"
    );
    assert!(no_approval(&events));
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The refusal text IS the tool result in the next request.
    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("denied by execution profile default"),
        "{}",
        second.body
    );
    assert!(
        !second.body.contains("no live shell"),
        "the call never reached the executor: {}",
        second.body
    );
}

/// A `command_allowlist` hit runs `run_command` without asking —
/// even at `always_ask` — opening a `Running` card straight to the
/// executor, whose result text re-enters the conversation as the
/// tool result.
#[test]
fn an_allowlisted_command_runs_without_asking() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "run_command",
            r#"{"command":"ls -la"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(
                &fixture.base_url,
                "execute_commands = \"always_ask\"\ncommand_allowlist = ['^ls']",
            ),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    assert_eq!(until_tool_call(&session), "call_1");
    let events = collect(&session);
    assert!(
        no_approval(&events),
        "an allowlisted command never parks on the card: {events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The call reached the executor — the no-bridge tier's
    // `Unbound` text is the tool result the second request carries.
    fixture.captured();
    let second = fixture.captured();
    assert!(
        second
            .body
            .contains("no live shell bound to run commands in"),
        "{}",
        second.body
    );
}

/// The denylist sits above the gate memory: after the user allowed
/// `run_command` for the rest of the task, a denylisted command
/// still refuses — no card ask, no run — while the unlisted
/// command that earned the memory ran ungated.
#[test]
fn the_denylist_beats_the_allow_for_task_memory() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "run_command",
            r#"{"command":"ls"}"#,
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "run_command",
            r#"{"command":"rm -rf note.txt"}"#,
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(
                &fixture.base_url,
                "execute_commands = \"always_ask\"\ncommand_denylist = ['^rm']",
            ),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // `ls` asks; the user allows `run_command` for the task…
    let (id, name, _) = next_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_1", "run_command"));
    session
        .approve(&id, ApprovalVerdict::AllowForTask)
        .expect("approve");

    // …yet the denylisted `rm` still refuses outright.
    let events = collect(&session);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Denied,
                detail: Some(detail),
                ..
            } if call_id == "call_2" && detail == "denied by execution profile default"
        )),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Running,
                ..
            } if call_id == "call_2"
        )),
        "a denied call never runs: {events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The first call ran (the executor's `Unbound` result rode the
    // second request); the refusal rode the third.
    fixture.captured();
    let second = fixture.captured();
    assert!(
        second
            .body
            .contains("no live shell bound to run commands in"),
        "the allow-for-task call ran ungated: {}",
        second.body
    );
    let third = fixture.captured();
    assert!(
        third.body.contains("denied by execution profile default"),
        "{}",
        third.body
    );
}

/// §5.7 `directory_allowlist` at the channel seam (R07-T4): the
/// file tools' confinement is the session cwd ∪ the profile's
/// listed roots. A `read_files` under the listed root runs and its
/// contents reach the model; an `edit_files` under the same root
/// still gates on the approve card — carrying its `/dev/null` diff
/// — and an allow verdict writes it; a path under neither root
/// answers the unchanged escape tool error. The allowlist widens
/// only what the tools may touch: verdicts, cards and diffs are
/// the same as inside the tree.
#[test]
fn directory_allowlist_widens_confinement_without_weakening_the_gate() {
    let dir = project();
    let shared = tempfile::tempdir().expect("shared dir");
    std::fs::write(shared.path().join("shared.txt"), "SHARED-CONTENTS").expect("shared");
    let outside = tempfile::tempdir().expect("outside dir");
    std::fs::write(outside.path().join("secret.txt"), "SECRET-OUTSIDE").expect("secret");
    let shared_file = shared.path().join("shared.txt");
    // Canonical spelling: the leaf does not exist, so confinement
    // compares its lexical form against the canonicalized root.
    let new_file = std::fs::canonicalize(shared.path())
        .expect("canonical shared")
        .join("fresh.txt");
    let secret = outside.path().join("secret.txt");
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "read_files",
            &serde_json::json!({"paths": [shared_file]}).to_string(),
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_2",
            "edit_files",
            &serde_json::json!({
                "files": [{"path": new_file, "content": "FRESH\n"}]
            })
            .to_string(),
        )])),
        sse(&chat_tool_turn(&[(
            0,
            "call_3",
            "read_files",
            &serde_json::json!({"paths": [secret]}).to_string(),
        )])),
        sse(&chat_text_turn("done")),
    ]);
    let session = session();
    session
        .submit(task(
            settings(
                &fixture.base_url,
                &format!(
                    "read_files = \"always_allow\"\n\
                     apply_code_diffs = \"always_ask\"\n\
                     directory_allowlist = [{}]",
                    json_string(shared.path().to_str().expect("utf-8 shared"))
                ),
            ),
            dir.path().to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    // `read_files` under the listed root: `always_allow` runs it
    // ungated, exactly as inside the tree.
    assert_eq!(until_tool_call(&session), "call_1");

    // `edit_files` under the listed root: `always_ask` still parks —
    // the allowlist never weakens consent — and the card carries the
    // `/dev/null` preview for the new file.
    let (id, name, summary) = until_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_2", "edit_files"));
    assert!(
        summary.starts_with("--- /dev/null\n+++ b/"),
        "new-file diff preview: {summary}"
    );
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // `read_files` under neither root: runs (the level allows it)
    // and fails inside confinement — the union's edge is the
    // unchanged escape tool error.
    assert_eq!(until_tool_call(&session), "call_3");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Failed,
                detail: Some(detail),
                ..
            } if call_id == "call_3" && detail.contains("outside the working directory")
        )),
        "call_3's card reads the confinement error: {events:?}"
    );
    // The allow verdict's plan applied — the write landed under the
    // listed root, not the session cwd.
    assert_eq!(
        std::fs::read_to_string(&new_file).expect("applied edit"),
        "FRESH\n"
    );

    // What reached the model: the listed tree's contents, the write
    // result, and the escape error — never the outside file's bytes.
    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("SHARED-CONTENTS"), "{}", second.body);
    let third = fixture.captured();
    assert!(third.body.contains("wrote"), "{}", third.body);
    let fourth = fixture.captured();
    assert!(
        fourth.body.contains("outside the working directory"),
        "{}",
        fourth.body
    );
    assert!(!fourth.body.contains("SECRET-OUTSIDE"), "{}", fourth.body);
    fixture.assert_no_more_requests();
}

/// Spec R07 us-22/26 — the request's `profile` field IS the active
/// profile (issue #101): a task submitted under `work` evaluates `work`'s
/// permission levels (`never` denies `run_command` where `default`'s
/// `always_ask` would have parked) and resolves `work`'s `coding_model`
/// to reach the model — the denial names the armed profile.
#[test]
fn the_request_profile_arms_its_own_stance() {
    let dir = project();
    let fixture = Fixture::serve(vec![
        sse(&chat_tool_turn(&[(
            0,
            "call_1",
            "run_command",
            r#"{"command":"rm -rf note.txt"}"#,
        )])),
        sse(&chat_text_turn("understood, switching approach")),
    ]);
    let settings = Settings::from_toml(&format!(
        r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "{}"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "coding-key"

[agents.execution_profiles.default]
coding_model = "coding-key"
execute_commands = "always_ask"

[agents.execution_profiles.work]
coding_model = "coding-key"
execute_commands = "never"
"#,
        fixture.base_url
    ))
    .expect("test settings must parse");
    let session = session();
    let mut request = task(settings, dir.path().to_str().expect("utf-8 tempdir"));
    request.profile = String::from("work");
    session.submit(request).expect("submit");

    // `work`'s `never` refused the call outright — `default`'s
    // `always_ask` would have parked an approve card instead.
    let events = collect(&session);
    assert!(
        events.iter().any(|e| matches!(
            e,
            AgentEvent::ToolUpdate {
                call_id,
                status: ToolStatus::Denied,
                detail: Some(detail),
                ..
            } if call_id == "call_1" && detail == "denied by execution profile work"
        )),
        "{events:?}"
    );
    assert!(no_approval(&events));
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // The denial text reached the model as the call's tool result.
    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("denied by execution profile work"),
        "{}",
        second.body
    );
}
