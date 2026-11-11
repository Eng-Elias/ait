//! Context-limit summarization policy (Spec R08 §5.8, ticket
//! R08-T7): the pure seams the worker's `run_loop` consults before
//! every model request.
//!
//! - [`effective_limit`] resolves the threshold's denominator:
//!   the active profile's `context_window_limit` when it is >0,
//!   else the resolved model's `context_window`, else `None` —
//!   summarization is off (us-19).
//! - [`covered_span`] picks the cut: the oldest ~half of the
//!   transcript, advanced to a boundary that never splits a
//!   `tool_call`/`tool_result` or `ask`/`answer` pair (us-21).
//! - [`render`] turns the covered span into the text the summarize
//!   call digests — items an earlier `summary` marker already
//!   replaced are skipped exactly like replay skips them.
//! - [`prompt`] wraps the rendered span in the summarization
//!   instruction; [`request`] builds the one-shot
//!   [`EndpointRequest`] in the endpoint's own schema (the task's
//!   coding endpoint — no tools, no system prompt).
//! - [`carrying_plan`] appends the current plan verbatim inside the
//!   summary text (us-20) — a `plan` item inside the covered span
//!   never replays, so the TODO list survives only through the
//!   marker.
//!
//! The token estimate the caller feeds the threshold is either the
//! provider's reported `usage` (`endpoint_bridge::reported_tokens`)
//! or `chars /`[`CHARS_PER_TOKEN`] over the serialized conversation
//! items — both computed in the worker, none of it here.

use std::collections::HashMap;

use ait_llm::{
    anthropic_messages, openai_chat_completions, openai_responses, EndpointRequest, EndpointSchema,
};

use crate::conversation::plan_context;
use crate::transcript::{PlanTodo, TranscriptItem};

/// Rough characters-per-token for the fallback estimate — the
/// chars/4 heuristic Spec R08 names. Provider-reported `usage`
/// always wins when the stream carries it.
pub(crate) const CHARS_PER_TOKEN: usize = 4;

/// The effective context limit (us-19): the active profile's
/// `context_window_limit` when set (>0), else the resolved model's
/// `context_window`, else `None` — no limit, no summarization.
pub(crate) fn effective_limit(profile_limit: usize, model_window: Option<usize>) -> Option<usize> {
    if profile_limit > 0 {
        Some(profile_limit)
    } else {
        model_window
    }
}

/// The summarization cut (us-20/21): the number of leading
/// transcript items the summary replaces — the `covers_seq` the
/// marker stamps. The target is the oldest ~half; the cut then
/// advances to the first boundary that splits no pair: every
/// `tool_call` keeps its `tool_result` and every `ask` its `answer`
/// on the same side. `None` when no safe boundary exists before
/// the tail — a giant unbroken call chain leaves nothing safe to
/// cover, and the check retries at a later turn.
pub(crate) fn covered_span(items: &[TranscriptItem]) -> Option<usize> {
    let target = items.len() / 2;
    if target == 0 {
        return None;
    }
    // `opener[i]` = the index of the item `i` closes a pair with —
    // a `tool_result`'s `tool_call`, an `answer`'s `ask`. The
    // boundary at `cut` is safe iff no closer at `>= cut` answers an
    // opener at `< cut`; the suffix minimum of `opener` answers that
    // in O(1) per candidate.
    let mut open_calls: HashMap<&str, usize> = HashMap::new();
    let mut open_asks: Vec<usize> = Vec::new();
    let mut opener = vec![usize::MAX; items.len()];
    for (index, item) in items.iter().enumerate() {
        match item {
            TranscriptItem::ToolCall { id, .. } => {
                open_calls.insert(id.as_str(), index);
            }
            TranscriptItem::Ask { .. } => open_asks.push(index),
            TranscriptItem::ToolResult { id, .. } => {
                if let Some(open) = open_calls.remove(id.as_str()) {
                    opener[index] = open;
                }
            }
            TranscriptItem::Answer { .. } => {
                // The answer pairs with the most recent open ask —
                // the synthesized replay id does the same.
                if let Some(open) = open_asks.pop() {
                    opener[index] = open;
                }
            }
            _ => {}
        }
    }
    let mut suffix_min = vec![usize::MAX; items.len() + 1];
    for index in (0..items.len()).rev() {
        suffix_min[index] = suffix_min[index + 1].min(opener[index]);
    }
    (target..items.len()).find(|&cut| suffix_min[cut] >= cut)
}

/// The covered span as prompt text — one line per item in the
/// transcript vocabulary. `summary` markers inside the span carry
/// their own rule with them: the items an earlier marker already
/// replaced are skipped (the marker's text renders instead), so the
/// model's summary never sees the pre-replacement raw history
/// twice — the same coverage rule [`Conversation::from_transcript`]
/// applies at replay.
pub(crate) fn render(items: &[TranscriptItem]) -> String {
    let mut covered = 0usize;
    for item in items {
        if let TranscriptItem::Summary { covers_seq, .. } = item {
            covered = covered.max(*covers_seq as usize);
        }
    }
    let mut out = String::new();
    for (index, item) in items.iter().enumerate() {
        if index < covered {
            continue;
        }
        match item {
            TranscriptItem::Task { text } => push_line(&mut out, "task", text),
            TranscriptItem::Steering { text } => push_line(&mut out, "steering", text),
            TranscriptItem::AssistantText { text } => push_line(&mut out, "assistant", text),
            TranscriptItem::ToolCall { name, args, .. } => {
                push_line(&mut out, &format!("call {name}"), args);
            }
            TranscriptItem::ToolResult { text, status, .. } => {
                let status = match status {
                    crate::ToolStatus::Done => "done",
                    crate::ToolStatus::Denied => "denied",
                    crate::ToolStatus::Failed => "failed",
                    crate::ToolStatus::Running => "running",
                };
                push_line(&mut out, &format!("result ({status})"), text);
            }
            TranscriptItem::Ask { question, choices } => {
                if choices.is_empty() {
                    push_line(&mut out, "ask", question);
                } else {
                    push_line(
                        &mut out,
                        "ask",
                        &format!("{question} (choices: {})", choices.join(", ")),
                    );
                }
            }
            TranscriptItem::Answer { text } => push_line(&mut out, "answer", text),
            TranscriptItem::Status { text } => push_line(&mut out, "status", text),
            TranscriptItem::Summary { text, .. } => {
                push_line(&mut out, "earlier summary", text);
            }
            TranscriptItem::Plan { todos } => {
                out.push_str(&plan_context(todos));
                out.push('\n');
            }
        }
    }
    out
}

/// `label: text` on one line — the span's line format.
fn push_line(out: &mut String, label: &str, text: &str) {
    out.push_str(label);
    out.push_str(": ");
    out.push_str(text);
    out.push('\n');
}

/// The summarize call's whole prompt: the instruction plus the
/// rendered span. Written for a coding agent continuing mid-task —
/// the summary must carry decisions, paths and open work forward,
/// not retell them.
pub(crate) fn prompt(rendered: &str) -> String {
    format!(
        "You are compacting the oldest part of an AI coding agent's \
         transcript so the conversation keeps fitting the model's \
         context window. Summarize the excerpt below into a compact \
         brief the agent continues from: the task being worked, what \
         was tried and learned (tool calls and their outcomes), the \
         file paths and identifiers involved, and what remains. Keep \
         paths and identifiers verbatim. Reply with the summary text \
         only — no preamble, no headings.\n\n\
         <transcript>\n{rendered}</transcript>"
    )
}

/// The one-shot [`EndpointRequest`] the summarize call sends —
/// the rendered span plus the instruction, in the endpoint's own
/// schema IR (the task's coding endpoint, Spec R08). No tools: the
/// model must answer the summary, not act on it.
pub(crate) fn request(schema: EndpointSchema, model: &str, prompt: String) -> EndpointRequest {
    match schema {
        EndpointSchema::OpenAiChatCompletions => {
            EndpointRequest::OpenAiChatCompletions(openai_chat_completions::Request {
                model: model.to_owned(),
                messages: vec![openai_chat_completions::Message::User { content: prompt }],
                tools: Vec::new(),
                temperature: None,
                max_tokens: None,
                stream: true,
            })
        }
        EndpointSchema::OpenAiResponses => {
            EndpointRequest::OpenAiResponses(openai_responses::Request {
                model: model.to_owned(),
                instructions: None,
                input: vec![openai_responses::Input::Message(
                    openai_responses::Message {
                        role: openai_responses::Role::User,
                        content: vec![openai_responses::Content::InputText { text: prompt }],
                    },
                )],
                tools: Vec::new(),
                temperature: None,
                max_output_tokens: None,
                stream: true,
            })
        }
        EndpointSchema::AnthropicMessages => {
            EndpointRequest::AnthropicMessages(anthropic_messages::Request {
                model: model.to_owned(),
                messages: vec![anthropic_messages::Message {
                    role: anthropic_messages::Role::User,
                    content: vec![anthropic_messages::ContentBlock::Text { text: prompt }],
                }],
                system: None,
                max_tokens: crate::conversation::ANTHROPIC_MAX_TOKENS,
                tools: Vec::new(),
                temperature: None,
                stream: true,
            })
        }
    }
}

/// The current plan — the latest `plan` snapshot's TODO list
/// (`update_plan` replaces wholesale, so earlier snapshots were
/// never state) — or an empty slice when the transcript has none.
pub(crate) fn current_plan(items: &[TranscriptItem]) -> &[PlanTodo] {
    items
        .iter()
        .rev()
        .find_map(|item| match item {
            TranscriptItem::Plan { todos } => Some(todos.as_slice()),
            _ => None,
        })
        .unwrap_or(&[])
}

/// The marker's stored text: the model's summary plus the current
/// plan verbatim inside it (us-20). A `plan` item the covered span
/// swallowed never replays, so this carriage is the only way the
/// TODO list reaches the model after a rewrite — and when the
/// plan item DID survive, the block simply restates labeled state.
pub(crate) fn carrying_plan(text: &str, todos: &[PlanTodo]) -> String {
    if todos.is_empty() {
        text.to_owned()
    } else {
        format!("{}\n\n{}", text.trim_end(), plan_context(todos))
    }
}

#[cfg(test)]
mod tests {
    //! The policy seams (Spec R08 testing decisions): limit
    //! resolution order, the pair-preserving cut, the rendered span
    //! and plan-verbatim carriage — all pure, all asserted on the
    //! transcript vocabulary itself.

    use super::*;
    use crate::transcript::{PlanTodoStatus, TranscriptItem};
    use crate::ToolStatus;

    /// Item shorthands.
    fn task(text: &str) -> TranscriptItem {
        TranscriptItem::Task { text: text.into() }
    }

    fn text(s: &str) -> TranscriptItem {
        TranscriptItem::AssistantText { text: s.into() }
    }

    fn call(id: &str) -> TranscriptItem {
        TranscriptItem::ToolCall {
            id: id.into(),
            name: "read_files".into(),
            args: "{}".into(),
            summary: String::new(),
        }
    }

    fn result(id: &str) -> TranscriptItem {
        TranscriptItem::ToolResult {
            id: id.into(),
            text: "out".into(),
            status: ToolStatus::Done,
        }
    }

    /// Spec us-19: profile > model > off.
    #[test]
    fn the_limit_resolves_profile_then_model_then_off() {
        assert_eq!(effective_limit(8_000, Some(200_000)), Some(8_000));
        assert_eq!(effective_limit(0, Some(200_000)), Some(200_000));
        assert_eq!(effective_limit(0, None), None);
        // A profile limit with no model window still applies.
        assert_eq!(effective_limit(4_000, None), Some(4_000));
    }

    /// Too little transcript to halve is no span at all.
    #[test]
    fn a_single_item_never_summarizes() {
        assert_eq!(covered_span(&[]), None);
        assert_eq!(covered_span(&[task("t")]), None);
    }

    /// With no pairs in the way the cut is exactly half the items.
    #[test]
    fn the_cut_is_the_oldest_half() {
        let items = vec![task("t"), text("a"), text("b"), text("c")];
        assert_eq!(covered_span(&items), Some(2));
        let items = vec![task("t"), text("a"), text("b"), text("c"), text("d")];
        assert_eq!(covered_span(&items), Some(2));
    }

    /// Spec us-21: a cut landing inside a `tool_call`/`tool_result`
    /// pair advances until the whole pair sits on one side — here
    /// the call's result trails the half-mark, so the pair gets
    /// covered whole rather than split.
    #[test]
    fn the_cut_never_splits_a_call_result_pair() {
        // task, CALL, text, text, RESULT, text — target 3 covers the
        // call but strands its result; the cut must move past it.
        let items = vec![
            task("t"),
            call("c1"),
            text("a"),
            text("b"),
            result("c1"),
            text("c"),
        ];
        assert_eq!(covered_span(&items), Some(5));
        // A cut right before the pair keeps it whole — equally safe.
        let items = vec![task("t"), text("a"), call("c1"), result("c1"), text("b")];
        assert_eq!(covered_span(&items), Some(2));
    }

    /// The same for an `ask`/`answer` pair — the ask is its own
    /// card, the answer its result.
    #[test]
    fn the_cut_never_splits_an_ask_answer_pair() {
        let items = vec![
            task("t"),
            TranscriptItem::Ask {
                question: "Ship?".into(),
                choices: vec![],
            },
            text("a"),
            text("b"),
            TranscriptItem::Answer { text: "yes".into() },
            text("c"),
        ];
        assert_eq!(covered_span(&items), Some(5));
    }

    /// A plan item rides inside its `update_plan` call/result span —
    /// the pair rule keeps it attached automatically.
    #[test]
    fn a_plan_item_inside_a_pair_keeps_the_whole_span() {
        let items = vec![
            task("t"),
            text("a"),
            call("c1"),
            TranscriptItem::Plan {
                todos: vec![PlanTodo {
                    content: "x".into(),
                    status: PlanTodoStatus::Pending,
                }],
            },
            result("c1"),
            text("b"),
        ];
        assert_eq!(covered_span(&items), Some(5));
    }

    /// No safe boundary before the tail — one long pending call
    /// spanning the whole second half — is no span, not a bad cut.
    #[test]
    fn an_unbroken_pair_chain_leaves_nothing_to_cover() {
        let items = vec![task("t"), call("c1"), text("a"), text("b"), result("c1")];
        // target 2: every boundary < len strands the result.
        assert_eq!(covered_span(&items), None);
    }

    /// A call the run never answered (a parked card, a crash) holds
    /// no closer — it blocks no boundary.
    #[test]
    fn an_unanswered_call_blocks_no_boundary() {
        let items = vec![task("t"), text("a"), text("b"), call("c1")];
        assert_eq!(covered_span(&items), Some(2));
    }

    /// `render` labels each kind and skips what an earlier `summary`
    /// marker already replaced — replay's coverage rule verbatim.
    #[test]
    fn render_skips_what_an_earlier_marker_covered() {
        let items = vec![
            task("COVERED-TASK"),
            text("COVERED-TEXT"),
            TranscriptItem::Summary {
                text: "FIRST-SUMMARY".into(),
                covers_seq: 2,
            },
            task("kept task"),
            call("c1"),
            result("c1"),
        ];
        let rendered = render(&items);
        assert!(!rendered.contains("COVERED"), "{rendered}");
        assert!(rendered.contains("FIRST-SUMMARY"), "{rendered}");
        assert!(rendered.contains("task: kept task"), "{rendered}");
        assert!(rendered.contains("call read_files: {}"), "{rendered}");
        assert!(rendered.contains("result (done): out"), "{rendered}");
    }

    /// Every kind renders a labeled line — the digest the summarize
    /// call reads.
    #[test]
    fn every_kind_renders_a_labeled_line() {
        let items = vec![
            task("fix it"),
            TranscriptItem::Steering {
                text: "quickly".into(),
            },
            text("on it"),
            call("c1"),
            result("c1"),
            TranscriptItem::Ask {
                question: "which?".into(),
                choices: vec!["a".into(), "b".into()],
            },
            TranscriptItem::Answer { text: "a".into() },
            TranscriptItem::Status {
                text: "done".into(),
            },
            TranscriptItem::Plan {
                todos: vec![PlanTodo {
                    content: "ship".into(),
                    status: PlanTodoStatus::InProgress,
                }],
            },
        ];
        let rendered = render(&items);
        for needle in [
            "task: fix it",
            "steering: quickly",
            "assistant: on it",
            "call read_files: {}",
            "result (done): out",
            "ask: which? (choices: a, b)",
            "answer: a",
            "status: done",
            "- in progress: ship",
        ] {
            assert!(rendered.contains(needle), "{needle} missing: {rendered}");
        }
    }

    /// The prompt wraps the span in the summarization instruction —
    /// and asks for the summary text only.
    #[test]
    fn the_prompt_instructions_wrap_the_span() {
        let prompt = prompt("task: fix it\n");
        assert!(prompt.contains("<transcript>\ntask: fix it\n</transcript>"));
        assert!(prompt.contains("summary"), "{prompt}");
    }

    /// The summarize request is the endpoint's own schema IR — a
    /// single user message, no tools — for whichever adapter serves
    /// the task's coding endpoint.
    #[test]
    fn the_request_is_built_in_each_schemas_own_ir() {
        match request(EndpointSchema::OpenAiChatCompletions, "m", "PROMPT".into()) {
            EndpointRequest::OpenAiChatCompletions(r) => {
                assert_eq!(r.messages.len(), 1);
                assert!(r.tools.is_empty());
                assert!(matches!(
                    &r.messages[0],
                    openai_chat_completions::Message::User { content } if content == "PROMPT"
                ));
            }
            _ => panic!("chat schema builds a chat request"),
        }
        match request(EndpointSchema::OpenAiResponses, "m", "PROMPT".into()) {
            EndpointRequest::OpenAiResponses(r) => {
                assert_eq!(r.input.len(), 1);
                assert!(r.tools.is_empty());
            }
            _ => panic!("responses schema builds a responses request"),
        }
        match request(EndpointSchema::AnthropicMessages, "m", "PROMPT".into()) {
            EndpointRequest::AnthropicMessages(r) => {
                assert_eq!(r.messages.len(), 1);
                assert_eq!(r.messages[0].role, anthropic_messages::Role::User);
                assert!(r.tools.is_empty());
            }
            _ => panic!("anthropic schema builds an anthropic request"),
        }
    }

    /// Spec us-20: the current plan — the LATEST snapshot — is
    /// carried verbatim inside the summary text; no plan means the
    /// summary stands alone.
    #[test]
    fn the_current_plan_rides_verbatim_inside_the_summary() {
        let items = vec![
            TranscriptItem::Plan {
                todos: vec![PlanTodo {
                    content: "STALE".into(),
                    status: PlanTodoStatus::Done,
                }],
            },
            TranscriptItem::Plan {
                todos: vec![
                    PlanTodo {
                        content: "fix".into(),
                        status: PlanTodoStatus::InProgress,
                    },
                    PlanTodo {
                        content: "verify".into(),
                        status: PlanTodoStatus::Pending,
                    },
                ],
            },
        ];
        let todos = current_plan(&items);
        assert_eq!(todos.len(), 2, "the latest snapshot wins");
        let carried = carrying_plan("SUMMARY-TEXT", todos);
        assert!(carried.starts_with("SUMMARY-TEXT"), "{carried}");
        assert!(carried.contains("- in progress: fix"), "{carried}");
        assert!(carried.contains("- pending: verify"), "{carried}");
        assert!(!carried.contains("STALE"), "{carried}");
        // No plan at all leaves the text untouched.
        assert_eq!(carrying_plan("just text", &[]), "just text");
        assert_eq!(current_plan(&items[..1]).len(), 1);
        assert!(current_plan(&[]).is_empty());
    }
}
