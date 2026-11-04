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
use crate::transcript::TranscriptItem;

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

    /// The continuity seam (Spec R08 §5.8, ticket R08-T4): build a
    /// conversation by replaying `items` — the neutral persisted
    /// transcript the frontend hands over on
    /// [`AgentRequest::prior`](crate::AgentRequest) — into `schema`'s
    /// IR. Every kind maps into the schema's own shape:
    ///
    /// - `task`/`steering` → user messages, verbatim;
    /// - `assistant_text` → the turn's assistant text — merged into
    ///   the call item when calls follow, exactly like the live
    ///   loop's `push_tool_calls(&text, …)`;
    /// - `tool_call`/`tool_result` → the schema's call and result
    ///   items, paired by id — a call with no recorded result (a
    ///   crash mid-run, an approve card parked at exit) gains a
    ///   synthetic [`INTERRUPTED_RESULT`] so no `tool_use` dangles
    ///   (anthropic hard-requires the pair, Spec R08 us-9);
    /// - `ask`/`answer` → a synthesized `ask_user_question` call plus
    ///   its result — the transcript records no call id for asks
    ///   (the card IS the record), so replay invents one; an
    ///   unanswered ask closes `interrupted` like any other;
    /// - `summary` → drops the leading `covers_seq` items and
    ///   injects its text as leading context;
    /// - `plan` → the latest snapshot injected as context (replace-
    ///   whole-list semantics — earlier snapshots never were model
    ///   state);
    /// - `status` → panel state only; dropped.
    ///
    /// An empty `items` is a fresh conversation — identical to
    /// [`new`](Self::new). Because the transcript is schema-neutral,
    /// replaying into a different schema than the items were
    /// produced under — a mid-conversation endpoint switch — works
    /// for free.
    pub(crate) fn from_transcript(
        schema: EndpointSchema,
        items: &[TranscriptItem],
        profile: Option<&ExecutionProfile>,
    ) -> Self {
        // Coverage: a `summary` marker replaces the leading items its
        // `covers_seq` counts — a prefix, since seqs run contiguous
        // from zero. Later markers cover earlier ones, so the
        // effective cut is the maximum.
        let mut covered = 0usize;
        for item in items {
            if let TranscriptItem::Summary { covers_seq, .. } = item {
                covered = covered.max(*covers_seq as usize);
            }
        }
        // Only the LAST `plan` snapshot reaches the model —
        // `update_plan` replaces the list wholesale, so earlier
        // snapshots were never conversation state.
        let last_plan = items
            .iter()
            .rposition(|item| matches!(item, TranscriptItem::Plan { .. }));

        let mut conversation = Self::new(schema, profile);
        let mut turn = ReplayTurn::default();
        // Whether any IR item has been appended — the first replayed
        // content must open on a user message (see `lead`).
        let mut emitted = false;
        let mut asks = 0usize;
        for (index, item) in items.iter().enumerate() {
            if index < covered {
                continue;
            }
            match item {
                TranscriptItem::Task { text } | TranscriptItem::Steering { text } => {
                    turn.flush(&mut conversation, &mut emitted);
                    conversation.push_user(text.clone());
                    emitted = true;
                }
                // Held, not pushed: when calls follow, the text merges
                // into their assistant item like the live loop's —
                // otherwise the flush lands it as `push_assistant`.
                // A text arriving while calls are already buffered
                // belongs to a LATER turn (the live stream orders a
                // turn's deltas before its calls), so it opens fresh.
                TranscriptItem::AssistantText { text } => {
                    if !turn.calls.is_empty() {
                        turn.flush(&mut conversation, &mut emitted);
                    }
                    turn.text.push_str(text);
                }
                TranscriptItem::ToolCall { id, name, args, .. } => {
                    turn.calls.push(ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: args.clone(),
                    });
                }
                TranscriptItem::ToolResult { id, text, .. } => {
                    turn.results.push((id.clone(), text.clone()));
                }
                TranscriptItem::Ask { question, choices } => {
                    // The ask card records no call id — the card IS
                    // the record — so the replayed call synthesizes
                    // one; its pairing with the result is what the
                    // schema requires, not the provider's original id.
                    asks += 1;
                    let id = format!("ask_replay_{asks}");
                    turn.calls.push(ToolCall {
                        id: id.clone(),
                        name: tools::ASK_USER_QUESTION.to_string(),
                        arguments: serde_json::json!({
                            "question": question,
                            "choices": choices,
                        })
                        .to_string(),
                    });
                    turn.open_ask = Some(id);
                }
                TranscriptItem::Answer { text } => {
                    // The answer IS its ask's tool result — the send
                    // side already skipped the `ToolUpdate` duplicate.
                    if let Some(id) = turn.open_ask.take() {
                        turn.results.push((id, text.clone()));
                    }
                }
                // Panel state, never model context — but it DOES end
                // an open turn: a crash mid-run leaves a parked call
                // that must close before the status lands.
                TranscriptItem::Status { .. } => turn.flush(&mut conversation, &mut emitted),
                TranscriptItem::Summary { text, .. } => {
                    turn.flush(&mut conversation, &mut emitted);
                    conversation.push_user(format!("{SUMMARY_CONTEXT}\n{text}"));
                    emitted = true;
                }
                TranscriptItem::Plan { todos } => {
                    if Some(index) == last_plan {
                        turn.flush(&mut conversation, &mut emitted);
                        conversation.push_user(plan_context(todos));
                        emitted = true;
                    }
                }
            }
        }
        turn.flush(&mut conversation, &mut emitted);
        conversation
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

/// The synthetic result a `tool_call` with no recorded `tool_result`
/// gains on replay (Spec R08 us-9): a crash mid-run, or an
/// approve/ask card parked when the session ended. Anthropic
/// hard-requires every `tool_use` to pair with a `tool_result`, and
/// the text tells the model why the call produced nothing.
const INTERRUPTED_RESULT: &str = "(interrupted — the run ended before this call produced a result)";

/// The label on a `summary` marker's injected text — context about
/// the earlier conversation, not a fresh user instruction.
const SUMMARY_CONTEXT: &str = "[the earlier conversation, summarized]";

/// The user item replay opens with when a slice's first model
/// content would be assistant text — a coverage cut or a partial
/// record starting mid-turn. Anthropic rejects a conversation whose
/// first message is `role:"assistant"` outright, so the marker keeps
/// the rebuilt IR valid whatever the slice holds.
const RESUMED_CONTEXT: &str = "[resumed conversation — earlier items unavailable]";

/// One assistant turn's buffered pieces while a transcript replays
/// ([`Conversation::from_transcript`]). Transcript items are
/// sequential — assistant text, then per call its `tool_call` and
/// `tool_result`, interleaved — but `push_tool_calls`/
/// `push_tool_results` take a whole turn at once, so calls and
/// results buffer until the turn closes (the next non-call/
/// non-result item, or the end of the slice) and land in one
/// assistant item and one results item exactly like the live loop
/// appends them.
#[derive(Default)]
struct ReplayTurn {
    /// The turn's assistant text — merges into `push_tool_calls`
    /// when calls buffered, else lands via `push_assistant`.
    text: String,
    /// The turn's calls in order — recorded `tool_call` items plus
    /// the calls `ask` items synthesize.
    calls: Vec<ToolCall>,
    /// Recorded `tool_result`/`answer` texts by call id — pairing
    /// happens at flush, so interleaved items order-free.
    results: Vec<(String, String)>,
    /// The synthesized id of the open `ask`, if any — the id its
    /// `answer` item attaches to.
    open_ask: Option<String>,
}

impl ReplayTurn {
    /// Close the turn: buffered calls land as one assistant item
    /// followed by their results — in call order, each answered by
    /// its recorded `tool_result` or the synthetic
    /// [`INTERRUPTED_RESULT`] (orphan results match no call and
    /// drop). A call-free turn lands its text via `push_assistant`.
    fn flush(&mut self, conversation: &mut Conversation, emitted: &mut bool) {
        if !self.calls.is_empty() {
            lead(conversation, emitted);
            conversation.push_tool_calls(&self.text, &self.calls);
            let results: Vec<String> = self
                .calls
                .iter()
                .map(|call| {
                    self.results
                        .iter()
                        .find(|(id, _)| id == &call.id)
                        .map(|(_, text)| text.clone())
                        .unwrap_or_else(|| INTERRUPTED_RESULT.to_string())
                })
                .collect();
            conversation.push_tool_results(&self.calls, &results);
        } else if !self.text.is_empty() {
            lead(conversation, emitted);
            conversation.push_assistant(std::mem::take(&mut self.text));
        }
        self.calls.clear();
        self.results.clear();
        self.text.clear();
        self.open_ask = None;
    }
}

/// Guarantee the first IR item the replay emits is user-role
/// ([`RESUMED_CONTEXT`]) — a slice opening on assistant content
/// would otherwise produce a conversation anthropic rejects.
fn lead(conversation: &mut Conversation, emitted: &mut bool) {
    if !*emitted {
        conversation.push_user(RESUMED_CONTEXT.to_string());
        *emitted = true;
    }
}

/// The latest `plan` snapshot as injected context — labeled state,
/// not a fresh instruction.
fn plan_context(todos: &[crate::transcript::PlanTodo]) -> String {
    let mut text = String::from("[the conversation's current plan]");
    for todo in todos {
        let status = match todo.status {
            crate::transcript::PlanTodoStatus::Pending => "pending",
            crate::transcript::PlanTodoStatus::InProgress => "in progress",
            crate::transcript::PlanTodoStatus::Done => "done",
        };
        text.push_str(&format!("\n- {status}: {}", todo.content));
    }
    text
}

#[cfg(test)]
mod tests {
    //! The replay seam (Spec R08 §5.8, ticket R08-T4): a transcript —
    //! the neutral persisted vocabulary — rebuilds whichever schema
    //! IR the resolved `coding_model` speaks, so continuity survives
    //! an endpoint switch the live IR could never carry. Assertions
    //! run on the typed request [`Conversation::request`] produces —
    //! the same shape the adapter serializes onto the wire.

    use super::*;
    use crate::transcript::{PlanTodo, PlanTodoStatus, TranscriptItem};

    /// A `ToolResult` item shorthand.
    fn result(id: &str, text: &str) -> TranscriptItem {
        TranscriptItem::ToolResult {
            id: id.into(),
            text: text.into(),
            status: crate::ToolStatus::Done,
        }
    }

    /// A `ToolCall` item shorthand.
    fn call(id: &str, name: &str) -> TranscriptItem {
        TranscriptItem::ToolCall {
            id: id.into(),
            name: name.into(),
            args: "{}".into(),
            summary: String::new(),
        }
    }

    /// Replay `items` under `schema` and return the built request —
    /// typed, so tests read the IR directly.
    fn replay(schema: EndpointSchema, items: &[TranscriptItem]) -> EndpointRequest {
        Conversation::from_transcript(schema, items, None).request("test-model", "SYSTEM")
    }

    /// The chat request's messages with the leading system item
    /// stripped — the transcript never produces one.
    fn chat_messages(items: &[TranscriptItem]) -> Vec<openai_chat_completions::Message> {
        match replay(EndpointSchema::OpenAiChatCompletions, items) {
            EndpointRequest::OpenAiChatCompletions(request) => {
                assert_eq!(
                    request.messages.first(),
                    Some(&openai_chat_completions::Message::System {
                        content: "SYSTEM".into(),
                    }),
                    "the system prompt still leads at request time"
                );
                request.messages[1..].to_vec()
            }
            _ => panic!("chat schema builds a chat request"),
        }
    }

    /// The anthropic request's messages.
    fn anthropic_messages(items: &[TranscriptItem]) -> Vec<anthropic_messages::Message> {
        match replay(EndpointSchema::AnthropicMessages, items) {
            EndpointRequest::AnthropicMessages(request) => request.messages,
            _ => panic!("anthropic schema builds an anthropic request"),
        }
    }

    /// The responses request's input items.
    fn responses_input(items: &[TranscriptItem]) -> Vec<openai_responses::Input> {
        match replay(EndpointSchema::OpenAiResponses, items) {
            EndpointRequest::OpenAiResponses(request) => request.input,
            _ => panic!("responses schema builds a responses request"),
        }
    }

    /// The corpus: one of every recordable kind, in the order a real
    /// task's drain produces them — user text, an assistant turn
    /// (text + calls + results), an ask round-trip, terminal status,
    /// and the snapshot kinds T5/T7 emit.
    fn corpus() -> Vec<TranscriptItem> {
        vec![
            TranscriptItem::Task {
                text: "fix the tests".into(),
            },
            TranscriptItem::AssistantText {
                text: "let me look".into(),
            },
            call("call_1", "read_files"),
            result("call_1", "NOTE-CONTENTS"),
            TranscriptItem::Ask {
                question: "Ship it?".into(),
                choices: vec!["yes".into(), "no".into()],
            },
            TranscriptItem::Answer { text: "no".into() },
            TranscriptItem::AssistantText {
                text: "all read".into(),
            },
            TranscriptItem::Steering {
                text: "and lint too".into(),
            },
            TranscriptItem::Status {
                text: "done".into(),
            },
            TranscriptItem::Plan {
                todos: vec![PlanTodo {
                    content: "verify".into(),
                    status: PlanTodoStatus::Pending,
                }],
            },
        ]
    }

    /// Every kind lands in the chat IR's own shape: user text, an
    /// assistant turn merging text + calls into ONE message, tool
    /// messages per result, the ask as a synthesized
    /// `ask_user_question` call whose answer is its result — and
    /// `status` never reaching the model at all.
    #[test]
    fn every_item_kind_lands_in_the_chat_ir() {
        let messages = chat_messages(&corpus());
        let (user, assistant, tool): (Vec<_>, Vec<_>, Vec<_>) = {
            let mut u = Vec::new();
            let mut a = Vec::new();
            let mut t = Vec::new();
            for message in &messages {
                match message {
                    openai_chat_completions::Message::User { .. } => u.push(message),
                    openai_chat_completions::Message::Assistant { .. } => a.push(message),
                    openai_chat_completions::Message::Tool { .. } => t.push(message),
                    openai_chat_completions::Message::System { .. } => {}
                }
            }
            (u, a, t)
        };
        // task + steering + the injected plan snapshot = user items.
        assert_eq!(user.len(), 3, "{messages:?}");
        assert!(
            user[0]
                == &openai_chat_completions::Message::User {
                    content: "fix the tests".into(),
                }
        );
        // The plan snapshot is labeled context, not fresh instruction.
        match user[2] {
            openai_chat_completions::Message::User { content } => {
                assert!(content.contains("verify"), "{content}");
            }
            _ => unreachable!(),
        }
        // Two assistant messages: the tool turn (text + call_1 + the
        // synthesized ask call in ONE message, like the live loop)
        // and the trailing text turn.
        assert_eq!(assistant.len(), 2, "{messages:?}");
        match &assistant[0] {
            openai_chat_completions::Message::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some("let me look"));
                assert_eq!(tool_calls.len(), 2, "{messages:?}");
                assert_eq!(tool_calls[0].id, "call_1");
                assert_eq!(tool_calls[0].function.name, "read_files");
                assert_eq!(tool_calls[1].function.name, "ask_user_question");
                let args: serde_json::Value =
                    serde_json::from_str(&tool_calls[1].function.arguments).expect("ask args");
                assert_eq!(args["question"], "Ship it?");
                assert_eq!(args["choices"], serde_json::json!(["yes", "no"]));
            }
            _ => unreachable!(),
        }
        assert_eq!(
            assistant[1],
            &openai_chat_completions::Message::Assistant {
                content: Some("all read".into()),
                tool_calls: vec![],
            }
        );
        // Two tool results: the recorded one and the ask's answer.
        assert_eq!(tool.len(), 2, "{messages:?}");
        assert!(tool.contains(&&openai_chat_completions::Message::Tool {
            tool_call_id: "call_1".into(),
            content: "NOTE-CONTENTS".into(),
        }));
        assert!(tool.iter().any(|message| matches!(
            message,
            openai_chat_completions::Message::Tool { content, .. } if content == "no"
        )));
        // `done` is panel state, not model context.
        assert!(!messages.iter().any(|message| matches!(
            message,
            openai_chat_completions::Message::User { content } if content == "done"
        )));
    }

    /// The same corpus replays into the anthropic IR: calls pack
    /// `tool_use` blocks into the assistant message and ALL of a
    /// turn's results pack into ONE user message of `tool_result`
    /// blocks — the pairing the API hard-requires.
    #[test]
    fn every_item_kind_lands_in_the_anthropic_ir() {
        let messages = anthropic_messages(&corpus());
        let tool_uses: Vec<_> = messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                anthropic_messages::ContentBlock::ToolUse { id, name, input } => {
                    Some((id, name, input))
                }
                _ => None,
            })
            .collect();
        let tool_results: Vec<_> = messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                anthropic_messages::ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                } => Some((tool_use_id, content)),
                _ => None,
            })
            .collect();
        assert_eq!(tool_uses.len(), 2, "{messages:?}");
        assert_eq!(tool_uses[0].0, "call_1");
        assert_eq!(
            tool_uses[0].2,
            &serde_json::json!({}),
            "the recorded args string re-parses into `input`"
        );
        assert_eq!(tool_uses[1].1, "ask_user_question");
        // Anthropic's pairing rule: every tool_use has a tool_result.
        assert_eq!(tool_results.len(), 2, "{messages:?}");
        for (id, ..) in &tool_uses {
            assert!(
                tool_results.iter().any(|(use_id, _)| use_id == id),
                "no tool_result answers {id}: {messages:?}"
            );
        }
        // Results ride user-role messages; the first message is user.
        assert_eq!(
            messages.first().map(|m| m.role),
            Some(anthropic_messages::Role::User)
        );
    }

    /// The responses IR: calls and results are sibling input items —
    /// `function_call` per call, `function_call_output` per result.
    #[test]
    fn every_item_kind_lands_in_the_responses_ir() {
        let input = responses_input(&corpus());
        let calls: Vec<_> = input
            .iter()
            .filter_map(|item| match item {
                openai_responses::Input::FunctionCall(call) => Some(call),
                _ => None,
            })
            .collect();
        let outputs: Vec<_> = input
            .iter()
            .filter_map(|item| match item {
                openai_responses::Input::FunctionCallOutput { call_id, output } => {
                    Some((call_id, output))
                }
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2, "{input:?}");
        assert_eq!(calls[0].call_id, "call_1");
        assert_eq!(calls[0].name, "read_files");
        assert_eq!(calls[1].name, "ask_user_question");
        assert_eq!(outputs.len(), 2, "{input:?}");
        assert!(outputs
            .iter()
            .any(|(id, text)| id.as_str() == "call_1" && text.as_str() == "NOTE-CONTENTS"));
        assert!(outputs.iter().any(|(_, text)| text.as_str() == "no"));
    }

    /// A `tool_call` the run never answered — a crash mid-run or a
    /// parked approve card at exit (Spec R08 us-9) — gains a
    /// synthetic "interrupted" result so no `tool_use` dangles; the
    /// model reads why the call produced nothing.
    #[test]
    fn an_unanswered_call_gains_a_synthetic_interrupted_result() {
        let items = vec![
            TranscriptItem::Task {
                text: "do it".into(),
            },
            call("call_9", "run_command"),
        ];
        for schema in [
            EndpointSchema::OpenAiChatCompletions,
            EndpointSchema::OpenAiResponses,
            EndpointSchema::AnthropicMessages,
        ] {
            let body = match replay(schema, &items) {
                EndpointRequest::OpenAiChatCompletions(r) => {
                    serde_json::to_string(&r.messages).unwrap()
                }
                EndpointRequest::OpenAiResponses(r) => serde_json::to_string(&r.input).unwrap(),
                EndpointRequest::AnthropicMessages(r) => {
                    serde_json::to_string(&r.messages).unwrap()
                }
            };
            assert!(body.contains("call_9"), "{schema}: {body}");
            assert!(body.contains("interrupted"), "{schema}: {body}");
        }
        // Anthropic pairing, asserted directly: the interrupted
        // result shares the call's id in a user message.
        let messages = anthropic_messages(&items);
        let (uses, results): (Vec<_>, Vec<_>) = (
            messages
                .iter()
                .flat_map(|m| &m.content)
                .filter(|b| matches!(b, anthropic_messages::ContentBlock::ToolUse { .. }))
                .collect(),
            messages
                .iter()
                .flat_map(|m| &m.content)
                .filter(|b| matches!(b, anthropic_messages::ContentBlock::ToolResult { .. }))
                .collect(),
        );
        assert_eq!(uses.len(), 1);
        assert_eq!(results.len(), 1);
        match (uses[0], results[0]) {
            (
                anthropic_messages::ContentBlock::ToolUse { id, .. },
                anthropic_messages::ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                },
            ) => {
                assert_eq!(id, tool_use_id);
                assert!(content.contains("interrupted"), "{content}");
            }
            _ => unreachable!(),
        }
    }

    /// An ask parked at exit — an `ask` whose `answer` never came —
    /// closes the same way: its synthesized call gains the
    /// interrupted result.
    #[test]
    fn an_unanswered_ask_gains_a_synthetic_interrupted_result() {
        let messages = anthropic_messages(&[
            TranscriptItem::Task { text: "t".into() },
            TranscriptItem::Ask {
                question: "Ship?".into(),
                choices: vec![],
            },
        ]);
        let results: Vec<_> = messages
            .iter()
            .flat_map(|m| &m.content)
            .filter(|b| matches!(b, anthropic_messages::ContentBlock::ToolResult { .. }))
            .collect();
        assert_eq!(results.len(), 1, "{messages:?}");
        match results[0] {
            anthropic_messages::ContentBlock::ToolResult { content, .. } => {
                assert!(content.contains("interrupted"), "{content}");
            }
            _ => unreachable!(),
        }
    }

    /// A `summary` marker drops the leading items it covers and
    /// injects its text as leading context — the covered task and
    /// tool turn never reach the model.
    #[test]
    fn a_summary_marker_drops_covered_items_and_injects_its_text() {
        // seqs: 0=task, 1=text, 2=call, 3=result, 4=summary(0..4),
        // 5=task, 6=text.
        let items = vec![
            TranscriptItem::Task {
                text: "COVERED-TASK".into(),
            },
            TranscriptItem::AssistantText {
                text: "COVERED-TEXT".into(),
            },
            call("covered_call", "glob"),
            result("covered_call", "COVERED-RESULT"),
            TranscriptItem::Summary {
                text: "SUMMARY-TEXT".into(),
                covers_seq: 4,
            },
            TranscriptItem::Task {
                text: "kept task".into(),
            },
            TranscriptItem::AssistantText {
                text: "kept text".into(),
            },
        ];
        let messages = chat_messages(&items);
        let serialized = serde_json::to_string(&messages).unwrap();
        assert!(!serialized.contains("COVERED"), "{serialized}");
        assert!(serialized.contains("SUMMARY-TEXT"), "{serialized}");
        assert!(serialized.contains("kept task"), "{serialized}");
        assert!(serialized.contains("kept text"), "{serialized}");
        // The injected summary leads the surviving context.
        assert!(
            matches!(&messages[0], openai_chat_completions::Message::User { content } if content.contains("SUMMARY-TEXT")),
            "{messages:?}"
        );
    }

    /// A slice whose first model item would be assistant content — a
    /// coverage cut or partial record starting mid-turn — still opens
    /// with a user message: anthropic rejects `role:"assistant"`
    /// first outright, so replay inserts a resume marker.
    #[test]
    fn a_mid_turn_opening_keeps_the_first_message_user() {
        let items = vec![TranscriptItem::AssistantText {
            text: "mid-conversation text".into(),
        }];
        let messages = anthropic_messages(&items);
        assert_eq!(
            messages.first().map(|m| m.role),
            Some(anthropic_messages::Role::User),
            "{messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|m| m.role == anthropic_messages::Role::Assistant),
            "{messages:?}"
        );
    }

    /// An empty transcript is exactly today's fresh conversation —
    /// `prior` empty ⇒ the request carries only the injected system
    /// prompt, identical to `Conversation::new`.
    #[test]
    fn an_empty_transcript_is_a_fresh_conversation() {
        for schema in [
            EndpointSchema::OpenAiChatCompletions,
            EndpointSchema::OpenAiResponses,
            EndpointSchema::AnthropicMessages,
        ] {
            let fresh = Conversation::new(schema, None).request("test-model", "SYSTEM");
            let replayed = replay(schema, &[]);
            // EndpointRequest has no Serialize — compare per variant.
            match (fresh, replayed) {
                (
                    EndpointRequest::OpenAiChatCompletions(a),
                    EndpointRequest::OpenAiChatCompletions(b),
                ) => assert_eq!(a.messages, b.messages),
                (EndpointRequest::OpenAiResponses(a), EndpointRequest::OpenAiResponses(b)) => {
                    assert_eq!(a.input, b.input)
                }
                (EndpointRequest::AnthropicMessages(a), EndpointRequest::AnthropicMessages(b)) => {
                    assert_eq!(a.messages, b.messages)
                }
                _ => panic!("same schema both ways"),
            }
        }
    }

    /// The ticket's freebie: one transcript rebuilds into EVERY
    /// schema — resuming onto a different resolved `coding_model`
    /// (even a different endpoint schema) loses nothing.
    #[test]
    fn one_transcript_rebuilds_every_schema() {
        let items = corpus();
        for schema in [
            EndpointSchema::OpenAiChatCompletions,
            EndpointSchema::OpenAiResponses,
            EndpointSchema::AnthropicMessages,
        ] {
            let body = match replay(schema, &items) {
                EndpointRequest::OpenAiChatCompletions(r) => serde_json::to_string(&r).unwrap(),
                EndpointRequest::OpenAiResponses(r) => serde_json::to_string(&r).unwrap(),
                EndpointRequest::AnthropicMessages(r) => serde_json::to_string(&r).unwrap(),
            };
            assert!(body.contains("fix the tests"), "{schema}: {body}");
            assert!(body.contains("NOTE-CONTENTS"), "{schema}: {body}");
            assert!(body.contains("ask_user_question"), "{schema}: {body}");
        }
    }
}
