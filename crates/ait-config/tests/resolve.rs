//! Tests at the model-role resolution seam (§4.2, user stories 22–24):
//! `Settings::resolve_model` turns a profile name + [`ModelRole`] into
//! the endpoint, wire model, alias and secret-or-none the status line
//! and later rounds need. Unset roles fall back to `base_model`; unknown
//! profiles and roles are errors that name them.

use std::collections::BTreeMap;
use std::env;

use ait_config::{ModelRole, SecretStore, Settings};

/// A `(service, account)` → secret map; same seam as the secrets tests.
struct StubStore(BTreeMap<(String, String), String>);

impl StubStore {
    fn with(service: &str, account: &str, secret: &str) -> Self {
        let mut entries = BTreeMap::new();
        entries.insert((service.into(), account.into()), secret.into());
        Self(entries)
    }
    fn empty() -> Self {
        Self(BTreeMap::new())
    }
}

impl SecretStore for StubStore {
    fn get(&self, service: &str, account: &str) -> Option<String> {
        self.0.get(&(service.into(), account.into())).cloned()
    }
}

/// Two endpoints, one model each; the `default` profile sets `base_model`
/// to the local model and `coding_model` to the hosted one, leaving
/// `command_model` unset. A second profile pins only `base_model`.
const DOCUMENT: &str = r#"
[agents.custom_endpoints.local]
name     = "Local Ollama"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "qwen2.5-coder:14b"
alias      = "Qwen Coder"
config_key = "key-local"

[agents.custom_endpoints.hosted]
name        = "Anthropic"
base_url    = "https://api.anthropic.com"
schema      = "anthropic_messages"
api_key_env = "AIT_TEST_RESOLVE_KEY"

[[agents.custom_endpoints.hosted.models]]
name       = "claude-sonnet-4-5"
alias      = "Sonnet"
config_key = "key-hosted"

[agents.execution_profiles.default]
base_model   = "key-local"
coding_model = "key-hosted"

[agents.execution_profiles.minimal]
base_model = "key-hosted"
"#;

fn settings() -> Settings {
    Settings::from_toml(DOCUMENT).unwrap()
}

#[test]
fn base_model_resolves_endpoint_model_alias_and_secret() {
    // The local endpoint has no `api_key_env`, so the keyring is the only
    // source — story 22's endpoint + model + secret bundle.
    let store = StubStore::with("ait", "local", "local-secret");
    let settings = settings();
    let resolved = settings
        .resolve_model("default", ModelRole::Base, &store)
        .unwrap()
        .unwrap();

    assert_eq!(resolved.endpoint_key, "local");
    assert_eq!(resolved.endpoint.name, "Local Ollama");
    assert_eq!(resolved.model.name, "qwen2.5-coder:14b");
    assert_eq!(resolved.model.alias, "Qwen Coder");
    assert_eq!(resolved.secret(), Some("local-secret"));
}

#[test]
fn coding_model_resolves_its_own_endpoint() {
    env::set_var("AIT_TEST_RESOLVE_KEY", "anthropic-key");
    let settings = settings();
    let resolved = settings
        .resolve_model("default", ModelRole::Coding, &StubStore::empty())
        .unwrap()
        .unwrap();
    env::remove_var("AIT_TEST_RESOLVE_KEY");

    assert_eq!(resolved.endpoint_key, "hosted");
    assert_eq!(resolved.model.name, "claude-sonnet-4-5");
    assert_eq!(resolved.secret(), Some("anthropic-key"));
}

#[test]
fn unset_command_role_falls_back_to_base_model() {
    // Story 23: `command_model` is unset in DOCUMENT, so resolving the
    // command role lands on `base_model`.
    let settings = settings();
    let resolved = settings
        .resolve_model("default", ModelRole::Command, &StubStore::empty())
        .unwrap()
        .unwrap();
    assert_eq!(resolved.model.config_key, "key-local");
}

#[test]
fn profile_with_only_base_model_resolves_every_role_to_it() {
    let settings = settings();
    for role in [ModelRole::Base, ModelRole::Coding, ModelRole::Command] {
        let resolved = settings
            .resolve_model("minimal", role, &StubStore::empty())
            .unwrap()
            .unwrap();
        assert_eq!(resolved.model.config_key, "key-hosted");
    }
}

#[test]
fn profile_without_any_model_roles_resolves_to_none() {
    // Story 24's `—` case: nothing configured is `None`, not an error.
    let settings = Settings::from_toml("[agents.execution_profiles.default]\n").unwrap();
    let resolved = settings
        .resolve_model("default", ModelRole::Base, &StubStore::empty())
        .unwrap();
    assert!(resolved.is_none());
}

#[test]
fn unknown_profile_is_an_error_naming_it() {
    let err = settings()
        .resolve_model("ghost", ModelRole::Base, &StubStore::empty())
        .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("ghost"), "actual: {message:?}");
    assert!(
        message.starts_with("agents.execution_profiles.ghost:"),
        "actual: {message:?}"
    );
}

/// us-23's "one configured model is enough": with no profiles declared
/// at all, the sole configured model is the implicit `default` — every
/// role lands on it.
#[test]
fn a_single_model_with_no_profiles_is_the_implicit_default() {
    let one_model = &DOCUMENT[..DOCUMENT.find("[agents.custom_endpoints.hosted").unwrap()];
    let settings = Settings::from_toml(one_model).unwrap();
    for role in [ModelRole::Base, ModelRole::Coding, ModelRole::Command] {
        let resolved = settings
            .resolve_model("default", role, &StubStore::empty())
            .unwrap()
            .unwrap_or_else(|| panic!("{role:?} should resolve"));
        assert_eq!(resolved.model.config_key, "key-local");
        assert_eq!(resolved.endpoint_key, "local");
    }
}

/// Two models and no profile is ambiguous — the error asks for the
/// `default` table rather than guessing.
#[test]
fn several_models_with_no_profiles_is_an_error_asking_for_default() {
    let no_profiles = &DOCUMENT[..DOCUMENT.find("[agents.execution_profiles").unwrap()];
    let settings = Settings::from_toml(no_profiles).unwrap();
    let err = settings
        .resolve_model("default", ModelRole::Base, &StubStore::empty())
        .unwrap_err();
    assert!(
        err.to_string().contains("agents.execution_profiles"),
        "actual: {err:?}"
    );
}

/// No profiles and no models at all resolves to `None`, never an error.
#[test]
fn no_profiles_and_no_models_resolves_to_none() {
    let settings = Settings::default();
    let resolved = settings
        .resolve_model("default", ModelRole::Command, &StubStore::empty())
        .unwrap();
    assert!(resolved.is_none());
}

#[test]
fn role_names_round_trip_and_unknown_roles_are_errors_naming_them() {
    assert_eq!(ModelRole::from_name("base_model").unwrap(), ModelRole::Base);
    assert_eq!(
        ModelRole::from_name("coding_model").unwrap(),
        ModelRole::Coding
    );
    assert_eq!(
        ModelRole::from_name("command_model").unwrap(),
        ModelRole::Command
    );
    assert_eq!(ModelRole::Base.name(), "base_model");
    assert_eq!(ModelRole::Coding.name(), "coding_model");
    assert_eq!(ModelRole::Command.name(), "command_model");

    let err = ModelRole::from_name("chess_model").unwrap_err();
    assert!(err.to_string().contains("chess_model"), "actual: {err:?}");
}

#[test]
fn display_name_prefers_the_alias_then_the_wire_name() {
    // Story 24's status-line rule: alias → wire name (→ `—` handled by
    // the caller's `None` branch).
    let settings = Settings::from_toml(
        r#"
        [agents.custom_endpoints.local]
        name = "n"
        base_url = "u"
        schema = "openai_responses"

        [[agents.custom_endpoints.local.models]]
        name = "wire-name"
        alias = ""
        config_key = "k"

        [agents.execution_profiles.default]
        base_model = "k"
        "#,
    )
    .unwrap();
    let resolved = settings
        .resolve_model("default", ModelRole::Base, &StubStore::empty())
        .unwrap()
        .unwrap();
    assert_eq!(resolved.display_name(), "wire-name");
}

#[test]
fn debug_never_leaks_the_secret() {
    // CODING_STANDARDS §4: secrets out of logs. ResolvedModel's Debug
    // renders everything but the secret.
    let store = StubStore::with("ait", "local", "do-not-log-me");
    let settings = settings();
    let resolved = settings
        .resolve_model("default", ModelRole::Base, &store)
        .unwrap()
        .unwrap();
    let rendered = format!("{resolved:?}");
    assert!(rendered.contains("Qwen Coder"), "actual: {rendered:?}");
    assert!(!rendered.contains("do-not-log-me"), "actual: {rendered:?}");
}
