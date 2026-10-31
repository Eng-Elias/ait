//! Tests at the `Endpoint` seam (Spec R03 acceptance: "one construction
//! path from endpoint config and dispatches to the right adapter"; us-30:
//! "stop promptly when cancelled").

use ait_llm::{
    openai_chat_completions as occ, Endpoint, EndpointConfig, EndpointRequest, EndpointSchema,
    LlmError,
};
use tokio_util::sync::CancellationToken;

fn config(schema: EndpointSchema) -> EndpointConfig {
    EndpointConfig {
        base_url: "http://localhost:11434/v1".into(),
        model: "qwen2.5-coder:14b".into(),
        secret: None,
        schema,
    }
}

fn chat_request() -> EndpointRequest {
    EndpointRequest::OpenAiChatCompletions(occ::Request {
        model: "qwen2.5-coder:14b".into(),
        messages: vec![occ::Message::User {
            content: "hi".into(),
        }],
        tools: vec![],
        temperature: None,
        max_tokens: None,
        stream: true,
    })
}

#[test]
fn new_dispatches_to_the_adapter_matching_the_schema() {
    let chat = Endpoint::new(config(EndpointSchema::OpenAiChatCompletions)).unwrap();
    assert!(matches!(chat, Endpoint::OpenAiChatCompletions(_)));
    assert_eq!(chat.schema(), EndpointSchema::OpenAiChatCompletions);
    assert_eq!(chat.config().model, "qwen2.5-coder:14b");

    let responses = Endpoint::new(config(EndpointSchema::OpenAiResponses)).unwrap();
    assert!(matches!(responses, Endpoint::OpenAiResponses(_)));
    assert_eq!(responses.schema(), EndpointSchema::OpenAiResponses);

    let anthropic = Endpoint::new(config(EndpointSchema::AnthropicMessages)).unwrap();
    assert!(matches!(anthropic, Endpoint::AnthropicMessages(_)));
    assert_eq!(anthropic.schema(), EndpointSchema::AnthropicMessages);
}

#[test]
fn endpoint_schema_uses_the_settings_toml_names() {
    // §4.1's `schema = "..."` strings — the T6 bridge deserializes these.
    let json = serde_json::to_value(EndpointSchema::AnthropicMessages).unwrap();
    assert_eq!(json, "anthropic_messages");
    let back: EndpointSchema = serde_json::from_value(json).unwrap();
    assert_eq!(back, EndpointSchema::AnthropicMessages);
    assert_eq!(
        EndpointSchema::OpenAiResponses.to_string(),
        "openai_responses"
    );
}

#[test]
fn config_debug_redacts_the_secret() {
    // CODING_STANDARDS §4: secrets never reach logs — Debug included.
    let mut cfg = config(EndpointSchema::OpenAiChatCompletions);
    cfg.secret = Some("sk-live-key".into());
    let debug = format!("{cfg:?}");
    assert!(!debug.contains("sk-live-key"), "secret leaked: {debug}");
}

#[tokio::test]
async fn stream_rejects_a_request_built_for_another_schema() {
    let endpoint = Endpoint::new(config(EndpointSchema::AnthropicMessages)).unwrap();
    let err = endpoint
        .stream(chat_request(), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            LlmError::SchemaMismatch {
                endpoint: EndpointSchema::AnthropicMessages,
                request: EndpointSchema::OpenAiChatCompletions,
            }
        ),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn a_cancelled_token_short_circuits_the_request() {
    let endpoint = Endpoint::new(config(EndpointSchema::OpenAiChatCompletions)).unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let err = endpoint.stream(chat_request(), token).await.unwrap_err();
    assert!(
        matches!(err, LlmError::Cancelled),
        "unexpected error: {err}"
    );
}
