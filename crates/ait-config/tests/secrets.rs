//! Tests at the secret-resolution seam (Spec R03, user stories 19–21):
//! the env var named by `api_key_env` wins; when it is unset or empty the
//! OS keyring entry `ait/<endpoint-key>` is tried through the injectable
//! [`SecretStore`] seam — no test needs a real keyring. A missing secret
//! is `None`, never an error.
//!
//! Each test sets its own uniquely-named env var, so no serialization or
//! cleanup-order worries: the names never collide and are removed after
//! use anyway.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::env;

use ait_config::{Endpoint, EndpointSchema, SecretStore};

/// The injectable seam's test double: a map of `(service, account)` to
/// stored secret, recording every lookup so tests can assert the
/// `ait/<endpoint-key>` naming contract.
#[derive(Default)]
struct StubStore {
    entries: BTreeMap<(String, String), String>,
    queries: RefCell<Vec<(String, String)>>,
}

impl StubStore {
    fn with_entry(service: &str, account: &str, secret: &str) -> Self {
        let mut store = Self::default();
        store
            .entries
            .insert((service.into(), account.into()), secret.into());
        store
    }
}

impl SecretStore for StubStore {
    fn get(&self, service: &str, account: &str) -> Option<String> {
        self.queries
            .borrow_mut()
            .push((service.into(), account.into()));
        self.entries.get(&(service.into(), account.into())).cloned()
    }
}

fn endpoint(api_key_env: Option<&str>) -> Endpoint {
    Endpoint {
        name: "n".into(),
        base_url: "u".into(),
        schema: EndpointSchema::OpenAiChatCompletions,
        api_key_env: api_key_env.map(str::to_string),
        models: Vec::new(),
    }
}

#[test]
fn env_var_wins_over_the_keyring() {
    env::set_var("AIT_TEST_SECRET_ENV_WINS", "from-env");
    let store = StubStore::with_entry("ait", "hosted", "from-keyring");

    let secret = endpoint(Some("AIT_TEST_SECRET_ENV_WINS")).resolve_secret("hosted", &store);

    assert_eq!(secret.as_deref(), Some("from-env"));
    // The env hit must short-circuit: the keyring is never consulted.
    assert!(store.queries.borrow().is_empty());
    env::remove_var("AIT_TEST_SECRET_ENV_WINS");
}

#[test]
fn empty_env_var_falls_through_to_the_keyring() {
    env::set_var("AIT_TEST_SECRET_ENV_EMPTY", "");
    let store = StubStore::with_entry("ait", "hosted", "from-keyring");

    let secret = endpoint(Some("AIT_TEST_SECRET_ENV_EMPTY")).resolve_secret("hosted", &store);

    assert_eq!(secret.as_deref(), Some("from-keyring"));
    env::remove_var("AIT_TEST_SECRET_ENV_EMPTY");
}

#[test]
fn unset_env_var_uses_the_keyring_entry_named_after_the_endpoint() {
    env::remove_var("AIT_TEST_SECRET_ENV_UNSET");
    let store = StubStore::with_entry("ait", "hosted", "from-keyring");

    let secret = endpoint(Some("AIT_TEST_SECRET_ENV_UNSET")).resolve_secret("hosted", &store);

    assert_eq!(secret.as_deref(), Some("from-keyring"));
    assert_eq!(
        store.queries.borrow().as_slice(),
        &[("ait".to_string(), "hosted".to_string())],
        "keyring lookup must be service `ait`, account `<endpoint-key>`"
    );
}

#[test]
fn endpoint_without_api_key_env_goes_straight_to_the_keyring() {
    let store = StubStore::with_entry("ait", "local", "stored");
    assert_eq!(
        endpoint(None).resolve_secret("local", &store).as_deref(),
        Some("stored")
    );
}

#[test]
fn missing_secret_everywhere_is_none_not_an_error() {
    // Story 21: local endpoints legitimately have no secret; `None`
    // means "send no Authorization header", it is not a failure.
    env::remove_var("AIT_TEST_SECRET_ENV_MISSING");
    let store = StubStore::default();

    assert_eq!(
        endpoint(Some("AIT_TEST_SECRET_ENV_MISSING")).resolve_secret("local", &store),
        None
    );
    assert_eq!(endpoint(None).resolve_secret("local", &store), None);
}
