//! The agent conversation, kept in the endpoint schema's own IR
//! (Spec R06 us-36, §5.3's no-unify rule): one variant per schema
//! holding that schema's message list and the registry's tool
//! declarations translated into its `Tool` shape. User tasks,
//! assistant replies, steering messages, assistant tool calls and
//! tool results all append in the schema's shape; the system prompt
//! is injected at request build time (top-level `system` /
//! `instructions` fields, or the leading `system` message in
//! `openai_chat_completions`).

use ait_config::ExecutionProfile;
use ait_llm::{
    anthropic_messages, openai_chat_completions, openai_responses, EndpointRequest, EndpointSchema,
};

use crate::tool_call::ToolCall;
use crate::tools::{self, ToolDecls};

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
    /// at request build time) plus the schema's tool declarations.
    OpenAiChatCompletions {
        /// The conversation so far.
        messages: Vec<openai_chat_completions::Message>,
        /// The registry in this schema's `Tool` IR.
        tools: Vec<openai_chat_completions::Tool>,
    },
    /// `openai_responses` input items plus tool declarations.
    OpenAiResponses {
        /// The conversation so far.
        input: Vec<openai_responses::Input>,
        /// The registry in this schema's `Tool` IR.
        tools: Vec<openai_responses::Tool>,
    },
    /// `anthropic_messages` messages plus tool declarations.
    AnthropicMessages {
        /// The conversation so far.
        messages: Vec<anthropic_messages::Message>,
        /// The registry in this schema's `Tool` IR.
        tools: Vec<anthropic_messages::Tool>,
    },
}

impl Conversation {
    /// An empty conversation for `schema`, carrying the tool registry
    /// translated into the schema's `Tool` shape ([`tools::declare`])
    /// under the task's active `profile` — the §5.7 stance decides
    /// which tools declare the `agent_decides` `safety` argument
    /// (R07-T2); `None` declares the registry verbatim.
    pub(crate) fn new(schema: EndpointSchema, profile: Option<&ExecutionProfile>) -> Self {
        match tools::declare(schema, profile) {
            ToolDecls::Chat(tools) => Self::OpenAiChatCompletions {
                messages: Vec::new(),
                tools,
            },
            ToolDecls::Responses(tools) => Self::OpenAiResponses {
                input: Vec::new(),
                tools,
            },
            ToolDecls::Anthropic(tools) => Self::AnthropicMessages {
                messages: Vec::new(),
                tools,
            },
        }
    }

    /// Append a user message — the task text or a steering message,
    /// verbatim (never redacted, the typed-request rule).
    pub(crate) fn push_user(&mut self, text: String) {
        match self {
            Self::OpenAiChatCompletions { messages, .. } => {
                messages.push(openai_chat_completions::Message::User { content: text });
            }
            Self::OpenAiResponses { input, .. } => {
                input.push(openai_responses::Input::Message(
                    openai_responses::Message {
                        role: openai_responses::Role::User,
                        content: vec![openai_responses::Content::InputText { text }],
                    },
                ));
            }
            Self::AnthropicMessages { messages, .. } => {
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
            Self::OpenAiChatCompletions { messages, .. } => {
                messages.push(openai_chat_completions::Message::Assistant {
                    content: Some(text),
                    tool_calls: vec![],
                });
            }
            Self::OpenAiResponses { input, .. } => {
                input.push(openai_responses::Input::Message(
                    openai_responses::Message {
                        role: openai_responses::Role::Assistant,
                        content: vec![openai_responses::Content::OutputText { text }],
                    },
                ));
            }
            Self::AnthropicMessages { messages, .. } => {
                messages.push(anthropic_messages::Message {
                    role: anthropic_messages::Role::Assistant,
                    content: vec![anthropic_messages::ContentBlock::Text { text }],
                });
            }
        }
    }

    /// Append the assistant item(s) carrying a tool turn's calls —
    /// the schema's own call IR, text included when the model wrote
    /// any (Spec R06 us-36: assistant calls in each endpoint's own
    /// shape). `arguments` stays the streamed string for
    /// chat/responses; anthropic's `input` field wants it decoded —
    /// an unparseable fragment becomes `null` (the call's own result
    /// reports the bad arguments, so the model still learns it
    /// failed).
    pub(crate) fn push_tool_calls(&mut self, text: &str, calls: &[ToolCall]) {
        match self {
            Self::OpenAiChatCompletions { messages, .. } => {
                messages.push(openai_chat_completions::Message::Assistant {
                    content: if text.is_empty() {
                        None
                    } else {
                        Some(text.to_string())
                    },
                    tool_calls: calls
                        .iter()
                        .map(|call| openai_chat_completions::ToolCall {
                            id: call.id.clone(),
                            kind: openai_chat_completions::ToolKind::Function,
                            function: openai_chat_completions::FunctionCall {
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            },
                        })
                        .collect(),
                });
            }
            Self::OpenAiResponses { input, .. } => {
                if !text.is_empty() {
                    input.push(openai_responses::Input::Message(
                        openai_responses::Message {
                            role: openai_responses::Role::Assistant,
                            content: vec![openai_responses::Content::OutputText {
                                text: text.to_string(),
                            }],
                        },
                    ));
                }
                input.extend(calls.iter().map(|call| {
                    openai_responses::Input::FunctionCall(openai_responses::FunctionCall {
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    })
                }));
            }
            Self::AnthropicMessages { messages, .. } => {
                let mut content = Vec::with_capacity(calls.len() + 1);
                if !text.is_empty() {
                    content.push(anthropic_messages::ContentBlock::Text {
                        text: text.to_string(),
                    });
                }
                content.extend(calls.iter().map(|call| {
                    anthropic_messages::ContentBlock::ToolUse {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: serde_json::from_str(&call.arguments)
                            .unwrap_or(serde_json::Value::Null),
                    }
                }));
                messages.push(anthropic_messages::Message {
                    role: anthropic_messages::Role::Assistant,
                    content,
                });
            }
        }
    }

    /// Append the tool-result item(s) answering `calls`, in call
    /// order — one result item per call, each in the schema's own
    /// shape (a `tool` message, a `function_call_output` item, or a
    /// `tool_result` block inside a `user` message). `results` is
    /// parallel to `calls`; the texts arrive already redacted.
    pub(crate) fn push_tool_results(&mut self, calls: &[ToolCall], results: &[String]) {
        debug_assert_eq!(calls.len(), results.len());
        match self {
            Self::OpenAiChatCompletions { messages, .. } => {
                for (call, result) in calls.iter().zip(results) {
                    messages.push(openai_chat_completions::Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: result.clone(),
                    });
                }
            }
            Self::OpenAiResponses { input, .. } => {
                for (call, result) in calls.iter().zip(results) {
                    input.push(openai_responses::Input::FunctionCallOutput {
                        call_id: call.id.clone(),
                        output: result.clone(),
                    });
                }
            }
            Self::AnthropicMessages { messages, .. } => {
                messages.push(anthropic_messages::Message {
                    role: anthropic_messages::Role::User,
                    content: calls
                        .iter()
                        .zip(results)
                        .map(
                            |(call, result)| anthropic_messages::ContentBlock::ToolResult {
                                tool_use_id: call.id.clone(),
                                content: result.clone(),
                            },
                        )
                        .collect(),
                });
            }
        }
    }

    /// Build this turn's request in the schema's own IR: the
    /// conversation, `model`, the freshly assembled `system` prompt
    /// and the schema's tool declarations.
    pub(crate) fn request(&self, model: &str, system: &str) -> EndpointRequest {
        match self {
            Self::OpenAiChatCompletions { messages, tools } => {
                let mut full = Vec::with_capacity(messages.len() + 1);
                full.push(openai_chat_completions::Message::System {
                    content: system.to_owned(),
                });
                full.extend(messages.iter().cloned());
                EndpointRequest::OpenAiChatCompletions(openai_chat_completions::Request {
                    model: model.to_owned(),
                    messages: full,
                    tools: tools.clone(),
                    temperature: None,
                    max_tokens: None,
                    stream: true,
                })
            }
            Self::OpenAiResponses { input, tools } => {
                EndpointRequest::OpenAiResponses(openai_responses::Request {
                    model: model.to_owned(),
                    instructions: Some(system.to_owned()),
                    input: input.clone(),
                    tools: tools.clone(),
                    temperature: None,
                    max_output_tokens: None,
                    stream: true,
                })
            }
            Self::AnthropicMessages { messages, tools } => {
                EndpointRequest::AnthropicMessages(anthropic_messages::Request {
                    model: model.to_owned(),
                    messages: messages.clone(),
                    system: Some(system.to_owned()),
                    max_tokens: ANTHROPIC_MAX_TOKENS,
                    tools: tools.clone(),
                    temperature: None,
                    stream: true,
                })
            }
        }
    }
}
