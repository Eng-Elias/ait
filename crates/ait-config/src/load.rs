//! File loading over the [`Settings::from_toml`] pure seam (Spec R03,
//! user stories 1, 6–8): XDG-aware path resolution, "missing means
//! defaults", and `ConfigError` passthrough for invalid files.

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use crate::{ConfigError, Settings};

/// Where `settings.toml` lives (§4): `$XDG_CONFIG_HOME/ait/settings.toml`,
/// falling back to `~/.config/ait/settings.toml`.
pub fn settings_path() -> PathBuf {
    config_dir(
        env::var_os("XDG_CONFIG_HOME"),
        env::home_dir().unwrap_or_default(),
    )
    .join("ait")
    .join("settings.toml")
}

/// Load `settings.toml` from [`settings_path`]: a missing file yields
/// built-in defaults (user story 6 — first run is not an error), a
/// present-but-invalid file yields the [`Settings::from_toml`]
/// `ConfigError` naming the offending key, and an unreadable file is an
/// error naming the path so a permissions problem cannot masquerade as
/// "no config".
pub fn load() -> Result<Settings, ConfigError> {
    load_from(settings_path())
}

/// [`load`] at an explicit path — the seam callers with their own path
/// (tests, `--config` flags) use instead of the environment.
pub fn load_from(path: impl AsRef<Path>) -> Result<Settings, ConfigError> {
    let path = path.as_ref();
    match std::fs::read_to_string(path) {
        Ok(text) => Settings::from_toml(&text),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(err) => Err(ConfigError::unlocated(format!(
            "{}: cannot read settings file: {err}",
            path.display()
        ))),
    }
}

/// The config directory half of [`settings_path`], split out so the XDG
/// rules are testable without mutating the process environment.
///
/// `$XDG_CONFIG_HOME` applies only when it is a non-empty absolute path —
/// the base-directory spec declares relative values invalid — and an
/// undiscoverable home directory falls back to a relative `.config`,
/// which resolves against the working directory like any other missing
/// config (i.e. yields defaults).
fn config_dir(xdg_config_home: Option<OsString>, home_dir: PathBuf) -> PathBuf {
    let xdg = PathBuf::from(xdg_config_home.unwrap_or_default());
    if xdg.is_absolute() {
        xdg
    } else {
        home_dir.join(".config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_xdg_wins() {
        assert_eq!(
            config_dir(Some(OsString::from("/tmp/xdg")), PathBuf::from("/home/u")),
            PathBuf::from("/tmp/xdg")
        );
    }

    #[test]
    fn unset_or_empty_xdg_uses_home_config() {
        for xdg in [None, Some(OsString::new())] {
            assert_eq!(
                config_dir(xdg, PathBuf::from("/home/u")),
                PathBuf::from("/home/u/.config")
            );
        }
    }

    #[test]
    fn relative_xdg_is_invalid_per_the_base_directory_spec() {
        assert_eq!(
            config_dir(
                Some(OsString::from("relative/xdg")),
                PathBuf::from("/home/u")
            ),
            PathBuf::from("/home/u/.config")
        );
    }
}
