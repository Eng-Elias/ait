//! The `settings.toml` model (REQUIREMENTS.md §4) and the pure
//! [`Settings::from_toml`] parse/validate seam.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::error::ConfigError;

/// Parsed `settings.toml` — the full §4 configuration model.
///
/// Construct only via [`Settings::from_toml`]: serde handles shape and
/// unknown-key rejection (`deny_unknown_fields` on every table), and
/// `from_toml` adds the validation serde cannot express — unique, non-empty
/// `config_key`s and resolvable model-role references.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// `[terminal]` — shell + scrollback applied to NEW sessions only
    /// (§4.3); live sessions keep their spawn-time values.
    #[serde(default)]
    pub terminal: TerminalSettings,
    /// `[agents]` — custom endpoints (§4.1) and execution profiles (§5.7).
    #[serde(default)]
    pub agents: AgentsSettings,
    /// `[ssh]` — the §5.9 SSH-integration policy (§4.3, Spec R09).
    #[serde(default)]
    pub ssh: SshSettings,
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
    ///   `config_key`, with the error naming the role's key path;
    /// - every `command_allowlist`/`command_denylist` entry compiles as
    ///   a `regex` pattern (§5.7) — a broken one would silently never
    ///   match in the permission engine, so it is a load error naming
    ///   the entry's own `…[index]` path. `directory_allowlist` holds
    ///   paths and the `mcp_*` lists are inert — neither is a regex
    ///   list, and neither is validated here;
    /// - every `[ssh]` `hosts_allowlist`/`hosts_denylist` entry is a
    ///   well-formed OpenSSH `Host`-pattern glob (§5.9, Spec R09) — a
    ///   malformed one fails load naming its `…[index]` path.
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

        for (list, entries) in [
            ("hosts_allowlist", &self.ssh.hosts_allowlist),
            ("hosts_denylist", &self.ssh.hosts_denylist),
        ] {
            for (index, pattern) in entries.iter().enumerate() {
                if let Err(detail) = validate_host_pattern(pattern) {
                    return Err(ConfigError::at(format!("ssh.{list}[{index}]"), detail));
                }
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
            for (list, entries) in [
                ("command_allowlist", &profile.command_allowlist),
                ("command_denylist", &profile.command_denylist),
            ] {
                for (index, pattern) in entries.iter().enumerate() {
                    if let Err(error) = regex::Regex::new(pattern) {
                        return Err(ConfigError::at(
                            format!("agents.execution_profiles.{profile_name}.{list}[{index}]"),
                            error,
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// `[terminal]` (§4.3) — settings applied when spawning NEW sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct TerminalSettings {
    /// Program new sessions spawn; unset falls back to `$SHELL` then
    /// `/bin/sh` (§5.1).
    pub shell: Option<String>,
    /// Scrollback capacity in lines for new sessions.
    pub scrollback_lines: Option<usize>,
    /// Let child applications write the host clipboard via OSC 52
    /// (Spec R02 — off by default; `ait` performs the write itself when
    /// enabled).
    pub osc52: Option<bool>,
    /// Inject shell integration (OSC 133 marks + OSC 7700 hook events)
    /// into new sessions on supported shells — on when unset (§5.5,
    /// Spec R05). `false` opts out: new sessions spawn as plain
    /// terminals, live ones are untouched.
    pub shell_integration: Option<bool>,
}

/// `[ssh]` (§4.3, Spec R09 / §5.9) — the per-host SSH-integration
/// policy the armed decision renders into each spawned session's
/// `AIT_SSH_*` environment; the snippet-side `ssh()` wrapper enforces
/// it inside the shell.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SshSettings {
    /// The global off switch for every SSH feature — on when unset
    /// (§5.9). `false` restores the pre-Round-9 behavior: `ssh` stays
    /// a plain command in every new session.
    pub enable_integration: Option<bool>,
    /// OpenSSH `Host`-pattern globs (`*`, `?` within the destination
    /// token, `!`-prefixed entries negating) matched against the
    /// destination the user typed (`user@host`, `host`, config alias).
    /// An empty list is open — every host integrates that the denylist
    /// doesn't refuse.
    #[serde(default)]
    pub hosts_allowlist: Vec<String>,
    /// Patterns that make a matching `ssh` a plain passthrough — no
    /// bootstrap, no ControlMaster, never blocked (Spec R09). Deny
    /// beats allow, matching §5.7's rule.
    #[serde(default)]
    pub hosts_denylist: Vec<String>,
    /// When the user's own ssh config already multiplexes a host
    /// (`ControlMaster`/`ControlPath` in `ssh -G`), reuse that socket
    /// rather than attaching a parallel ait-managed one — on when
    /// unset (§5.9).
    pub reuse_existing_control_master: Option<bool>,
}

impl SshSettings {
    /// The armed enable flag — `true` when unset (§5.9's default-on).
    pub fn integration_enabled(&self) -> bool {
        self.enable_integration.unwrap_or(true)
    }

    /// The armed reuse flag — `true` when unset (Spec R09's default).
    pub fn reuses_control_master(&self) -> bool {
        self.reuse_existing_control_master.unwrap_or(true)
    }
}

/// One `hosts_*` entry's shape: a single OpenSSH `Host`-pattern glob
/// (`*`/`?`, an optional leading `!`). Empty entries, a bare `!`,
/// embedded whitespace and commas are malformed — a comma reads like
/// OpenSSH's in-pattern list separator, which this model spells as
/// separate list entries instead; rejecting it keeps a misread
/// pattern from silently never matching.
fn validate_host_pattern(pattern: &str) -> Result<(), String> {
    let body = pattern.strip_prefix('!').unwrap_or(pattern);
    if body.is_empty() {
        return Err(String::from("must be a non-empty host pattern"));
    }
    if body.contains(',') {
        return Err(String::from(
            "one glob per entry — spell `a,b` as two list entries",
        ));
    }
    if body.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(String::from("must not contain whitespace"));
    }
    Ok(())
}

/// `[agents]` — endpoint declarations (§4.1) and execution profiles (§5.7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
