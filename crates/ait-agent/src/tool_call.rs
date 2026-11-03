//! Streamed tool calls (Spec R06, §5.6): every schema streams a call
//! as `ToolCallDelta` fragments — index-addressed
//! (`openai_chat_completions`, `anthropic_messages`) or
//! `item_id`-addressed (`openai_responses`) — with `id`/`name`
//! announced once and the `arguments` JSON arriving in pieces.
//! [`CallAccumulator`] folds a turn's fragments into complete
//! [`ToolCall`]s in model order; the loop gates and executes them
//! sequentially.
//!
//! The cross-schema "the model asked for tools" signal is end-of-
//! stream with ≥1 accumulated call — `Finish` reasons differ per
//! schema (`tool_calls`, `completed`, `tool_use`), so the accumulator
//! does not read them.

use ait_llm::{anthropic_messages, openai_chat_completions, openai_responses, EndpointStreamEvent};

/// One complete tool invocation, assembled from a turn's streamed
/// deltas. `arguments` is the raw concatenated JSON fragment — it is
/// parsed once, at execution (for `anthropic_messages` the
/// conversation also re-encodes it into the `tool_use` block's
/// `input`). `id` is the provider's call id (`call_…`/`toolu_…`) the
/// tool-result item answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// The provider-assigned call id the result item references.
    pub id: String,
    /// The registry name — `read_files`, `grep`, …
    pub name: String,
    /// The call's JSON arguments, as one concatenated string.
    pub arguments: String,
}

/// A tool card's status transition, emitted as
/// [`AgentEvent::ToolUpdate`](crate::AgentEvent::ToolUpdate) (Spec R06
/// us-5). `AwaitingApproval` is not listed: the approve card is its
/// own event — a call either asks first or starts `Running`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Approved (or gate-exempt) and executing.
    Running,
    /// Executed; the result entered the conversation.
    Done,
    /// The user denied the call; the denial text became its result.
    Denied,
    /// The tool ran and reported an error (bad arguments, path escape,
    /// oversized read, executor unavailable) — still a tool result to
    /// the model, never a task failure.
    Failed,
}

/// A turn's in-progress calls — fragments keyed by first-seen order,
/// which for every addressing scheme is the model's own order
/// (parallel-call indices announce in order; `item_id`s announce at
/// item creation).
pub(crate) struct CallAccumulator {
    calls: Vec<(String, Pending)>,
}

/// The fragments seen so far for one call.
#[derive(Default)]
struct Pending {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl CallAccumulator {
    /// An empty accumulator for one model turn.
    pub(crate) fn new() -> Self {
        Self { calls: Vec::new() }
    }

    /// Fold one stream event in — a no-op for anything that is not a
    /// schema's `ToolCallDelta`.
    pub(crate) fn push(&mut self, event: &EndpointStreamEvent) {
        match event {
            EndpointStreamEvent::OpenAiChatCompletions(
                openai_chat_completions::StreamEvent::ToolCallDelta(delta),
            ) => {
                self.fragment(
                    &delta.index.to_string(),
                    &delta.id,
                    &delta.name,
                    &delta.arguments,
                );
            }
            EndpointStreamEvent::OpenAiResponses(openai_responses::StreamEvent::ToolCallDelta(
                delta,
            )) => {
                // Arguments deltas address the item, not the call;
                // `call_id` arrives on the announce fragment only.
                self.fragment(
                    delta.item_id.as_deref().unwrap_or_default(),
                    &delta.call_id,
                    &delta.name,
                    &delta.arguments,
                );
            }
            EndpointStreamEvent::AnthropicMessages(
                anthropic_messages::StreamEvent::ToolCallDelta(delta),
            ) => {
                self.fragment(
                    &delta.index.to_string(),
                    &delta.id,
                    &delta.name,
                    &delta.arguments,
                );
            }
            _ => {}
        }
    }

    /// Fold one fragment into its call — `id`/`name` overwrite only
    /// when the fragment carries them (they announce once),
    /// `arguments` always appends.
    fn fragment(&mut self, key: &str, id: &Option<String>, name: &Option<String>, args: &str) {
        let pending = match self.calls.iter_mut().find(|(k, _)| k == key) {
            Some((_, pending)) => pending,
            None => {
                self.calls.push((key.to_string(), Pending::default()));
                &mut self.calls.last_mut().expect("just pushed").1
            }
        };
        if let Some(id) = id {
            pending.id = Some(id.clone());
        }
        if let Some(name) = name {
            pending.name = Some(name.clone());
        }
        pending.arguments.push_str(args);
    }

    /// The turn's complete calls in model order. A fragment with an
    /// empty name is kept — the executor turns it into a tool error
    /// the model can react to rather than dropping its reply slot.
    pub(crate) fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_iter()
            .map(|(_, pending)| ToolCall {
                id: pending.id.unwrap_or_default(),
                name: pending.name.unwrap_or_default(),
                arguments: pending.arguments,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    //! Fragment-accumulation units (Spec R06 testing decisions — pure
    //! seams): hand-built per-schema delta events in, ordered
    //! [`ToolCall`]s out.

    use super::*;

    /// A chat-schema delta event.
    fn chat(index: u32, id: Option<&str>, name: Option<&str>, args: &str) -> EndpointStreamEvent {
        EndpointStreamEvent::OpenAiChatCompletions(
            openai_chat_completions::StreamEvent::ToolCallDelta(
                openai_chat_completions::ToolCallDelta {
                    index,
                    id: id.map(str::to_owned),
                    name: name.map(str::to_owned),
                    arguments: args.to_string(),
                },
            ),
        )
    }

    /// A responses-schema delta event.
    fn responses(
        item: Option<&str>,
        call: Option<&str>,
        name: Option<&str>,
        args: &str,
    ) -> EndpointStreamEvent {
        EndpointStreamEvent::OpenAiResponses(openai_responses::StreamEvent::ToolCallDelta(
            openai_responses::ToolCallDelta {
                item_id: item.map(str::to_owned),
                call_id: call.map(str::to_owned),
                name: name.map(str::to_owned),
                arguments: args.to_string(),
            },
        ))
    }

    /// An anthropic-schema delta event.
    fn anthropic(
        index: u32,
        id: Option<&str>,
        name: Option<&str>,
        args: &str,
    ) -> EndpointStreamEvent {
        EndpointStreamEvent::AnthropicMessages(anthropic_messages::StreamEvent::ToolCallDelta(
            anthropic_messages::ToolCallDelta {
                index,
                id: id.map(str::to_owned),
                name: name.map(str::to_owned),
                arguments: args.to_string(),
            },
        ))
    }

    /// Index-addressed fragments assemble in announce order —
    /// including two interleaved calls keeping their own buffers.
    #[test]
    fn chat_fragments_assemble_per_index_in_order() {
        let mut acc = CallAccumulator::new();
        acc.push(&chat(0, Some("call_a"), Some("read_files"), ""));
        acc.push(&chat(1, Some("call_b"), Some("grep"), ""));
        acc.push(&chat(0, None, None, "{\"paths\":["));
        acc.push(&chat(1, None, None, "{\"pattern\":"));
        acc.push(&chat(0, None, None, "\"a.rs\"]}"));
        acc.push(&chat(1, None, None, "\"x\"}"));

        assert_eq!(
            acc.finish(),
            vec![
                ToolCall {
                    id: "call_a".into(),
                    name: "read_files".into(),
                    arguments: "{\"paths\":[\"a.rs\"]}".into(),
                },
                ToolCall {
                    id: "call_b".into(),
                    name: "grep".into(),
                    arguments: "{\"pattern\":\"x\"}".into(),
                },
            ]
        );
    }

    /// `item_id`-addressed fragments pick up `call_id`/`name` from the
    /// announce event and accumulate only argument pieces afterwards.
    #[test]
    fn responses_fragments_address_by_item_id() {
        let mut acc = CallAccumulator::new();
        acc.push(&responses(Some("fc_1"), Some("call_1"), Some("glob"), ""));
        acc.push(&responses(Some("fc_1"), None, None, "{\"pattern\":"));
        acc.push(&responses(Some("fc_1"), None, None, " \"**/*.rs\"}"));

        assert_eq!(
            acc.finish(),
            vec![ToolCall {
                id: "call_1".into(),
                name: "glob".into(),
                arguments: "{\"pattern\": \"**/*.rs\"}".into(),
            }]
        );
    }

    /// Content-block-index addressing assembles identically to the
    /// chat schema's call indexing.
    #[test]
    fn anthropic_fragments_assemble_per_block_index() {
        let mut acc = CallAccumulator::new();
        acc.push(&anthropic(1, Some("toolu_1"), Some("read_files"), ""));
        acc.push(&anthropic(1, None, None, "{\"paths\":[\"x\"]}"));

        assert_eq!(
            acc.finish(),
            vec![ToolCall {
                id: "toolu_1".into(),
                name: "read_files".into(),
                arguments: "{\"paths\":[\"x\"]}".into(),
            }]
        );
    }

    /// Text/finish events and an empty turn accumulate nothing.
    #[test]
    fn non_tool_events_accumulate_nothing() {
        let mut acc = CallAccumulator::new();
        acc.push(&EndpointStreamEvent::OpenAiChatCompletions(
            openai_chat_completions::StreamEvent::TextDelta { text: "hi".into() },
        ));
        acc.push(&EndpointStreamEvent::OpenAiChatCompletions(
            openai_chat_completions::StreamEvent::Finish {
                reason: openai_chat_completions::FinishReason::Stop,
                usage: None,
            },
        ));
        assert!(acc.finish().is_empty());
    }
}
