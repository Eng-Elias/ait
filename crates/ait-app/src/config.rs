//! Where `ait-config`'s [`Settings`] meet the app's live state (Spec
//! R03 `ait` binary wiring): the pure mappings the loop consults —
//! `[terminal]` → spawn options for NEW sessions, `base_model` → the
//! status line's model slot — plus [`LiveSettings`], the last-good
//! container a hot reload swaps wholesale.

use std::ffi::OsString;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::Instant;

use ait_config::{ConfigError, ModelRole, SecretStore, Settings, SettingsWatcher};
use ait_term::LiveSessionOptions;

use crate::profile::ProfilePicker;
use crate::status::Toast;
use crate::tabs::{SessionTemplate, TabManager};

/// The model slot's text when no model resolves — Spec R02's `—`
/// placeholder becomes story 24's "unset" rendering.
pub const MODEL_UNSET: &str = "—";

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

/// The last-good [`Settings`] the loop reads every tick. A reload
/// adopts a whole new struct, so the derived state can never lag the
/// settings it came from (stories 10–11's keep-last-good is simply
/// not adopting on error). The status-line model slot is NOT adopted
/// state: it resolves on demand against the caller's active profile
/// (Spec R07 §5.7), so a profile switch needs no re-adoption.
pub struct LiveSettings {
    settings: Settings,
}

impl LiveSettings {
    /// Adopt `settings` as the new last-good.
    pub fn adopt(settings: Settings) -> Self {
        Self { settings }
    }

    /// The live settings.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The status line's model slot (story 24) under `profile` — the
    /// active execution profile's `base_model` (§4.2), resolved
    /// through [`NoSecrets`]: the slot needs only the display name,
    /// never the secret a request would send.
    pub fn model(&self, profile: &str) -> String {
        model_slot(&self.settings, profile, &NoSecrets)
    }
}

/// [`ait_config::watch`] after making sure the settings directory exists
/// (stories 10–11). The watcher subscribes to the file's PARENT
/// directory — a first run without `--init` has no config dir yet, and
/// without creating it the user's first mid-session `settings.toml`
/// write could never reload. Creating the app's own config dir is safe
/// (`--init` does the same); a genuine watch failure still yields `None`
/// — hot reload just stays off, never an error.
pub fn watcher() -> Option<SettingsWatcher> {
    watcher_at(&ait_config::settings_path())
}

/// [`watcher`] at an explicit path — the test seam mirroring
/// `ait_config::watch_from`.
fn watcher_at(path: &Path) -> Option<SettingsWatcher> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    ait_config::watch_from(path).ok()
}

/// `[terminal]` + `[ssh]` as the spawn template for NEW sessions
/// (stories 12–13, Spec R05 us-3, Spec R09-T1): `shell` becomes the
/// spawned program — unset keeps `ait-term`'s `$SHELL` → `/bin/sh`
/// resolution (§5.1) — `scrollback_lines` the scrollback capacity,
/// `osc52` the child-app clipboard-write opt-in (Spec R02) and
/// `shell_integration` whether the injection planner runs at spawn —
/// on when unset (§5.5). `[ssh]` arms the `AIT_SSH_*` env the
/// planner exports (§5.9). The rest of the options stay the
/// manager's (size is the content area, cwd is inherited).
pub fn session_template(settings: &Settings) -> SessionTemplate {
    let terminal = &settings.terminal;
    let mut options = LiveSessionOptions::default();
    if let Some(shell) = &terminal.shell {
        options.program = Some(OsString::from(shell));
    }
    if let Some(lines) = terminal.scrollback_lines {
        options.session.scrollback = lines;
    }
    if let Some(osc52) = terminal.osc52 {
        options.session.osc52 = osc52;
    }
    SessionTemplate {
        options,
        shell_integration: terminal.shell_integration.unwrap_or(true),
        ssh: crate::remote::SshSpawn::armed(&settings.ssh),
        // Instance state, not a setting — `TabManager::set_session_template`
        // carries the bound one across reloads (Spec R11).
        ctl_socket: None,
    }
}

/// The status-line text for a failed `mcp.json` load (Spec R10
/// us-4's "malformed mcp-file events toast"): the [`ConfigError`]'s
/// message already names the offending file (`<path>: <key-path>:
/// <detail>`), so the toast only labels which file failed — defined
/// once beside the skills' `skipped_text` precedent
/// (CODING_STANDARDS §17), shared by the submit-seam toast and any
/// later drain site.
///
/// [`ConfigError`]: ait_config::ConfigError
pub fn mcp_error_text(error: &ait_config::ConfigError) -> String {
    format!("mcp.json failed to load: {}", error.message())
}

/// The toast text a `settings.toml` load/reload failure produces —
/// "config error: <message>". The boot-degradation toast and the
/// watcher drain both label it identically (Spec R13 §5.12,
/// CODING_STANDARDS §17); `error.message()` already names the file.
pub fn error_text(message: &str) -> String {
    format!("config error: {message}")
}

/// One tick of settings-watcher events (Spec R03 us-10/11 + R07
/// us-28) — the dispatch both frontends ran: a good reload adopts the
/// new last-good and replaces the NEW-session template, an armed
/// profile missing from the new file toasts instead of silently
/// switching, and an open picker drops (its names belong to the old
/// file). A broken save toasts and keeps last-good. The frontend's
/// `watcher.receiver()` drives it. Returns the drained event count so
/// a repaint-gated frontend (the GUI's pump) can flag the activity.
pub fn drain_config_events(
    receiver: &Receiver<Result<Settings, ConfigError>>,
    config: &mut LiveSettings,
    profile: &str,
    picker: &mut Option<ProfilePicker>,
    toast: &mut Option<Toast>,
    tabs: &mut TabManager,
) -> usize {
    let mut drained = 0;
    while let Ok(event) = receiver.try_recv() {
        drained += 1;
        match event {
            Ok(settings) => {
                tabs.set_session_template(session_template(&settings));
                // The OLD declared names decide "deleted" vs
                // "never declared" — an armed `default` deleted
                // with the whole table toasts; one that never was
                // stays silent (us-28). The profile stays armed
                // either way: the R6 fail-safe asks on every tool
                // call until the user picks a live name.
                let vanished =
                    crate::profile::active_missing(config.settings(), &settings, profile)
                        .then(|| profile.to_string());
                *config = LiveSettings::adopt(settings);
                *picker = None;
                if let Some(name) = vanished {
                    *toast = Some(Toast::new(
                        format!("execution profile \"{name}\" no longer declared"),
                        Instant::now(),
                    ));
                }
            }
            Err(error) => {
                *toast = Some(Toast::new(error_text(error.message()), Instant::now()));
            }
        }
    }
    drained
}

/// The status line's model slot (story 24): `profile`'s resolved
/// `base_model` display name — its alias, else the wire name — or `—`
/// when the role is unset or unresolvable (e.g. an unknown profile).
pub fn model_slot(settings: &Settings, profile: &str, store: &impl SecretStore) -> String {
    settings
        .resolve_model(profile, ModelRole::Base, store)
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

    /// `[terminal]` `shell` becomes the spawned program,
    /// `scrollback_lines` the scrollback capacity, `osc52` the
    /// child-app clipboard opt-in (stories 12–13, Spec R02) and
    /// `shell_integration` the injection planner's switch (Spec R05
    /// us-3); unset fields keep `ait-term`'s defaults.
    #[test]
    fn session_template_maps_terminal_settings() {
        let terminal = ait_config::TerminalSettings {
            shell: Some("/usr/bin/fish".to_string()),
            scrollback_lines: Some(42_000),
            osc52: Some(true),
            shell_integration: Some(false),
        };
        let template = session_template(&Settings {
            terminal,
            ..Default::default()
        });
        let options = &template.options;
        assert_eq!(
            options.program.as_deref(),
            Some(OsStr::new("/usr/bin/fish"))
        );
        assert_eq!(options.session.scrollback, 42_000);
        assert!(options.session.osc52);
        assert!(!template.shell_integration, "explicit false opts out");

        let unset = session_template(&Settings::default());
        assert!(
            unset.options.program.is_none(),
            "unset shell keeps ait-term's $SHELL → /bin/sh resolution (§5.1)"
        );
        assert_eq!(unset.options.session.scrollback, DEFAULT_SCROLLBACK);
        assert!(!unset.options.session.osc52, "OSC 52 stays opt-in");
        assert!(
            unset.shell_integration,
            "shell_integration defaults on when unset (§5.5)"
        );
    }

    /// Story 24: the model slot is the resolved `base_model`'s display
    /// name — the alias when set, else the wire name — and `—` when no
    /// model resolves.
    #[test]
    fn model_slot_is_alias_then_wire_name_then_dash() {
        let settings = Settings::from_toml(CONFIG).unwrap();
        assert_eq!(model_slot(&settings, "default", &NoSecrets), "Alias Name");

        // An empty alias falls back to the wire name.
        let no_alias = Settings::from_toml(
            &CONFIG.replace("alias      = \"Alias Name\"", "alias      = \"\""),
        )
        .unwrap();
        assert_eq!(model_slot(&no_alias, "default", &NoSecrets), "wire-name");

        // No profiles at all: the sole configured model is the
        // implicit `default` (us-23); empty settings — zero models —
        // still render the placeholder dash.
        let no_profile =
            Settings::from_toml(&CONFIG[..CONFIG.find("[agents.execution_profiles").unwrap()])
                .unwrap();
        assert_eq!(
            model_slot(&no_profile, "default", &NoSecrets),
            "Alias Name",
            "one configured model resolves without a profile"
        );
        assert_eq!(model_slot(&Settings::default(), "default", &NoSecrets), "—");
    }

    /// Spec R07 §4.2/us-25 (issue #101): the slot resolves the ARMED
    /// profile's `base_model` — switching stances switches the model
    /// the status line names, and an unknown name resolves to `—`
    /// (the existing missing-profile `resolve_model` behavior).
    #[test]
    fn model_slot_follows_the_armed_profile() {
        let settings = Settings::from_toml(
            r#"
[agents.custom_endpoints.local]
name     = "Local"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "wire-name"
alias      = "Default Model"
config_key = "key-1"

[[agents.custom_endpoints.local.models]]
name       = "other-wire"
alias      = "Work Model"
config_key = "key-2"

[agents.execution_profiles.default]
base_model = "key-1"

[agents.execution_profiles.work]
base_model = "key-2"
"#,
        )
        .unwrap();

        assert_eq!(
            model_slot(&settings, "default", &NoSecrets),
            "Default Model"
        );
        assert_eq!(model_slot(&settings, "work", &NoSecrets), "Work Model");
        assert_eq!(
            model_slot(&settings, "gone", &NoSecrets),
            "—",
            "an unknown profile renders the placeholder"
        );
    }

    /// Stories 10–11's first-run case: no config dir exists yet —
    /// `watch_from` alone errors on the missing parent, so `watcher_at`
    /// must create it and the user's first mid-session `settings.toml`
    /// write still reloads.
    #[test]
    fn watcher_creates_a_missing_config_dir_and_reloads() {
        let dirs = tempfile::tempdir().unwrap();
        let path = dirs.path().join("ait").join("settings.toml");

        let watcher = watcher_at(&path).expect("watcher must land on a fresh config dir");

        std::fs::write(&path, "[terminal]\nshell = \"/bin/fish\"\n").unwrap();
        let settings = watcher
            .receiver()
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("no reload event arrived")
            .expect("a valid first write should emit Settings");
        assert_eq!(settings.terminal.shell.as_deref(), Some("/bin/fish"));
    }

    /// Adopting swaps the settings the slot resolves against — the
    /// live value can never lag the settings it came from (stories
    /// 10–11), and the armed profile is the caller's input, not
    /// adopted state (Spec R07 §5.7).
    #[test]
    fn live_settings_derives_the_model_slot_on_adoption() {
        let settings = Settings::from_toml(CONFIG).unwrap();
        let live = LiveSettings::adopt(settings);
        assert_eq!(live.model("default"), "Alias Name");

        let swapped = LiveSettings::adopt(Settings::default());
        assert_eq!(swapped.model("default"), "—");
    }
}
