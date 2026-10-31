//! `chat` — the manual smoke vehicle for the provider stack (Spec R03
//! user story 35; REQUIREMENTS.md §5.3).
//!
//! `cargo run -p ait-llm --example chat -- "prompt"` loads
//! `settings.toml` via `ait-config` (a dev-dependency — `ait-llm` itself
//! stays config-free), resolves a model role to endpoint + model +
//! secret, sends one user message and prints the streamed reply:
//!
//! - text deltas inline on stdout,
//! - tool-call fragments as a growing `[tool_call …]` line on stdout,
//! - the finish reason + token usage as a trailing line on stderr,
//! - provider-reported error events on stderr (and a non-zero exit).
//!
//! Ctrl-C cancels the in-flight request through its `CancellationToken`
//! (us-30). The example owns a current-thread tokio runtime — `tokio`
//! stays inside `ait-llm`, never in `ait` (spec notes).
//!
//! Exit status: `0` on a completed stream, `1` on usage/config/request
//! errors or a provider-reported error, `130` on Ctrl-C.

use std::io::Write as _;
use std::process::ExitCode;

use ait_config::{ModelRole, OsKeyring};
use ait_llm::{
    anthropic_messages, openai_chat_completions, openai_responses, Endpoint, EndpointConfig,
    EndpointRequest, EndpointSchema, EndpointStream, EndpointStreamEvent, LlmError,
};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

/// `anthropic_messages` requires `max_tokens` — the schema has no default
/// (provider-schemas.md §c). 4096 bounds a smoke reply without truncating
/// ordinary answers.
const ANTHROPIC_MAX_TOKENS: u32 = 4096;

const USAGE: &str = "\
usage: chat [--role <role>] [--profile <name>] <prompt>…

  <prompt>…          the user message (several words are joined with spaces;
                     `--` ends flag parsing for prompts starting with `-`)
  --role <role>      model role to resolve: base_model (default),
                     coding_model or command_model
  --profile <name>   execution profile to resolve on (default: \"default\")
  -h, --help         show this help

Reads $XDG_CONFIG_HOME/ait/settings.toml (or ~/.config/ait/settings.toml),
resolves the role to endpoint + model + secret, and streams the reply.";

fn main() -> ExitCode {
    match parse_args(std::env::args().skip(1)) {
        Err(message) => {
            eprintln!("chat: {message}\n\n{USAGE}");
            ExitCode::FAILURE
        }
        Ok(Cli::Help) => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Ok(Cli::Run(args)) => run(&args),
    }
}

/// What the command line asked for.
#[derive(Debug)]
enum Cli {
    /// `-h`/`--help`.
    Help,
    /// Run a prompt.
    Run(Args),
}

/// Parsed flags plus the prompt.
#[derive(Debug)]
struct Args {
    /// `--role` — the `settings.toml` role key spelling.
    role: String,
    /// `--profile` — the execution profile name.
    profile: String,
    /// The user message, words joined with spaces.
    prompt: String,
}

/// Parse argv (minus argv[0]). Pure — the example's testable seam for
/// usage errors.
fn parse_args(args: impl Iterator<Item = String>) -> Result<Cli, String> {
    let mut role = String::from("base_model");
    let mut profile = String::from("default");
    let mut words = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        if arg == "--" {
            words.extend(args.by_ref());
            break;
        } else if let Some(value) = arg.strip_prefix("--role=") {
            role = value.to_string();
        } else if let Some(value) = arg.strip_prefix("--profile=") {
            profile = value.to_string();
        } else {
            match arg.as_str() {
                "-h" | "--help" => return Ok(Cli::Help),
                "--role" => {
                    role = args.next().ok_or("--role expects a value")?;
                }
                "--profile" => {
                    profile = args.next().ok_or("--profile expects a value")?;
                }
                _ if arg.starts_with('-') && arg != "-" => {
                    return Err(format!("unknown flag {arg:?}"));
                }
                _ => words.push(arg),
            }
        }
    }
    if words.is_empty() {
        return Err(String::from("missing <prompt>"));
    }
    Ok(Cli::Run(Args {
        role,
        profile,
        prompt: words.join(" "),
    }))
}

/// Load settings, resolve the role, build the endpoint, run the stream.
/// Everything synchronous lives here; only the request/response crosses
/// the tokio boundary.
fn run(args: &Args) -> ExitCode {
    let path = ait_config::settings_path();
    let settings = match ait_config::load() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("chat: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    if settings.agents.custom_endpoints.is_empty() {
        eprintln!(
            "chat: no endpoints configured — {} declares no \
             [agents.custom_endpoints]",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    let role = match ModelRole::from_name(&args.role) {
        Ok(role) => role,
        Err(error) => {
            eprintln!("chat: {error}");
            return ExitCode::FAILURE;
        }
    };
    let resolved = match settings.resolve_model(&args.profile, role, &OsKeyring) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("chat: {error}");
            return ExitCode::FAILURE;
        }
    };
    let Some(resolved) = resolved else {
        eprintln!(
            "chat: profile {:?} has no {} configured (and no base_model \
             fallback) — set it in {}",
            args.profile,
            role.name(),
            path.display()
        );
        return ExitCode::FAILURE;
    };

    let schema = llm_schema(resolved.endpoint.schema);
    eprintln!(
        "chat: {} ({}) via endpoint {:?} [{schema}] {}",
        resolved.display_name(),
        resolved.model.name,
        resolved.endpoint_key,
        resolved.endpoint.base_url,
    );

    let endpoint = match Endpoint::new(EndpointConfig {
        base_url: resolved.endpoint.base_url.clone(),
        model: resolved.model.name.clone(),
        secret: resolved.secret().map(str::to_owned),
        schema,
    }) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            eprintln!("chat: {error}");
            return ExitCode::FAILURE;
        }
    };
    let request = build_request(schema, resolved.model.name.clone(), args.prompt.clone());

    // The spec's runtime: a current-thread tokio the example owns (the
    // TUI stays synchronous). Ctrl-C cancels the in-flight request's
    // token rather than hard-killing mid-write.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("chat: failed to start the tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let cancel = CancellationToken::new();
    let on_sigint = cancel.clone();
    runtime.spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_sigint.cancel();
        }
    });

    match runtime.block_on(drain(&endpoint, request, cancel)) {
        Ok(false) => ExitCode::SUCCESS,
        // A provider-reported error event is data, not a machinery
        // failure — still a non-zero exit for scripting.
        Ok(true) => ExitCode::FAILURE,
        Err(LlmError::Cancelled) => {
            eprintln!("chat: cancelled");
            ExitCode::from(130)
        }
        Err(error) => {
            eprintln!("chat: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `ait-config`'s schema enum and `ait-llm`'s are deliberately separate
/// types (the llm crate owns its wire vocabulary); the example bridges
/// them. Two copies exist by design (ait-llm stays config-free); keep
/// them in sync with `crates/ait-agent/src/suggester.rs`'s `llm_schema`
/// when a fourth `EndpointSchema` variant lands.
fn llm_schema(schema: ait_config::EndpointSchema) -> EndpointSchema {
    match schema {
        ait_config::EndpointSchema::OpenAiChatCompletions => EndpointSchema::OpenAiChatCompletions,
        ait_config::EndpointSchema::OpenAiResponses => EndpointSchema::OpenAiResponses,
        ait_config::EndpointSchema::AnthropicMessages => EndpointSchema::AnthropicMessages,
    }
}

/// Build the schema-matched request IR for one user message — the
/// example's second testable seam. `stream` stays `true` on the IR; the
/// adapters additionally force it on the wire body.
fn build_request(schema: EndpointSchema, model: String, prompt: String) -> EndpointRequest {
    match schema {
        EndpointSchema::OpenAiChatCompletions => {
            EndpointRequest::OpenAiChatCompletions(openai_chat_completions::Request {
                model,
                messages: vec![openai_chat_completions::Message::User { content: prompt }],
                tools: vec![],
                temperature: None,
                max_tokens: None,
                stream: true,
            })
        }
        EndpointSchema::OpenAiResponses => {
            EndpointRequest::OpenAiResponses(openai_responses::Request {
                model,
                instructions: None,
                input: vec![openai_responses::Input::Message(
                    openai_responses::Message {
                        role: openai_responses::Role::User,
                        content: vec![openai_responses::Content::InputText { text: prompt }],
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
                model,
                messages: vec![anthropic_messages::Message {
                    role: anthropic_messages::Role::User,
                    content: vec![anthropic_messages::ContentBlock::Text { text: prompt }],
                }],
                system: None,
                max_tokens: ANTHROPIC_MAX_TOKENS,
                tools: vec![],
                temperature: None,
                stream: true,
            })
        }
    }
}

/// What the drain loop prints — a display-only flattening of the three
/// per-schema event IRs (the IRs themselves stay distinct, §5.3).
enum Rendered {
    /// A fragment of assistant text.
    Text(String),
    /// A fragment of a streamed tool call; `at` names the fragment's
    /// address (`index=0` or `item=fc_…`).
    ToolCallDelta {
        /// Where the fragment belongs.
        at: String,
        /// The call id, when this fragment carries it.
        id: Option<String>,
        /// The tool/function name, when this fragment carries it.
        name: Option<String>,
        /// A fragment of the JSON arguments (may be empty).
        arguments: String,
    },
    /// The reply finished: wire stop reason plus formatted usage.
    Finish {
        /// The schema's wire stop reason.
        reason: String,
        /// Token accounting, preformatted per schema.
        usage: Option<String>,
    },
    /// A provider-reported error event.
    Error {
        /// The provider's message.
        message: String,
        /// The provider's machine-readable code, when it sends one.
        code: Option<String>,
    },
}

/// Map one tagged [`EndpointStreamEvent`] to its display form — the
/// example's third testable seam.
fn render_event(event: EndpointStreamEvent) -> Rendered {
    match event {
        EndpointStreamEvent::OpenAiChatCompletions(event) => match event {
            openai_chat_completions::StreamEvent::TextDelta { text } => Rendered::Text(text),
            openai_chat_completions::StreamEvent::ToolCallDelta(delta) => Rendered::ToolCallDelta {
                at: format!("index={}", delta.index),
                id: delta.id,
                name: delta.name,
                arguments: delta.arguments,
            },
            openai_chat_completions::StreamEvent::Finish { reason, usage } => Rendered::Finish {
                reason: wire_name(&reason),
                usage: usage.map(|u| {
                    format!(
                        "prompt={} completion={} total={}",
                        u.prompt_tokens, u.completion_tokens, u.total_tokens
                    )
                }),
            },
            openai_chat_completions::StreamEvent::Error { message, code } => {
                Rendered::Error { message, code }
            }
        },
        EndpointStreamEvent::OpenAiResponses(event) => match event {
            openai_responses::StreamEvent::TextDelta { text } => Rendered::Text(text),
            openai_responses::StreamEvent::ToolCallDelta(delta) => Rendered::ToolCallDelta {
                at: format!(
                    "item={}",
                    delta.item_id.unwrap_or_else(|| String::from("?"))
                ),
                id: delta.call_id,
                name: delta.name,
                arguments: delta.arguments,
            },
            openai_responses::StreamEvent::Finish { reason, usage } => Rendered::Finish {
                reason: wire_name(&reason),
                usage: usage.map(|u| {
                    format!(
                        "input={} output={} total={}",
                        u.input_tokens, u.output_tokens, u.total_tokens
                    )
                }),
            },
            openai_responses::StreamEvent::Error { message, code } => {
                Rendered::Error { message, code }
            }
        },
        EndpointStreamEvent::AnthropicMessages(event) => match event {
            anthropic_messages::StreamEvent::TextDelta { text } => Rendered::Text(text),
            anthropic_messages::StreamEvent::ToolCallDelta(delta) => Rendered::ToolCallDelta {
                at: format!("index={}", delta.index),
                id: delta.id,
                name: delta.name,
                arguments: delta.arguments,
            },
            anthropic_messages::StreamEvent::Finish { reason, usage } => Rendered::Finish {
                reason: wire_name(&reason),
                usage: usage.and_then(|u| {
                    let mut parts = Vec::new();
                    if let Some(input) = u.input_tokens {
                        parts.push(format!("input={input}"));
                    }
                    if let Some(output) = u.output_tokens {
                        parts.push(format!("output={output}"));
                    }
                    (!parts.is_empty()).then(|| parts.join(" "))
                }),
            },
            anthropic_messages::StreamEvent::Error { message, code } => {
                Rendered::Error { message, code }
            }
        },
    }
}

/// The serde `snake_case` spelling of a wire enum (`EndTurn` →
/// `end_turn`), so the finish line reads like the provider docs.
fn wire_name(value: &(impl serde::Serialize + std::fmt::Debug)) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{value:?}"))
}

/// Stream the request and print events as they arrive. Returns whether a
/// provider-reported error event was seen (a non-zero exit).
async fn drain(
    endpoint: &Endpoint,
    request: EndpointRequest,
    cancel: CancellationToken,
) -> Result<bool, LlmError> {
    let mut stream: EndpointStream = endpoint.stream(request, cancel).await?;
    let mut saw_provider_error = false;
    let mut mid_text = false;
    let mut open_call: Option<String> = None;
    while let Some(item) = stream.next().await {
        match render_event(item?) {
            Rendered::Text(text) => {
                if open_call.take().is_some() {
                    println!("]");
                }
                print!("{text}");
                std::io::stdout().flush().ok();
                mid_text = true;
            }
            Rendered::ToolCallDelta {
                at,
                id,
                name,
                arguments,
            } => {
                if open_call.as_deref() != Some(at.as_str()) {
                    if open_call.take().is_some() {
                        println!("]");
                    }
                    if mid_text {
                        println!();
                        mid_text = false;
                    }
                    print!("[tool_call {at}");
                    if let Some(id) = id {
                        print!(" id={id}");
                    }
                    if let Some(name) = name {
                        print!(" name={name}");
                    }
                    print!(" args=");
                    open_call = Some(at);
                }
                print!("{arguments}");
                std::io::stdout().flush().ok();
            }
            Rendered::Finish { reason, usage } => {
                if open_call.take().is_some() {
                    println!("]");
                }
                if mid_text {
                    println!();
                    mid_text = false;
                }
                match usage {
                    Some(usage) => eprintln!("[finish {reason} {usage}]"),
                    None => eprintln!("[finish {reason}]"),
                }
            }
            Rendered::Error { message, code } => {
                if open_call.take().is_some() {
                    println!("]");
                }
                if mid_text {
                    println!();
                    mid_text = false;
                }
                match code {
                    Some(code) => eprintln!("[error {code}] {message}"),
                    None => eprintln!("[error] {message}"),
                }
                saw_provider_error = true;
            }
        }
    }
    if open_call.is_some() {
        println!("]");
    }
    if mid_text {
        println!();
    }
    Ok(saw_provider_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(argv: &[&str]) -> Result<Cli, String> {
        parse_args(argv.iter().map(|arg| String::from(*arg)))
    }

    fn run_args(cli: Cli) -> Args {
        match cli {
            Cli::Run(args) => args,
            Cli::Help => panic!("expected Run"),
        }
    }

    #[test]
    fn defaults_and_prompt_joining() {
        let args = run_args(args(&["what", "is", "up"]).unwrap());
        assert_eq!(args.role, "base_model");
        assert_eq!(args.profile, "default");
        assert_eq!(args.prompt, "what is up");
    }

    #[test]
    fn role_and_profile_flags_both_forms() {
        let args = run_args(args(&["--role", "coding_model", "--profile=work", "hi"]).unwrap());
        assert_eq!(args.role, "coding_model");
        assert_eq!(args.profile, "work");
        assert_eq!(args.prompt, "hi");
    }

    #[test]
    fn double_dash_ends_flag_parsing() {
        let args = run_args(args(&["--", "-n", "5"]).unwrap());
        assert_eq!(args.prompt, "-n 5");
    }

    #[test]
    fn usage_errors_name_the_problem() {
        assert_eq!(args(&[]).unwrap_err(), "missing <prompt>");
        assert_eq!(
            args(&["--bogus", "hi"]).unwrap_err(),
            "unknown flag \"--bogus\""
        );
        assert_eq!(args(&["--role"]).unwrap_err(), "--role expects a value");
    }

    #[test]
    fn help_short_circuits() {
        assert!(matches!(args(&["--help", "ignored"]).unwrap(), Cli::Help));
        assert!(matches!(args(&["-h"]).unwrap(), Cli::Help));
    }

    #[test]
    fn build_request_per_schema() {
        let chat = build_request(
            EndpointSchema::OpenAiChatCompletions,
            String::from("m1"),
            String::from("hi"),
        );
        let EndpointRequest::OpenAiChatCompletions(request) = chat else {
            panic!("wrong variant");
        };
        assert_eq!(request.model, "m1");
        assert!(request.stream);
        assert_eq!(
            request.messages,
            vec![openai_chat_completions::Message::User {
                content: String::from("hi")
            }]
        );

        let responses = build_request(
            EndpointSchema::OpenAiResponses,
            String::from("m2"),
            String::from("hi"),
        );
        let EndpointRequest::OpenAiResponses(request) = responses else {
            panic!("wrong variant");
        };
        assert_eq!(request.model, "m2");
        assert!(request.stream);

        let anthropic = build_request(
            EndpointSchema::AnthropicMessages,
            String::from("m3"),
            String::from("hi"),
        );
        let EndpointRequest::AnthropicMessages(request) = anthropic else {
            panic!("wrong variant");
        };
        assert_eq!(request.model, "m3");
        assert!(request.stream);
        assert_eq!(request.max_tokens, ANTHROPIC_MAX_TOKENS);
        assert_eq!(
            request.messages,
            vec![anthropic_messages::Message {
                role: anthropic_messages::Role::User,
                content: vec![anthropic_messages::ContentBlock::Text {
                    text: String::from("hi")
                }],
            }]
        );
    }

    #[test]
    fn render_event_flattens_each_schema() {
        let event = render_event(EndpointStreamEvent::OpenAiChatCompletions(
            openai_chat_completions::StreamEvent::TextDelta {
                text: String::from("hi"),
            },
        ));
        assert!(matches!(event, Rendered::Text(text) if text == "hi"));

        let event = render_event(EndpointStreamEvent::AnthropicMessages(
            anthropic_messages::StreamEvent::ToolCallDelta(anthropic_messages::ToolCallDelta {
                index: 1,
                id: Some(String::from("toolu_1")),
                name: Some(String::from("get_weather")),
                arguments: String::from("{\"loc\":"),
            }),
        ));
        let Rendered::ToolCallDelta {
            at,
            id,
            name,
            arguments,
        } = event
        else {
            panic!("wrong render");
        };
        assert_eq!(at, "index=1");
        assert_eq!(id.as_deref(), Some("toolu_1"));
        assert_eq!(name.as_deref(), Some("get_weather"));
        assert_eq!(arguments, "{\"loc\":");

        let event = render_event(EndpointStreamEvent::OpenAiResponses(
            openai_responses::StreamEvent::Finish {
                reason: openai_responses::FinishReason::Completed,
                usage: Some(openai_responses::Usage {
                    input_tokens: 3,
                    output_tokens: 5,
                    total_tokens: 8,
                }),
            },
        ));
        let Rendered::Finish { reason, usage } = event else {
            panic!("wrong render");
        };
        assert_eq!(reason, "completed");
        assert_eq!(usage.as_deref(), Some("input=3 output=5 total=8"));
    }
}
