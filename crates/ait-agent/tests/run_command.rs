//! `run_command` at the `AgentSession` channel seam (Spec R06
//! testing decisions / issue #87): a task carrying a scripted
//! `ShellBridge` — no PTY — driven through the loop against the
//! in-process SSE fixture (`ait_llm::testing`). The bridge's
//! recorded traffic is the assertion for "no request bytes reach
//! the PTY before the approve card resolves", and the fixture's
//! captured request bodies prove the merged output + exit code
//! re-enter as the schema's own tool-result item.
//!
//! The `7701` responses are resolved the way `ait`'s `InBandExec`
//! resolves them: hand-built `A…R…B` frame streams fed through
//! ait-shell's real `RunAccumulator`.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ait_agent::{
    AgentContext, AgentEvent, AgentRequest, AgentSession, ApprovalVerdict, BridgeError,
    BridgeEvent, ShellBridge, ShellIntegration, ToolStatus,
};
use ait_config::{SecretStore, Settings};
use ait_llm::testing::{sse, Fixture};
use ait_shell::{RunAccumulator, RunResult, ShellEvent};

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
/// at the fixture URL.
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

/// A task bound to `bridge` — `None` is the detached-conversation
/// path (`run_command` answers a clean tool error).
fn task(settings: Settings, bridge: Option<Box<dyn ShellBridge>>) -> AgentRequest {
    AgentRequest {
        settings,
        task: String::from("run a command for me"),
        context: AgentContext {
            cwd: String::from("/tmp"),
            shell: String::from("bash"),
            os: String::from("linux"),
            output_lines: Vec::new(),
        },
        bridge,
    }
}

/// One complete `A … R×N … B` response stream for `payload` — the
/// events a well-formed `__ait_run` reply decodes to.
fn run_stream(payload: &[u8]) -> Vec<ShellEvent> {
    let mut events = vec![ShellEvent::RunBegin];
    for chunk in payload.chunks(500) {
        events.push(ShellEvent::RunChunk {
            payload: chunk.to_vec(),
        });
    }
    events.push(ShellEvent::RunEnd);
    events
}

/// What the scripted bridge recorded — the "did the executor reach
/// the shell" assertions.
#[derive(Clone, Default)]
struct Shares {
    /// The commands `run_in_band` was asked to run, in order.
    commands: Arc<Mutex<Vec<Vec<u8>>>>,
    /// A waiter per `run_in_band` request, in order — the test sends
    /// each request's `RunResult` on its channel.
    runs: Arc<Mutex<Vec<Sender<RunResult>>>>,
    /// Every raw `send_bytes` call — the in-band tier never uses it.
    sent: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// The scripted bridge behind a task's `AgentRequest::bridge` —
/// everything it sees lands in `shares`, which the test clones
/// before the bridge is boxed.
struct ScriptedBridge {
    state: ShellIntegration,
    shares: Shares,
}

impl ShellBridge for ScriptedBridge {
    fn integration_state(&self) -> Result<ShellIntegration, BridgeError> {
        Ok(self.state)
    }

    fn run_in_band(&self, command: &[u8]) -> Result<Receiver<RunResult>, BridgeError> {
        let (tx, rx) = channel();
        self.shares
            .commands
            .lock()
            .expect("commands lock")
            .push(command.to_vec());
        self.shares.runs.lock().expect("runs lock").push(tx);
        Ok(rx)
    }

    fn send_bytes(&self, bytes: &[u8]) -> Result<(), BridgeError> {
        self.shares
            .sent
            .lock()
            .expect("sent lock")
            .push(bytes.to_vec());
        Ok(())
    }

    fn subscribe(&self) -> Result<Receiver<BridgeEvent>, BridgeError> {
        Ok(channel().1)
    }

    fn grid_lines(&self, _start: i32, _end: i32) -> Result<Vec<String>, BridgeError> {
        Ok(Vec::new())
    }

    fn output_tail(&self) -> Result<Vec<u8>, BridgeError> {
        Ok(Vec::new())
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

/// A chat-completions tool turn calling `run_command` once with
/// `args` (JSON). `arguments` streams in one fragment — accumulation
/// is covered by the other tool tests.
fn run_command_turn(call_id: &str, args: &str) -> String {
    let mut body = sse_tool_fragment(0, Some((call_id, "run_command")), "");
    body.push_str(&sse_tool_fragment(0, None, args));
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

/// The next `ApprovalRequest` — fails on anything else.
fn next_approval(session: &AgentSession) -> (String, String, String) {
    match next(session) {
        AgentEvent::ApprovalRequest { call, summary } => (call.id, call.name, summary),
        other => panic!("expected an ApprovalRequest, got {other:?}"),
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

/// The central criterion (Spec R06, issue #87): `run_command` parks
/// on the approve card — NO request reaches the shell before the
/// verdict — then an allow runs it in-band and the merged output +
/// exit code re-enter as the schema's own tool-result item.
#[test]
fn run_command_gates_then_result_reenters_as_tool_item() {
    let bridge = ScriptedBridge {
        state: ShellIntegration::Integrated,
        shares: Shares::default(),
    };
    // The bridge records what reaches it; the test answers the run
    // once the verdict lands. `respond` needs the bridge after it's
    // boxed into the task — the shares handle sees everything anyway.
    let shares = bridge.shares.clone();
    let respond = move |payload: &[u8]| {
        let mut accumulator = RunAccumulator::new();
        let mut result = None;
        for event in run_stream(payload) {
            if let Some(done) = accumulator.feed(&event) {
                result = Some(done);
            }
        }
        shares
            .runs
            .lock()
            .expect("runs lock")
            .last()
            .expect("a pending run_in_band request")
            .send(result.expect("the scripted stream completes"))
            .expect("the executor still waits");
    };
    let commands = bridge.shares.clone();
    let reached = move || {
        !commands.commands.lock().expect("commands lock").is_empty()
            || !commands.sent.lock().expect("sent lock").is_empty()
    };

    let fixture = Fixture::serve(vec![
        sse(&run_command_turn("call_1", r#"{"command":"echo hi"}"#)),
        sse(&chat_text_turn("all done")),
    ]);
    let session = session();
    session
        .submit(task(settings(&fixture.base_url), Some(Box::new(bridge))))
        .expect("submit");

    let (id, name, summary) = next_approval(&session);
    assert_eq!(name, "run_command");
    assert_eq!(summary, "echo hi", "the card shows the command text");
    // The gate is parked — and nothing has reached the shell.
    assert!(!reached(), "no request bytes before approval");

    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");
    // The verdict releases the executor; the shell answers through
    // real `7701` frames once the request lands.
    let responder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        respond(b"0;MERGED-OUT;3");
    });

    let events = collect(&session);
    responder.join().expect("responder");
    assert_eq!(
        events[..2],
        [
            AgentEvent::ToolUpdate {
                call_id: "call_1".into(),
                status: ToolStatus::Running,
                detail: None,
            },
            AgentEvent::ToolUpdate {
                call_id: "call_1".into(),
                status: ToolStatus::Done,
                detail: Some("MERGED-OUT …".into()),
            },
        ],
        "{events:?}"
    );
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    // Request 2 carries the tool result in the schema's own IR: the
    // merged output AND the exit code, both model-readable.
    fixture.captured();
    let second = fixture.captured();
    let body = &second.body;
    assert!(body.contains("\"role\":\"tool\""), "{body}");
    assert!(body.contains("\"tool_call_id\":\"call_1\""), "{body}");
    assert!(body.contains("MERGED-OUT"), "{body}");
    assert!(body.contains("3"), "{body}");
    fixture.assert_no_more_requests();
}

/// *Deny* never touches the shell — the bridge records nothing — and
/// the denial text is the tool result the model reads (us-8).
#[test]
fn denied_run_command_never_reaches_the_shell() {
    let bridge = ScriptedBridge {
        state: ShellIntegration::Integrated,
        shares: Shares::default(),
    };
    let shares = bridge.shares.clone();

    let fixture = Fixture::serve(vec![
        sse(&run_command_turn("call_1", r#"{"command":"rm -rf x"}"#)),
        sse(&chat_text_turn("understood")),
    ]);
    let session = session();
    session
        .submit(task(settings(&fixture.base_url), Some(Box::new(bridge))))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::Deny("do not delete".into()))
        .expect("deny");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert!(
        shares.commands.lock().expect("commands lock").is_empty()
            && shares.sent.lock().expect("sent lock").is_empty(),
        "a denied call offered nothing to the bridge"
    );

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("do not delete"), "{}", second.body);
}

/// A `MarksOnly` session: the typed-fallback tier is R06-T5's — until
/// then the call answers a clean tool error the model can report,
/// and the bridge still sees nothing (standard 1's degrade, us-34's
/// clean-error rule applied to a state, not just an exit).
#[test]
fn marks_only_session_reports_a_clean_tool_error() {
    let bridge = ScriptedBridge {
        state: ShellIntegration::MarksOnly,
        shares: Shares::default(),
    };
    let shares = bridge.shares.clone();

    let fixture = Fixture::serve(vec![
        sse(&run_command_turn("call_1", r#"{"command":"echo hi"}"#)),
        sse(&chat_text_turn("noted")),
    ]);
    let session = session();
    session
        .submit(task(settings(&fixture.base_url), Some(Box::new(bridge))))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    let events = collect(&session);
    // The card reports `Failed` with a reason — never a hang.
    let failed = events.iter().find(|event| {
        matches!(
            event,
            AgentEvent::ToolUpdate {
                status: ToolStatus::Failed,
                ..
            }
        )
    });
    assert!(failed.is_some(), "{events:?}");
    assert_eq!(events.last(), Some(&AgentEvent::Done));
    assert!(
        shares.commands.lock().expect("commands lock").is_empty(),
        "a refused tier typed nothing"
    );

    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("MarksOnly"),
        "the model reads the state: {}",
        second.body
    );
}

/// No bridge at all — a detached conversation — is the §5.9 no-op
/// tier's answer: a clean tool error, not a task failure.
#[test]
fn no_bound_shell_is_a_clean_tool_error() {
    let fixture = Fixture::serve(vec![
        sse(&run_command_turn("call_1", r#"{"command":"echo hi"}"#)),
        sse(&chat_text_turn("noted")),
    ]);
    let session = session();
    session
        .submit(task(settings(&fixture.base_url), None))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    let events = collect(&session);
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::ToolUpdate {
                status: ToolStatus::Failed,
                ..
            }
        )
    }));
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    fixture.captured();
    let second = fixture.captured();
    assert!(
        second.body.contains("no live shell"),
        "the model reads the reason: {}",
        second.body
    );
}

/// The `cwd` argument can't be honored by the user-shell tier — the
/// run happens in the live shell's own cwd — so it is refused
/// explicitly (the model adjusts with `cd`) rather than silently
/// dropped.
#[test]
fn cwd_argument_is_refused_as_a_tool_error() {
    let bridge = ScriptedBridge {
        state: ShellIntegration::Integrated,
        shares: Shares::default(),
    };
    let shares = bridge.shares.clone();

    let fixture = Fixture::serve(vec![
        sse(&run_command_turn(
            "call_1",
            r#"{"command":"ls","cwd":"/var"}"#,
        )),
        sse(&chat_text_turn("ok")),
    ]);
    let session = session();
    session
        .submit(task(settings(&fixture.base_url), Some(Box::new(bridge))))
        .expect("submit");

    let (id, _, _) = next_approval(&session);
    session
        .approve(&id, ApprovalVerdict::AllowOnce)
        .expect("approve");

    let events = collect(&session);
    assert!(events.iter().any(|event| {
        matches!(
            event,
            AgentEvent::ToolUpdate {
                status: ToolStatus::Failed,
                ..
            }
        )
    }));
    assert!(
        shares.commands.lock().expect("commands lock").is_empty(),
        "a refused call reached the shell"
    );

    fixture.captured();
    let second = fixture.captured();
    assert!(second.body.contains("cwd"), "{}", second.body);
}
