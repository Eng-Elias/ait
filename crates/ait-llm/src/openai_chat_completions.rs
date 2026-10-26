//! IR and adapter for the `openai_chat_completions` endpoint schema —
//! `POST {base_url}/chat/completions` (§4.1). Covers OpenAI itself plus the
//! compatible servers (Ollama, LM Studio, vLLM, …).
//!
//! The types serialize to the wire shape directly: the adapter serializes
//! the [`Request`], forces `stream`/`stream_options` on and decodes
//! data-only SSE chunks into [`StreamEvent`]s (`data: [DONE]` ends the
//! stream; the trailing empty-`choices` chunk carries usage).

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::provider::EventStream;
use crate::{http, EndpointConfig, EndpointSchema, LlmError, LlmProvider};

/// The `openai_chat_completions` adapter (us-26).
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

    /// `POST {base_url}/chat/completions`; `Authorization: Bearer` only
    /// when a secret is configured (us-21).
    fn build_request(&self, request: &Request) -> Result<reqwest::Request, LlmError> {
        let mut body = serde_json::to_value(request)
            .map_err(|e| LlmError::Parse(format!("failed to serialize request: {e}")))?;
        if let Some(object) = body.as_object_mut() {
            // Streaming is the adapter's wire detail, not the caller's IR
            // choice; `include_usage` asks for the trailing usage chunk so
            // `Finish` can carry tokens.
            object.insert("stream".into(), true.into());
            object.insert(
                "stream_options".into(),
                serde_json::json!({"include_usage": true}),
            );
        }
        let mut builder = self
            .client
            .post(http::url(&self.config.base_url, "chat/completions"))
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
        EndpointSchema::OpenAiChatCompletions
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

// ---- SSE decode (wire → IR) ----

/// The `ChatCompletionChunk` wire shape. Fields the IR doesn't need —
/// `id`, `created`, `system_fingerprint`, `obfuscation`, `logprobs` — are
/// ignored so provider padding stays padding.
#[derive(Debug, Deserialize)]
struct Chunk {
    /// The chunk's choices; empty on the trailing usage chunk.
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    /// Token accounting — `null` until the final usage chunk.
    #[serde(default)]
    usage: Option<ChunkUsage>,
    /// Mid-stream provider errors arrive as `{"error": {…}}` payloads.
    #[serde(default)]
    error: Option<ChunkError>,
}

#[derive(Debug, Deserialize)]
struct ChunkChoice {
    /// The incremental payload.
    #[serde(default)]
    delta: ChunkDelta,
    /// `stop`/`length`/`tool_calls`/`content_filter` once finished.
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChunkDelta {
    /// A text fragment; `null` on tool-call-only chunks.
    #[serde(default)]
    content: Option<String>,
    /// Tool-call fragments keyed by `index` (us-29).
    #[serde(default)]
    tool_calls: Vec<ChunkToolCall>,
}

#[derive(Debug, Deserialize)]
struct ChunkToolCall {
    /// Which parallel call this fragment belongs to.
    #[serde(default)]
    index: u32,
    /// The call id — only on the first fragment of a call.
    #[serde(default)]
    id: Option<String>,
    /// The function fragment; absent fields mean "not this fragment".
    #[serde(default)]
    function: Option<ChunkFunction>,
}

#[derive(Debug, Default, Deserialize)]
struct ChunkFunction {
    /// The function name — only on the first fragment of a call.
    #[serde(default)]
    name: Option<String>,
    /// A fragment of the JSON arguments string.
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChunkUsage {
    /// Prompt tokens billed.
    #[serde(default)]
    prompt_tokens: u64,
    /// Completion tokens billed.
    #[serde(default)]
    completion_tokens: u64,
    /// Total tokens billed.
    #[serde(default)]
    total_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ChunkError {
    /// The provider's message.
    #[serde(default)]
    message: String,
    /// The provider's machine-readable code, when it sends one.
    #[serde(default)]
    code: Option<String>,
}

/// Stateful chunk → [`StreamEvent`] decoder.
///
/// A `finish_reason` chunk precedes the trailing empty-`choices` usage
/// chunk, so the `Finish` event is held until usage arrives or the stream
/// ends — `usage` is `None` when the provider (or a cut stream) sends none.
#[derive(Default)]
struct Decoder {
    /// A finish reason waiting for the usage chunk.
    pending_finish: Option<FinishReason>,
}

impl Decoder {
    /// Decode one SSE frame (`None` = the body ended — flush the held
    /// `Finish`, if any).
    fn decode(&mut self, event: Option<eventsource_stream::Event>) -> http::Decoded<StreamEvent> {
        let Some(event) = event else {
            return http::Decoded::over(self.flush());
        };
        // Data-only stream; the `event:` field is always empty. `[DONE]`
        // is a literal sentinel payload, not JSON.
        if event.data.trim() == "[DONE]" {
            return http::Decoded::over(self.flush());
        }
        match serde_json::from_str::<Chunk>(&event.data) {
            Err(e) => http::Decoded::over(vec![Err(LlmError::Parse(format!(
                "invalid chat-completion chunk: {e}"
            )))]),
            Ok(chunk) => http::Decoded::more(self.chunk(chunk)),
        }
    }

    /// Emit the held `Finish` on stream end (usage never arrived, or the
    /// `[DONE]` marker closed the stream first).
    fn flush(&mut self) -> Vec<Result<StreamEvent, LlmError>> {
        self.pending_finish
            .take()
            .map(|reason| {
                Ok(StreamEvent::Finish {
                    reason,
                    usage: None,
                })
            })
            .into_iter()
            .collect()
    }

    fn chunk(&mut self, chunk: Chunk) -> Vec<Result<StreamEvent, LlmError>> {
        let mut events = Vec::new();
        if let Some(error) = chunk.error {
            events.push(Ok(StreamEvent::Error {
                message: error.message,
                code: error.code,
            }));
        }
        for choice in chunk.choices {
            if let Some(text) = choice.delta.content {
                if !text.is_empty() {
                    events.push(Ok(StreamEvent::TextDelta { text }));
                }
            }
            for call in choice.delta.tool_calls {
                let function = call.function.unwrap_or_default();
                events.push(Ok(StreamEvent::ToolCallDelta(ToolCallDelta {
                    index: call.index,
                    id: call.id,
                    name: function.name,
                    arguments: function.arguments.unwrap_or_default(),
                })));
            }
            if let Some(reason) = choice.finish_reason {
                self.pending_finish = Some(finish_reason(&reason));
            }
        }
        // The trailing usage chunk (choices: []) completes a held finish.
        if let Some(usage) = chunk.usage {
            events.push(Ok(StreamEvent::Finish {
                reason: self.pending_finish.take().unwrap_or(FinishReason::Stop),
                usage: Some(Usage {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                }),
            }));
        }
        events
    }
}

/// Wire `finish_reason` → [`FinishReason`]; unknown strings (newer
/// providers add values) degrade to `Stop` rather than failing the stream.
fn finish_reason(reason: &str) -> FinishReason {
    match reason {
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        "function_call" => FinishReason::FunctionCall,
        _ => FinishReason::Stop,
    }
}
