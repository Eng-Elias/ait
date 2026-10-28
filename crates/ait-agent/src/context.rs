//! The context-assembly pure seam (Spec R04, user stories 15–19, 27):
//! cwd, shell, OS and the last [`OUTPUT_LINE_BUDGET`] logical lines of
//! terminal output plus the typed request become a single-turn
//! [`EndpointRequest`] — a system prompt demanding one bare shell
//! command, a user message carrying the assembled context.
//!
//! Redaction ordering (§5.4, §5.10.3): terminal output lines pass
//! through [`redact`](crate::redact) here, at assembly time — never
//! continuously on the PTY stream, and never on the typed request
//! itself (us-19). Blocks replace grid lines as the line source in
//! Round 5 through this same seam.

use ait_llm::{
    anthropic_messages, openai_chat_completions, openai_responses, EndpointRequest, EndpointSchema,
};

use crate::redact;

/// How many recent output lines a request may carry — §5.4's "last N
/// lines", a fixed budget per the spec (configurability is a later
/// round's decision). 100 lines covers several screens of scrollback
/// without bloating the prompt.
pub const OUTPUT_LINE_BUDGET: usize = 100;

/// `anthropic_messages` requires `max_tokens` — the schema has no
/// default. A one-line command never approaches this cap; it only
/// bounds a runaway reply.
const ANTHROPIC_MAX_TOKENS: u32 = 1024;

/// The fixed system prompt (§5.4): "reply with one shell command, no
/// markdown, no explanation". Written in-house (CODING_STANDARDS §10);
/// the assembled context rides in the user message instead.
const SYSTEM_PROMPT: &str = "You turn a natural-language request into a shell command for the \
     user's terminal. Reply with one shell command, no markdown, no \
     explanation.";

/// The terminal-side context bound to one `# ` or `Alt+Enter` request —
/// everything §5.4 lets the model see. Fields stay borrowed; the
/// frontend owns them.
pub struct SuggestContext<'a> {
    /// The tab's working directory (OSC 7 with a `/proc` fallback).
    pub cwd: &'a str,
    /// The shell name — `$SHELL`'s basename, e.g. `bash` (us-17).
    pub shell: &'a str,
    /// The OS name — `std::env::consts::OS`, e.g. `linux` (us-17).
    pub os: &'a str,
    /// Recent logical output lines, oldest first. Only the last
    /// [`OUTPUT_LINE_BUDGET`] are sent and each passes through
    /// [`redact`](crate::redact) (us-16, 18).
    pub output_lines: &'a [String],
    /// The typed request — sent verbatim, never redacted (us-19).
    pub request: &'a str,
}

/// Assemble the single-turn provider request for `context` (§5.4).
/// Pure — no I/O, no clock; same inputs, same request.
///
/// `schema` and `model` come from the resolved endpoint
/// (`endpoint.schema()`, `endpoint.config().model`), so the returned
/// [`EndpointRequest`] variant can never hit
/// [`LlmError::SchemaMismatch`](ait_llm::LlmError); the caller hands it
/// straight to `Endpoint::stream`.
pub fn assemble_request(
    context: &SuggestContext,
    schema: EndpointSchema,
    model: &str,
) -> EndpointRequest {
    let system = String::from(SYSTEM_PROMPT);
    let user = user_content(context);
    match schema {
        EndpointSchema::OpenAiChatCompletions => {
            EndpointRequest::OpenAiChatCompletions(openai_chat_completions::Request {
                model: model.to_owned(),
                messages: vec![
                    openai_chat_completions::Message::System { content: system },
                    openai_chat_completions::Message::User { content: user },
                ],
                tools: vec![],
                temperature: None,
                max_tokens: None,
                stream: true,
            })
        }
        EndpointSchema::OpenAiResponses => {
            EndpointRequest::OpenAiResponses(openai_responses::Request {
                model: model.to_owned(),
                instructions: Some(system),
                input: vec![openai_responses::Input::Message(
                    openai_responses::Message {
                        role: openai_responses::Role::User,
                        content: vec![openai_responses::Content::InputText { text: user }],
                    },
                )],
                tools: vec![],
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
                    content: vec![anthropic_messages::ContentBlock::Text { text: user }],
                }],
                system: Some(system),
                max_tokens: ANTHROPIC_MAX_TOKENS,
                tools: vec![],
                temperature: None,
                stream: true,
            })
        }
    }
}

/// The user message: labeled environment lines, the redacted output
/// tail, then the verbatim request under `request:` — labeled rather
/// than prose so the model reads the ask apart from the context.
fn user_content(context: &SuggestContext) -> String {
    let mut content = format!(
        "cwd: {}\nshell: {}\nos: {}\n",
        context.cwd, context.shell, context.os
    );
    let tail_start = context
        .output_lines
        .len()
        .saturating_sub(OUTPUT_LINE_BUDGET);
    let tail = &context.output_lines[tail_start..];
    if !tail.is_empty() {
        content.push_str("recent terminal output (oldest first):\n");
        for line in tail {
            content.push_str(&redact(line));
            content.push('\n');
        }
    }
    content.push_str("request: ");
    content.push_str(context.request);
    content
}
