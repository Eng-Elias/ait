//! The `mcp.json` model and the project-local `.ait/` config layer
//! (REQUIREMENTS.md §5.10.1, Spec R10).
//!
//! [`McpConfig::from_json`] is the pure seam — feed JSON text in, get a
//! validated [`McpConfig`] back, or a [`ConfigError`] naming the
//! offending key path (`mcpServers.db.command: must not be empty`).
//! [`mcp_path`] resolves `$XDG_CONFIG_HOME/ait/mcp.json` beside
//! [`settings_path`], [`project_ait_dir`] finds the nearest `.ait`
//! directory walking up from a session cwd — the same walk ait-agent's
//! `find_agents_md` uses for `AGENTS.md` — and [`load_mcp`] merges the
//! two layers into one origin-labeled view where a project server
//! shadows a same-named user server.
//!
//! Transport is stdio only this round (§5.10.1): a `url`-keyed remote
//! entry is a load error naming the server, not a silent skip.
//!
//! [`settings_path`]: crate::settings_path

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::ConfigError;
use crate::load::config_dir;

/// The `mcp.json` file name searched under `$XDG_CONFIG_HOME/ait/` and
/// the project `.ait/` directory.
const MCP_JSON: &str = "mcp.json";

/// The `.ait` directory name searched walking up from a session cwd —
/// the project-local config layer (§5.10.1).
const AIT_DIR: &str = ".ait";

/// Parsed `mcp.json` — the MCP server declarations of one config layer.
///
/// Construct only via [`McpConfig::from_json`]: serde handles shape and
/// unknown-key rejection (`deny_unknown_fields` on every object), and
/// `from_json` adds the validation serde cannot express — non-empty
/// `command`s and the stdio-only `url` rejection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpConfig {
    /// The `mcpServers` map: server name → its stdio declaration.
    pub servers: BTreeMap<String, McpServer>,
}

/// One declared MCP server — an `mcpServers.<name>` entry (§5.10.1).
/// Stdio transport only: ait spawns `command` with `args` and the
/// literal `env` additions (no expansion — Spec R10, out of scope).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// The program to spawn — required and non-empty.
    pub command: String,
    /// Arguments to `command`; empty when unset.
    pub args: Vec<String>,
    /// Extra environment entries passed through literally.
    pub env: BTreeMap<String, String>,
}

/// The serde shape of `mcp.json` — kept private so [`McpConfig`] is the
/// only model consumers see. `url` is a known field so it survives
/// `deny_unknown_fields` and can fail with the round's stdio-only
/// message instead of a generic unknown-field error.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpFile {
    #[serde(rename = "mcpServers", default)]
    servers: BTreeMap<String, McpServerRaw>,
}

/// The serde shape of one `mcpServers.<name>` entry.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpServerRaw {
    /// Optional in serde so a `url`-only remote entry reaches
    /// validation and fails with the stdio-only message rather than
    /// a generic missing-field error.
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Remote transport marker — rejected in validation (stdio only).
    url: Option<serde::de::IgnoredAny>,
}

impl McpConfig {
    /// Parse and validate `mcp.json` text — the pure seam mirroring
    /// [`Settings::from_toml`](crate::Settings::from_toml): no
    /// filesystem, no environment.
    ///
    /// JSON syntax errors render the parser's line/column message;
    /// every other error renders `<key-path>: <detail>` where the path
    /// is the JSON key that failed (`mcpServers.db.command`). A
    /// `url`-keyed server entry fails `mcpServers.<name>.url: stdio
    /// transport only …` — remote transports are out of scope this
    /// round (§5.10.1).
    pub fn from_json(input: &str) -> Result<Self, ConfigError> {
        // Mirror `from_toml`: the syntax pass runs through the same
        // deserializer `serde_path_to_error` wraps, and `de.end()`
        // rejects trailing junk the way `serde_json::from_str` does.
        let mut de = serde_json::Deserializer::from_str(input);
        let file: McpFile = serde_path_to_error::deserialize(&mut de).map_err(|err| {
            let path = err.path().to_string();
            // serde_path_to_error renders the root as `.`; a root-level
            // error has no key path to name.
            if path == "." {
                ConfigError::unlocated(err.inner().to_string())
            } else {
                ConfigError::at(path, err.inner().to_string())
            }
        })?;
        de.end().map_err(ConfigError::unlocated)?;

        let mut servers = BTreeMap::new();
        for (name, raw) in file.servers {
            if raw.url.is_some() {
                return Err(ConfigError::at(
                    format!("mcpServers.{name}.url"),
                    "stdio transport only this round — remote MCP servers are unsupported",
                ));
            }
            let Some(command) = raw.command else {
                return Err(ConfigError::at(
                    format!("mcpServers.{name}.command"),
                    "is required",
                ));
            };
            if command.trim().is_empty() {
                return Err(ConfigError::at(
                    format!("mcpServers.{name}.command"),
                    "must not be empty",
                ));
            }
            servers.insert(
                name,
                McpServer {
                    command,
                    args: raw.args,
                    env: raw.env,
                },
            );
        }
        Ok(Self { servers })
    }
}

/// Where the user-layer `mcp.json` lives (§5.10.1):
/// `$XDG_CONFIG_HOME/ait/mcp.json`, falling back to
/// `~/.config/ait/mcp.json` — beside [`settings_path`](crate::settings_path).
pub fn mcp_path() -> PathBuf {
    config_dir(
        env::var_os("XDG_CONFIG_HOME"),
        env::home_dir().unwrap_or_default(),
    )
    .join("ait")
    .join(MCP_JSON)
}

/// The nearest `.ait` directory to `cwd` (§5.10.1): walk `cwd`, then
/// each ancestor, and return the first `.ait` that is a directory —
/// the same nearest-wins walk `find_agents_md` uses for `AGENTS.md`.
/// `None` when none exists.
pub fn project_ait_dir(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        let candidate = dir.join(AIT_DIR);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// The merged, origin-labeled MCP server view for one session (Spec
/// R10): the user layer at [`mcp_path`] plus the project layer at
/// `<project_ait_dir(cwd)>/mcp.json`, each effective server labeled
/// with the layer that declared it. A project server shadows a
/// same-named user server — the more specific layer wins.
///
/// A missing file contributes no servers (a first run is not an
/// error); a malformed or unreadable file fails with a [`ConfigError`]
/// whose message names the offending path.
///
/// Nothing is spawned and no tools are declared here — this is the
/// validated contract the MCP pool reconciles against per task.
pub fn load_mcp(cwd: &Path) -> Result<McpServers, ConfigError> {
    let mut merged = McpServers::default();
    merged.merge(&mcp_path(), McpServerOrigin::User)?;
    if let Some(ait_dir) = project_ait_dir(cwd) {
        merged.merge(&ait_dir.join(MCP_JSON), McpServerOrigin::Project)?;
    }
    Ok(merged)
}

/// What [`load_mcp`] returns: the effective MCP servers for one
/// session, keyed by name and labeled with the layer that declared
/// each (Spec R10, user story 9 — the approve card names the layer).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct McpServers {
    /// Effective servers keyed by `mcpServers` name. A project entry
    /// replaces a same-named user entry wholesale.
    pub servers: BTreeMap<String, McpServerEntry>,
}

/// One effective server in the merged view: the parsed declaration
/// plus which config layer it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerEntry {
    /// Which layer declared this server.
    pub origin: McpServerOrigin,
    /// The parsed `mcpServers.<name>` entry.
    pub server: McpServer,
}

/// The config layer that declared an [`McpServerEntry`] — user config
/// or the project-local `.ait/` (GLOSSARY "project-local config
/// layer").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpServerOrigin {
    /// Declared by `$XDG_CONFIG_HOME/ait/mcp.json`.
    User,
    /// Declared by `<project>/.ait/mcp.json`.
    Project,
}

/// The "which layer declared it" phrases the prompt and the approve
/// cards render (Spec R10 us-9) — shared by the MCP catalog and
/// ait-agent's skills catalog, whose `SkillOrigin` mirrors this enum
/// (CODING_STANDARDS §17: one phrase, one definition site).
pub const USER_LAYER_LABEL: &str = "user config";

/// The project-local `.ait/` layer's phrase — see
/// [`USER_LAYER_LABEL`].
pub const PROJECT_LAYER_LABEL: &str = "project .ait/";

impl McpServerOrigin {
    /// The rendered layer phrase — [`USER_LAYER_LABEL`] or
    /// [`PROJECT_LAYER_LABEL`].
    pub fn label(self) -> &'static str {
        match self {
            Self::User => USER_LAYER_LABEL,
            Self::Project => PROJECT_LAYER_LABEL,
        }
    }
}

impl McpServers {
    /// Fold one layer's `mcp.json` into the merged view: a missing file
    /// contributes nothing, an invalid one fails naming its path, and
    /// later layers shadow earlier ones on name collision.
    fn merge(&mut self, path: &Path, origin: McpServerOrigin) -> Result<(), ConfigError> {
        let config = load_mcp_file(path)?;
        for (name, server) in config.servers {
            self.servers.insert(name, McpServerEntry { origin, server });
        }
        Ok(())
    }
}

/// One layer's `mcp.json` — missing means no servers, unreadable or
/// malformed means a [`ConfigError`] naming the path (mirroring
/// [`load_from`](crate::load_from)'s file rules).
fn load_mcp_file(path: &Path) -> Result<McpConfig, ConfigError> {
    match fs::read_to_string(path) {
        Ok(text) => McpConfig::from_json(&text)
            .map_err(|err| ConfigError::unlocated(format!("{}: {err}", path.display()))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(McpConfig::default()),
        Err(err) => Err(ConfigError::unlocated(format!(
            "{}: cannot read mcp.json: {err}",
            path.display()
        ))),
    }
}
