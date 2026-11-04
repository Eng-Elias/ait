//! Table-driven tests at the `Settings::from_toml` pure seam (Spec R03
//! testing decisions, standard 11 — no filesystem, no network): valid
//! documents produce typed `Settings`; invalid ones produce `ConfigError`s
//! whose text names the offending key path
//! (`agents.custom_endpoints.local.models[0].config_key: …`).

use ait_config::{EndpointSchema, PermissionLevel, Settings};

/// The §4.1 endpoint + §5.7 default-profile document, verbatim shape.
const FULL_SPEC_DOCUMENT: &str = r#"
[terminal]
shell = "/usr/bin/fish"
scrollback_lines = 20_000

[agents.custom_endpoints.local]
name        = "Local Ollama"
base_url    = "http://localhost:11434/v1"
schema      = "openai_chat_completions"
api_key_env = "OLLAMA_API_KEY"

[[agents.custom_endpoints.local.models]]
name       = "qwen2.5-coder:14b"
alias      = "Qwen Coder"
config_key = "6f1c0000-0000-4000-8000-000000000001"
context_window = 32_768

[agents.custom_endpoints.hosted]
name     = "Anthropic"
base_url = "https://api.anthropic.com"
schema   = "anthropic_messages"

[[agents.custom_endpoints.hosted.models]]
name       = "claude-sonnet-4-5"
alias      = "Sonnet"
config_key = "6f1c0000-0000-4000-8000-000000000002"

[agents.execution_profiles.default]
read_files        = "agent_decides"
write_to_pty      = "ask_on_first_write"
execute_commands  = "always_ask"
apply_code_diffs  = "always_ask"
mcp_permissions   = "always_ask"
ask_user_question = "always_allow"
command_allowlist   = ["^git status$"]
command_denylist    = ['^(bash|sh|zsh|fish)(\s.*)?$', '^rm(\s.*)?$',
                       '^(curl|wget)(\s.*)?$', '^(ssh|scp|rsync)(\s.*)?$']
directory_allowlist = ["/home/user/project"]
mcp_allowlist = ["fs"]
mcp_denylist  = ["shell"]
base_model    = "6f1c0000-0000-4000-8000-000000000001"
coding_model  = "6f1c0000-0000-4000-8000-000000000002"
command_model = ""
context_window_limit = 128_000
"#;

#[test]
fn empty_document_yields_defaults() {
    let settings = Settings::from_toml("").unwrap();
    assert_eq!(settings, Settings::default());
}

#[test]
fn comment_only_document_yields_defaults() {
    let settings = Settings::from_toml("# nothing here yet\n").unwrap();
    assert_eq!(settings, Settings::default());
}

#[test]
fn terminal_only_document() {
    let settings = Settings::from_toml(
        r#"
        [terminal]
        shell = "/bin/zsh"
        scrollback_lines = 5_000
        osc52 = true
        shell_integration = false
        "#,
    )
    .unwrap();
    assert_eq!(settings.terminal.shell.as_deref(), Some("/bin/zsh"));
    assert_eq!(settings.terminal.scrollback_lines, Some(5_000));
    assert_eq!(settings.terminal.osc52, Some(true));
    assert_eq!(settings.terminal.shell_integration, Some(false));
    assert!(settings.agents.custom_endpoints.is_empty());
}

#[test]
fn minimal_endpoint_document() {
    // Story 21: endpoints without `api_key_env` are valid — they send no
    // Authorization header.
    let settings = Settings::from_toml(
        r#"
        [agents.custom_endpoints.local]
        name     = "Local Ollama"
        base_url = "http://localhost:11434/v1"
        schema   = "openai_responses"
        "#,
    )
    .unwrap();
    let endpoint = &settings.agents.custom_endpoints["local"];
    assert_eq!(endpoint.schema, EndpointSchema::OpenAiResponses);
    assert_eq!(endpoint.api_key_env, None);
    assert!(endpoint.models.is_empty());
}

#[test]
fn full_spec_document_parses() {
    let settings = Settings::from_toml(FULL_SPEC_DOCUMENT).unwrap();

    assert_eq!(settings.terminal.shell.as_deref(), Some("/usr/bin/fish"));
    assert_eq!(settings.terminal.scrollback_lines, Some(20_000));

    let endpoints = &settings.agents.custom_endpoints;
    let local = &endpoints["local"];
    assert_eq!(local.schema, EndpointSchema::OpenAiChatCompletions);
    assert_eq!(local.api_key_env.as_deref(), Some("OLLAMA_API_KEY"));
    assert_eq!(local.models[0].alias, "Qwen Coder");
    assert_eq!(local.models[0].context_window, Some(32_768));
    assert_eq!(
        endpoints["hosted"].schema,
        EndpointSchema::AnthropicMessages
    );

    let profile = &settings.agents.execution_profiles["default"];
    assert_eq!(profile.read_files, Some(PermissionLevel::AgentDecides));
    assert_eq!(profile.write_to_pty, Some(PermissionLevel::AskOnFirstWrite));
    assert_eq!(
        profile.ask_user_question,
        Some(PermissionLevel::AlwaysAllow)
    );
    assert_eq!(profile.command_allowlist, ["^git status$"]);
    assert_eq!(profile.command_denylist.len(), 4);
    assert_eq!(profile.directory_allowlist, ["/home/user/project"]);
    assert_eq!(profile.mcp_allowlist, ["fs"]);
    assert_eq!(profile.mcp_denylist, ["shell"]);
    assert_eq!(
        profile.base_model.as_deref(),
        Some("6f1c0000-0000-4000-8000-000000000001")
    );
    assert_eq!(
        profile.coding_model.as_deref(),
        Some("6f1c0000-0000-4000-8000-000000000002")
    );
    assert_eq!(profile.context_window_limit, 128_000);
}

#[test]
fn empty_role_string_means_unset() {
    // §5.7's shipped default spells unset roles as `""`; it must parse to
    // "unset", not fail as a dangling reference.
    let settings = Settings::from_toml(
        r#"
        [agents.execution_profiles.default]
        base_model = ""
        "#,
    )
    .unwrap();
    assert_eq!(
        settings.agents.execution_profiles["default"].base_model,
        None
    );
}

#[test]
fn unknown_keys_at_every_level_are_rejected() {
    // (input, expected error text) — the path prefix IS the contract:
    // the error must name the offending key path (§5.3, stories 7–9).
    let cases: &[(&str, &str)] = &[
        (
            "bogus = 1",
            "bogus: unknown field `bogus`, expected `terminal` or `agents`",
        ),
        (
            "[terminal]\nshells = \"/bin/sh\"",
            "terminal.shells: unknown field `shells`, expected one of `shell`, `scrollback_lines`, `osc52`, `shell_integration`",
        ),
        (
            "[agents]\nbogus = 1",
            "agents.bogus: unknown field `bogus`, expected `custom_endpoints` or `execution_profiles`",
        ),
        (
            "[agents.custom_endpoints.local]\nname = \"n\"\nbase_url = \"u\"\nschema = \"anthropic_messages\"\nbogus = 1",
            "agents.custom_endpoints.local.bogus: unknown field `bogus`, expected one of `name`, `base_url`, `schema`, `api_key_env`, `models`",
        ),
        (
            "[agents.custom_endpoints.local]\nname = \"n\"\nbase_url = \"u\"\nschema = \"anthropic_messages\"\n\n[[agents.custom_endpoints.local.models]]\nname = \"m\"\nalias = \"a\"\nconfig_key = \"k\"\nbogus = 1",
            "agents.custom_endpoints.local.models[0].bogus: unknown field `bogus`, expected one of `name`, `alias`, `config_key`, `context_window`",
        ),
        (
            "[agents.execution_profiles.default]\nbogus = 1",
            "agents.execution_profiles.default.bogus: unknown field `bogus`",
        ),
    ];
    for (input, expected) in cases {
        let err = Settings::from_toml(input).unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with(expected),
            "input {input:?}\nexpected prefix {expected:?}\nactual   {message:?}"
        );
    }
}

#[test]
fn bad_schema_enum_value_names_the_key() {
    let err = Settings::from_toml(
        r#"
        [agents.custom_endpoints.local]
        name     = "n"
        base_url = "u"
        schema   = "openai"
        "#,
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.starts_with("agents.custom_endpoints.local.schema:"),
        "actual: {message:?}"
    );
    assert!(message.contains("openai"), "actual: {message:?}");
}

#[test]
fn bad_permission_level_names_the_key() {
    let err = Settings::from_toml(
        r#"
        [agents.execution_profiles.default]
        read_files = "sometimes"
        "#,
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.starts_with("agents.execution_profiles.default.read_files:"),
        "actual: {message:?}"
    );
    assert!(message.contains("sometimes"), "actual: {message:?}");
}

#[test]
fn wrong_type_names_the_key() {
    let err = Settings::from_toml("[terminal]\nscrollback_lines = \"many\"").unwrap_err();
    let message = err.to_string();
    assert!(
        message.starts_with("terminal.scrollback_lines:"),
        "actual: {message:?}"
    );
}

#[test]
fn empty_config_key_is_rejected() {
    let err = Settings::from_toml(
        r#"
        [agents.custom_endpoints.local]
        name     = "n"
        base_url = "u"
        schema   = "openai_responses"

        [[agents.custom_endpoints.local.models]]
        name       = "m"
        alias      = "a"
        config_key = ""
        "#,
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "agents.custom_endpoints.local.models[0].config_key: must not be empty"
    );
}

#[test]
fn duplicate_config_key_across_endpoints_is_rejected() {
    let err = Settings::from_toml(
        r#"
        [agents.custom_endpoints.alpha]
        name     = "a"
        base_url = "u"
        schema   = "openai_responses"

        [[agents.custom_endpoints.alpha.models]]
        name       = "m"
        alias      = "a"
        config_key = "dup"

        [agents.custom_endpoints.beta]
        name     = "b"
        base_url = "u"
        schema   = "anthropic_messages"

        [[agents.custom_endpoints.beta.models]]
        name       = "m"
        alias      = "a"
        config_key = "dup"
        "#,
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "agents.custom_endpoints.beta.models[0].config_key: duplicate \
         config_key \"dup\" (first declared at \
         agents.custom_endpoints.alpha.models[0].config_key)"
    );
}

#[test]
fn duplicate_config_key_within_one_endpoint_is_rejected() {
    let err = Settings::from_toml(
        r#"
        [agents.custom_endpoints.local]
        name     = "n"
        base_url = "u"
        schema   = "openai_responses"

        [[agents.custom_endpoints.local.models]]
        name       = "m1"
        alias      = "a"
        config_key = "dup"

        [[agents.custom_endpoints.local.models]]
        name       = "m2"
        alias      = "a"
        config_key = "dup"
        "#,
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "agents.custom_endpoints.local.models[1].config_key: duplicate \
         config_key \"dup\" (first declared at \
         agents.custom_endpoints.local.models[0].config_key)"
    );
}

#[test]
fn dangling_model_role_names_the_role() {
    let err = Settings::from_toml(
        r#"
        [agents.execution_profiles.default]
        coding_model = "ghost"
        "#,
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "agents.execution_profiles.default.coding_model: references \
         unknown config_key \"ghost\""
    );
}

#[test]
fn valid_role_reference_resolves() {
    let settings = Settings::from_toml(
        r#"
        [agents.custom_endpoints.local]
        name     = "n"
        base_url = "u"
        schema   = "anthropic_messages"

        [[agents.custom_endpoints.local.models]]
        name       = "m"
        alias      = "a"
        config_key = "key-1"

        [agents.execution_profiles.default]
        base_model = "key-1"
        "#,
    )
    .unwrap();
    assert_eq!(
        settings.agents.execution_profiles["default"]
            .base_model
            .as_deref(),
        Some("key-1")
    );
}

#[test]
fn toml_syntax_error_still_reports_location() {
    let err = Settings::from_toml("[terminal\nshell = 1").unwrap_err();
    let message = err.to_string();
    // No key path exists for a syntax error; the parser's own line/column
    // rendering is the message.
    assert!(message.contains("line"), "actual: {message:?}");
}

#[test]
fn missing_required_endpoint_field_names_the_table() {
    let err = Settings::from_toml(
        r#"
        [agents.custom_endpoints.local]
        name     = "n"
        schema   = "openai_responses"
        "#,
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(
        message.starts_with("agents.custom_endpoints.local:"),
        "actual: {message:?}"
    );
    assert!(message.contains("base_url"), "actual: {message:?}");
}

#[test]
fn a_broken_command_list_regex_names_the_entry() {
    // §5.7's command lists are regexes the permission engine matches
    // unanchored — a broken pattern would silently never match, so it
    // fails at load naming its `…[index]` path. Valid entries already
    // ride `full_spec_document_parses` (the shipped denylist).
    let cases: &[(&str, &str)] = &[
        (
            "[agents.execution_profiles.default]\ncommand_denylist = ['^ok$', '(']",
            "agents.execution_profiles.default.command_denylist[1]:",
        ),
        (
            "[agents.execution_profiles.default]\ncommand_allowlist = ['*']",
            "agents.execution_profiles.default.command_allowlist[0]:",
        ),
        (
            "[agents.execution_profiles.work]\ncommand_denylist = ['[']",
            "agents.execution_profiles.work.command_denylist[0]:",
        ),
    ];
    for (input, expected) in cases {
        let err = Settings::from_toml(input).unwrap_err();
        let message = err.to_string();
        assert!(
            message.starts_with(expected),
            "input {input:?}\nexpected prefix {expected:?}\nactual   {message:?}"
        );
        assert!(
            message.len() > expected.len(),
            "the regex error itself must follow the path: {message:?}"
        );
    }
}

#[test]
fn directory_and_mcp_lists_are_not_regex_validated() {
    // `directory_allowlist` holds paths and the `mcp_*` lists are inert
    // (§5.7, Rounds 10/8) — only the command lists compile, so
    // non-regex text parses here untouched.
    let settings = Settings::from_toml(
        r#"
        [agents.execution_profiles.default]
        directory_allowlist = ["(", "/tmp"]
        mcp_allowlist = ["("]
        mcp_denylist = ["*"]
        "#,
    )
    .unwrap();
    let profile = &settings.agents.execution_profiles["default"];
    assert_eq!(profile.directory_allowlist, ["(", "/tmp"]);
    assert_eq!(profile.mcp_allowlist, ["("]);
    assert_eq!(profile.mcp_denylist, ["*"]);
}
