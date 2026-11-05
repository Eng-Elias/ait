//! Integration tests at the `CommandSuggester` channel seam (Spec R04
//! testing decisions): a real request in on one `mpsc` channel, ordered
//! `Delta`s / `Done` / `Error` out on the other — against the in-process
//! `tiny_http` SSE fixture ait-llm exports for tests
//! (`ait_llm::testing`, `test-fixtures` feature), so the full
//! HTTP+SSE+retry+sanitize path runs with zero external network
//! (§6, standard 11).
//!
//! The fixture server speaks `openai_chat_completions`; secrets resolve
//! through a stub `SecretStore` (`NoSecrets`) — the production path uses
//! `OsKeyring`.

use std::sync::mpsc;
use std::time::Duration;

use ait_agent::{CommandSuggester, SuggestEvent, SuggestRequest};
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

fn suggester() -> CommandSuggester {
    CommandSuggester::with_secret_store(NoSecrets)
}

/// A `settings.toml` snapshot with one `openai_chat_completions`
/// endpoint pointed at the fixture, and profile `default` binding
/// `role` (a `ModelRole` TOML key) to the endpoint's model.
fn settings(base_url: &str, role_binding: &str) -> Settings {
    Settings::from_toml(&format!(
        r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "{base_url}"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "cmd-key"

[agents.execution_profiles.default]
{role_binding} = "cmd-key"
"#
    ))
    .expect("test settings must parse")
}

/// Settings with profile `default` declared but no model roles bound —
/// `resolve_model`'s `Ok(None)` path.
fn unconfigured_settings() -> Settings {
    Settings::from_toml("[agents.execution_profiles.default]\n").expect("test settings must parse")
}

/// The terminal-side context one request carries (us-15–17, 19); the
/// output lines include a credential so the captured request can prove
/// redaction ran in transit (§5.10.3).
fn request(settings: Settings) -> SuggestRequest {
    SuggestRequest {
        settings,
        profile: String::from("default"),
        cwd: String::from("/home/u/proj"),
        shell: String::from("bash"),
        os: String::from("linux"),
        output_lines: vec![
            String::from("$ aws s3 ls"),
            String::from("AKIAIOSFODNN7EXAMPLE is my key"),
            String::from("hi"),
        ],
        request: String::from("list files"),
    }
}

/// A JSON string literal (`"…"`) for the chunk body — the fixture
/// replies are built inline, so escaping is hand-rolled and minimal.
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

/// Drain events until a terminal `Done`/`Error` or the bound —
/// returning everything seen so the test asserts the whole ordering,
/// not just the tail.
fn collect(suggester: &CommandSuggester) -> Vec<SuggestEvent> {
    let mut events = Vec::new();
    loop {
        match suggester.events().recv_timeout(BOUND) {
            Ok(event @ (SuggestEvent::Done | SuggestEvent::Error(_))) => {
                events.push(event);
                return events;
            }
            Ok(event) => events.push(event),
            Err(_) => panic!("timed out waiting for a terminal event; got {events:?}"),
        }
    }
}

/// The concatenated text of every `Delta` — what the frontend would
/// have typed into the input line.
fn delta_text(events: &[SuggestEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            SuggestEvent::Delta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// us-3/4/5 + us-20: a request streams ordered sanitized `Delta`s
/// ending in `Done`, and the assembled request really reached the
/// endpoint — with output-line secrets already redacted (us-18).
#[test]
fn request_streams_ordered_deltas_then_done() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&["git ", "status", " --short"]))]);
    let suggester = suggester();
    suggester
        .suggest(request(settings(&fixture.base_url, "command_model")))
        .expect("suggest");

    let events = collect(&suggester);
    assert_eq!(delta_text(&events), "git status --short");
    assert_eq!(events.last(), Some(&SuggestEvent::Done));
    for event in &events {
        if let SuggestEvent::Delta(text) = event {
            assert!(!text.contains(['\n', '\r']), "line break in {text:?}");
        }
    }

    // The request the fixture captured proves the pipeline ran: POST to
    // the schema's path, the typed request verbatim, and the
    // output-line credential already redacted in transit.
    let captured = fixture.captured();
    assert_post(&captured);
    assert_eq!(captured.url, "/chat/completions");
    assert!(captured.body.contains("list files"));
    assert!(captured.body.contains("[REDACTED]"));
    assert!(!captured.body.contains("AKIAIOSFODNN7EXAMPLE"));
}

/// Spec R10-T1: the request's `[redaction]` settings reach the
/// suggester's context assembly — a `disable`d built-in's specimen
/// goes to the provider verbatim while an `extra` pattern masks what
/// no built-in catches. Per-request construction is what makes a
/// settings hot-reload effective on the next submission (us-37).
#[test]
fn the_request_redaction_settings_drive_context_redaction() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&["ls"]))]);
    let suggester = suggester();
    let mut settings = settings(&fixture.base_url, "command_model");
    settings.redaction.disable.push(String::from("aws_access_key"));
    settings
        .redaction
        .extra
        .push(String::from(r"\btoken-[A-Z0-9]{8}"));
    let mut request = request(settings);
    request
        .output_lines
        .push(String::from("token-A1B2C3D4 logged"));
    suggester.suggest(request).expect("suggest");

    let events = collect(&suggester);
    assert_eq!(events.last(), Some(&SuggestEvent::Done));

    let captured = fixture.captured();
    assert!(
        captured.body.contains("AKIAIOSFODNN7EXAMPLE"),
        "a disabled built-in passes through: {}",
        captured.body
    );
    assert!(
        !captured.body.contains("token-A1B2C3D4"),
        "the extra pattern must mask: {}",
        captured.body
    );
    assert!(captured.body.contains("[REDACTED]"), "{}", captured.body);
}

/// us-21: `command_model` unset falls back to `base_model` — the
/// request still resolves and streams.
#[test]
fn command_model_falls_back_to_base_model() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&["pwd"]))]);
    let suggester = suggester();
    suggester
        .suggest(request(settings(&fixture.base_url, "base_model")))
        .expect("suggest");

    let events = collect(&suggester);
    assert_eq!(delta_text(&events), "pwd");
    assert_eq!(events.last(), Some(&SuggestEvent::Done));
}

/// us-5 at the channel seam: a multi-line reply can never leak a
/// newline mid-stream — emitted `Delta`s truncate at the first line
/// break exactly as `sanitize_reply` would.
#[test]
fn multi_line_reply_never_leaks_a_line_break() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&[
        "ls -l",
        "a\nrm -rf ",
        "/ --no-preserve-root",
    ]))]);
    let suggester = suggester();
    suggester
        .suggest(request(settings(&fixture.base_url, "command_model")))
        .expect("suggest");

    let events = collect(&suggester);
    assert_eq!(delta_text(&events), "ls -la");
    assert_eq!(events.last(), Some(&SuggestEvent::Done));
}

/// A fenced, `$ `-prefixed reply arrives already stripped — the
/// never-execute shaping happens inside the worker, not the caller.
#[test]
fn fenced_dollar_reply_streams_sanitized() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&[
        "```sh\n$ sudo ",
        "apt update\n```",
    ]))]);
    let suggester = suggester();
    suggester
        .suggest(request(settings(&fixture.base_url, "command_model")))
        .expect("suggest");

    let events = collect(&suggester);
    assert_eq!(delta_text(&events), "sudo apt update");
    assert_eq!(events.last(), Some(&SuggestEvent::Done));
}

/// us-8 (T5's Esc): `cancel()` during a stalled stream terminates the
/// request promptly — the adapter's 60 s idle-timeout never comes into
/// play, and a cancelled request surfaces as an `Error` event (§6).
#[test]
fn cancel_terminates_a_stalled_stream_promptly() {
    let (drip_tx, drip_rx) = mpsc::channel::<Vec<u8>>();
    let fixture = Fixture::serve(vec![Script::DripSse(drip_rx)]);
    let suggester = suggester();
    suggester
        .suggest(request(settings(&fixture.base_url, "command_model")))
        .expect("suggest");

    // Feed one >8KiB write (tiny_http buffers ~8 KiB — the `:`-comment
    // is spec-legal SSE padding; the real event goes first and the
    // comment's missing second newline keeps it a silently-held
    // incomplete line, per the harness's DripSse docs), then hold the
    // sender open: the stream stalls mid-reply.
    let mut chunk = sse_delta("git sta").into_bytes();
    chunk.extend_from_slice(format!(": {}\n", "x".repeat(9 * 1024)).as_bytes());
    drip_tx.send(chunk).expect("drip feed");

    let first = suggester.events().recv_timeout(BOUND).expect("first event");
    assert_eq!(first, SuggestEvent::Delta(String::from("git sta")));

    // Esc: the in-flight request must end promptly — well under the
    // idle-timeout — with a cancellation error, not a hang.
    suggester.cancel();
    match suggester.events().recv_timeout(BOUND) {
        Ok(SuggestEvent::Error(message)) => {
            assert!(
                message.contains("cancelled"),
                "unexpected error {message:?}"
            )
        }
        other => panic!("expected a cancellation Error, got {other:?}"),
    }
    drop(drip_tx);
}

/// us-22: a fixture-side HTTP failure surfaces as an `Error` event for
/// the status toast — never a panic, never a hang.
#[test]
fn endpoint_http_error_maps_to_error_event() {
    // 400 is terminal — the adapter's bounded retry skips it, so one
    // scripted response is enough.
    let fixture = Fixture::serve(vec![error_status(
        400,
        "{\"error\":{\"message\":\"bad request\"}}",
    )]);
    let suggester = suggester();
    suggester
        .suggest(request(settings(&fixture.base_url, "command_model")))
        .expect("suggest");

    match suggester.events().recv_timeout(BOUND) {
        Ok(SuggestEvent::Error(message)) => {
            assert!(message.contains("400"), "unexpected error {message:?}")
        }
        other => panic!("expected an Error event, got {other:?}"),
    }
}

/// us-22/§6: no `command_model` and no `base_model` → `resolve_model`'s
/// `Ok(None)` becomes an `Error` event, and the worker survives to
/// answer the next request — never a panic, never a wedge.
#[test]
fn unresolvable_model_errors_without_panic_or_hang() {
    let suggester = suggester();
    suggester
        .suggest(request(unconfigured_settings()))
        .expect("suggest");

    match suggester.events().recv_timeout(BOUND) {
        Ok(SuggestEvent::Error(message)) => assert!(
            message.contains("command_model"),
            "unexpected error {message:?}"
        ),
        other => panic!("expected an Error event, got {other:?}"),
    }

    // The worker is still alive: an unknown profile errors the same way.
    let bare = Settings::from_toml("").expect("empty settings parse");
    suggester.suggest(request(bare)).expect("suggest");
    match suggester.events().recv_timeout(BOUND) {
        Ok(SuggestEvent::Error(_)) => {}
        other => panic!("expected an Error event, got {other:?}"),
    }
}

/// Spec R07 §4.2/us-25 (issue #101): the request's `profile` field
/// picks which execution profile's `command_model` resolves — a
/// request armed with `work` streams where `default` (undeclared
/// here) can only error.
#[test]
fn the_request_profile_resolves_its_own_command_model() {
    let fixture = Fixture::serve(vec![sse(&sse_transcript(&["git ", "status"]))]);
    let work_only = Settings::from_toml(&format!(
        r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "{}"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "cmd-key"

[agents.execution_profiles.work]
command_model = "cmd-key"
"#,
        fixture.base_url
    ))
    .expect("test settings must parse");
    let suggester = suggester();

    let mut armed = request(work_only.clone());
    armed.profile = String::from("work");
    suggester.suggest(armed).expect("suggest");

    let events = collect(&suggester);
    assert_eq!(delta_text(&events), "git status");
    assert_eq!(events.last(), Some(&SuggestEvent::Done));

    // The same settings under `default` fail closed at model
    // resolution — the armed name is what resolved, not a constant.
    let mut missing = request(work_only);
    missing.profile = String::from("default");
    suggester.suggest(missing).expect("suggest");
    match suggester.events().recv_timeout(BOUND) {
        Ok(SuggestEvent::Error(message)) => assert!(
            message.contains("unknown execution profile"),
            "unexpected error {message:?}"
        ),
        other => panic!("expected an Error event, got {other:?}"),
    }
}
