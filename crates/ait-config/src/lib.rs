//! `ait-config` — AIT's `settings.toml` model and file layer
//! (REQUIREMENTS.md §4).
//!
//! [`Settings::from_toml`] is the pure seam (Spec R03): feed TOML text in,
//! get a validated [`Settings`] back, or a [`ConfigError`] whose message
//! names the offending key path
//! (`agents.custom_endpoints.local.models[0].config_key: …`). Layered on
//! top, this crate owns everything `settings.toml` needs short of
//! hot-reload: [`load`]/[`settings_path`] resolve
//! `$XDG_CONFIG_HOME/ait/settings.toml` (falling back to `~/.config`),
//! [`Endpoint::resolve_secret`] reads secrets from the env var named by
//! `api_key_env` or the OS keyring behind the injectable [`SecretStore`]
//! seam, [`Settings::resolve_model`] turns a profile + [`ModelRole`] into
//! endpoint + model + secret-or-none, and [`default_template`]/
//! [`json_schema`] back `ait --init`/`--print-schema`.

mod emit;
mod error;
mod load;
mod resolve;
mod secrets;
mod settings;

pub use emit::{default_template, json_schema};
pub use error::ConfigError;
pub use load::{load, load_from, settings_path};
pub use resolve::{ModelRole, ResolvedModel};
pub use secrets::{OsKeyring, SecretStore};
pub use settings::{
    AgentsSettings, Endpoint, EndpointModel, EndpointSchema, ExecutionProfile, PermissionLevel,
    Settings, TerminalSettings,
};
