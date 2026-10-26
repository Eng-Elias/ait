//! Contract tests for the `openai_responses` adapter (Spec R03 us-34):
//! recorded SSE transcripts replayed through the in-process fixture
//! server — the real HTTP+SSE path, zero external network.

mod support;

use ait_llm::openai_responses as or;
use ait_llm::{testing::Retry, EndpointConfig, EndpointSchema, LlmError, LlmProvider};
use futures_util::StreamExt;
use support::{assert_post, error_status, sse, Fixture};
use tokio_util::sync::CancellationToken;

fn provider(base_url: &str, secret: Option<&str>) -> or::Provider {
    or::Provider::new(EndpointConfig {
        base_url: base_url.into(),
        model: "gpt-6-astra".into(),
        secret: secret.map(str::to_string),
        schema: EndpointSchema::OpenAiResponses,
    })
    .unwrap()
}

fn request() -> or::Request {
    or::Request {
        model: "gpt-6-astra".into(),
        instructions: Some("be terse".into()),
        input: vec![or::Input::Message(or::Message {
            role: or::Role::User,
            content: vec![or::Content::InputText {
                text: "hello".into(),
            }],
        })],
        tools: vec![or::Tool {
            kind: or::ToolKind::Function,
            name: "get_weather".into(),
            description: Some("look up the weather".into()),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"location": {"type": "string"}},
            }),
            strict: None,
        }],
        temperature: None,
        max_output_tokens: None,
        stream: true,
    }
}

/// Drain the stream to end — bounded so a wedged pump fails, not hangs —
/// and unwrap every item: these transcripts contain no errors.
async fn collect(stream: ait_llm::EventStream<or::StreamEvent>) -> Vec<or::StreamEvent> {
    let items: Vec<Result<or::StreamEvent, LlmError>> =
        tokio::time::timeout(std::time::Duration::from_secs(10), stream.collect())
            .await
            .expect("event stream did not terminate");
    items.into_iter().collect::<Result<_, _>>().unwrap()
}

#[tokio::test]
async fn text_reply_streams_deltas_then_finish_with_usage() {
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/openai_responses_text.sse"
    ))]);
    let provider = provider(&fixture.base_url, Some("sk-test-key"));

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    // The bookkeeping events around the deltas — `response.created`,
    // `in_progress`, `content_part.*`, `*.done` — stay inside the wire.
    assert_eq!(
        events,
        vec![
            or::StreamEvent::TextDelta {
                text: "Hello".into()
            },
            or::StreamEvent::TextDelta {
                text: ", world!".into()
            },
            or::StreamEvent::Finish {
                reason: or::FinishReason::Completed,
                usage: Some(or::Usage {
                    input_tokens: 37,
                    output_tokens: 11,
                    total_tokens: 48,
                }),
            },
        ]
    );

    let captured = fixture.captured();
    assert_post(&captured);
    assert_eq!(captured.url, "/responses");
    assert_eq!(captured.header("authorization"), Some("Bearer sk-test-key"));
    let body: serde_json::Value = serde_json::from_str(&captured.body).unwrap();
    assert_eq!(body["model"], "gpt-6-astra");
    assert_eq!(body["stream"], true);
    assert_eq!(body["store"], false);
    assert_eq!(body["instructions"], "be terse");
    // Tools are flat in this schema — no `function` wrapper.
    assert_eq!(body["tools"][0]["type"], "function");
    assert_eq!(body["tools"][0]["name"], "get_weather");
    assert!(body["tools"][0].get("function").is_none());
}

#[tokio::test]
async fn tool_call_streams_announced_item_then_argument_fragments() {
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/openai_responses_tool_call.sse"
    ))]);
    let provider = provider(&fixture.base_url, Some("sk-test-key"));

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    // us-29: call_id + name arrive complete on `output_item.added`; only
    // arguments stream afterwards, addressed by `item_id`.
    assert_eq!(
        events[0],
        or::StreamEvent::ToolCallDelta(or::ToolCallDelta {
            item_id: Some("fc_fix01".into()),
            call_id: Some("call_fix01".into()),
            name: Some("get_weather".into()),
            arguments: String::new(),
        })
    );
    let mut fragments = String::new();
    for event in &events {
        if let or::StreamEvent::ToolCallDelta(delta) = event {
            assert_eq!(delta.item_id.as_deref(), Some("fc_fix01"));
            fragments.push_str(&delta.arguments);
        }
    }
    assert_eq!(fragments, "{\"location\": \"SF\"}");
    assert_eq!(
        events.last().unwrap(),
        &or::StreamEvent::Finish {
            reason: or::FinishReason::Completed,
            usage: Some(or::Usage {
                input_tokens: 41,
                output_tokens: 8,
                total_tokens: 49,
            }),
        }
    );
}

#[tokio::test]
async fn a_non_2xx_status_surfaces_http_status_with_the_error_body() {
    // us-34's error shape for this schema: a non-2xx reply's `{"error"}`
    // body is carried in `LlmError::HttpStatus` — one request, no retry
    // on a terminal status.
    let fixture = Fixture::serve(vec![error_status(
        400,
        "{\"error\":{\"message\":\"'instructions' is too long\",\"type\":\"invalid_request_error\",\"code\":\"value_too_large\"}}",
    )]);
    let provider = provider(&fixture.base_url, Some("sk-test-key"));

    let err = provider
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a 400 should fail");

    match err {
        LlmError::HttpStatus { status, body } => {
            assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
            assert!(body.contains("'instructions' is too long"), "{body}");
        }
        other => panic!("unexpected error: {other}"),
    }
    let captured = fixture.captured();
    assert_post(&captured);
    assert_eq!(captured.url, "/responses");
    fixture.assert_no_more_requests();
}

#[tokio::test]
async fn a_retryable_status_retries_then_surfaces_the_last_error() {
    // The schema's error body rides through the same bounded-retry
    // machinery — a single-attempt injection surfaces the 429's body.
    let fixture = Fixture::serve(vec![error_status(
        429,
        "{\"error\":{\"message\":\"Rate limit reached.\",\"type\":\"tokens\",\"code\":\"rate_limit_exceeded\"}}",
    )]);
    let provider = or::Provider::with_retry(
        EndpointConfig {
            base_url: fixture.base_url.clone(),
            model: "gpt-6-astra".into(),
            secret: None,
            schema: EndpointSchema::OpenAiResponses,
        },
        Retry {
            max_attempts: 1,
            ..Retry::default()
        },
    )
    .unwrap();

    let err = provider
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .expect("a 429 should fail with one attempt");

    match &err {
        LlmError::HttpStatus { status, body } => {
            assert_eq!(*status, reqwest::StatusCode::TOO_MANY_REQUESTS);
            assert!(body.contains("Rate limit reached"), "{body}");
        }
        other => panic!("unexpected error: {other}"),
    }
    assert!(err.is_retryable(), "429 stays classified as transient");
    fixture.captured();
    fixture.assert_no_more_requests();
}

#[tokio::test]
async fn a_mid_stream_failure_streams_error_events_then_failed_finish() {
    // us-28/34's error shape mid-stream: `response.error` is provider-
    // reported data, and `response.failed` carries both the error
    // payload and the terminal status — all `Ok` items, never `Err`.
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/openai_responses_error_event.sse"
    ))]);
    let provider = provider(&fixture.base_url, Some("sk-test-key"));

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(
        events,
        vec![
            or::StreamEvent::TextDelta {
                text: "Partial answer".into()
            },
            or::StreamEvent::Error {
                message: "Upstream rate limit hit mid-stream.".into(),
                code: Some("rate_limit_exceeded".into()),
            },
            or::StreamEvent::Error {
                message: "Model generation failed.".into(),
                code: Some("server_error".into()),
            },
            or::StreamEvent::Finish {
                reason: or::FinishReason::Failed,
                usage: None,
            },
        ]
    );
}
