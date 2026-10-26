//! `ait-config` — AIT's `settings.toml` model (REQUIREMENTS.md §4).
//!
//! [`Settings::from_toml`] is the pure seam (Spec R03): feed TOML text in,
//! get a validated [`Settings`] back, or a [`ConfigError`] whose message
//! names the offending key path
//! (`agents.custom_endpoints.local.models[0].config_key: …`). File loading
//! (`~/.config/ait/settings.toml`, respecting `$XDG_CONFIG_HOME`), hot
//! reload, secret resolution and `--init`/`--print-schema` support layer on
//! top in later Round 3 tickets — nothing here touches the filesystem or
//! the environment.

mod error;
mod settings;

pub use error::ConfigError;
pub use settings::{
    AgentsSettings, Endpoint, EndpointModel, EndpointSchema, ExecutionProfile, PermissionLevel,
    Settings, TerminalSettings,
};
