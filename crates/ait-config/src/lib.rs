//! `ait-config` — AIT's `settings.toml` model and file layer
//! (REQUIREMENTS.md §4).
//!
//! [`Settings::from_toml`] is the pure seam (Spec R03): feed TOML text in,
//! get a validated [`Settings`] back, or a [`ConfigError`] whose message
//! names the offending key path
//! (`agents.custom_endpoints.local.models[0].config_key: …`). Layered on
//! top, this crate owns everything `settings.toml` needs:
//! [`load`]/[`settings_path`] resolve
//! `$XDG_CONFIG_HOME/ait/settings.toml` (falling back to `~/.config`),
//! the `watch` feature hot-reloads it onto a channel the consumer
//! drains (Spec R03, user stories 10–11), [`Endpoint::resolve_secret`]
//! reads secrets from the env var named by `api_key_env` or the OS keyring
//! behind the injectable [`SecretStore`] seam,
//! [`Settings::resolve_model`] turns a profile + [`ModelRole`] into
//! endpoint + model + secret-or-none, and [`default_template`]/
//! `json_schema` back `ait --init`/`--print-schema`. The §5.10.1 MCP
//! surface mirrors that split: [`McpConfig::from_json`] is the pure
//! `mcp.json` seam, [`mcp_path`]/[`project_ait_dir`] resolve the user
//! and project `.ait/` layers, and [`load_mcp`] merges them into one
//! origin-labeled view. §5.10.2's skills loader reuses the same
//! layers: [`user_skills_dir`]/[`project_skills_dir`] resolve the two
//! roots ait-agent's `SkillsCatalog` scans.
//!
//! ## Cargo features
//!
//! The R05-T8 dependency audit (issue #77) puts each heavy dependency
//! behind the feature area that uses it, so a consumer that only needs
//! the model takes no filesystem/keyring/schema machinery — the `ait`
//! binary enables `schema` + `watch`, `ait-agent` `secrets`:
//!
//! - `secrets` — `OsKeyring`, the `keyring`-backed production
//!   [`SecretStore`] (zbus Secret Service on Linux, Keychain on macOS,
//!   Credential Manager on Windows). The trait and [`Endpoint`]'s
//!   env-first resolution are always present; only the concrete OS
//!   store is gated.
//! - `watch` — `watch`/`watch_from`/`SettingsWatcher` hot reload on
//!   `notify`.
//! - `schema` — `json_schema` (`ait --print-schema`) plus the
//!   `schemars::JsonSchema` derives on the settings model, on
//!   `schemars` + `serde_json`.

mod emit;
mod error;
mod load;
mod mcp;
mod resolve;
mod secrets;
mod settings;
mod skills;
#[cfg(feature = "watch")]
mod watch;

#[cfg(feature = "schema")]
pub use emit::json_schema;
pub use emit::{default_template, init_settings};
pub use error::ConfigError;
pub use load::{load, load_from, settings_path};
pub use mcp::{
    load_mcp, mcp_path, project_ait_dir, McpConfig, McpServer, McpServerEntry, McpServerOrigin,
    McpServers, PROJECT_LAYER_LABEL, USER_LAYER_LABEL,
};
pub use resolve::{ModelRole, ResolvedModel};
#[cfg(feature = "secrets")]
pub use secrets::OsKeyring;
pub use secrets::SecretStore;
pub use settings::{
    pattern_names, AgentsSettings, Endpoint, EndpointModel, EndpointSchema, ExecutionProfile,
    PermissionLevel, RedactionSettings, Settings, SshSettings, TerminalSettings,
};
pub use skills::{project_skills_dir, user_skills_dir};
#[cfg(feature = "watch")]
pub use watch::{watch, watch_dirs, watch_from, DirsWatcher, SettingsWatcher};
