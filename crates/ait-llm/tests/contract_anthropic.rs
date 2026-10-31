//! Contract tests for the `anthropic_messages` adapter (Spec R03 us-34):
//! recorded SSE transcripts replayed through the in-process fixture
//! server — the real HTTP+SSE path, zero external network.

use ait_llm::anthropic_messages as am;
use ait_llm::testing::{assert_post, sse, Fixture};
use ait_llm::{EndpointConfig, EndpointSchema, LlmError, LlmProvider};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

fn provider(base_url: &str, secret: Option<&str>) -> am::Provider {
    am::Provider::new(EndpointConfig {
        base_url: base_url.into(),
        model: "claude-opus-5-5".into(),
        secret: secret.map(str::to_string),
        schema: EndpointSchema::AnthropicMessages,
    })
    .unwrap()
}

fn request() -> am::Request {
    am::Request {
        model: "claude-opus-5-5".into(),
        messages: vec![am::Message {
            role: am::Role::User,
            content: vec![am::ContentBlock::Text {
                text: "hello".into(),
            }],
        }],
        system: Some("be terse".into()),
        max_tokens: 1024,
        tools: vec![am::Tool {
            name: "get_weather".into(),
            description: Some("look up the weather".into()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"location": {"type": "string"}},
                "required": ["location"],
            }),
        }],
        temperature: None,
        stream: true,
    }
}

/// Drain the stream to end — bounded so a wedged pump fails, not hangs —
/// and unwrap every item: these transcripts contain no `Err` machinery
/// failures (`Error` is data, so it stays in the list).
async fn collect(stream: ait_llm::EventStream<am::StreamEvent>) -> Vec<am::StreamEvent> {
    let items: Vec<Result<am::StreamEvent, LlmError>> =
        tokio::time::timeout(std::time::Duration::from_secs(10), stream.collect())
            .await
            .expect("event stream did not terminate");
    items.into_iter().collect::<Result<_, _>>().unwrap()
}

#[tokio::test]
async fn text_reply_streams_deltas_then_finish_with_usage() {
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/anthropic_messages_text.sse"
    ))]);
    let provider = provider(&fixture.base_url, Some("sk-ant-test"));

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    // `message_start`, `content_block_start`, `ping`, `*_stop` stay inside
    // the wire; `input_tokens` from message_start joins `output_tokens`
    // from message_delta on the Finish.
    assert_eq!(
        events,
        vec![
            am::StreamEvent::TextDelta {
                text: "Hello".into()
            },
            am::StreamEvent::TextDelta {
                text: ", world!".into()
            },
            am::StreamEvent::Finish {
                reason: am::FinishReason::EndTurn,
                usage: Some(am::Usage {
                    input_tokens: Some(25),
                    output_tokens: Some(15),
                }),
            },
        ]
    );

    // The schema's own auth shape: `x-api-key` — never `Authorization` —
    // plus the pinned `anthropic-version`.
    let captured = fixture.captured();
    assert_post(&captured);
    assert_eq!(captured.url, "/messages");
    assert_eq!(captured.header("x-api-key"), Some("sk-ant-test"));
    assert_eq!(captured.header("anthropic-version"), Some("2023-06-01"));
    assert_eq!(captured.header("authorization"), None);
    let body: serde_json::Value = serde_json::from_str(&captured.body).unwrap();
    assert_eq!(body["model"], "claude-opus-5-5");
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["system"], "be terse");
    assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
}

#[tokio::test]
async fn tool_use_streams_block_open_then_json_fragments() {
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/anthropic_messages_tool_call.sse"
    ))]);
    let provider = provider(&fixture.base_url, Some("sk-ant-test"));

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(
        events[0],
        am::StreamEvent::TextDelta {
            text: "Checking.".into()
        }
    );
    // us-29: id + name arrive complete when the `tool_use` block opens;
    // `input_json_delta` fragments stream afterwards, addressed by `index`.
    assert_eq!(
        events[1],
        am::StreamEvent::ToolCallDelta(am::ToolCallDelta {
            index: 1,
            id: Some("toolu_01Fix".into()),
            name: Some("get_weather".into()),
            arguments: String::new(),
        })
    );
    let mut fragments = String::new();
    for event in &events {
        if let am::StreamEvent::ToolCallDelta(delta) = event {
            assert_eq!(delta.index, 1);
            fragments.push_str(&delta.arguments);
        }
    }
    assert_eq!(fragments, "{\"location\": \"San Francisco, CA\"}");
    assert_eq!(
        events.last().unwrap(),
        &am::StreamEvent::Finish {
            reason: am::FinishReason::ToolUse,
            usage: Some(am::Usage {
                input_tokens: Some(31),
                output_tokens: Some(28),
            }),
        }
    );
}

#[tokio::test]
async fn a_mid_stream_error_event_surfaces_as_data() {
    // A 200 can precede `event: error` — the overloaded case. It is
    // provider-reported data, not machinery failure: an `Error` item, not
    // an `Err`.
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/anthropic_messages_error_event.sse"
    ))]);
    let provider = provider(&fixture.base_url, Some("sk-ant-test"));

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(
        events,
        vec![am::StreamEvent::Error {
            message: "Overloaded".into(),
            code: Some("overloaded_error".into()),
        }]
    );
}

#[tokio::test]
async fn a_finish_without_any_reported_usage_carries_none() {
    // The schema reports `input_tokens` at `message_start` and
    // `output_tokens` at `message_delta` — when neither arrives, the
    // Finish's `usage` is `None`, not a shell of `None` fields.
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/anthropic_messages_no_usage.sse"
    ))]);
    let provider = provider(&fixture.base_url, Some("sk-ant-test"));

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(
        events,
        vec![
            am::StreamEvent::TextDelta {
                text: "Done.".into()
            },
            am::StreamEvent::Finish {
                reason: am::FinishReason::EndTurn,
                usage: None,
            },
        ]
    );
}

#[tokio::test]
async fn an_endpoint_without_a_secret_sends_no_api_key_header() {
    // us-21: `x-api-key` omitted entirely; `anthropic-version` is
    // protocol, not secret, so it is still sent.
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/anthropic_messages_text.sse"
    ))]);
    let provider = provider(&fixture.base_url, None);

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    collect(stream).await;

    let captured = fixture.captured();
    assert_eq!(captured.header("x-api-key"), None);
    assert_eq!(captured.header("authorization"), None);
    assert_eq!(captured.header("anthropic-version"), Some("2023-06-01"));
}
