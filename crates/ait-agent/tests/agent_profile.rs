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
/// `[agents.execution_profiles.default]` table — the T1 knob the
/// tests turn.
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

/// `agent_decides` and `ask_on_first_write` degrade to `always_ask`
/// in T1 — an unflagged call is an unsafe call, and the first-write
/// memory lands in T5. Both calls park on the approve card.
#[test]
fn agent_decides_and_ask_on_first_write_ask_every_call() {
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

    // Even a self-flagged "safe" call asks — the flag is T2's.
    let (id, name, _) = next_approval(&session);
    assert_eq!((id.as_str(), name.as_str()), ("call_1", "read_files"));
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    // `ask_on_first_write` asks too — deny it so no PTY spawns.
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
}

/// Zero declared profiles — a `default` that was never written or
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
        second.body.contains("no live shell bound to run commands in"),
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
        sse(&chat_tool_turn(&[(0, "call_1", "run_command", r#"{"command":"ls"}"#)])),
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
        second.body.contains("no live shell bound to run commands in"),
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
