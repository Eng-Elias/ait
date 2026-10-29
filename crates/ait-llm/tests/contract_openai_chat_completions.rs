//! Contract tests for the `openai_chat_completions` adapter (Spec R03
//! us-34): recorded SSE transcripts replayed through the in-process
//! fixture server — the real HTTP+SSE path, zero external network.

use ait_llm::openai_chat_completions as occ;
use ait_llm::testing::{assert_post, sse, Fixture};
use ait_llm::{EndpointConfig, EndpointSchema, LlmError, LlmProvider};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

fn provider(base_url: &str, secret: Option<&str>) -> occ::Provider {
    occ::Provider::new(EndpointConfig {
        base_url: base_url.into(),
        model: "gpt-6-astra".into(),
        secret: secret.map(str::to_string),
        schema: EndpointSchema::OpenAiChatCompletions,
    })
    .unwrap()
}

fn request() -> occ::Request {
    occ::Request {
        model: "gpt-6-astra".into(),
        messages: vec![
            occ::Message::System {
                content: "be terse".into(),
            },
            occ::Message::User {
                content: "hello".into(),
            },
        ],
        tools: vec![occ::Tool {
            kind: occ::ToolKind::Function,
            function: occ::ToolFunction {
                name: "get_weather".into(),
                description: Some("look up the weather".into()),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                }),
            },
        }],
        temperature: None,
        max_tokens: None,
        stream: true,
    }
}

/// Drain the stream to end — bounded so a wedged pump fails, not hangs —
/// and unwrap every item: these transcripts contain no errors.
async fn collect(stream: ait_llm::EventStream<occ::StreamEvent>) -> Vec<occ::StreamEvent> {
    let items: Vec<Result<occ::StreamEvent, LlmError>> =
        tokio::time::timeout(std::time::Duration::from_secs(10), stream.collect())
            .await
            .expect("event stream did not terminate");
    items.into_iter().collect::<Result<_, _>>().unwrap()
}

#[tokio::test]
async fn text_reply_streams_deltas_then_finish_with_usage() {
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/openai_chat_completions_text.sse"
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
            occ::StreamEvent::TextDelta {
                text: "Hello".into()
            },
            occ::StreamEvent::TextDelta {
                text: ", world!".into()
            },
            occ::StreamEvent::Finish {
                reason: occ::FinishReason::Stop,
                usage: Some(occ::Usage {
                    prompt_tokens: 12,
                    completion_tokens: 3,
                    total_tokens: 15,
                }),
            },
        ]
    );

    // The request went on the wire with the schema's auth shape (us-21).
    let captured = fixture.captured();
    assert_post(&captured);
    assert_eq!(captured.url, "/chat/completions");
    assert_eq!(captured.header("authorization"), Some("Bearer sk-test-key"));
    let body: serde_json::Value = serde_json::from_str(&captured.body).unwrap();
    assert_eq!(body["model"], "gpt-6-astra");
    assert_eq!(body["stream"], true);
    // The adapter asks for the trailing usage chunk itself.
    assert_eq!(body["stream_options"]["include_usage"], true);
    assert_eq!(body["tools"][0]["function"]["name"], "get_weather");
}

#[tokio::test]
async fn tool_call_streams_id_name_then_argument_fragments() {
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/openai_chat_completions_tool_call.sse"
    ))]);
    let provider = provider(&fixture.base_url, Some("sk-test-key"));

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    // us-29: id + name on the first fragment, arguments streamed by index.
    assert_eq!(
        events[0],
        occ::StreamEvent::ToolCallDelta(occ::ToolCallDelta {
            index: 0,
            id: Some("call_9KxV".into()),
            name: Some("get_weather".into()),
            arguments: String::new(),
        })
    );
    let mut fragments = String::new();
    for event in &events {
        if let occ::StreamEvent::ToolCallDelta(delta) = event {
            assert_eq!(delta.index, 0);
            fragments.push_str(&delta.arguments);
        }
    }
    assert_eq!(fragments, "{\"location\": \"SF\"}");
    assert_eq!(
        events.last().unwrap(),
        &occ::StreamEvent::Finish {
            reason: occ::FinishReason::ToolCalls,
            usage: Some(occ::Usage {
                prompt_tokens: 40,
                completion_tokens: 9,
                total_tokens: 49,
            }),
        }
    );
}

#[tokio::test]
async fn an_endpoint_without_a_secret_sends_no_auth_header() {
    // us-21: local OpenAI-compatible servers (Ollama, …) must see no
    // Authorization header at all — not even an empty one.
    let fixture = Fixture::serve(vec![sse(include_str!(
        "fixtures/openai_chat_completions_text.sse"
    ))]);
    let provider = provider(&fixture.base_url, None);

    let stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    collect(stream).await;

    let captured = fixture.captured();
    assert_eq!(captured.header("authorization"), None);
    assert_eq!(captured.header("x-api-key"), None);
}
