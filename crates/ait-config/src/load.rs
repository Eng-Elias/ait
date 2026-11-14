//! File loading over the [`Settings::from_toml`] pure seam (Spec R03,
//! user stories 1, 6–8): XDG-aware path resolution, "missing means
//! defaults", and `ConfigError` passthrough for invalid files.

use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use crate::{ConfigError, Settings};

/// Where `settings.toml` lives (§4): `$XDG_CONFIG_HOME/ait/settings.toml`
/// — the override applies on every platform — falling back to
/// `~/.config/ait/settings.toml` on unix and `%APPDATA%\ait\settings.toml`
/// on Windows (§5.14).
pub fn settings_path() -> PathBuf {
    config_dir(
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("APPDATA"),
        env::home_dir().unwrap_or_default(),
        cfg!(windows),
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

/// The config directory half of [`settings_path`], split out so the
/// whole resolution chain — the Windows arm included — is testable
/// without mutating the process environment.
///
/// `$XDG_CONFIG_HOME` applies on every platform, only when it is a
/// non-empty absolute path — the base-directory spec declares relative
/// values invalid. The platform (`windows` = `cfg!(windows)` at the
/// call sites, parameterized so the arm is exercised by tests
/// everywhere) picks the default root when XDG is absent: unix takes
/// `~/.config`, Windows takes `%APPDATA%` verbatim when set and
/// non-empty — Windows expands the variable verbatim; there is no
/// absolute-path rule — else `~\AppData\Roaming`, the folder the
/// variable names on a stock install (§5.14). An undiscoverable home
/// directory falls back to the relative `.config`/`AppData\Roaming`,
/// which resolves against the working directory like any other
/// missing config (i.e. yields defaults).
pub(crate) fn config_dir(
    xdg_config_home: Option<OsString>,
    appdata: Option<OsString>,
    home_dir: PathBuf,
    windows: bool,
) -> PathBuf {
    let xdg = PathBuf::from(xdg_config_home.unwrap_or_default());
    if xdg.is_absolute() {
        return xdg;
    }
    if windows {
        match appdata.filter(|dir| !dir.is_empty()).map(PathBuf::from) {
            Some(dir) => dir,
            None => home_dir.join("AppData").join("Roaming"),
        }
    } else {
        home_dir.join(".config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The non-XDG root for the host the suite runs on — the unix
    /// `~/.config`, the Windows `~\AppData\Roaming`.
    fn host_fallback(home: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(home).join("AppData").join("Roaming")
        } else {
            PathBuf::from(home).join(".config")
        }
    }

    #[test]
    fn absolute_xdg_wins() {
        assert_eq!(
            config_dir(
                Some(OsString::from("/tmp/xdg")),
                None,
                PathBuf::from("/home/u"),
                cfg!(windows)
            ),
            PathBuf::from("/tmp/xdg")
        );
    }

    #[test]
    fn unset_or_empty_xdg_uses_the_platform_fallback() {
        for xdg in [None, Some(OsString::new())] {
            assert_eq!(
                config_dir(xdg, None, PathBuf::from("/home/u"), cfg!(windows)),
                host_fallback("/home/u")
            );
        }
    }

    #[test]
    fn relative_xdg_is_invalid_per_the_base_directory_spec() {
        assert_eq!(
            config_dir(
                Some(OsString::from("relative/xdg")),
                None,
                PathBuf::from("/home/u"),
                cfg!(windows)
            ),
            host_fallback("/home/u")
        );
    }

    /// The Windows arm (§5.14, Spec R16): `%APPDATA%` is the config
    /// root, `~\AppData\Roaming` the fallback when the variable is
    /// unset or empty, and XDG still wins over both — an explicit
    /// override applies on every platform. The `windows` flag makes
    /// these branches exercisable off-Windows; the path literals are
    /// Windows-shaped on purpose — equality is verbatim.
    #[test]
    fn windows_resolves_appdata_then_roaming_fallback() {
        let roaming = "C:\\Users\\u\\AppData\\Roaming";
        let home = || PathBuf::from("C:\\Users\\u");

        // %APPDATA% set → verbatim.
        assert_eq!(
            config_dir(None, Some(OsString::from(roaming)), home(), true),
            PathBuf::from(roaming)
        );

        // Unset or empty %APPDATA% → ~\AppData\Roaming.
        for appdata in [None, Some(OsString::new())] {
            assert_eq!(
                config_dir(None, appdata, home(), true),
                home().join("AppData").join("Roaming")
            );
        }

        // No home either → the relative AppData/Roaming, mirroring the
        // unix `.config` fallback's resolve-against-cwd behavior.
        assert_eq!(
            config_dir(None, None, PathBuf::new(), true),
            PathBuf::from("AppData").join("Roaming")
        );

        // XDG still wins on Windows (a host-absolute value here —
        // `is_absolute` is compiled per-platform).
        assert_eq!(
            config_dir(
                Some(OsString::from("/tmp/xdg")),
                Some(OsString::from(roaming)),
                home(),
                true
            ),
            PathBuf::from("/tmp/xdg")
        );
    }
}
