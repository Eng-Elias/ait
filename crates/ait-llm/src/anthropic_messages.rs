//! IR and adapter for the `anthropic_messages` endpoint schema —
//! `POST {base_url}/messages` (§4.1).
//!
//! The types serialize to the wire shape directly: the adapter (HTTP lands
//! in R03-T5) only has to `serde_json::to_vec` the [`Request`] and decode
//! SSE `content_block_*`/`message_*` events into [`StreamEvent`]s.

use std::future::Future;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::provider::EventStream;
use crate::{EndpointConfig, EndpointSchema, LlmError, LlmProvider};

/// The `anthropic_messages` adapter (us-26).
///
/// Holds the shared HTTP client and endpoint config; the request path —
/// `x-api-key` + `anthropic-version` headers, SSE decode, retry — lands in
/// R03-T5. Until then [`stream`](LlmProvider::stream) fails with
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
        EndpointSchema::AnthropicMessages
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
                schema: EndpointSchema::AnthropicMessages,
            })
        }
    }
}

/// A Messages-API request (us-27). `stream` stays on the IR so the request
/// a consumer builds is the request that goes on the wire; adapters send it
/// `true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The model name sent on the wire (§4.1 `models[].name`).
    pub model: String,
    /// The conversation so far — `user`/`assistant` only; the system prompt
    /// travels top-level.
    pub messages: Vec<Message>,
    /// The system prompt — the schema keeps it top-level, not as a message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Output-token cap — required by this schema.
    pub max_tokens: u32,
    /// Tools the model may invoke.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Sampling temperature, if the caller wants one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Whether the reply streams over SSE.
    pub stream: bool,
}

/// One message; `role` is `user` or `assistant` and `content` is a list of
/// typed blocks (us-27).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who produced the message.
    pub role: Role,
    /// The message's content blocks.
    pub content: Vec<ContentBlock>,
}

/// Message roles — this schema has no system role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// User input; also carries `tool_result` blocks.
    User,
    /// Assistant output; also carries `tool_use` blocks.
    Assistant,
}

/// One content block of a [`Message`]; the `type` field is the wire
/// discriminator (us-27).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// An assistant tool invocation. `input` is the decoded JSON arguments
    /// object — it only exists once the streamed fragments have been
    /// assembled (us-29).
    ToolUse {
        /// Provider-assigned call id (`toolu_…`).
        id: String,
        /// The tool name.
        name: String,
        /// The arguments, as a decoded JSON object.
        input: serde_json::Value,
    },
    /// A tool result answering a `tool_use` block.
    ToolResult {
        /// The `id` of the call this result answers.
        tool_use_id: String,
        /// The result text.
        content: String,
    },
}

/// A tool the model may invoke.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// The tool name.
    pub name: String,
    /// What the tool does, shown to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The arguments' JSON Schema.
    pub input_schema: serde_json::Value,
}

/// One typed event of a streamed reply (us-28). Serialized with a `type`
/// tag so logs and fixtures stay self-describing; this is the IR's own
/// shape, not the `content_block_*`/`message_*` wire events the adapter
/// decodes.
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
        /// The provider's machine-readable error type, when it sends one.
        #[serde(skip_serializing_if = "Option::is_none")]
        code: Option<String>,
    },
}

/// An incremental piece of a streamed tool call (us-29). A `tool_use`
/// content block opens with its `id`/`name`, then `input_json_delta`
/// argument fragments stream in — all addressed by block `index`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    /// Which content block this fragment belongs to.
    pub index: u32,
    /// The call id — present when the block opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The tool name — present when the block opens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A fragment of the JSON arguments (`partial_json`; may be empty).
    pub arguments: String,
}

/// Why generation stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The model finished on its own.
    EndTurn,
    /// The output-token cap was hit.
    MaxTokens,
    /// A configured stop sequence was reached.
    StopSequence,
    /// The model asked for a tool call.
    ToolUse,
    /// A long-running tool turn paused the reply.
    PauseTurn,
    /// The model refused.
    Refusal,
}

/// Token accounting. The schema reports `input_tokens` at message start and
/// `output_tokens` at message end, so each half is optional on a single
/// `Finish` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens billed, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    /// Output tokens billed, when reported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
}
