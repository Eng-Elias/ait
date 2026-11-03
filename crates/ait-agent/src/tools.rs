//! The tool registry (Spec R06, §5.6): a neutral [`ToolSpec`] per
//! tool — name, in-house description (CODING_STANDARDS §10),
//! JSON-Schema parameters — translated to each endpoint schema's own
//! `Tool` shape at request assembly ([`declare`]). §5.3's no-unify
//! rule governs the wire shapes, not the registry.
//!
//! The v1 list is fixed (Spec R06: `run_command`, `read_files`,
//! `edit_files`, `grep`, `glob`, `ask_user_question`,
//! `write_to_pty`). Round 6 ships the three read tools' executors in
//! [`files`](crate::files) and `write_to_pty`'s agent-owned PTY in
//! [`pty`](crate::pty) (R06-T7); the rest declare their specs now so
//! the model sees the whole vocabulary, and [`execute`] answers them
//! with a tool error — the drop-in point their tickets replace, not
//! a stub the model has to special-case.
//!
//! [`summarize_args`] renders a call's arguments for the approve
//! card — the frontend never parses tool JSON (standard 9).

use std::sync::LazyLock;

use ait_llm::{anthropic_messages, openai_chat_completions, openai_responses, EndpointSchema};

use tokio_util::sync::CancellationToken;

use crate::files::{ToolCwd, ToolOutput};
use crate::pty::AgentPty;
use crate::tool_call::ToolCall;

/// The `ask_user_question` tool's registry name — [`gate_exempt`]
/// compares against it; its interactive card lands with its ticket.
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
                        },
                        "cwd": {
                            "type": "string",
                            "description": "Directory to run in. Defaults to the working directory."
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

/// Whether the interim gate asks before `name` runs (Spec R06's
/// ask-everything rule): every tool asks except
/// `ask_user_question` — its own card IS the ask (us-10). The
/// exemption point lives here even before that tool lands, so its
/// ticket drops in without touching the gate.
pub(crate) fn gate_exempt(name: &str) -> bool {
    name == ASK_USER_QUESTION
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
            .unwrap_or_default()
            .to_string(),
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

/// Run `call` against `cwd` — the sequential executor the loop
/// calls after the gate (or directly, for gate-exempt calls). `pty`
/// is the worker's lazily-spawned agent PTY (`write_to_pty`'s
/// drop-in seam, R06-T7); `cancel` lets a tool's bounded waits —
/// the PTY settle — end early on Esc. Every failure mode is a
/// [`ToolOutput`] error: a tool error is model input, never a task
/// failure. The tickets owning the remaining tools replace their
/// arms here — the dispatch IS the drop-in seam.
pub(crate) fn execute(
    call: &ToolCall,
    cwd: &ToolCwd,
    pty: &mut AgentPty,
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
        "write_to_pty" => pty.write(&args, cwd.root(), cancel),
        "run_command" | "edit_files" | ASK_USER_QUESTION => ToolOutput::error(format!(
            "{} is not available yet — its executor lands in a later ticket",
            call.name
        )),
        other => ToolOutput::error(format!("unknown tool {other:?}")),
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
            "cargo test"
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

    /// `ask_user_question` is the gate's one exemption — its card is
    /// the ask (us-10).
    #[test]
    fn gate_exempts_only_ask_user_question() {
        assert!(gate_exempt(ASK_USER_QUESTION));
        for name in [
            "read_files",
            "grep",
            "glob",
            "run_command",
            "edit_files",
            "write_to_pty",
        ] {
            assert!(!gate_exempt(name), "{name}");
        }
    }

    /// Dispatch: the shipped tools execute, the declared-but-unbuilt
    /// ones and unknown names answer tool errors, bad JSON errors —
    /// and `write_to_pty`'s argument check fails before any PTY
    /// spawns (R06-T7's lazy rule).
    #[test]
    fn execute_dispatches_and_errors_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = ToolCwd::new(dir.path().to_str().expect("utf-8"));
        let mut pty = AgentPty::new();
        let cancel = CancellationToken::new();
        let call = |name: &str, args: &str| ToolCall {
            id: String::new(),
            name: name.into(),
            arguments: args.into(),
        };

        let out = execute(
            &call("glob", r#"{"pattern": "*.rs"}"#),
            &cwd,
            &mut pty,
            &cancel,
        );
        assert!(!out.is_error);

        for name in ["run_command", "edit_files", ASK_USER_QUESTION] {
            let out = execute(&call(name, "{}"), &cwd, &mut pty, &cancel);
            assert!(
                out.is_error && out.text.contains("not available yet"),
                "{name}"
            );
        }
        let out = execute(&call("write_to_pty", "{}"), &cwd, &mut pty, &cancel);
        assert!(out.is_error && out.text.contains("input"), "{:?}", out.text);
        let out = execute(
            &call("definitely_not_a_tool", "{}"),
            &cwd,
            &mut pty,
            &cancel,
        );
        assert!(out.is_error && out.text.contains("unknown tool"));
        let out = execute(&call("read_files", "{oops"), &cwd, &mut pty, &cancel);
        assert!(out.is_error && out.text.contains("not valid JSON"));
    }
}
