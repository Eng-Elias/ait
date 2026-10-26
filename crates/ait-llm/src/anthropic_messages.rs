//! IR and adapter for the `anthropic_messages` endpoint schema —
//! `POST {base_url}/messages` (§4.1; the real API mounts it at `/v1`, so a
//! `base_url` ending in `/v1` covers both hosted and compatible servers).
//!
//! The types serialize to the wire shape directly: the adapter serializes
//! the [`Request`], forces `stream` on and decodes named
//! `content_block_*`/`message_*` SSE events into [`StreamEvent`]s — there
//! is no `[DONE]` marker; `message_stop` ends the stream.

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::provider::EventStream;
use crate::{http, EndpointConfig, EndpointSchema, LlmError, LlmProvider};

/// The API version sent on `anthropic-version`. Newer versions remove
/// `temperature`/`top_p`/`top_k`; `2023-06-01` is what the documented
/// examples use and matches the SSE event format decoded here — bump
/// deliberately, with the wire format.
const API_VERSION: &str = "2023-06-01";

/// The `anthropic_messages` adapter (us-26).
///
/// `x-api-key` (not `Authorization`) plus `anthropic-version` are sent
/// when the endpoint has a secret — `x-api-key` omitted entirely when it
/// doesn't (us-21); `anthropic-version` is protocol, always sent. Retry
/// and timeout policy lives in `crate::http`.
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

    /// `POST {base_url}/messages`; `x-api-key` only when a secret is
    /// configured (us-21), `anthropic-version` always.
    fn build_request(&self, request: &Request) -> Result<reqwest::Request, LlmError> {
        let mut body = serde_json::to_value(request)
            .map_err(|e| LlmError::Parse(format!("failed to serialize request: {e}")))?;
        if let Some(object) = body.as_object_mut() {
            object.insert("stream".into(), true.into());
        }
        let mut builder = self
            .client
            .post(http::url(&self.config.base_url, "messages"))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("anthropic-version", API_VERSION)
            .body(body.to_string());
        if let Some(secret) = &self.config.secret {
            builder = builder.header("x-api-key", secret);
        }
        builder.build().map_err(LlmError::Transport)
    }
}

impl LlmProvider for Provider {
    type Request = Request;
    type StreamEvent = StreamEvent;

    fn schema(&self) -> EndpointSchema {
        EndpointSchema::AnthropicMessages
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
        let mut decoder = Decoder::default();
        Ok(http::sse_stream(
            response,
            cancel,
            move |event| decoder.decode(event),
            self.retry.idle_timeout,
        ))
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

// ---- SSE decode (wire → IR) ----

/// Stateful event → [`StreamEvent`] decoder.
///
/// `input_tokens` is reported once at `message_start` while
/// `output_tokens` arrives (cumulative) on `message_delta`, so the decoder
/// holds the input count to attach to `Finish`. Unknown event and delta
/// types (`ping`, `thinking_delta`, `citations_delta`, …) are skipped —
/// the API's versioning policy adds them over time.
#[derive(Default)]
struct Decoder {
    /// `message_start.usage.input_tokens`, held for `Finish`.
    input_tokens: Option<u64>,
}

impl Decoder {
    /// Decode one SSE frame (`None` = the body ended — no state to flush;
    /// `message_stop` is what marks the end, not EOF).
    fn decode(&mut self, event: Option<eventsource_stream::Event>) -> http::Decoded<StreamEvent> {
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
                    "invalid anthropic event: {e}"
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
        let index = || {
            value
                .get("index")
                .and_then(|v| v.as_u64())
                .unwrap_or_default() as u32
        };
        match kind {
            "message_start" => {
                self.input_tokens = value
                    .pointer("/message/usage/input_tokens")
                    .and_then(|v| v.as_u64());
                http::Decoded::more(vec![])
            }
            // `id` + `name` arrive complete in the start event; only the
            // arguments stream afterwards (us-29).
            "content_block_start"
                if value
                    .pointer("/content_block/type")
                    .and_then(|t| t.as_str())
                    == Some("tool_use") =>
            {
                http::Decoded::more(vec![Ok(StreamEvent::ToolCallDelta(ToolCallDelta {
                    index: index(),
                    id: text("/content_block/id"),
                    name: text("/content_block/name"),
                    arguments: String::new(),
                }))])
            }
            "content_block_delta" => match value.pointer("/delta/type").and_then(|t| t.as_str()) {
                Some("text_delta") => http::Decoded::more(vec![Ok(StreamEvent::TextDelta {
                    text: text("/delta/text").unwrap_or_default(),
                })]),
                Some("input_json_delta") => {
                    http::Decoded::more(vec![Ok(StreamEvent::ToolCallDelta(ToolCallDelta {
                        index: index(),
                        id: None,
                        name: None,
                        arguments: text("/delta/partial_json").unwrap_or_default(),
                    }))])
                }
                _ => http::Decoded::more(vec![]),
            },
            // `delta.stop_reason` is the finish; `usage.output_tokens` is
            // cumulative. A message_delta without a stop reason carries
            // bookkeeping only.
            "message_delta" => match text("/delta/stop_reason") {
                Some(reason) => {
                    let input_tokens = self.input_tokens;
                    let output_tokens = value
                        .pointer("/usage/output_tokens")
                        .and_then(|v| v.as_u64());
                    // The schema reported nothing at all → `None`, not a
                    // Usage shell of Nones.
                    let usage =
                        (input_tokens.is_some() || output_tokens.is_some()).then_some(Usage {
                            input_tokens,
                            output_tokens,
                        });
                    http::Decoded::more(vec![Ok(StreamEvent::Finish {
                        reason: finish_reason(&reason),
                        usage,
                    })])
                }
                None => http::Decoded::more(vec![]),
            },
            "message_stop" => http::Decoded::over(vec![]),
            // Mid-stream errors (e.g. `overloaded_error` after the 200)
            // are provider-reported events — data, not machinery failure.
            "error" => http::Decoded::over(vec![Ok(StreamEvent::Error {
                message: text("/error/message").unwrap_or_default(),
                code: text("/error/type"),
            })]),
            _ => http::Decoded::more(vec![]),
        }
    }
}

/// Wire `stop_reason` → [`FinishReason`]; unknown strings (newer models
/// add values) degrade to `EndTurn` rather than failing the stream.
fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "max_tokens" => FinishReason::MaxTokens,
        "stop_sequence" => FinishReason::StopSequence,
        "tool_use" => FinishReason::ToolUse,
        "pause_turn" => FinishReason::PauseTurn,
        "refusal" => FinishReason::Refusal,
        _ => FinishReason::EndTurn,
    }
}
