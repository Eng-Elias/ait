//! Tests at the file-loading seam (Spec R03, user stories 1, 6–8):
//! `settings_path()` resolves `$XDG_CONFIG_HOME/ait/settings.toml` with a
//! platform fallback (`~/.config` on unix, `%APPDATA%` on Windows —
//! §5.14); `load()`/`load_from()` turn a missing file into
//! built-in defaults and a present-but-invalid file into the T1
//! `ConfigError` naming the offending key.
//!
//! Env-overriding tests share `ENV_LOCK` — the settings path is process
//! environment, so tests that mutate it must not run concurrently.

use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ait_config::{load, load_from, settings_path, Settings};

/// Serializes tests that mutate the config-path environment.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Runs `body` with the config-path env pointed at the given dirs —
/// `XDG_CONFIG_HOME` → `xdg`, `%APPDATA%` (the Windows config root —
/// inert elsewhere) → `appdata`, and the home lookup → `home` (`HOME`
/// plus `USERPROFILE`, so `env::home_dir()` is pinned on Windows too).
/// `None` unsets. Previous values restore even on panic, so a failure
/// cannot poison the next test.
fn with_env<T>(
    xdg: Option<&Path>,
    appdata: Option<&Path>,
    home: Option<&Path>,
    body: impl FnOnce() -> T,
) -> T {
    let _guard = ENV_LOCK.lock().unwrap();
    let saved =
        ["XDG_CONFIG_HOME", "APPDATA", "HOME", "USERPROFILE"].map(|name| (name, env::var_os(name)));
    let home = home.map(|p| p.as_os_str().to_os_string());
    set_or_remove("XDG_CONFIG_HOME", xdg.map(|p| p.as_os_str().to_os_string()));
    set_or_remove("APPDATA", appdata.map(|p| p.as_os_str().to_os_string()));
    set_or_remove("HOME", home.clone());
    set_or_remove("USERPROFILE", home);

    struct Restore([(&'static str, Option<OsString>); 4]);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (name, value) in std::mem::take(&mut self.0) {
                set_or_remove(name, value);
            }
        }
    }
    let _restore = Restore(saved);

    body()
}

fn set_or_remove(name: &str, value: Option<OsString>) {
    match value {
        Some(value) => env::set_var(name, value),
        None => env::remove_var(name),
    }
}

#[test]
fn xdg_config_home_wins_over_home() {
    let dirs = tempfile::tempdir().unwrap();
    let xdg = dirs.path().join("xdg");
    let appdata = dirs.path().join("appdata");
    let home = dirs.path().join("home");
    with_env(Some(&xdg), Some(&appdata), Some(&home), || {
        assert_eq!(settings_path(), xdg.join("ait").join("settings.toml"));
    });
}

/// The platform default when XDG is unset: `~/.config` on unix,
/// `%APPDATA%` verbatim on Windows (§5.14).
#[test]
fn unset_xdg_uses_the_platform_config_root() {
    let dirs = tempfile::tempdir().unwrap();
    let appdata = dirs.path().join("appdata");
    let home = dirs.path().join("home");
    with_env(None, Some(&appdata), Some(&home), || {
        let root = if cfg!(windows) {
            appdata.clone()
        } else {
            home.join(".config")
        };
        assert_eq!(settings_path(), root.join("ait").join("settings.toml"));
    });
}

/// The Windows-only tail of the chain (§5.14): `%APPDATA%` unset or
/// empty resolves `~\AppData\Roaming`. Off-Windows coverage of the
/// same chain lives in `load.rs`'s `config_dir` unit tests.
#[cfg(windows)]
#[test]
fn unset_or_empty_appdata_falls_back_to_roaming() {
    let dirs = tempfile::tempdir().unwrap();
    let home = dirs.path().join("home");
    for appdata in [None, Some(Path::new(""))] {
        with_env(None, appdata, Some(&home), || {
            assert_eq!(
                settings_path(),
                home.join("AppData")
                    .join("Roaming")
                    .join("ait")
                    .join("settings.toml")
            );
        });
    }
}

#[test]
fn load_missing_file_yields_defaults() {
    // Story 6: no settings.toml on first run is not an error.
    let dirs = tempfile::tempdir().unwrap();
    with_env(
        Some(dirs.path()),
        Some(dirs.path()),
        Some(dirs.path()),
        || {
            assert_eq!(load().unwrap(), Settings::default());
        },
    );
}

#[test]
fn load_reads_the_xdg_settings_file() {
    let dirs = tempfile::tempdir().unwrap();
    let ait_dir = dirs.path().join("ait");
    std::fs::create_dir_all(&ait_dir).unwrap();
    std::fs::write(
        ait_dir.join("settings.toml"),
        "[terminal]\nshell = \"/bin/zsh\"\n",
    )
    .unwrap();

    with_env(Some(dirs.path()), Some(dirs.path()), None, || {
        let settings = load().unwrap();
        assert_eq!(settings.terminal.shell.as_deref(), Some("/bin/zsh"));
    });
}

#[test]
fn load_invalid_file_names_the_offending_key() {
    // Stories 7–8: a present-but-invalid file is the T1 ConfigError.
    let dirs = tempfile::tempdir().unwrap();
    let ait_dir = dirs.path().join("ait");
    std::fs::create_dir_all(&ait_dir).unwrap();
    std::fs::write(ait_dir.join("settings.toml"), "[terminal]\nbogus = 1\n").unwrap();

    with_env(Some(dirs.path()), Some(dirs.path()), None, || {
        let err = load().unwrap_err();
        assert!(
            err.to_string().starts_with("terminal.bogus:"),
            "actual: {err:?}"
        );
    });
}

#[test]
fn load_from_missing_path_yields_defaults() {
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("nope.toml");
    assert_eq!(load_from(&path).unwrap(), Settings::default());
}

#[test]
fn load_from_reads_and_validates() {
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    std::fs::write(&path, "[terminal]\nscrollback_lines = 9_000\n").unwrap();
    assert_eq!(
        load_from(&path).unwrap().terminal.scrollback_lines,
        Some(9_000)
    );
}

#[test]
fn load_from_unreadable_path_is_an_error() {
    // A file that exists but cannot be read is neither "missing" nor
    // valid TOML — it surfaces as an error naming the path, so a
    // permissions problem cannot masquerade as "no config".
    let dirs = tempfile::tempdir().unwrap();
    let ait_dir = dirs.path().join("ait");
    std::fs::create_dir_all(&ait_dir).unwrap();
    // A directory at the settings path fails reads on every platform.
    let path: PathBuf = ait_dir.join("settings.toml");
    std::fs::create_dir(&path).unwrap();
    let err = load_from(&path).unwrap_err();
    assert!(err.to_string().contains("settings.toml"), "actual: {err:?}");
}
