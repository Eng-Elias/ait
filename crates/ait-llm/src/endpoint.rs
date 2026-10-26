//! The [`Endpoint`] enum: one construction path and one dispatch point over
//! the three schema adapters (Spec R03 us-26).

use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::Stream;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::provider::EventStream;
use crate::{anthropic_messages, openai_chat_completions, openai_responses, LlmError, LlmProvider};

/// The wire format an endpoint speaks — the values of `schema = "…"` in
/// `settings.toml` (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointSchema {
    /// `POST {base_url}/chat/completions` — OpenAI plus compatible servers
    /// (Ollama, LM Studio, vLLM, …).
    OpenAiChatCompletions,
    /// `POST {base_url}/responses` — OpenAI's Responses API.
    OpenAiResponses,
    /// `POST {base_url}/messages` — Anthropic's Messages API.
    AnthropicMessages,
}

impl fmt::Display for EndpointSchema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OpenAiChatCompletions => "openai_chat_completions",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
        })
    }
}

/// Everything an adapter needs to reach one model on one endpoint.
///
/// This is the crate's configuration input (Spec R03): `ait-config`'s
/// resolved model role is bridged into it by the caller — `ait-llm` never
/// sees `ait-config` types.
#[derive(Clone)]
pub struct EndpointConfig {
    /// Base URL of the endpoint, including any version prefix —
    /// e.g. `http://localhost:11434/v1` (§4.1 `base_url`). Adapters append
    /// the resource path to it.
    pub base_url: String,
    /// The model name sent on the wire (§4.1 `models[].name`).
    pub model: String,
    /// The resolved secret, if the endpoint has one. `None` sends no
    /// `Authorization`/`x-api-key` header at all (us-21).
    pub secret: Option<String>,
    /// Which wire schema the endpoint speaks — selects the adapter in
    /// [`Endpoint::new`].
    pub schema: EndpointSchema,
}

// CODING_STANDARDS §4: secrets never reach logs — derive would print it.
impl fmt::Debug for EndpointConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .field("schema", &self.schema)
            .finish()
    }
}

/// One entry point over the three schema adapters (us-26).
///
/// [`Endpoint::new`] is the single construction path: it picks the adapter
/// by [`EndpointConfig::schema`]. Consumers `match` on the variant (or on
/// the [`EndpointStream`] they get back) to reach the schema-specific IR —
/// the IRs stay distinct (§5.3).
pub enum Endpoint {
    /// The `openai_chat_completions` adapter.
    OpenAiChatCompletions(openai_chat_completions::Provider),
    /// The `openai_responses` adapter.
    OpenAiResponses(openai_responses::Provider),
    /// The `anthropic_messages` adapter.
    AnthropicMessages(anthropic_messages::Provider),
}

impl Endpoint {
    /// Build the adapter matching `config.schema`.
    ///
    /// Fails only if the shared HTTP client cannot be built — the request
    /// path itself lands in R03-T5.
    pub fn new(config: EndpointConfig) -> Result<Self, LlmError> {
        Ok(match config.schema {
            EndpointSchema::OpenAiChatCompletions => {
                Self::OpenAiChatCompletions(openai_chat_completions::Provider::new(config)?)
            }
            EndpointSchema::OpenAiResponses => {
                Self::OpenAiResponses(openai_responses::Provider::new(config)?)
            }
            EndpointSchema::AnthropicMessages => {
                Self::AnthropicMessages(anthropic_messages::Provider::new(config)?)
            }
        })
    }

    /// The endpoint schema this endpoint speaks.
    pub fn schema(&self) -> EndpointSchema {
        match self {
            Self::OpenAiChatCompletions(p) => p.schema(),
            Self::OpenAiResponses(p) => p.schema(),
            Self::AnthropicMessages(p) => p.schema(),
        }
    }

    /// The endpoint configuration this endpoint was built from.
    pub fn config(&self) -> &EndpointConfig {
        match self {
            Self::OpenAiChatCompletions(p) => p.config(),
            Self::OpenAiResponses(p) => p.config(),
            Self::AnthropicMessages(p) => p.config(),
        }
    }

    /// Dispatch `request` to the right adapter (us-26..30).
    ///
    /// `request`'s variant must match the endpoint's schema — anything else
    /// is a [`LlmError::SchemaMismatch`], since mixing IRs across schemas is
    /// exactly what the per-schema IR rule forbids (§5.3).
    pub async fn stream(
        &self,
        request: EndpointRequest,
        cancel: CancellationToken,
    ) -> Result<EndpointStream, LlmError> {
        match (self, request) {
            (Self::OpenAiChatCompletions(p), EndpointRequest::OpenAiChatCompletions(r)) => Ok(
                EndpointStream::OpenAiChatCompletions(p.stream(r, cancel).await?),
            ),
            (Self::OpenAiResponses(p), EndpointRequest::OpenAiResponses(r)) => {
                Ok(EndpointStream::OpenAiResponses(p.stream(r, cancel).await?))
            }
            (Self::AnthropicMessages(p), EndpointRequest::AnthropicMessages(r)) => Ok(
                EndpointStream::AnthropicMessages(p.stream(r, cancel).await?),
            ),
            (endpoint, request) => Err(LlmError::SchemaMismatch {
                endpoint: endpoint.schema(),
                request: request.schema(),
            }),
        }
    }
}

/// A request IR tagged by schema, for [`Endpoint::stream`].
#[derive(Debug)]
pub enum EndpointRequest {
    /// An `openai_chat_completions` request.
    OpenAiChatCompletions(openai_chat_completions::Request),
    /// An `openai_responses` request.
    OpenAiResponses(openai_responses::Request),
    /// An `anthropic_messages` request.
    AnthropicMessages(anthropic_messages::Request),
}

impl EndpointRequest {
    /// The schema this request was built for.
    pub fn schema(&self) -> EndpointSchema {
        match self {
            Self::OpenAiChatCompletions(_) => EndpointSchema::OpenAiChatCompletions,
            Self::OpenAiResponses(_) => EndpointSchema::OpenAiResponses,
            Self::AnthropicMessages(_) => EndpointSchema::AnthropicMessages,
        }
    }
}

/// What [`Endpoint::stream`] hands back: the schema-matched event stream.
///
/// `match` on it to consume the schema-specific [`StreamEvent`]s directly —
/// or use it as a single [`Stream`] of tagged [`EndpointStreamEvent`]s.
pub enum EndpointStream {
    /// An `openai_chat_completions` event stream.
    OpenAiChatCompletions(EventStream<openai_chat_completions::StreamEvent>),
    /// An `openai_responses` event stream.
    OpenAiResponses(EventStream<openai_responses::StreamEvent>),
    /// An `anthropic_messages` event stream.
    AnthropicMessages(EventStream<anthropic_messages::StreamEvent>),
}

// The boxed streams have no Debug impl; name the variant instead.
impl fmt::Debug for EndpointStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::OpenAiChatCompletions(_) => "EndpointStream::OpenAiChatCompletions(..)",
            Self::OpenAiResponses(_) => "EndpointStream::OpenAiResponses(..)",
            Self::AnthropicMessages(_) => "EndpointStream::AnthropicMessages(..)",
        })
    }
}

/// A [`StreamEvent`](LlmProvider::StreamEvent) tagged by schema — the
/// unified item type when an [`EndpointStream`] is consumed as one
/// [`Stream`]. The wrapped events stay per-schema (§5.3).
#[derive(Debug)]
pub enum EndpointStreamEvent {
    /// An `openai_chat_completions` event.
    OpenAiChatCompletions(openai_chat_completions::StreamEvent),
    /// An `openai_responses` event.
    OpenAiResponses(openai_responses::StreamEvent),
    /// An `anthropic_messages` event.
    AnthropicMessages(anthropic_messages::StreamEvent),
}

impl Stream for EndpointStream {
    type Item = Result<EndpointStreamEvent, LlmError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            Self::OpenAiChatCompletions(s) => s
                .as_mut()
                .poll_next(cx)
                .map(|o| o.map(|r| r.map(EndpointStreamEvent::OpenAiChatCompletions))),
            Self::OpenAiResponses(s) => s
                .as_mut()
                .poll_next(cx)
                .map(|o| o.map(|r| r.map(EndpointStreamEvent::OpenAiResponses))),
            Self::AnthropicMessages(s) => s
                .as_mut()
                .poll_next(cx)
                .map(|o| o.map(|r| r.map(EndpointStreamEvent::AnthropicMessages))),
        }
    }
}

/// The HTTP client shared by every adapter.
///
/// Spec R03 us-33: a 10 s connect timeout, a stream idle-timeout enforced
/// at the event pump (R03-T5) — and deliberately no whole-request deadline
/// so long generations are never killed.
pub(crate) fn shared_client() -> Result<reqwest::Client, LlmError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(LlmError::from)
}
