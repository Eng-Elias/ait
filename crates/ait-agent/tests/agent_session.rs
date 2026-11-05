//! Integration tests at the `AgentSession` channel seam (Spec R06
//! testing decisions): an [`AgentRequest`] in on one `mpsc` channel,
//! [`AgentEvent`]s out on the other — against the in-process
//! `tiny_http` SSE fixture ait-llm exports for tests
//! (`ait_llm::testing`, `test-fixtures` feature), so the full
//! HTTP+SSE+retry path runs with zero external network (standard 11).
//!
//! The fixture server speaks whichever schema the test scripts;
//! secrets resolve through a stub `SecretStore` (`NoSecrets`) — the
//! production path uses `OsKeyring`.

use std::sync::mpsc;
use std::time::Duration;

use ait_agent::{AgentContext, AgentEvent, AgentRequest, AgentSession};
use ait_config::{SecretStore, Settings};
use ait_llm::testing::{assert_post, error_status, sse, Fixture, Script};

/// Tests never touch a real keyring — a missing secret is `None`, so
/// fixture requests go out with no `Authorization` header.
struct NoSecrets;

impl SecretStore for NoSecrets {
    fn get(&self, _service: &str, _account: &str) -> Option<String> {
        None
    }
}

/// Bound on "the worker answered" — loopback is fast; a hang is a
/// failure, not a slow pass. Well under the adapters' 60 s stream
/// idle-timeout so the cancel test actually asserts *prompt*
/// termination.
const BOUND: Duration = Duration::from_secs(10);

fn session() -> AgentSession {
    AgentSession::with_secret_store(NoSecrets)
}

/// A `settings.toml` snapshot with one endpoint pointed at the
/// fixture, and profile `default` binding `role` (a `ModelRole` TOML
/// key) to the endpoint's model.
fn settings(base_url: &str, schema: &str, role_binding: &str) -> Settings {
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
{role_binding} = "coding-key"
"#
    ))
    .expect("test settings must parse")
}

/// Chat-completions `settings` — the common case.
fn chat_settings(base_url: &str, role_binding: &str) -> Settings {
    settings(base_url, "openai_chat_completions", role_binding)
}

/// Settings with profile `default` declared but no model roles bound —
/// `resolve_model`'s `Ok(None)` path.
fn unconfigured_settings() -> Settings {
    Settings::from_toml("[agents.execution_profiles.default]\n").expect("test settings must parse")
}

/// A task on `/home/u/proj` whose output tail carries a credential, so
/// captured request bodies can prove redaction ran in transit
/// (§5.10.3).
fn task(settings: Settings, cwd: &str) -> AgentRequest {
    AgentRequest {
        settings,
        profile: String::from("default"),
        task: String::from("find why the tests fail"),
        context: AgentContext {
            cwd: cwd.to_owned(),
            shell: String::from("bash"),
            os: String::from("linux"),
            output_lines: vec![
                String::from("$ cargo test"),
                String::from("AKIAIOSFODNN7EXAMPLE is my key"),
                String::from("test result: FAILED"),
            ],
        },
        prior: Vec::new(),
        bridge: None,
        skills: ait_agent::SkillsCatalog::default(),
    }
}

/// A JSON string literal (`"…"`) for hand-built chunk bodies.
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// One `chat.completion.chunk` `data:` line carrying a content delta.
fn sse_delta(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n",
        json_string(text)
    )
}

/// A complete `openai_chat_completions` SSE transcript streaming
/// `deltas` then the `[DONE]` sentinel.
fn sse_transcript(deltas: &[&str]) -> String {
    let mut body = deltas.iter().map(|d| sse_delta(d)).collect::<String>();
    body.push_str("data: [DONE]\n\n");
    body
}

/// A complete `openai_responses` SSE transcript streaming `deltas`
/// then a `response.completed`.
fn responses_transcript(deltas: &[&str]) -> String {
    let mut body = String::new();
    for delta in deltas {
        body.push_str(&format!(
            "event: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"delta\":{}}}\n\n",
            json_string(delta)
        ));
    }
    body.push_str(
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n",
    );
    body
}

/// Drain events until a terminal `Done`/`Cancelled`/`BudgetReached`/
/// `Error` or the bound — returning everything seen so the test
/// asserts the whole ordering, not just the tail.
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

/// The concatenated text of every `TextDelta` — what the panel would
/// render for the task.
fn delta_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TextDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A task produces ordered `TextDelta`s ending in `Done`, and the
/// assembled request really reached the endpoint.
#[test]
fn task_streams_ordered_deltas_then_done() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&["The tests ", "fail because"]))]);
    let session = session();
    session
        .submit(task(
            chat_settings(&fixture.base_url, "coding_model"),
            "/home/u/proj",
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(delta_text(&events), "The tests fail because");
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let captured = fixture.captured();
    assert_post(&captured);
    assert_eq!(captured.url, "/chat/completions");
    assert!(captured.body.contains("find why the tests fail"));
}

/// The system prompt carries the environment block, the nearest
/// `AGENTS.md` to cwd and the redacted output tail — and a
/// credential-looking string in the tail never reaches the wire
/// (§5.6, §5.10.3).
#[test]
fn system_prompt_carries_context_agents_md_and_redacted_tail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let project = dir.path().join("project").join("sub");
    std::fs::create_dir_all(&project).expect("mkdir");
    std::fs::write(
        dir.path().join("AGENTS.md"),
        "OUTER-AGENTS-MARKER must not appear",
    )
    .expect("outer AGENTS.md");
    std::fs::write(
        dir.path().join("project").join("AGENTS.md"),
        "INNER-AGENTS-MARKER guidance",
    )
    .expect("inner AGENTS.md");

    let fixture = Fixture::serve(vec![sse(&sse_transcript(&["ok"]))]);
    let session = session();
    session
        .submit(task(
            chat_settings(&fixture.base_url, "coding_model"),
            project.to_str().expect("utf-8 tempdir"),
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let body = fixture.captured().body;
    // Environment block (§5.6: cwd, OS, shell).
    assert!(body.contains(project.to_str().expect("utf-8")), "{body}");
    assert!(body.contains("bash"), "{body}");
    assert!(body.contains("linux"), "{body}");
    // The *nearest* AGENTS.md wins over one further up the tree.
    assert!(body.contains("INNER-AGENTS-MARKER"), "{body}");
    assert!(!body.contains("OUTER-AGENTS-MARKER"), "{body}");
    // The output tail is present — already redacted.
    assert!(body.contains("test result: FAILED"), "{body}");
    assert!(body.contains("[REDACTED]"), "{body}");
    assert!(!body.contains("AKIAIOSFODNN7EXAMPLE"), "{body}");
}

/// Spec R10-T1: the request's `[redaction]` settings drive the output
/// tail the same way — a `disable`d built-in's specimen reaches the
/// wire verbatim while an `enable`d PII category masks its specimen.
#[test]
fn redaction_settings_drive_the_output_tail() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&["ok"]))]);
    let session = session();
    let mut settings = chat_settings(&fixture.base_url, "coding_model");
    settings
        .redaction
        .disable
        .push(String::from("aws_access_key"));
    settings.redaction.enable.push(String::from("pii_email"));
    let mut task = task(settings, "/home/u/proj");
    task.context
        .output_lines
        .push(String::from("bounce alerts@example.com"));
    session.submit(task).expect("submit");

    let events = collect(&session);
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let body = fixture.captured().body;
    assert!(
        body.contains("AKIAIOSFODNN7EXAMPLE"),
        "a disabled built-in passes through: {body}"
    );
    assert!(
        !body.contains("alerts@example.com"),
        "an enabled PII category must mask: {body}"
    );
    assert!(body.contains("[REDACTED]"), "{body}");
}

/// §4.2: `coding_model` unset falls back to `base_model` — the task
/// still resolves and streams.
#[test]
fn coding_model_falls_back_to_base_model() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&["done"]))]);
    let session = session();
    session
        .submit(task(
            chat_settings(&fixture.base_url, "base_model"),
            "/home/u/proj",
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(delta_text(&events), "done");
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// An unconfigured `coding_model` (and no `base_model` fallback) is a
/// terminal `Error` event, never a wedge — and the worker serves the
/// next queued task.
#[test]
fn unconfigured_model_errors_terminally_and_worker_survives() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&["recovered"]))]);
    let session = session();
    session
        .submit(task(unconfigured_settings(), "/home/u/proj"))
        .expect("submit");

    match session.events().recv_timeout(BOUND) {
        Ok(AgentEvent::Error(message)) => assert!(
            message.contains("coding_model"),
            "unexpected error {message:?}"
        ),
        other => panic!("expected an Error event, got {other:?}"),
    }

    // The worker is still alive: a configured task streams normally.
    session
        .submit(task(
            chat_settings(&fixture.base_url, "coding_model"),
            "/home/u/proj",
        ))
        .expect("submit");
    let events = collect(&session);
    assert_eq!(delta_text(&events), "recovered");
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// The Esc path: `cancel()` during a stalled stream terminates the
/// in-flight request promptly with `Cancelled` — well under the
/// adapter's 60 s idle-timeout — and the worker then serves the next
/// queued task.
#[test]
fn cancel_stops_stream_promptly_and_worker_serves_next_task() {
    let (drip_tx, drip_rx) = mpsc::channel::<Vec<u8>>();
    let fixture = Fixture::serve(vec![
        Script::DripSse(drip_rx),
        sse(&sse_transcript(&["after cancel"])),
    ]);
    let session = session();
    session
        .submit(task(
            chat_settings(&fixture.base_url, "coding_model"),
            "/home/u/proj",
        ))
        .expect("submit");

    // Feed one >8KiB write (tiny_http buffers ~8 KiB — the `:`-comment
    // is spec-legal SSE padding), then hold the sender open: the
    // stream stalls mid-reply.
    let mut chunk = sse_delta("working on it").into_bytes();
    chunk.extend_from_slice(format!(": {}\n", "x".repeat(9 * 1024)).as_bytes());
    drip_tx.send(chunk).expect("drip feed");

    let first = session.events().recv_timeout(BOUND).expect("first event");
    assert_eq!(first, AgentEvent::TextDelta(String::from("working on it")));

    session.cancel();
    match session.events().recv_timeout(BOUND) {
        Ok(AgentEvent::Cancelled) => {}
        other => panic!("expected Cancelled, got {other:?}"),
    }
    drop(drip_tx);

    // The next task runs normally — cancellation poisoned nothing.
    session
        .submit(task(
            chat_settings(&fixture.base_url, "coding_model"),
            "/home/u/proj",
        ))
        .expect("submit");
    let events = collect(&session);
    assert_eq!(delta_text(&events), "after cancel");
    assert_eq!(events.last(), Some(&AgentEvent::Done));
}

/// A steering message queued mid-run is appended to the conversation
/// as a user message and reaches the model in the next request — it
/// never interrupts the in-flight stream.
#[test]
fn steering_queued_mid_run_reaches_the_model() {
    let (drip_tx, drip_rx) = mpsc::channel::<Vec<u8>>();
    let fixture = Fixture::serve(vec![
        Script::DripSse(drip_rx),
        sse(&sse_transcript(&["understood"])),
    ]);
    let session = session();
    session
        .submit(task(
            chat_settings(&fixture.base_url, "coding_model"),
            "/home/u/proj",
        ))
        .expect("submit");

    // Stall the first reply mid-stream, then queue steering while the
    // run is still in flight.
    let mut chunk = sse_delta("first reply").into_bytes();
    chunk.extend_from_slice(format!(": {}\n", "x".repeat(9 * 1024)).as_bytes());
    drip_tx.send(chunk).expect("drip feed");
    let first = session.events().recv_timeout(BOUND).expect("first event");
    assert_eq!(first, AgentEvent::TextDelta(String::from("first reply")));

    session
        .steer(String::from("STEERING-MARKER"))
        .expect("steer");

    // End the stream; the loop carries the steering into a second
    // request instead of finishing. (The first turn's delta was
    // already drained above, so only the second reply remains.)
    drop(drip_tx);
    let events = collect(&session);
    assert_eq!(delta_text(&events), "understood");
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let first_request = fixture.captured();
    assert!(first_request.body.contains("find why the tests fail"));
    assert!(!first_request.body.contains("STEERING-MARKER"));

    // The second request carries the conversation in the schema's own
    // IR: turn 1's assistant text, then the steering as a user message.
    let second_request = fixture.captured();
    assert!(second_request.body.contains("first reply"));
    assert!(second_request.body.contains("STEERING-MARKER"));
    assert!(second_request.body.contains("\"role\":\"user\""));
    fixture.assert_no_more_requests();
}

/// The request is assembled in the endpoint schema's own IR (§5.3) —
/// here `openai_responses`: top-level `instructions`, `input` items,
/// POST to `/responses`.
#[test]
fn request_is_assembled_in_the_schemas_own_ir() {
    let fixture = Fixture::serve(vec![sse(&responses_transcript(&["resp ", "ok"]))]);
    let session = session();
    session
        .submit(task(
            settings(&fixture.base_url, "openai_responses", "coding_model"),
            "/home/u/proj",
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(delta_text(&events), "resp ok");
    assert_eq!(events.last(), Some(&AgentEvent::Done));

    let captured = fixture.captured();
    assert_post(&captured);
    assert_eq!(captured.url, "/responses");
    assert!(
        captured.body.contains("\"instructions\""),
        "{}",
        captured.body
    );
    assert!(captured.body.contains("\"input\""), "{}", captured.body);
    assert!(captured.body.contains("find why the tests fail"));
}

/// A fixture-side HTTP failure surfaces as a terminal `Error` event —
/// never a panic, never a hang.
#[test]
fn endpoint_http_error_maps_to_terminal_error() {
    // 400 is terminal — the adapter's bounded retry skips it, so one
    // scripted response is enough.
    let fixture = Fixture::serve(vec![error_status(
        400,
        "{\"error\":{\"message\":\"bad request\"}}",
    )]);
    let session = session();
    session
        .submit(task(
            chat_settings(&fixture.base_url, "coding_model"),
            "/home/u/proj",
        ))
        .expect("submit");

    match session.events().recv_timeout(BOUND) {
        Ok(AgentEvent::Error(message)) => {
            assert!(message.contains("400"), "unexpected error {message:?}")
        }
        other => panic!("expected an Error event, got {other:?}"),
    }
}

/// A provider-reported error event mid-stream is a terminal `Error`
/// event — never a panic, never a hang.
#[test]
fn provider_error_event_maps_to_terminal_error() {
    let mut body = sse_delta("partial");
    body.push_str(
        "data: {\"error\":{\"message\":\"model overloaded\",\"code\":\"overloaded\"}}\n\n",
    );
    body.push_str("data: [DONE]\n\n");
    let fixture = Fixture::serve(vec![sse(&body)]);
    let session = session();
    session
        .submit(task(
            chat_settings(&fixture.base_url, "coding_model"),
            "/home/u/proj",
        ))
        .expect("submit");

    let events = collect(&session);
    assert_eq!(delta_text(&events), "partial");
    match events.last() {
        Some(AgentEvent::Error(message)) => {
            assert!(message.contains("model overloaded"), "{message:?}")
        }
        other => panic!("expected a terminal Error, got {other:?}"),
    }
}
