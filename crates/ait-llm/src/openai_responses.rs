//! IR and adapter for the `openai_responses` endpoint schema —
//! `POST {base_url}/responses` (§4.1).
//!
//! The types serialize to the wire shape directly: the adapter (HTTP lands
//! in R03-T5) only has to `serde_json::to_vec` the [`Request`] and decode
//! SSE `response.*` events into [`StreamEvent`]s.

use std::future::Future;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::provider::EventStream;
use crate::{EndpointConfig, EndpointSchema, LlmError, LlmProvider};

/// The `openai_responses` adapter (us-26).
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
        EndpointSchema::OpenAiResponses
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
                schema: EndpointSchema::OpenAiResponses,
            })
        }
    }
}

/// A Responses-API request (us-27). `stream` stays on the IR so the request
/// a consumer builds is the request that goes on the wire; adapters send it
/// `true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The model name sent on the wire (§4.1 `models[].name`).
    pub model: String,
    /// System-level guidance — the schema keeps it top-level rather than in
    /// a message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The conversation so far, as input items.
    pub input: Vec<Input>,
    /// Tools the model may invoke.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Sampling temperature, if the caller wants one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Output-token cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// Whether the reply streams over SSE.
    pub stream: bool,
}

/// One item of `input` (us-27); the `type` field is the wire discriminator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Input {
    /// A role-tagged message.
    Message(Message),
    /// An assistant tool invocation carried as an item.
    FunctionCall(FunctionCall),
    /// A tool result answering a [`FunctionCall`].
    FunctionCallOutput {
        /// The `call_id` of the call this result answers.
        call_id: String,
        /// The result text.
        output: String,
    },
}

/// A role-tagged message item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who produced the message.
    pub role: Role,
    /// The message's content parts.
    pub content: Vec<Content>,
}

/// Message roles; `developer` is the schema's name for system-prompt
/// messages (the top-level `instructions` field is the other channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Developer/system guidance inside `input`.
    Developer,
    /// A system message.
    System,
    /// User input.
    User,
    /// Assistant output.
    Assistant,
}

/// One content part of a [`Message`]; the `type` field is the wire
/// discriminator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    /// Text going into the model.
    InputText {
        /// The text.
        text: String,
    },
    /// Text the model produced.
    OutputText {
        /// The text.
        text: String,
    },
}

/// An assistant tool invocation as an input item (us-27): `arguments` stays
/// a JSON fragment — streamed calls arrive piecewise before they parse
/// (us-29).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Provider-assigned call id (`call_…`).
    pub call_id: String,
    /// The function name.
    pub name: String,
    /// The arguments, as a JSON-encoded object.
    pub arguments: String,
}

/// A function tool the model may invoke.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    /// The wire `"type"` — always `"function"` today.
    #[serde(rename = "type")]
    pub kind: ToolKind,
    /// The function name.
    pub name: String,
    /// What the function does, shown to the model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The arguments' JSON Schema.
    pub parameters: serde_json::Value,
    /// Whether strict schema adherence is requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
}

/// The only tool `"type"` the IR models.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// A function tool.
    Function,
}

/// One typed event of a streamed reply (us-28). Serialized with a `type`
/// tag so logs and fixtures stay self-describing; this is the IR's own
/// shape, not the `response.*` wire events the adapter decodes.
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
    /// The reply finished: terminal status, plus usage where reported
    /// (us-28).
    Finish {
        /// The response's terminal status.
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

/// An incremental piece of a streamed tool call (us-29). The schema
/// announces a `function_call` output item with its ids and name, then
/// streams argument fragments addressed by `item_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    /// The output item this fragment belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// The provider's call id — present when the call is announced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    /// The function name — present when the call is announced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// A fragment of the JSON arguments (empty on the announce event).
    pub arguments: String,
}

/// The response's terminal status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// The response completed.
    Completed,
    /// The response stopped early (e.g. output-token cap).
    Incomplete,
    /// The response failed.
    Failed,
    /// The response was cancelled server-side.
    Cancelled,
}

/// Token accounting, where the endpoint reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Input tokens billed.
    pub input_tokens: u64,
    /// Output tokens billed.
    pub output_tokens: u64,
    /// Total tokens billed.
    pub total_tokens: u64,
}
