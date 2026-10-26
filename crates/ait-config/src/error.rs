//! [`ConfigError`] — the error half of the [`Settings::from_toml`] seam.
//!
//! [`Settings::from_toml`]: crate::Settings::from_toml

/// Error produced by [`Settings::from_toml`].
///
/// §5.3 requires errors that "point at the offending key", so `Display`
/// renders `<key-path>: <detail>` whenever a key path can be identified —
/// e.g. `agents.custom_endpoints.local.models[0].config_key: must not be
/// empty`. TOML syntax errors have no key path; they render the parser's own
/// line/column message instead.
///
/// [`Settings::from_toml`]: crate::Settings::from_toml
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    /// The rendered `<key-path>: <detail>` text — what the status line and
    /// CLI show the user.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// An error located at `path` (`agents.custom_endpoints.local.models[0].config_key`).
    pub(crate) fn at(path: String, detail: impl std::fmt::Display) -> Self {
        Self {
            message: format!("{path}: {detail}"),
        }
    }

    /// An error with no identifiable key path (TOML syntax errors): the
    /// detail carries the parser's own line/column rendering.
    pub(crate) fn unlocated(detail: impl std::fmt::Display) -> Self {
        Self {
            message: detail.to_string(),
        }
    }
}
