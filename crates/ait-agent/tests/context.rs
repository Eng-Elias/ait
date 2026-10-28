//! Table-driven tests at the context-assembly pure seam (Spec R04, user
//! stories 15–19, 27; standard 11 — no PTY, model or TTY): fixed
//! (cwd, shell, OS, lines, request) inputs produce a schema-matched
//! `EndpointRequest` whose system prompt demands one bare shell command
//! and whose user message carries the assembled context — output lines
//! redacted, the typed request verbatim (§5.4, §5.10.3).

use ait_agent::{assemble_request, SuggestContext, OUTPUT_LINE_BUDGET};
use ait_llm::{
    anthropic_messages, openai_chat_completions, openai_responses, EndpointRequest, EndpointSchema,
};

/// The spec's fixed system prompt (§5.4) restated here as a known-good
/// literal — the test, not the crate's constant, is the source of
/// truth.
const SYSTEM_PROMPT: &str = "You turn a natural-language request into a shell command for the \
     user's terminal. Reply with one shell command, no markdown, no \
     explanation.";

const ALL_SCHEMAS: [EndpointSchema; 3] = [
    EndpointSchema::OpenAiChatCompletions,
    EndpointSchema::OpenAiResponses,
    EndpointSchema::AnthropicMessages,
];

/// Pull (system prompt, user/context text, model, streaming) out of
/// whichever per-schema IR the request uses — each schema carries the
/// system prompt in a different place.
fn parts(request: &EndpointRequest) -> (Option<&str>, &str, &str, bool) {
    match request {
        EndpointRequest::OpenAiChatCompletions(request) => {
            let system = match &request.messages[0] {
                openai_chat_completions::Message::System { content } => content.as_str(),
                other => panic!("messages[0] is not System: {other:?}"),
            };
            let user = match &request.messages[1] {
                openai_chat_completions::Message::User { content } => content.as_str(),
                other => panic!("messages[1] is not User: {other:?}"),
            };
            (Some(system), user, &request.model, request.stream)
        }
        EndpointRequest::OpenAiResponses(request) => {
            let user = match &request.input[0] {
                openai_responses::Input::Message(openai_responses::Message {
                    role: openai_responses::Role::User,
                    content,
                }) => match &content[0] {
                    openai_responses::Content::InputText { text } => text.as_str(),
                    other => panic!("input[0] content is not input_text: {other:?}"),
                },
                other => panic!("input[0] is not a user message: {other:?}"),
            };
            (
                request.instructions.as_deref(),
                user,
                &request.model,
                request.stream,
            )
        }
        EndpointRequest::AnthropicMessages(request) => {
            assert_eq!(request.messages[0].role, anthropic_messages::Role::User);
            let user = match &request.messages[0].content[0] {
                anthropic_messages::ContentBlock::Text { text } => text.as_str(),
                other => panic!("messages[0] content is not text: {other:?}"),
            };
            (
                request.system.as_deref(),
                user,
                &request.model,
                request.stream,
            )
        }
    }
}

#[test]
fn assembles_a_schema_matched_single_turn_request() {
    let lines = vec![
        String::from("total 8"),
        String::from("token ghp_16C7e42F292c6912E7710c838347Ae178B4a leaked"),
        String::from("drwxr-xr-x src"),
    ];
    let context = SuggestContext {
        cwd: "/home/u/ait",
        shell: "bash",
        os: "linux",
        output_lines: &lines,
        request: "list files here",
    };
    let expected_user = "\
cwd: /home/u/ait
shell: bash
os: linux
recent terminal output (oldest first):
total 8
token [REDACTED] leaked
drwxr-xr-x src
request: list files here";
    for schema in ALL_SCHEMAS {
        let request = assemble_request(&context, schema, "cmd-model");
        assert_eq!(request.schema(), schema, "wrong variant for {schema}");
        let (system, user, model, stream) = parts(&request);
        assert_eq!(system, Some(SYSTEM_PROMPT), "wrong system for {schema}");
        assert_eq!(user, expected_user, "wrong user content for {schema}");
        assert_eq!(model, "cmd-model", "wrong model for {schema}");
        assert!(stream, "stream flag off for {schema}");
    }
}

/// us-19: the typed request reaches the provider verbatim even when it
/// contains a credential-shaped string — redaction is for terminal
/// output only, never for what the user actually asked.
#[test]
fn the_typed_request_is_never_redacted() {
    let lines = vec![String::from("AKIAIOSFODNN7EXAMPLE")];
    let context = SuggestContext {
        cwd: "/tmp",
        shell: "zsh",
        os: "macos",
        output_lines: &lines,
        request: "echo AKIAIOSFODNN7EXAMPLE",
    };
    let request = assemble_request(&context, EndpointSchema::OpenAiChatCompletions, "m");
    let (_, user, _, _) = parts(&request);
    assert_eq!(
        user.matches("AKIAIOSFODNN7EXAMPLE").count(),
        1,
        "the request should carry the only verbatim copy: {user:?}"
    );
    assert!(user.ends_with("request: echo AKIAIOSFODNN7EXAMPLE"));
    assert!(user.contains("[REDACTED]"));
}

/// §5.4's "last N lines": when more than `OUTPUT_LINE_BUDGET` lines are
/// offered, only the tail reaches the prompt.
#[test]
fn only_the_last_budget_lines_reach_the_prompt() {
    let extra = 7;
    let lines: Vec<String> = (0..OUTPUT_LINE_BUDGET + extra)
        .map(|i| format!("line-{i}"))
        .collect();
    let context = SuggestContext {
        cwd: "/tmp",
        shell: "bash",
        os: "linux",
        output_lines: &lines,
        request: "retry that",
    };
    let request = assemble_request(&context, EndpointSchema::AnthropicMessages, "m");
    let (_, user, _, _) = parts(&request);
    assert!(
        !user.contains("line-6\n"),
        "a line older than the budget leaked: {user:?}"
    );
    assert!(
        user.contains("recent terminal output (oldest first):\nline-7\n"),
        "the oldest kept line should be line-7: {user:?}"
    );
    assert!(
        user.contains(&format!(
            "line-{}\nrequest:",
            OUTPUT_LINE_BUDGET + extra - 1
        )),
        "the newest line should sit last: {user:?}"
    );
}

/// With no output to offer there is no empty output section — just the
/// environment lines and the request.
#[test]
fn no_output_lines_means_no_output_section() {
    let context = SuggestContext {
        cwd: "/tmp",
        shell: "fish",
        os: "linux",
        output_lines: &[],
        request: "undo the last commit",
    };
    let request = assemble_request(&context, EndpointSchema::OpenAiResponses, "m");
    let (_, user, _, _) = parts(&request);
    assert_eq!(
        user,
        "cwd: /tmp\nshell: fish\nos: linux\nrequest: undo the last commit"
    );
}
