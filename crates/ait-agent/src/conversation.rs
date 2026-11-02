//! The agent conversation, kept in the endpoint schema's own IR
//! (Spec R06 us-36, §5.3's no-unify rule): one variant per schema
//! holding that schema's message list. User tasks, assistant replies
//! and steering messages append in the schema's shape; the system
//! prompt is injected at request build time (top-level `system` /
//! `instructions` fields, or the leading `system` message in
//! `openai_chat_completions`).
//!
//! Tool-call and tool-result items land in these same vectors when the
//! registry ticket arrives — nothing about their schema-specific
//! shapes forces a different container.

use ait_llm::{
    anthropic_messages, openai_chat_completions, openai_responses, EndpointRequest, EndpointSchema,
};

/// `anthropic_messages` requires `max_tokens` — the schema has no
/// default. Generous enough for long agent replies; the `chat` example
/// uses 4096 for smoke answers.
const ANTHROPIC_MAX_TOKENS: u32 = 8192;

/// One task's conversation in a single endpoint schema's message IR.
/// Created from the resolved endpoint's schema, so the
/// [`EndpointRequest`] it builds can never trip
/// [`LlmError::SchemaMismatch`](ait_llm::LlmError).
pub(crate) enum Conversation {
    /// `openai_chat_completions` messages (system excluded — injected
    /// at request build time).
    OpenAiChatCompletions(Vec<openai_chat_completions::Message>),
    /// `openai_responses` input items.
    OpenAiResponses(Vec<openai_responses::Input>),
    /// `anthropic_messages` messages.
    AnthropicMessages(Vec<anthropic_messages::Message>),
}

impl Conversation {
    /// An empty conversation for `schema`.
    pub(crate) fn new(schema: EndpointSchema) -> Self {
        match schema {
            EndpointSchema::OpenAiChatCompletions => Self::OpenAiChatCompletions(Vec::new()),
            EndpointSchema::OpenAiResponses => Self::OpenAiResponses(Vec::new()),
            EndpointSchema::AnthropicMessages => Self::AnthropicMessages(Vec::new()),
        }
    }

    /// Append a user message — the task text or a steering message,
    /// verbatim (never redacted, the typed-request rule).
    pub(crate) fn push_user(&mut self, text: String) {
        match self {
            Self::OpenAiChatCompletions(messages) => {
                messages.push(openai_chat_completions::Message::User { content: text });
            }
            Self::OpenAiResponses(input) => {
                input.push(openai_responses::Input::Message(
                    openai_responses::Message {
                        role: openai_responses::Role::User,
                        content: vec![openai_responses::Content::InputText { text }],
                    },
                ));
            }
            Self::AnthropicMessages(messages) => {
                messages.push(anthropic_messages::Message {
                    role: anthropic_messages::Role::User,
                    content: vec![anthropic_messages::ContentBlock::Text { text }],
                });
            }
        }
    }

    /// Append a completed assistant reply so later turns see it. An
    /// empty reply is skipped: `anthropic_messages` rejects empty text
    /// blocks, and a no-content assistant item carries nothing anyway.
    pub(crate) fn push_assistant(&mut self, text: String) {
        if text.is_empty() {
            return;
        }
        match self {
            Self::OpenAiChatCompletions(messages) => {
                messages.push(openai_chat_completions::Message::Assistant {
                    content: Some(text),
                    tool_calls: vec![],
                });
            }
            Self::OpenAiResponses(input) => {
                input.push(openai_responses::Input::Message(
                    openai_responses::Message {
                        role: openai_responses::Role::Assistant,
                        content: vec![openai_responses::Content::OutputText { text }],
                    },
                ));
            }
            Self::AnthropicMessages(messages) => {
                messages.push(anthropic_messages::Message {
                    role: anthropic_messages::Role::Assistant,
                    content: vec![anthropic_messages::ContentBlock::Text { text }],
                });
            }
        }
    }

    /// Build this turn's request in the schema's own IR: the
    /// conversation plus `model` and the freshly assembled `system`
    /// prompt. No tools yet — the registry ticket fills `tools`.
    pub(crate) fn request(&self, model: &str, system: &str) -> EndpointRequest {
        match self {
            Self::OpenAiChatCompletions(messages) => {
                let mut full = Vec::with_capacity(messages.len() + 1);
                full.push(openai_chat_completions::Message::System {
                    content: system.to_owned(),
                });
                full.extend(messages.iter().cloned());
                EndpointRequest::OpenAiChatCompletions(openai_chat_completions::Request {
                    model: model.to_owned(),
                    messages: full,
                    tools: vec![],
                    temperature: None,
                    max_tokens: None,
                    stream: true,
                })
            }
            Self::OpenAiResponses(input) => {
                EndpointRequest::OpenAiResponses(openai_responses::Request {
                    model: model.to_owned(),
                    instructions: Some(system.to_owned()),
                    input: input.clone(),
                    tools: vec![],
                    temperature: None,
                    max_output_tokens: None,
                    stream: true,
                })
            }
            Self::AnthropicMessages(messages) => {
                EndpointRequest::AnthropicMessages(anthropic_messages::Request {
                    model: model.to_owned(),
                    messages: messages.clone(),
                    system: Some(system.to_owned()),
                    max_tokens: ANTHROPIC_MAX_TOKENS,
                    tools: vec![],
                    temperature: None,
                    stream: true,
                })
            }
        }
    }
}
