//! IR and adapter for the `openai_chat_completions` endpoint schema —
//! `POST {base_url}/chat/completions` (§4.1). Covers OpenAI itself plus the
//! compatible servers (Ollama, LM Studio, vLLM, …).
//!
//! The types serialize to the wire shape directly: the adapter (HTTP lands
//! in R03-T5) only has to `serde_json::to_vec` the [`Request`] and decode
//! SSE `data:` payloads into [`StreamEvent`]s.

use std::future::Future;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::provider::EventStream;
use crate::{EndpointConfig, EndpointSchema, LlmError, LlmProvider};

/// The `openai_chat_completions` adapter (us-26).
///
/// Holds the shared HTTP client and endpoint config; the request path —
/// `Authorization: Bearer <secret>`, SSE decode, retry — lands in R03-T5.
/// Until then [`stream`](LlmProvider::stream) fails with
/// [`LlmError::NotImplemented`] (or [`LlmError::Cancelled`] when cancelled).
pub struct Provider {
    /// The shared client; T5's request path uses it.
    client: reqwest::Client,
    /// The endpoint configuration this adapter was built from.
    config: EndpointConfig,
}

impl Provider {
    /// Build the adapter for one endpoint+model pair — normally reached via
    /// [`Endpoint::new`](crate::Endpoint::new), the single construction path.
    pub fn new(config: EndpointConfig) -> Result<Self, LlmError> {
        Ok(Self {
            client: crate::endpoint::shared_client()?,
            config,
        })
    }

    /// The endpoint configuration this adapter was built from.
    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }
}

impl LlmProvider for Provider {
    type Request = Request;
    type StreamEvent = StreamEvent;

    fn schema(&self) -> EndpointSchema {
        EndpointSchema::OpenAiChatCompletions
    }

    fn stream(
        &self,
        _request: Request,
        cancel: CancellationToken,
    ) -> impl Future<Output = Result<EventStream<StreamEvent>, LlmError>> + Send {
        // R03-T5 stub: fail loudly rather than silently doing nothing.
        let _ = &self.client;
        async move {
            if cancel.is_cancelled() {
                return Err(LlmError::Cancelled);
            }
            Err(LlmError::NotImplemented {
                schema: EndpointSchema::OpenAiChatCompletions,
            })
        }
    }
}

/// A chat-completions request (us-27). `stream` stays on the IR so the
/// request a consumer builds is the request that goes on the wire; adapters
/// send it `true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The model name sent on the wire (§4.1 `models[].name`).
    pub model: String,
    /// The conversation so far.
    pub messages: Vec<Message>,
    /// Tools the model may invoke.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Sampling temperature, if the caller wants one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output-token cap (`max_tokens` is what compatible servers accept).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Whether the reply streams over SSE.
    pub stream: bool,
}

/// One chat message; the `role` field is the wire discriminator and covers
/// all four roles (us-27).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    /// System prompt.
    System {
        /// The message text.
        content: String,
    },
    /// User input.
    User {
        /// The message text.
        content: String,
    },
    /// Assistant output: text, tool calls, or both.
    Assistant {
        /// The assistant's text, if it produced any.
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        /// Tool invocations the assistant requests.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    /// A tool result answering one assistant [`ToolCall`].
    Tool {
        /// The `id` of the call this result answers.
        tool_call_id: String,
        /// The result text.
        content: String,
    },
}

/// An assistant tool invocation (us-27): `arguments` stays a JSON fragment
/// — streamed calls arrive piecewise before they parse (us-29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id (`call_…`).
    pub id: String,
    /// The wire `"type"` — always `"function"` today.
    #[serde(rename = "type")]
    pub kind: ToolKind,
    /// The invoked function.
    pub function: FunctionCall,
}

/// A function name plus its JSON-encoded arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// The function name.
    pub name: String,
    /// The arguments, as a JSON-encoded object.
    pub arguments: String,
}

/// A tool the model may invoke.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// The wire `"type"` — always `"function"` today.
    #[serde(rename = "type")]
    pub kind: ToolKind,
    /// The function signature.
    pub function: ToolFunction,
}

/// A function signature advertised to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolFunction {
    /// The function name.
    pub name: String,
    /// What the function does, shown to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The arguments' JSON Schema.
    pub parameters: serde_json::Value,
}

/// The only tool `"type"` the schema defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// A function tool.
    Function,
}

/// One typed event of a streamed reply (us-28). Serialized with a `type`
/// tag so logs and fixtures stay self-describing; this is the IR's own
/// shape, not the wire chunk shape the adapter decodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A fragment of assistant text.
    TextDelta {
        /// The text fragment.
        text: String,
    },
    /// A fragment of a streamed tool call (us-29).
    ToolCallDelta(ToolCallDelta),
    /// The reply finished: stop reason, plus usage where reported (us-28).
    Finish {
        /// Why generation stopped.
        reason: FinishReason,
        /// Token accounting, when the endpoint reports it.
        #[serde(skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// A provider-reported error event.
    Error {
        /// The provider's message.
        message: String,
        /// The provider's machine-readable code, when it sends one.
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
}

/// An incremental piece of a streamed tool call (us-29). Fragments are
/// index-addressed: `id`/`name` typically appear on the first fragment of a
/// call, `arguments` streams as JSON fragments across many events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    /// Which parallel tool call this fragment belongs to.
    pub index: u32,
    /// The call id — present on the first fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The function name — present on the first fragment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A fragment of the JSON arguments (may be empty).
    pub arguments: String,
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model finished on its own.
    Stop,
    /// The output-token cap was hit.
    Length,
    /// The model asked for tool calls.
    ToolCalls,
    /// A content filter stopped generation.
    ContentFilter,
    /// Legacy `function_call` finish.
    FunctionCall,
}

/// Token accounting, where the endpoint reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens billed.
    pub prompt_tokens: u64,
    /// Completion tokens billed.
    pub completion_tokens: u64,
    /// Total tokens billed.
    pub total_tokens: u64,
}
