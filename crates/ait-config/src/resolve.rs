//! Model-role resolution (§4.2, user stories 22–24): a profile name plus
//! a [`ModelRole`] resolves to the endpoint, wire model and
//! secret-or-none that the status line and `ait-llm` consume. Unset
//! roles fall back to `base_model`; a profile with no model configured
//! resolves to `None` (the status line's `—`), never an error.

use std::fmt;

use crate::{ConfigError, Endpoint, EndpointModel, ExecutionProfile, SecretStore, Settings};

/// One of the §4.2 model roles an execution profile binds to a model
/// `config_key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRole {
    /// `base_model` — everything falls back to this role.
    Base,
    /// `coding_model` — the coding-oriented model.
    Coding,
    /// `command_model` — the NL→command model.
    Command,
}

impl ModelRole {
    /// The `settings.toml` key spelling (`"base_model"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Base => "base_model",
            Self::Coding => "coding_model",
            Self::Command => "command_model",
        }
    }

    /// A role by its TOML key name — for callers that take the role as
    /// text (CLI flags). Unknown names are errors that name them.
    pub fn from_name(name: &str) -> Result<Self, ConfigError> {
        match name {
            "base_model" => Ok(Self::Base),
            "coding_model" => Ok(Self::Coding),
            "command_model" => Ok(Self::Command),
            _ => Err(ConfigError::unlocated(format!(
                "unknown model role {name:?} \
                 (expected base_model, coding_model or command_model)"
            ))),
        }
    }
}

/// The outcome of [`Settings::resolve_model`]: which endpoint serves the
/// role, which model on it, and the secret-or-none to authenticate with
/// (§4.1). Borrows the settings; owns the resolved secret.
///
/// `Debug` redacts the secret — secrets never reach logs
/// (CODING_STANDARDS §4).
pub struct ResolvedModel<'a> {
    /// The endpoint's table key (`local`, `work`, …).
    pub endpoint_key: &'a str,
    /// The resolved endpoint.
    pub endpoint: &'a Endpoint,
    /// The resolved model on that endpoint — `model.name` is the wire id,
    /// `model.alias` the display name.
    pub model: &'a EndpointModel,
    secret: Option<String>,
}

impl ResolvedModel<'_> {
    /// The endpoint's secret, or `None` to send no `Authorization`
    /// header (user story 21).
    pub fn secret(&self) -> Option<&str> {
        self.secret.as_deref()
    }

    /// What the status line shows (story 24): the alias, falling back to
    /// the wire name.
    pub fn display_name(&self) -> &str {
        if self.model.alias.is_empty() {
            &self.model.name
        } else {
            &self.model.alias
        }
    }
}

impl fmt::Debug for ResolvedModel<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedModel")
            .field("endpoint_key", &self.endpoint_key)
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Settings {
    /// Resolve `role` on `profile` (conventionally `"default"`) to its
    /// endpoint + model + secret (§4.2).
    ///
    /// `Ok(None)` means the role — and its `base_model` fallback — is
    /// unset; `Err` means a named profile is missing (with any profiles
    /// declared), or a role references a `config_key` no model declares
    /// (unreachable through [`Settings::from_toml`], which rejects
    /// dangling references, but the fields are public). With NO
    /// profiles declared, the sole configured model is an implicit
    /// `default` for every role (us-23). The `?Sized` store lets a
    /// `&dyn SecretStore` flow straight through — e.g. from the boxed
    /// store `ait-agent`'s worker holds — with no sized wrapper in
    /// between.
    pub fn resolve_model(
        &self,
        profile: &str,
        role: ModelRole,
        store: &(impl SecretStore + ?Sized),
    ) -> Result<Option<ResolvedModel<'_>>, ConfigError> {
        let Some(profile_ref) = self.agents.execution_profiles.get(profile) else {
            // us-23's "one configured model is enough": when no
            // profiles are declared at all, the sole configured model
            // serves every role implicitly. Zero models resolves to
            // `None`; two or more is ambiguous, so the error asks for
            // the profile. Named profiles exist → the missing name
            // stays a hard "unknown profile" error.
            return if self.agents.execution_profiles.is_empty() {
                self.resolve_implicit_default(store)
            } else {
                Err(ConfigError::at(
                    format!("agents.execution_profiles.{profile}"),
                    "unknown execution profile",
                ))
            };
        };

        let (effective_role, config_key) = match role_key(profile_ref, role) {
            Some(config_key) => (role.name(), config_key),
            None => match profile_ref.base_model.as_deref() {
                Some(config_key) => ("base_model", config_key),
                None => return Ok(None),
            },
        };

        for (endpoint_key, endpoint) in &self.agents.custom_endpoints {
            if let Some(model) = endpoint
                .models
                .iter()
                .find(|model| model.config_key == config_key)
            {
                return Ok(Some(ResolvedModel {
                    endpoint_key,
                    endpoint,
                    model,
                    secret: endpoint.resolve_secret(endpoint_key, store),
                }));
            }
        }
        Err(ConfigError::at(
            format!("agents.execution_profiles.{profile}.{effective_role}"),
            format!("references unknown config_key {config_key:?}"),
        ))
    }
}

impl Settings {
    /// The no-profiles fallback (us-23): exactly one model declared
    /// across all endpoints resolves for any role; zero resolves to
    /// `None` (the status line's `—`); two or more is ambiguous — the
    /// error names the missing profile table.
    fn resolve_implicit_default(
        &self,
        store: &(impl SecretStore + ?Sized),
    ) -> Result<Option<ResolvedModel<'_>>, ConfigError> {
        let mut models =
            self.agents
                .custom_endpoints
                .iter()
                .flat_map(|(endpoint_key, endpoint)| {
                    endpoint
                        .models
                        .iter()
                        .map(move |model| (endpoint_key, endpoint, model))
                });
        let Some((endpoint_key, endpoint, model)) = models.next() else {
            return Ok(None);
        };
        if models.next().is_some() {
            return Err(ConfigError::at(
                String::from("agents.execution_profiles"),
                "no `default` profile — add \
                 `[agents.execution_profiles.default]` with `base_model` \
                 pointing at a model's `config_key`",
            ));
        }
        Ok(Some(ResolvedModel {
            endpoint_key,
            endpoint,
            model,
            secret: endpoint.resolve_secret(endpoint_key, store),
        }))
    }
}

/// The `config_key` bound to `role` before the `base_model` fallback.
fn role_key(profile: &ExecutionProfile, role: ModelRole) -> Option<&str> {
    match role {
        ModelRole::Base => profile.base_model.as_deref(),
        ModelRole::Coding => profile.coding_model.as_deref(),
        ModelRole::Command => profile.command_model.as_deref(),
    }
}
