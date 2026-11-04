//! The tool registry (Spec R06, §5.6): a neutral [`ToolSpec`] per
//! tool — name, in-house description (CODING_STANDARDS §10),
//! JSON-Schema parameters — translated to each endpoint schema's own
//! `Tool` shape at request assembly ([`declare`]). §5.3's no-unify
//! rule governs the wire shapes, not the registry.
//!
//! The v1 list is fixed (Spec R06: `run_command`, `read_files`,
//! `edit_files`, `grep`, `glob`, `ask_user_question`,
//! `write_to_pty`). Every declared tool executes: the file tools
//! live in [`files`](crate::files), `write_to_pty`'s agent-owned
//! PTY in [`pty`](crate::pty) (R06-T7), `run_command` runs through
//! the task's [`CommandExecutor`](crate::CommandExecutor) (R06-T4/T5)
//! and `ask_user_question` answers on its own card channel (us-10)
//! — [`execute`] is the dispatcher; an unknown name is the only
//! tool error left.
//!
//! [`summarize_args`] renders a call's arguments for the approve
//! card — the frontend never parses tool JSON (standard 9).
//! `edit_files` goes further: [`plan_edit`] computes the call's
//! [`EditPlan`] before the card so the unified diff the user
//! approves IS the write that lands (us-22).

use std::sync::LazyLock;

use ait_llm::{anthropic_messages, openai_chat_completions, openai_responses, EndpointSchema};

use tokio_util::sync::CancellationToken;

use crate::executor::{CommandExecutor, CommandResult};
use crate::files::{EditPlan, ToolCwd, ToolOutput};
use crate::pty::AgentPty;
use crate::tool_call::ToolCall;

/// The `ask_user_question` tool's registry name — the permission
/// engine (`permissions::evaluate`) special-cases it and the agent
/// loop diverts it into the ask-card flow before [`execute`] is
/// ever called.
pub(crate) const ASK_USER_QUESTION: &str = "ask_user_question";

/// One tool, provider-neutrally: what the model sees, translated
/// per schema by [`declare`].
pub(crate) struct ToolSpec {
    /// The name the model calls.
    pub(crate) name: &'static str,
    /// What the tool does — written in-house (standard 10).
    pub(crate) description: &'static str,
    /// The arguments' JSON Schema.
    pub(crate) parameters: serde_json::Value,
}

/// The v1 tool list, in registry order (Spec R06). Descriptions are
/// authored here, not quoted from anywhere; built once behind
/// [`LazyLock`] because `serde_json::json!` is not const.
fn registry() -> &'static [ToolSpec] {
    static REGISTRY: LazyLock<Vec<ToolSpec>> = LazyLock::new(|| {
        vec![
            ToolSpec {
                name: "run_command",
                description: "Run a shell command in the user's terminal. The command runs in the \
                          working directory; its output and exit code come back as the result.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The command line to run."
                        }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            },
            ToolSpec {
                name: "read_files",
                description: "Read files from the working directory tree. Returns each file's \
                          contents, or a per-file error (missing, oversized or binary).",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "paths": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Files to read, relative to the working directory or \
                                            absolute paths inside it."
                        }
                    },
                    "required": ["paths"],
                    "additionalProperties": false
                }),
            },
            ToolSpec {
                name: "edit_files",
                description:
                    "Rewrite files in the working directory tree. Each entry replaces the \
                          whole file; a new file needs an existing parent directory.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "files": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": {"type": "string"},
                                    "content": {"type": "string"}
                                },
                                "required": ["path", "content"],
                                "additionalProperties": false
                            },
                            "description": "The files to write, each as path + full new content."
                        }
                    },
                    "required": ["files"],
                    "additionalProperties": false
                }),
            },
            ToolSpec {
                name: "grep",
                description: "Search file contents in the working directory tree with a regular \
                          expression. Returns matching lines as path:line: text.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "The regular expression to search for."
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory to search under. Defaults to the working \
                                            directory."
                        }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
            },
            ToolSpec {
                name: "glob",
                description: "List files in the working directory tree matching a glob pattern \
                          (`*`, `?` within a path segment, `**` across segments).",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "The glob pattern, e.g. \"src/**/*.rs\"."
                        },
                        "path": {
                            "type": "string",
                            "description": "Directory to match under. Defaults to the working \
                                            directory."
                        }
                    },
                    "required": ["pattern"],
                    "additionalProperties": false
                }),
            },
            ToolSpec {
                name: ASK_USER_QUESTION,
                description:
                    "Ask the user a question when the task needs a decision only they can \
                          make. Either offer choices or leave the answer free-form.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "The question to ask."
                        },
                        "choices": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Optional answer choices; omit for a free-form answer."
                        }
                    },
                    "required": ["question"],
                    "additionalProperties": false
                }),
            },
            ToolSpec {
                name: "write_to_pty",
                description: "Send input to the agent's own interactive terminal session and read \
                          the output it produced. For REPLs, prompts, ssh and sudo.",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "The bytes to write — a line of input, a keystroke, or \
                                            control characters."
                        }
                    },
                    "required": ["input"],
                    "additionalProperties": false
                }),
            },
        ]
    });
    &REGISTRY
}

/// Parse an `ask_user_question` call's arguments into the card's
/// pieces: the question to render and the offered choices — an empty
/// list means the card takes a free-text answer. A missing or
/// non-string `question` is the only failure: a `choices` member
/// that is not a string or a `choices` that is not an array simply
/// contributes nothing (the card must render for every call whose
/// question is usable).
pub(crate) fn parse_question(call: &ToolCall) -> Result<(String, Vec<String>), String> {
    let args: serde_json::Value = serde_json::from_str(&call.arguments)
        .map_err(|error| format!("{}: arguments are not valid JSON: {error}", call.name))?;
    let Some(question) = args.get("question").and_then(|v| v.as_str()) else {
        return Err(format!("{}: `question` must be a string", call.name));
    };
    let choices = args
        .get("choices")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Ok((question.to_string(), choices))
}

/// The registry translated into `schema`'s own `Tool` IR — attached
/// to the [`Conversation`](crate::conversation::Conversation) so each
/// request carries the declarations.
pub(crate) enum ToolDecls {
    /// `openai_chat_completions` tools (nested `function` shape).
    Chat(Vec<openai_chat_completions::Tool>),
    /// `openai_responses` tools (flat shape).
    Responses(Vec<openai_responses::Tool>),
    /// `anthropic_messages` tools (flat `input_schema` shape).
    Anthropic(Vec<anthropic_messages::Tool>),
}

/// Translate every [`ToolSpec`] into `schema`'s wire shape. The
/// mapping is mechanical: same name, description and JSON-Schema
/// parameters, wrapped in the schema's own `Tool` type.
pub(crate) fn declare(schema: EndpointSchema) -> ToolDecls {
    match schema {
        EndpointSchema::OpenAiChatCompletions => ToolDecls::Chat(
            registry()
                .iter()
                .map(|spec| openai_chat_completions::Tool {
                    kind: openai_chat_completions::ToolKind::Function,
                    function: openai_chat_completions::ToolFunction {
                        name: spec.name.to_string(),
                        description: Some(spec.description.to_string()),
                        parameters: spec.parameters.clone(),
                    },
                })
                .collect(),
        ),
        EndpointSchema::OpenAiResponses => ToolDecls::Responses(
            registry()
                .iter()
                .map(|spec| openai_responses::Tool {
                    kind: openai_responses::ToolKind::Function,
                    name: spec.name.to_string(),
                    description: Some(spec.description.to_string()),
                    parameters: spec.parameters.clone(),
                    strict: None,
                })
                .collect(),
        ),
        EndpointSchema::AnthropicMessages => ToolDecls::Anthropic(
            registry()
                .iter()
                .map(|spec| anthropic_messages::Tool {
                    name: spec.name.to_string(),
                    description: Some(spec.description.to_string()),
                    input_schema: spec.parameters.clone(),
                })
                .collect(),
        ),
    }
}

/// The renderable argument summary the approve card and tool card
/// show (Spec R06 us-5: name, argument summary, status). Per-tool
/// renderings keep the card scannable; anything unrecognized falls
/// back to compact JSON. Runs on the parsed arguments — `None` (bad
/// JSON) renders the raw string, since the card must show *something*
/// for a call the executor will fail.
pub(crate) fn summarize_args(call: &ToolCall) -> String {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
        return call.arguments.clone();
    };
    match call.name.as_str() {
        "run_command" => args
            .get("command")
            .and_then(|v| v.as_str())
            // Debug-quoted like `write_to_pty`'s input: the card
            // shows the would-be-typed line with control bytes made
            // visible — never raw bytes rendering invisibly.
            .map(|command| format!("{command:?}"))
            .unwrap_or_default(),
        "read_files" => args
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "edit_files" => args
            .get("files")
            .and_then(|v| v.as_array())
            .map(|files| {
                files
                    .iter()
                    .filter_map(|f| f.get("path").and_then(|p| p.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default(),
        "grep" | "glob" => {
            let pattern = args
                .get("pattern")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            match args.get("path").and_then(|v| v.as_str()) {
                Some(path) => format!("{pattern:?} in {path}"),
                None => format!("{pattern:?}"),
            }
        }
        ASK_USER_QUESTION => args
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        "write_to_pty" => args
            .get("input")
            .and_then(|v| v.as_str())
            .map(|input| format!("{input:?}"))
            .unwrap_or_default(),
        _ => compact(&args),
    }
}

/// Compact one-line JSON — the fallback summary for an unrecognized
/// call.
fn compact(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| String::from("{…}"))
}

/// `edit_files`' gate preparation (Spec R06 us-22/23): parse the
/// arguments and plan the call against `cwd`. The plan's unified
/// diff is the approve card's summary and the plan itself is what
/// an allow verdict applies — planning once keeps the rendered
/// diff byte-for-byte the applied change. A planning failure is a
/// `String` the gate renders on the card AND replays as the tool
/// result so the model reads why.
pub(crate) fn plan_edit(call: &ToolCall, cwd: &ToolCwd) -> Result<EditPlan, String> {
    let args: serde_json::Value = serde_json::from_str(&call.arguments)
        .map_err(|error| format!("edit_files: arguments are not valid JSON: {error}"))?;
    cwd.plan_edits(&args)
}

/// Run `call` against `cwd` — the sequential executor the loop
/// calls after the gate (or directly, for calls the engine allowed).
/// `pty` is the worker's lazily-spawned agent PTY (`write_to_pty`'s
/// drop-in seam, R06-T7); `executor` is the task's §5.9
/// [`CommandExecutor`] — only `run_command` consumes it, so the
/// file tools never notice which tier is bound; `cancel` lets a
/// tool's bounded waits — the PTY settle — end early on Esc. Every
/// failure mode is a [`ToolOutput`] error: a tool error is model
/// input, never a task failure. This arm IS the dispatcher: the
/// `edit_files` arm only serves callers that arrive unplanned (the
/// gate applies its own pre-rendered plan, us-22) and
/// `ask_user_question` never reaches here (its card answers on the
/// ask channel, us-10).
pub(crate) fn execute(
    call: &ToolCall,
    cwd: &ToolCwd,
    pty: &mut AgentPty,
    executor: &mut dyn CommandExecutor,
    cancel: &CancellationToken,
) -> ToolOutput {
    let args: serde_json::Value = match serde_json::from_str(&call.arguments) {
        Ok(args) => args,
        Err(error) => {
            return ToolOutput::error(format!(
                "{}: arguments are not valid JSON: {error}",
                call.name
            ))
        }
    };
    match call.name.as_str() {
        "read_files" => cwd.read_files(&args),
        "grep" => cwd.grep(&args),
        "glob" => cwd.glob(&args),
        "edit_files" => cwd.edit_files(&args),
        "run_command" => run_command(&args, executor),
        "write_to_pty" => pty.write(&args, cwd.root(), cancel),
        // The ask card answers on the session's answer channel —
        // `execute` is never its path; a call reaching here had no
        // usable question (the loop's error arm already reported it).
        ASK_USER_QUESTION => ToolOutput::error(format!(
            "{} answers through the ask card, not the executor",
            call.name
        )),
        other => ToolOutput::error(format!("unknown tool {other:?}")),
    }
}

/// `run_command` (Spec R06-T4): validate the arguments, run the
/// command through the task's [`CommandExecutor`] — which is where
/// the shell is actually reached — and render the §5.9 `{merged
/// output, exit code}` as result text. Every executor failure mode
/// is already a clean [`ExecutorError`](crate::ExecutorError), so
/// this arm only translates.
fn run_command(args: &serde_json::Value, executor: &mut dyn CommandExecutor) -> ToolOutput {
    let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
        return ToolOutput::error(String::from("run_command: `command` must be a string"));
    };
    if command.trim().is_empty() {
        return ToolOutput::error(String::from("run_command: `command` is empty"));
    }
    // CODING_STANDARDS §13: the typed tiers write `command` to the
    // user's PTY verbatim — a newline would self-execute the extra
    // lines and control bytes would reach the terminal raw. A single
    // line is the contract on EVERY tier (`__ait_run` types one line
    // too), so the refusal lives at validation, before the executor
    // ever sees the text.
    if command.chars().any(char::is_control) {
        return ToolOutput::error(String::from(
            "run_command: `command` must be a single line with no control \
             characters — chain steps with `&&` or `;` instead",
        ));
    }
    match executor.run(command) {
        Ok(result) => ToolOutput::ok(render_run_result(&result)),
        Err(error) => ToolOutput::error(format!("run_command: {error}")),
    }
}

/// The result text for one finished run: the merged stdout+stderr,
/// then the exit status on its own line — `unknown` when the shell
/// reported completion without a payload (us-15: never a guessed
/// code). The trailer is newline-separated so it can never glue
/// itself onto the last output byte.
fn render_run_result(result: &CommandResult) -> String {
    let output = String::from_utf8_lossy(&result.output);
    let status = match result.exit_code {
        Some(code) => format!("exit code: {code}"),
        None => String::from("exit code: unknown"),
    };
    if output.is_empty() {
        status
    } else if output.ends_with('\n') {
        format!("{output}{status}")
    } else {
        format!("{output}\n{status}")
    }
}

#[cfg(test)]
mod tests {
    //! Registry seams (Spec R06 testing decisions — pure): the
    //! `ToolSpec` → per-schema `Tool` translation tables, the
    //! argument-summary renderings and the executor dispatch.

    use super::*;

    /// Every schema's translation carries all seven tools with name,
    /// description and parameters — in its own `Tool` shape (§5.3).
    #[test]
    fn declare_translates_every_spec_per_schema() {
        let ToolDecls::Chat(tools) = declare(EndpointSchema::OpenAiChatCompletions) else {
            panic!("chat decls")
        };
        assert_eq!(tools.len(), 7);
        assert!(tools
            .iter()
            .all(|t| t.kind == openai_chat_completions::ToolKind::Function));
        let read = tools
            .iter()
            .find(|t| t.function.name == "read_files")
            .unwrap();
        assert!(read.function.description.is_some());
        assert_eq!(read.function.parameters["required"][0], "paths");

        let ToolDecls::Responses(tools) = declare(EndpointSchema::OpenAiResponses) else {
            panic!("responses decls")
        };
        let grep = tools.iter().find(|t| t.name == "grep").unwrap();
        assert_eq!(grep.parameters["properties"]["pattern"]["type"], "string");

        let ToolDecls::Anthropic(tools) = declare(EndpointSchema::AnthropicMessages) else {
            panic!("anthropic decls")
        };
        let glob = tools.iter().find(|t| t.name == "glob").unwrap();
        assert_eq!(glob.input_schema["required"][0], "pattern");
    }

    /// The card summary renders the argument a user recognizes —
    /// per tool, with a JSON fallback.
    #[test]
    fn summarize_renders_each_tools_arguments() {
        let call = |name: &str, args: &str| ToolCall {
            id: String::new(),
            name: name.into(),
            arguments: args.into(),
        };
        assert_eq!(
            summarize_args(&call("run_command", r#"{"command": "cargo test"}"#)),
            "\"cargo test\"",
            "the would-be-typed line renders quoted — escapes stay visible"
        );
        assert_eq!(
            summarize_args(&call("run_command", r#"{"command": "echo a && rm -rf /"}"#)),
            "\"echo a && rm -rf /\""
        );
        assert_eq!(
            summarize_args(&call("read_files", r#"{"paths": ["a.rs", "b.rs"]}"#)),
            "a.rs, b.rs"
        );
        assert_eq!(
            summarize_args(&call("grep", r#"{"pattern": "TODO", "path": "src"}"#)),
            "\"TODO\" in src"
        );
        assert_eq!(
            summarize_args(&call("glob", r#"{"pattern": "*.rs"}"#)),
            "\"*.rs\""
        );
        // Bad JSON still renders — the card must show something.
        assert_eq!(
            summarize_args(&call("read_files", "{not json")),
            "{not json"
        );
    }

    /// The ask card's pure seam: `question` plus `choices` come out
    /// of the call's arguments — absent choices mean free text —
    /// and a missing or non-string question is the only error.
    #[test]
    fn parse_question_reads_the_card_pieces() {
        let call = |args: &str| ToolCall {
            id: String::new(),
            name: ASK_USER_QUESTION.into(),
            arguments: args.into(),
        };
        assert_eq!(
            parse_question(&call(r#"{"question":"Which?","choices":["a","b"]}"#)),
            Ok(("Which?".to_string(), vec!["a".into(), "b".into()]))
        );
        // No `choices` — the free-text card.
        assert_eq!(
            parse_question(&call(r#"{"question":"Why?"}"#)),
            Ok(("Why?".to_string(), Vec::new()))
        );
        // Lenient on junk: a non-array `choices` and non-string
        // members contribute nothing.
        assert_eq!(
            parse_question(&call(r#"{"question":"Which?","choices":"all"}"#)),
            Ok(("Which?".to_string(), Vec::new()))
        );
        assert_eq!(
            parse_question(&call(r#"{"question":"Which?","choices":["a",1,true]}"#)),
            Ok(("Which?".to_string(), vec!["a".into()]))
        );
        for args in [r#"{}"#, r#"{"question":42}"#, "{not json"] {
            assert!(parse_question(&call(args)).is_err(), "{args}");
        }
    }

    /// Dispatch: every shipped tool executes (an unbound shell turns
    /// `run_command` into a clean tool error), unknown names answer
    /// tool errors, bad JSON errors — and `write_to_pty`'s argument
    /// check fails before any PTY spawns (R06-T7's lazy rule).
    #[test]
    fn execute_dispatches_and_errors_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = ToolCwd::new(dir.path().to_str().expect("utf-8"), &[]);
        let mut pty = AgentPty::new();
        let cancel = CancellationToken::new();
        let call = |name: &str, args: &str| ToolCall {
            id: String::new(),
            name: name.into(),
            arguments: args.into(),
        };
        let mut executor = crate::executor::DetachedExecutor;

        let out = execute(
            &call("glob", r#"{"pattern": "*.rs"}"#),
            &cwd,
            &mut pty,
            &mut executor,
            &cancel,
        );
        assert!(!out.is_error);

        // `run_command` executes now — but a task bound to no shell
        // (the `DetachedExecutor`) answers a clean error, not a hang.
        let out = execute(
            &call("run_command", r#"{"command": "echo hi"}"#),
            &cwd,
            &mut pty,
            &mut executor,
            &cancel,
        );
        assert!(
            out.is_error && out.text.contains("no live shell"),
            "{:?}",
            out.text
        );

        // edit_files dispatches now — `{}` is a well-formed call
        // with bad arguments, which is a tool error, not a stub.
        let out = execute(
            &call("edit_files", "{}"),
            &cwd,
            &mut pty,
            &mut executor,
            &cancel,
        );
        assert!(
            out.is_error && out.text.contains("`files`"),
            "{:?}",
            out.text
        );

        let out = execute(
            &call("run_command", "{}"),
            &cwd,
            &mut pty,
            &mut executor,
            &cancel,
        );
        assert!(
            out.is_error && out.text.contains("`command` must be a string"),
            "{:?}",
            out.text
        );
        let out = execute(
            &call("write_to_pty", "{}"),
            &cwd,
            &mut pty,
            &mut executor,
            &cancel,
        );
        assert!(out.is_error && out.text.contains("input"), "{:?}", out.text);
        // `ask_user_question` answers on the ask channel — `execute`
        // is never its path, but errors cleanly if reached.
        let out = execute(
            &call(ASK_USER_QUESTION, "{}"),
            &cwd,
            &mut pty,
            &mut executor,
            &cancel,
        );
        assert!(out.is_error && out.text.contains("ask card"));
        let out = execute(
            &call("definitely_not_a_tool", "{}"),
            &cwd,
            &mut pty,
            &mut executor,
            &cancel,
        );
        assert!(out.is_error && out.text.contains("unknown tool"));
        let out = execute(
            &call("read_files", "{oops"),
            &cwd,
            &mut pty,
            &mut executor,
            &cancel,
        );
        assert!(out.is_error && out.text.contains("not valid JSON"));
    }

    /// `run_command` rejects any command carrying control bytes
    /// (CODING_STANDARDS §13): a `\n` would self-execute extra lines
    /// on the typed tiers and `\r`/escape/control bytes would reach
    /// the terminal raw. The refusal is a tool error on every tier —
    /// never bytes typed at the user's shell.
    #[test]
    fn run_command_rejects_control_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = ToolCwd::new(dir.path().to_str().expect("utf-8"), &[]);
        let mut pty = AgentPty::new();
        let cancel = CancellationToken::new();
        let call = |args: &str| ToolCall {
            id: String::new(),
            name: "run_command".into(),
            arguments: args.into(),
        };
        let mut executor = crate::executor::DetachedExecutor;

        for command in ["echo a\nrm -rf /", "echo \r", "\x03", "\x1b[2J", "a\tb"] {
            let out = execute(
                &call(&format!(
                    "{{\"command\": {}}}",
                    serde_json::to_string(command).expect("json literal")
                )),
                &cwd,
                &mut pty,
                &mut executor,
                &cancel,
            );
            assert!(
                out.is_error && out.text.contains("single line"),
                "{command:?}: {:?}",
                out.text
            );
        }
    }

    /// `run_command`'s result rendering: merged output first, then
    /// the exit status — `unknown` stands in for a missing payload,
    /// never a guessed code (us-15).
    #[test]
    fn render_run_result_formats_output_then_status() {
        let result = CommandResult {
            output: b"hello\n".to_vec(),
            exit_code: Some(0),
        };
        assert_eq!(render_run_result(&result), "hello\nexit code: 0");

        // No trailing newline in the output: the status still lands
        // on its own line.
        let result = CommandResult {
            output: b"tail".to_vec(),
            exit_code: Some(3),
        };
        assert_eq!(render_run_result(&result), "tail\nexit code: 3");

        // Empty output and an absent code both render honestly.
        let result = CommandResult {
            output: Vec::new(),
            exit_code: None,
        };
        assert_eq!(render_run_result(&result), "exit code: unknown");
    }
}
