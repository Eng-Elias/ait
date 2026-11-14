//! XDG-aware resolution of the store's on-disk location (Spec R08,
//! user story 1): `$XDG_DATA_HOME/ait/store.db`, falling back to
//! `~/.local/share/ait/store.db` on unix and
//! `%LOCALAPPDATA%\ait\store.db` on Windows (§5.14).

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

/// Where `store.db` lives (§5.8): `$XDG_DATA_HOME/ait/store.db`,
/// falling back to `~/.local/share/ait/store.db` on unix and
/// `%LOCALAPPDATA%\ait\store.db` on Windows (§5.14).
pub fn store_path() -> PathBuf {
    data_dir(
        env::var_os("XDG_DATA_HOME"),
        env::var_os("LOCALAPPDATA"),
        env::home_dir().unwrap_or_default(),
        cfg!(windows),
    )
    .join("ait")
    .join("store.db")
}

/// The data-directory half of [`store_path`], split out so the whole
/// resolution chain — the Windows arm included — is testable without
/// mutating the process environment. It is the same shape as
/// ait-config's `config_dir`: `$XDG_DATA_HOME` applies on every
/// platform, only when it is a non-empty absolute path (the
/// base-directory spec declares relative values invalid). The
/// platform (`windows` = `cfg!(windows)` at the call site,
/// parameterized so the arm is exercised by tests everywhere) picks
/// the default root when XDG is absent: unix takes
/// `~/.local/share`, Windows takes `%LOCALAPPDATA%` verbatim when set
/// and non-empty — Windows expands the variable verbatim; there is
/// no absolute-path rule — else `~\AppData\Local`, the folder the
/// variable names on a stock install (§5.14: machine-local data
/// stays local). An undiscoverable home directory falls back to the
/// relative `.local/share`/`AppData\Local`, which resolves against
/// the working directory like any other missing base.
fn data_dir(
    xdg_data_home: Option<OsString>,
    local_appdata: Option<OsString>,
    home_dir: PathBuf,
    windows: bool,
) -> PathBuf {
    let xdg = PathBuf::from(xdg_data_home.unwrap_or_default());
    if xdg.is_absolute() {
        return xdg;
    }
    if windows {
        match local_appdata
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
        {
            Some(dir) => dir,
            None => home_dir.join("AppData").join("Local"),
        }
    } else {
        home_dir.join(".local").join("share")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The non-XDG root for the host the suite runs on — the unix
    /// `~/.local/share`, the Windows `~\AppData\Local`.
    fn host_fallback(home: &str) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(home).join("AppData").join("Local")
        } else {
            PathBuf::from(home).join(".local").join("share")
        }
    }

    #[test]
    fn absolute_xdg_data_home_wins() {
        assert_eq!(
            data_dir(
                Some(OsString::from("/tmp/xdg")),
                None,
                PathBuf::from("/home/u"),
                cfg!(windows)
            ),
            PathBuf::from("/tmp/xdg")
        );
    }

    #[test]
    fn unset_or_empty_or_relative_xdg_uses_the_platform_fallback() {
        for xdg in [
            None,
            Some(OsString::new()),
            Some(OsString::from("rel/path")),
        ] {
            assert_eq!(
                data_dir(xdg, None, PathBuf::from("/home/u"), cfg!(windows)),
                host_fallback("/home/u")
            );
        }
    }

    #[test]
    fn missing_home_falls_back_to_the_relative_platform_root() {
        let expected = if cfg!(windows) {
            PathBuf::from("AppData").join("Local")
        } else {
            PathBuf::from(".local/share")
        };
        assert_eq!(
            data_dir(None, None, PathBuf::new(), cfg!(windows)),
            expected
        );
    }

    /// The Windows arm (§5.14, Spec R16): `%LOCALAPPDATA%` is the data
    /// root — machine-local data stays local — `~\AppData\Local` the
    /// fallback when the variable is unset or empty, and XDG still
    /// wins over both. The `windows` flag makes these branches
    /// exercisable off-Windows; the path literals are Windows-shaped
    /// on purpose — equality is verbatim.
    #[test]
    fn windows_resolves_localappdata_then_local_fallback() {
        let local = "C:\\Users\\u\\AppData\\Local";
        let home = || PathBuf::from("C:\\Users\\u");

        // %LOCALAPPDATA% set → verbatim.
        assert_eq!(
            data_dir(None, Some(OsString::from(local)), home(), true),
            PathBuf::from(local)
        );

        // Unset or empty %LOCALAPPDATA% → ~\AppData\Local.
        for appdata in [None, Some(OsString::new())] {
            assert_eq!(
                data_dir(None, appdata, home(), true),
                home().join("AppData").join("Local")
            );
        }

        // XDG still wins on Windows (a host-absolute value here —
        // `is_absolute` is compiled per-platform).
        assert_eq!(
            data_dir(
                Some(OsString::from("/tmp/xdg")),
                Some(OsString::from(local)),
                home(),
                true
            ),
            PathBuf::from("/tmp/xdg")
        );
    }
}
