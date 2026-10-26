//! The `settings.toml` model (REQUIREMENTS.md §4) and the pure
//! [`Settings::from_toml`] parse/validate seam.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;

use crate::error::ConfigError;

/// Parsed `settings.toml` — the full §4 configuration model.
///
/// Construct only via [`Settings::from_toml`]: serde handles shape and
/// unknown-key rejection (`deny_unknown_fields` on every table), and
/// `from_toml` adds the validation serde cannot express — unique, non-empty
/// `config_key`s and resolvable model-role references.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// `[terminal]` — shell + scrollback applied to NEW sessions only
    /// (§4.3); live sessions keep their spawn-time values.
    #[serde(default)]
    pub terminal: TerminalSettings,
    /// `[agents]` — custom endpoints (§4.1) and execution profiles (§5.7).
    #[serde(default)]
    pub agents: AgentsSettings,
}

impl Settings {
    /// Parse and validate `settings.toml` text — the crate's pure seam
    /// (Spec R03): no filesystem, no environment, no network.
    ///
    /// TOML syntax errors render the parser's line/column message; every
    /// other error renders `<key-path>: <detail>` where the path is the
    /// TOML key that failed (`agents.custom_endpoints.local.models[0].config_key`).
    /// Structural errors get their path from `serde_path_to_error` over
    /// `toml`'s DOM deserializer; validation errors get theirs from
    /// [`Settings::validate`].
    pub fn from_toml(input: &str) -> Result<Self, ConfigError> {
        // `Deserializer::parse` does the DOM parse eagerly, so TOML *syntax*
        // errors surface here — before any serde path exists to name them.
        let deserializer = toml::de::Deserializer::parse(input).map_err(ConfigError::unlocated)?;
        let settings: Settings = serde_path_to_error::deserialize(deserializer).map_err(|err| {
            let path = err.path().to_string();
            // serde_path_to_error renders the root as `.`; a root-level
            // error has no key path to name.
            if path == "." {
                ConfigError::unlocated(err.inner().message())
            } else {
                ConfigError::at(path, err.inner().message())
            }
        })?;
        settings.validate()?;
        Ok(settings)
    }

    /// Validation beyond serde (Spec R03):
    ///
    /// - every model `config_key` is non-empty and unique across all
    ///   endpoints' models — duplicate keys would make §4.2 model-role
    ///   references ambiguous;
    /// - every model-role reference (`base_model`, `coding_model`,
    ///   `command_model` on every execution profile) resolves to a declared
    ///   `config_key`, with the error naming the role's key path.
    fn validate(&self) -> Result<(), ConfigError> {
        // `config_key` -> key path of its first declaration, for duplicate
        // reporting and role-reference lookups.
        let mut declared: BTreeMap<&str, String> = BTreeMap::new();
        for (endpoint_key, endpoint) in &self.agents.custom_endpoints {
            for (index, model) in endpoint.models.iter().enumerate() {
                let path =
                    format!("agents.custom_endpoints.{endpoint_key}.models[{index}].config_key");
                if model.config_key.is_empty() {
                    return Err(ConfigError::at(path, "must not be empty"));
                }
                if let Some(first) = declared.get(model.config_key.as_str()) {
                    return Err(ConfigError::at(
                        path,
                        format!(
                            "duplicate config_key {:?} (first declared at {first})",
                            model.config_key
                        ),
                    ));
                }
                declared.insert(model.config_key.as_str(), path);
            }
        }

        for (profile_name, profile) in &self.agents.execution_profiles {
            for (role, config_key) in profile.model_roles() {
                if let Some(config_key) = config_key {
                    if !declared.contains_key(config_key.as_str()) {
                        return Err(ConfigError::at(
                            format!("agents.execution_profiles.{profile_name}.{role}"),
                            format!("references unknown config_key {config_key:?}"),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// `[terminal]` (§4.3) — settings applied when spawning NEW sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalSettings {
    /// Program new sessions spawn; unset falls back to `$SHELL` then
    /// `/bin/sh` (§5.1).
    pub shell: Option<String>,
    /// Scrollback capacity in lines for new sessions.
    pub scrollback_lines: Option<usize>,
}

/// `[agents]` — endpoint declarations (§4.1) and execution profiles (§5.7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentsSettings {
    /// `[agents.custom_endpoints.<key>]` — endpoint declarations keyed by
    /// their table key (`local`, `work`, …).
    #[serde(default)]
    pub custom_endpoints: BTreeMap<String, Endpoint>,
    /// `[agents.execution_profiles.<name>]` — named profiles (§5.7). Parsed
    /// and validated now but inert: the permission engine consumes them in
    /// Round 7.
    #[serde(default)]
    pub execution_profiles: BTreeMap<String, ExecutionProfile>,
}

/// One declared endpoint — `[agents.custom_endpoints.<key>]` (§4.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Display name (`"Local Ollama"`).
    pub name: String,
    /// Base URL requests are sent to (`"http://localhost:11434/v1"`).
    pub base_url: String,
    /// Wire format — selects the `ait-llm` adapter (§4.1).
    pub schema: EndpointSchema,
    /// Environment variable holding the API key. Unset means the OS keyring
    /// entry `ait/<endpoint-key>` — and when that is absent too, no
    /// `Authorization` header at all (user story 21). Secrets never live in
    /// the TOML file.
    pub api_key_env: Option<String>,
    /// `[[agents.custom_endpoints.<key>.models]]` — the endpoint's models.
    #[serde(default)]
    pub models: Vec<EndpointModel>,
}

/// One entry in an endpoint's `[[models]]` list (§4.1).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EndpointModel {
    /// Model id sent on the wire (`"qwen2.5-coder:14b"`).
    pub name: String,
    /// Display name (`"Qwen Coder"`) — what the status line shows.
    pub alias: String,
    /// Stable id that model roles reference; renaming `name`/`alias` never
    /// breaks references. Non-empty and unique across all endpoints —
    /// enforced by [`Settings::from_toml`]. UUID format is conventional but
    /// not required.
    pub config_key: String,
    /// Context window cap in tokens; unset means the model's own limit
    /// (user story 25).
    pub context_window: Option<usize>,
}

/// Wire format of an [`Endpoint`] (§4.1): which `ait-llm` adapter serves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
pub enum EndpointSchema {
    /// `openai_chat_completions` — OpenAI-compatible `/chat/completions`.
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
    /// `openai_responses` — OpenAI `/responses`.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// `anthropic_messages` — Anthropic `/messages`.
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

/// `[agents.execution_profiles.<name>]` — the full §5.7 profile shape.
///
/// Parsed and validated so the emitted JSON Schema is stable, but inert:
/// no permission engine consumes it until Round 7.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfile {
    /// Permission level for reading files.
    pub read_files: Option<PermissionLevel>,
    /// Permission level for writing to the agent-owned or user PTY;
    /// `ask_on_first_write` is meaningful here (§5.7).
    pub write_to_pty: Option<PermissionLevel>,
    /// Permission level for executing commands.
    pub execute_commands: Option<PermissionLevel>,
    /// Permission level for applying code diffs.
    pub apply_code_diffs: Option<PermissionLevel>,
    /// Permission level for MCP tool calls.
    pub mcp_permissions: Option<PermissionLevel>,
    /// Permission level for asking the user a question.
    pub ask_user_question: Option<PermissionLevel>,
    /// Regexes matched against the full command; allowlist entries win
    /// unless the denylist matches — denylist beats allowlist (§5.7).
    #[serde(default)]
    pub command_allowlist: Vec<String>,
    /// Regexes that always deny a command (§5.7).
    #[serde(default)]
    pub command_denylist: Vec<String>,
    /// Directories file access is restricted to (§5.7).
    #[serde(default)]
    pub directory_allowlist: Vec<String>,
    /// MCP servers/tools always allowed (§5.7).
    #[serde(default)]
    pub mcp_allowlist: Vec<String>,
    /// MCP servers/tools always denied (§5.7).
    #[serde(default)]
    pub mcp_denylist: Vec<String>,
    /// Model role: `config_key` of the model everything falls back to
    /// (§4.2). `""` spells "unset" per §5.7.
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub base_model: Option<String>,
    /// Model role: `config_key` of the coding model; falls back to
    /// `base_model` when unset (§4.2).
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub coding_model: Option<String>,
    /// Model role: `config_key` of the NL→command model; falls back to
    /// `base_model` when unset (§4.2).
    #[serde(default, deserialize_with = "empty_string_as_none")]
    pub command_model: Option<String>,
    /// Context-window cap for this profile; `0` means the model's own limit
    /// (§5.7, user story 25).
    #[serde(default)]
    pub context_window_limit: usize,
}

impl ExecutionProfile {
    /// The three model roles (§4.2) as `(key, referenced config_key)` pairs,
    /// in declaration order — for validation now and role resolution later.
    fn model_roles(&self) -> [(&'static str, &Option<String>); 3] {
        [
            ("base_model", &self.base_model),
            ("coding_model", &self.coding_model),
            ("command_model", &self.command_model),
        ]
    }
}

/// Per-tool permission level (§5.7, GLOSSARY "Permission level").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    /// The model flags the call safe/unsafe; unsafe falls back to asking.
    AgentDecides,
    /// Never ask.
    AlwaysAllow,
    /// Ask every time.
    AlwaysAsk,
    /// Ask on the first write to the PTY, then allow (§5.7).
    AskOnFirstWrite,
    /// Never permit.
    Never,
}

/// §5.7 spells an unset model role as `""`; normalize it to `None` so the
/// model holds a single representation of "unset".
fn empty_string_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.filter(|value| !value.is_empty()))
}
