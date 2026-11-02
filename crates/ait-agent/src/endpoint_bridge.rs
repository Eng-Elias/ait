//! Plumbing shared by the worker-thread channel APIs
//! ([`CommandSuggester`](crate::CommandSuggester),
//! [`AgentSession`](crate::AgentSession)):
//!
//! - [`llm_schema`] bridges `ait-config`'s endpoint-schema enum to
//!   `ait-llm`'s — the two are deliberately separate types (ait-llm
//!   owns its wire vocabulary).
//! - [`extract`]/[`Extracted`]/[`provider_error`] flatten the three
//!   schema-tagged [`EndpointStreamEvent`] IRs into what a channel
//!   event needs — the IRs stay distinct (§5.3); the flattening
//!   happens here at the channel edge, not in the IRs.
//! - [`slot`] locks the shared in-flight [`CancellationToken`] slot,
//!   recovering a poisoned guard — a poisoned
//!   `Option<CancellationToken>` is still sound to read (the token may
//!   simply be stale).

use std::sync::{Mutex, MutexGuard};

use ait_llm::{
    anthropic_messages, openai_chat_completions, openai_responses, EndpointSchema,
    EndpointStreamEvent,
};
use tokio_util::sync::CancellationToken;

/// What a schema-tagged [`EndpointStreamEvent`] means to a channel
/// event: text worth streaming, a provider-reported error worth
/// surfacing, or anything else (`Finish`, `ToolCallDelta`) the
/// consumer ignores for now.
pub(crate) enum Extracted {
    /// Assistant text.
    Text(String),
    /// A provider-reported error event, rendered for the channel.
    ProviderError(String),
    /// Everything else.
    Other,
}

/// Flatten the three per-schema event IRs into [`Extracted`].
pub(crate) fn extract(event: EndpointStreamEvent) -> Extracted {
    match event {
        EndpointStreamEvent::OpenAiChatCompletions(event) => match event {
            openai_chat_completions::StreamEvent::TextDelta { text } => Extracted::Text(text),
            openai_chat_completions::StreamEvent::Error { message, code } => {
                Extracted::ProviderError(provider_error(message, code))
            }
            _ => Extracted::Other,
        },
        EndpointStreamEvent::OpenAiResponses(event) => match event {
            openai_responses::StreamEvent::TextDelta { text } => Extracted::Text(text),
            openai_responses::StreamEvent::Error { message, code } => {
                Extracted::ProviderError(provider_error(message, code))
            }
            _ => Extracted::Other,
        },
        EndpointStreamEvent::AnthropicMessages(event) => match event {
            anthropic_messages::StreamEvent::TextDelta { text } => Extracted::Text(text),
            anthropic_messages::StreamEvent::Error { message, code } => {
                Extracted::ProviderError(provider_error(message, code))
            }
            _ => Extracted::Other,
        },
    }
}

/// A provider error event for the channel: the message, with the
/// machine-readable code appended when the endpoint sent one.
fn provider_error(message: String, code: Option<String>) -> String {
    match code {
        Some(code) => format!("{message} ({code})"),
        None => message,
    }
}

/// `ait-config`'s schema enum and `ait-llm`'s are deliberately separate
/// types (the llm crate owns its wire vocabulary); bridge them here —
/// the same seam `ait-llm`'s `chat` example uses. Two copies exist by
/// design (ait-llm stays config-free); keep them in sync with
/// `crates/ait-llm/examples/chat.rs`'s `llm_schema` when a fourth
/// `EndpointSchema` variant lands.
pub(crate) fn llm_schema(schema: ait_config::EndpointSchema) -> EndpointSchema {
    match schema {
        ait_config::EndpointSchema::OpenAiChatCompletions => EndpointSchema::OpenAiChatCompletions,
        ait_config::EndpointSchema::OpenAiResponses => EndpointSchema::OpenAiResponses,
        ait_config::EndpointSchema::AnthropicMessages => EndpointSchema::AnthropicMessages,
    }
}

/// Lock the shared cancellation-token slot, recovering a poisoned
/// guard — a poisoned `Option<CancellationToken>` is still sound to
/// read (the token may simply be stale).
pub(crate) fn slot(
    mutex: &Mutex<Option<CancellationToken>>,
) -> MutexGuard<'_, Option<CancellationToken>> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
