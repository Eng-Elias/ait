//! IR and adapter for the `openai_responses` endpoint schema —
//! `POST {base_url}/responses` (§4.1).
//!
//! The types serialize to the wire shape directly: the adapter serializes
//! the [`Request`], forces `stream`/`store` on and decodes named
//! `response.*` SSE events into [`StreamEvent`]s. The `ResponseStreamEvent`
//! union has dozens of variants — the adapter reads the ones it needs and
//! skips the rest, so new event types never break it.

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::provider::EventStream;
use crate::{http, EndpointConfig, EndpointSchema, LlmError, LlmProvider};

/// The `openai_responses` adapter (us-26).
///
/// `Authorization: Bearer <secret>` is sent when the endpoint has a
/// secret, and no auth header at all when it doesn't (us-21). Retry and
/// timeout policy lives in `crate::http`.
pub struct Provider {
    /// The shared client (10 s connect timeout, no total deadline, us-33).
    client: reqwest::Client,
    /// The bounded-retry policy — injectable for tests.
    retry: http::Retry,
    /// The endpoint configuration this adapter was built from.
    config: EndpointConfig,
}

impl Provider {
    /// Build the adapter for one endpoint+model pair — normally reached via
    /// [`Endpoint::new`](crate::Endpoint::new), the single construction path.
    pub fn new(config: EndpointConfig) -> Result<Self, LlmError> {
        Self::with_retry(config, http::Retry::default())
    }

    /// Build the adapter with an explicit retry policy — the internal
    /// seam contract tests use to compress backoff, not settings surface.
    #[doc(hidden)]
    pub fn with_retry(config: EndpointConfig, retry: http::Retry) -> Result<Self, LlmError> {
        Ok(Self {
            client: crate::endpoint::shared_client()?,
            retry,
            config,
        })
    }

    /// The endpoint configuration this adapter was built from.
    pub fn config(&self) -> &EndpointConfig {
        &self.config
    }

    /// `POST {base_url}/responses`; `Authorization: Bearer` only when a
    /// secret is configured (us-21).
    fn build_request(&self, request: &Request) -> Result<reqwest::Request, LlmError> {
        let mut body = serde_json::to_value(request)
            .map_err(|e| LlmError::Parse(format!("failed to serialize request: {e}")))?;
        if let Some(object) = body.as_object_mut() {
            // Streaming is the adapter's wire detail; `store: false` keeps
            // responses stateless (no server-side retention).
            object.insert("stream".into(), true.into());
            object.insert("store".into(), false.into());
        }
        let mut builder = self
            .client
            .post(http::url(&self.config.base_url, "responses"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string());
        if let Some(secret) = &self.config.secret {
            builder = builder.bearer_auth(secret);
        }
        builder.build().map_err(LlmError::Transport)
    }
}

impl LlmProvider for Provider {
    type Request = Request;
    type StreamEvent = StreamEvent;

    fn schema(&self) -> EndpointSchema {
        EndpointSchema::OpenAiResponses
    }

    async fn stream(
        &self,
        request: Request,
        cancel: CancellationToken,
    ) -> Result<EventStream<StreamEvent>, LlmError> {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let http_request = self.build_request(&request)?;
        let response =
            http::send_with_retry(&self.client, &http_request, &self.retry, &cancel).await?;
        Ok(http::sse_stream(response, cancel, decode))
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

// ---- SSE decode (wire → IR) ----

/// The `response.usage` wire shape nested inside `response.completed` /
/// `response.incomplete` / `response.failed` payloads.
#[derive(Debug, Deserialize)]
struct WireUsage {
    /// Input tokens billed.
    #[serde(default)]
    input_tokens: u64,
    /// Output tokens billed.
    #[serde(default)]
    output_tokens: u64,
    /// Total tokens billed.
    #[serde(default)]
    total_tokens: u64,
}

/// Stateless envelope → [`StreamEvent`] decoder: every payload carries its
/// `type` (which doubles as the SSE `event:` name), so one `serde_json`
/// document drives dispatch. Unknown `type`s — the union grows — are
/// skipped, never fatal (`None` input means the body ended; there is no
/// held state to flush).
fn decode(event: Option<eventsource_stream::Event>) -> http::Decoded<StreamEvent> {
    let Some(event) = event else {
        return http::Decoded::over(vec![]);
    };
    if event.data.trim() == "[DONE]" {
        return http::Decoded::over(vec![]);
    }
    let value = match serde_json::from_str::<serde_json::Value>(&event.data) {
        Ok(value) => value,
        Err(e) => {
            return http::Decoded::over(vec![Err(LlmError::Parse(format!(
                "invalid responses event: {e}"
            )))])
        }
    };
    let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let text = |path: &str| {
        value
            .pointer(path)
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    let usage = || {
        value
            .pointer("/response/usage")
            .and_then(|v| serde_json::from_value::<WireUsage>(v.clone()).ok())
            .map(|u| Usage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
                total_tokens: u.total_tokens,
            })
    };
    match kind {
        "response.output_text.delta" => http::Decoded::more(vec![Ok(StreamEvent::TextDelta {
            text: text("/delta").unwrap_or_default(),
        })]),
        // `call_id` + `name` arrive complete on the announced item; only
        // `arguments` stream afterwards (us-29).
        "response.output_item.added"
            if value.pointer("/item/type").and_then(|t| t.as_str()) == Some("function_call") =>
        {
            http::Decoded::more(vec![Ok(StreamEvent::ToolCallDelta(ToolCallDelta {
                item_id: text("/item/id"),
                call_id: text("/item/call_id"),
                name: text("/item/name"),
                arguments: String::new(),
            }))])
        }
        "response.function_call_arguments.delta" => {
            http::Decoded::more(vec![Ok(StreamEvent::ToolCallDelta(ToolCallDelta {
                item_id: text("/item_id"),
                call_id: None,
                name: None,
                arguments: text("/delta").unwrap_or_default(),
            }))])
        }
        "response.completed" => http::Decoded::over(vec![Ok(StreamEvent::Finish {
            reason: FinishReason::Completed,
            usage: usage(),
        })]),
        "response.incomplete" => http::Decoded::over(vec![Ok(StreamEvent::Finish {
            reason: FinishReason::Incomplete,
            usage: usage(),
        })]),
        // A failed response carries both an error payload and a terminal
        // status — emit both so callers see the reason and the end.
        "response.failed" => http::Decoded::over(vec![
            Ok(StreamEvent::Error {
                message: text("/response/error/message").unwrap_or_default(),
                code: text("/response/error/code"),
            }),
            Ok(StreamEvent::Finish {
                reason: FinishReason::Failed,
                usage: usage(),
            }),
        ]),
        "response.error" => http::Decoded::more(vec![Ok(StreamEvent::Error {
            message: text("/message").unwrap_or_default(),
            code: text("/code"),
        })]),
        // `response.created`, `response.in_progress`, `content_part.*`,
        // `output_text.done`, `output_item.done` (non-tool items),
        // `function_call_arguments.done` (fragments already streamed),
        // unknown future events: nothing to surface.
        _ => http::Decoded::more(vec![]),
    }
}
