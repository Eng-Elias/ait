//! Where `ait-config`'s [`Settings`] meet the app's live state (Spec
//! R03 `ait` binary wiring): the pure mappings the loop consults —
//! `[terminal]` → spawn options for NEW sessions, `base_model` → the
//! status line's model slot — plus [`LiveSettings`], the last-good
//! container a hot reload swaps wholesale.

use std::ffi::OsString;

use ait_config::{ModelRole, SecretStore, Settings, TerminalSettings};
use ait_term::LiveSessionOptions;

/// The model slot's text when no model resolves — Spec R02's `—`
/// placeholder becomes story 24's "unset" rendering.
pub const MODEL_UNSET: &str = "—";

/// The profile whose model roles drive the status line (§4.2).
const DEFAULT_PROFILE: &str = "default";

/// The [`SecretStore`] the status line resolves through. The model slot
/// needs only the display name — never the secret a request would send
/// — so OS-keyring lookups (which can block on the Secret Service) are
/// skipped entirely; `api_key_env` vars still win first inside
/// `Endpoint::resolve_secret` per §4.1's env-first rule.
pub struct NoSecrets;

impl SecretStore for NoSecrets {
    fn get(&self, _service: &str, _account: &str) -> Option<String> {
        None
    }
}

/// The last-good [`Settings`] plus the values derived from it that the
/// loop reads every tick. A reload adopts a whole new struct, so the
/// derived state can never lag the settings it came from (stories
/// 10–11's keep-last-good is simply not adopting on error).
pub struct LiveSettings {
    settings: Settings,
    /// `base_model`'s display name for the status slot, resolved at
    /// adoption time — never recomputed per frame.
    model: String,
}

impl LiveSettings {
    /// Adopt `settings`, resolving the status-line model slot through
    /// `store`.
    pub fn adopt(settings: Settings, store: &impl SecretStore) -> Self {
        let model = model_slot(&settings, store);
        Self { settings, model }
    }

    /// The live settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The status line's model slot (story 24).
    pub fn model(&self) -> &str {
        &self.model
    }
}

/// `[terminal]` as spawn options for a NEW session (stories 12–13):
/// `shell` becomes the spawned program — unset keeps `ait-term`'s
/// `$SHELL` → `/bin/sh` resolution (§5.1) — and `scrollback_lines` the
/// scrollback capacity. The rest of the options stay the manager's
/// (size is the content area, cwd is inherited).
pub fn session_options(terminal: &TerminalSettings) -> LiveSessionOptions {
    let mut options = LiveSessionOptions::default();
    if let Some(shell) = &terminal.shell {
        options.program = Some(OsString::from(shell));
    }
    if let Some(lines) = terminal.scrollback_lines {
        options.session.scrollback = lines;
    }
    options
}

/// The status line's model slot (story 24): the resolved `base_model`'s
/// display name — its alias, else the wire name — or `—` when the role
/// is unset or unresolvable (e.g. no `default` profile).
pub fn model_slot(settings: &Settings, store: &impl SecretStore) -> String {
    settings
        .resolve_model(DEFAULT_PROFILE, ModelRole::Base, store)
        .ok()
        .flatten()
        .map(|resolved| resolved.display_name().to_string())
        .unwrap_or_else(|| MODEL_UNSET.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ait_config::Settings;
    use ait_term::DEFAULT_SCROLLBACK;
    use std::ffi::OsStr;

    /// A valid `settings.toml` with one endpoint, one model and a
    /// `default` profile's `base_model` pointing at it.
    const CONFIG: &str = r#"
[agents.custom_endpoints.local]
name     = "Local"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "wire-name"
alias      = "Alias Name"
config_key = "key-1"

[agents.execution_profiles.default]
base_model = "key-1"
"#;

    /// `[terminal]` `shell` becomes the spawned program and
    /// `scrollback_lines` the scrollback capacity (stories 12–13);
    /// unset fields keep `ait-term`'s defaults.
    #[test]
    fn session_options_maps_terminal_settings() {
        let terminal = TerminalSettings {
            shell: Some("/usr/bin/fish".to_string()),
            scrollback_lines: Some(42_000),
        };
        let options = session_options(&terminal);
        assert_eq!(
            options.program.as_deref(),
            Some(OsStr::new("/usr/bin/fish"))
        );
        assert_eq!(options.session.scrollback, 42_000);

        let unset = session_options(&TerminalSettings::default());
        assert!(
            unset.program.is_none(),
            "unset shell keeps ait-term's $SHELL → /bin/sh resolution (§5.1)"
        );
        assert_eq!(unset.session.scrollback, DEFAULT_SCROLLBACK);
    }

    /// Story 24: the model slot is the resolved `base_model`'s display
    /// name — the alias when set, else the wire name — and `—` when no
    /// model resolves.
    #[test]
    fn model_slot_is_alias_then_wire_name_then_dash() {
        let settings = Settings::from_toml(CONFIG).unwrap();
        assert_eq!(model_slot(&settings, &NoSecrets), "Alias Name");

        // An empty alias falls back to the wire name.
        let no_alias = Settings::from_toml(
            &CONFIG.replace("alias      = \"Alias Name\"", "alias      = \"\""),
        )
        .unwrap();
        assert_eq!(model_slot(&no_alias, &NoSecrets), "wire-name");

        // Unset roles — empty settings, or a config whose only profile
        // binds no base_model — render the placeholder dash.
        assert_eq!(model_slot(&Settings::default(), &NoSecrets), "—");
        let no_profile =
            Settings::from_toml(&CONFIG[..CONFIG.find("[agents.execution_profiles").unwrap()])
                .unwrap();
        assert_eq!(
            model_slot(&no_profile, &NoSecrets),
            "—",
            "no default profile means no model"
        );
    }

    /// Adopting settings resolves the model slot once — the live value
    /// can never lag the settings it came from (stories 10–11).
    #[test]
    fn live_settings_derives_the_model_slot_on_adoption() {
        let settings = Settings::from_toml(CONFIG).unwrap();
        let live = LiveSettings::adopt(settings, &NoSecrets);
        assert_eq!(live.model(), "Alias Name");

        let swapped = LiveSettings::adopt(Settings::default(), &NoSecrets);
        assert_eq!(swapped.model(), "—");
    }
}
