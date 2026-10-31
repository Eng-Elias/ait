//! The two artifacts the CLI prints or writes (Spec R03, user stories
//! 2–5): [`default_template`] backs `ait --init` and [`json_schema`]
//! backs `ait --print-schema`. The binary shells over these — no
//! business logic lives in `ait` itself (CODING_STANDARDS §9).

use std::path::Path;

use crate::ConfigError;
#[cfg(feature = "schema")]
use crate::Settings;

/// The commented default `settings.toml` `ait --init` writes (§4): an
/// example local endpoint whose model gets a fresh v4 UUID `config_key`
/// — so the file is immediately valid — plus the §5.7 default execution
/// profile as the safe starting point.
pub fn default_template() -> String {
    let config_key = uuid::Uuid::new_v4();
    format!(
        r#"# AIT settings
#
# The full model is documented in REQUIREMENTS.md §4/§5.7;
# `ait --print-schema` emits the JSON Schema for editor completion.
#
# Secrets never live in this file: an endpoint's key comes from the env
# var named by `api_key_env`, or the OS keyring entry
# `ait/<endpoint-key>` (e.g. `ait/local`) — store it with your
# platform's tools.

# [terminal]
# shell            = "/usr/bin/fish"  # program new sessions spawn (default: $SHELL)
# scrollback_lines = 20000            # scrollback capacity for new sessions
# osc52            = false            # let child apps write the host clipboard via OSC 52

# A declared endpoint — any OpenAI- or Anthropic-compatible server.
[agents.custom_endpoints.local]
name     = "Local Ollama"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions" # | "openai_responses" | "anthropic_messages"
# api_key_env = "OLLAMA_API_KEY"     # optional; else the keyring entry "ait/local"

[[agents.custom_endpoints.local.models]]
name       = "qwen2.5-coder:14b"   # model id sent on the wire
alias      = "Qwen Coder"          # display name (status line)
config_key = "{config_key}"        # stable id model roles reference
# context_window = 32768           # optional cap; unset = the model's own limit

# The default execution profile — the §5.7 safe starting point. Model
# roles reference config_keys; coding_model and command_model fall back
# to base_model when unset ("").
[agents.execution_profiles.default]
read_files        = "agent_decides" # agent_decides | always_allow | always_ask | never
write_to_pty      = "always_ask"      # also: ask_on_first_write
execute_commands  = "always_ask"
apply_code_diffs  = "always_ask"
mcp_permissions   = "always_ask"
ask_user_question = "always_allow"
command_allowlist   = []
command_denylist    = ['^(bash|sh|zsh|fish)(\s.*)?$', '^rm(\s.*)?$',
                       '^(curl|wget)(\s.*)?$', '^(ssh|scp|rsync)(\s.*)?$']
directory_allowlist = []
mcp_allowlist = []
mcp_denylist  = []
base_model    = "{config_key}"
coding_model  = ""
command_model = ""
context_window_limit = 0 # 0 = the model's own limit
"#
    )
}

/// `ait --init` (user stories 2–4): write [`default_template`] to `path`,
/// creating missing parent directories. An existing file is refused unless
/// `force` — a flag the user never passed must not destroy a hand-tuned
/// config (story 3); the refusal error names the path.
pub fn init_settings(path: impl AsRef<Path>, force: bool) -> Result<(), ConfigError> {
    let path = path.as_ref();
    if !force && path.exists() {
        return Err(ConfigError::unlocated(format!(
            "{}: already exists (pass --force to overwrite)",
            path.display()
        )));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|err| {
            ConfigError::unlocated(format!(
                "{}: cannot create config directory: {err}",
                dir.display()
            ))
        })?;
    }
    std::fs::write(path, default_template()).map_err(|err| {
        ConfigError::unlocated(format!(
            "{}: cannot write settings file: {err}",
            path.display()
        ))
    })
}

/// The JSON Schema of the [`Settings`] model (schemars 1.x, draft
/// 2020-12) — what `ait --print-schema` prints for editor validation
/// and completion (user story 5). `deny_unknown_fields` on the model
/// surfaces as `additionalProperties: false`, so editors flag typos the
/// same way the loader does. Behind the `schema` cargo feature (issue
/// #77) — `ait --print-schema` is the only consumer.
#[cfg(feature = "schema")]
pub fn json_schema() -> String {
    let schema = schemars::schema_for!(Settings);
    serde_json::to_string_pretty(&schema).expect("serializing a JsonSchema cannot fail")
}
