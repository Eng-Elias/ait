//! Secret resolution (§4.1, user stories 19–21): the env var named by
//! `api_key_env` first, then the OS keyring entry `ait/<endpoint-key>`.
//! Secrets never live in `settings.toml` (CODING_STANDARDS §4) and a
//! missing secret is `None`, never an error — local endpoints simply
//! send no `Authorization` header.

use std::env;

use crate::Endpoint;

/// Keyring service name for every ait-managed secret; the account is the
/// endpoint's table key (`ait/local`, `ait/work`, …).
const SERVICE: &str = "ait";

/// The injectable secret-store seam (Spec R03): what ait needs from an
/// OS keyring, nothing more — lookups only, no ait-side setter, and
/// every failure mode (no entry, no provider, locked store) collapses to
/// `None` because a missing secret is not a validation error.
pub trait SecretStore {
    /// The stored secret for `service`/`account`, or `None`.
    fn get(&self, service: &str, account: &str) -> Option<String>;
}

/// The production [`SecretStore`]: the platform keyring via the
/// `keyring` crate's default `v1` store set (zbus Secret Service on
/// Linux, Keychain on macOS, Windows Credential Manager on Windows).
/// Behind the `secrets` cargo feature (issue #77): the heavy
/// platform-store tree stays out of builds that never touch an OS
/// keyring — the trait and [`Endpoint::resolve_secret`] are
/// unconditional, so injecting another store needs no feature.
#[cfg(feature = "secrets")]
pub struct OsKeyring;

#[cfg(feature = "secrets")]
impl SecretStore for OsKeyring {
    fn get(&self, service: &str, account: &str) -> Option<String> {
        keyring::Entry::new(service, account)
            .and_then(|entry| entry.get_password())
            .ok()
            .filter(|secret| !secret.is_empty())
    }
}

impl Endpoint {
    /// This endpoint's secret, resolved per §4.1: the env var named by
    /// `api_key_env` wins when it is set and non-empty; otherwise the
    /// keyring entry `ait/<endpoint-key>` is tried. Endpoints without
    /// `api_key_env` go straight to the keyring, and an endpoint with no
    /// secret anywhere resolves to `None` — request-time code sends no
    /// `Authorization` header then (user story 21). The `?Sized` store
    /// lets a `&dyn SecretStore` flow straight through — e.g. from a
    /// boxed store — with no sized wrapper in between.
    pub fn resolve_secret(
        &self,
        endpoint_key: &str,
        store: &(impl SecretStore + ?Sized),
    ) -> Option<String> {
        if let Some(env_name) = &self.api_key_env {
            if let Ok(value) = env::var(env_name) {
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
        store.get(SERVICE, endpoint_key)
    }
}
