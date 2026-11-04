//! XDG-aware resolution of the store's on-disk location (Spec R08,
//! user story 1): `$XDG_DATA_HOME/ait/store.db`, falling back to
//! `~/.local/share/ait/store.db`.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;

/// Where `store.db` lives (§5.8): `$XDG_DATA_HOME/ait/store.db`,
/// falling back to `~/.local/share/ait/store.db`.
pub fn store_path() -> PathBuf {
    data_dir(
        env::var_os("XDG_DATA_HOME"),
        env::home_dir().unwrap_or_default(),
    )
    .join("ait")
    .join("store.db")
}

/// The data-directory half of [`store_path`], split out so the XDG
/// rules are testable without mutating the process environment — the
/// same shape as ait-config's `config_dir`: `$XDG_DATA_HOME` applies
/// only when it is a non-empty absolute path (the base-directory spec
/// declares relative values invalid), and an undiscoverable home
/// directory falls back to a relative `.local/share`, which resolves
/// against the working directory like any other missing base.
fn data_dir(xdg_data_home: Option<OsString>, home_dir: PathBuf) -> PathBuf {
    let xdg = PathBuf::from(xdg_data_home.unwrap_or_default());
    if xdg.is_absolute() {
        xdg
    } else {
        home_dir.join(".local").join("share")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_xdg_data_home_wins() {
        assert_eq!(
            data_dir(Some(OsString::from("/tmp/xdg")), PathBuf::from("/home/u")),
            PathBuf::from("/tmp/xdg")
        );
    }

    #[test]
    fn unset_or_empty_or_relative_xdg_uses_home_local_share() {
        for xdg in [
            None,
            Some(OsString::new()),
            Some(OsString::from("rel/path")),
        ] {
            assert_eq!(
                data_dir(xdg, PathBuf::from("/home/u")),
                PathBuf::from("/home/u/.local/share")
            );
        }
    }

    #[test]
    fn missing_home_falls_back_to_relative_local_share() {
        assert_eq!(
            data_dir(None, PathBuf::new()),
            PathBuf::from(".local/share")
        );
    }
}
