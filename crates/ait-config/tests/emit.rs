//! Tests at the emitted-artifacts seam (Spec R03, user stories 2, 4–5):
//! `default_template()` is the commented `settings.toml` `ait --init`
//! writes — it must parse through `Settings::from_toml` immediately and
//! carry a real v4 UUID; `json_schema()` is what `ait --print-schema`
//! prints — a JSON Schema covering endpoints, models, profiles and
//! `[terminal]`.

use ait_config::{default_template, init_settings, json_schema, Settings};

#[test]
fn template_parses_through_from_toml() {
    let settings = Settings::from_toml(&default_template())
        .expect("the shipped template must be immediately valid (story 4)");
    let profile = &settings.agents.execution_profiles["default"];
    assert!(
        profile.base_model.is_some(),
        "the template should ship a usable base_model"
    );
    // §5.7's shipped default profile: write_to_pty is always_ask.
    assert_eq!(
        profile.write_to_pty,
        Some(ait_config::PermissionLevel::AlwaysAsk),
        "the template's default profile must match REQUIREMENTS.md §5.7"
    );
}

#[test]
fn template_embeds_a_real_v4_uuid_as_the_example_config_key() {
    let settings = Settings::from_toml(&default_template()).unwrap();
    let config_key = &settings.agents.custom_endpoints["local"].models[0].config_key;
    let uuid = uuid::Uuid::parse_str(config_key)
        .expect("config_key in the template must be a UUID (story 4)");
    assert_eq!(uuid.get_version_num(), 4);
    // The default profile's base_model references the same key.
    assert_eq!(
        settings.agents.execution_profiles["default"]
            .base_model
            .as_deref(),
        Some(config_key.as_str())
    );
}

#[test]
fn each_template_gets_a_fresh_uuid() {
    let first = Settings::from_toml(&default_template()).unwrap();
    let second = Settings::from_toml(&default_template()).unwrap();
    fn key(s: &Settings) -> &str {
        s.agents.custom_endpoints["local"].models[0]
            .config_key
            .as_str()
    }
    assert_ne!(key(&first), key(&second));
}

#[test]
fn template_teaches_where_secrets_live() {
    // Story 19 guidance belongs in the file users edit: the template
    // must point at api_key_env / the ait/<endpoint-key> keyring entry.
    let template = default_template();
    assert!(template.contains("api_key_env"), "missing env-var guidance");
    assert!(
        template.contains("ait/local") || template.contains("ait/<endpoint-key>"),
        "missing keyring-entry guidance"
    );
}

/// `ait --init` (stories 2–4): `init_settings` writes the template,
/// creating missing parent directories, and the written file parses
/// immediately.
#[test]
fn init_writes_a_valid_template_creating_parents() {
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("ait").join("settings.toml");

    init_settings(&path, false).expect("init on a fresh path must succeed");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# AIT settings"), "not the template");
    Settings::from_toml(&text).expect("the written file must parse (story 4)");
}

/// Story 3: an existing `settings.toml` is refused without `force` — the
/// error names the path, and the file is left byte-for-byte untouched.
#[test]
fn init_refuses_to_overwrite_an_existing_file() {
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    std::fs::write(&path, "# hand-tuned\n").unwrap();

    let err = init_settings(&path, false).unwrap_err();

    assert!(
        err.to_string().contains("already exists"),
        "actual: {err:?}"
    );
    assert!(err.to_string().contains("settings.toml"), "actual: {err:?}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "# hand-tuned\n",
        "a refused init must not touch the file"
    );
}

/// Story 3: `--force` replaces the existing file with a valid template.
#[test]
fn init_force_overwrites_the_existing_file() {
    let dirs = tempfile::tempdir().unwrap();
    let path = dirs.path().join("settings.toml");
    std::fs::write(&path, "nonsense = [\n").unwrap();

    init_settings(&path, true).expect("init --force must succeed");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# AIT settings"), "not the template");
    Settings::from_toml(&text).expect("the forced file must parse");
}

#[test]
fn schema_is_valid_json_covering_the_settings_model() {
    let schema: serde_json::Value =
        serde_json::from_str(&json_schema()).expect("json_schema must emit JSON");

    // deny_unknown_fields must surface as a closed schema so editor
    // tooling catches typos like the loader does (story 9).
    assert_eq!(schema["additionalProperties"], serde_json::json!(false));
    assert!(schema["properties"]["terminal"].is_object());
    assert!(schema["properties"]["agents"].is_object());

    let defs = &schema["$defs"];
    for name in [
        "Endpoint",
        "EndpointModel",
        "EndpointSchema",
        "ExecutionProfile",
        "TerminalSettings",
        "SshSettings",
        "RedactionSettings",
        "InputSettings",
        "ClassifierMode",
    ] {
        assert!(defs[name].is_object(), "$defs missing {name}");
    }
    // Spec R09-T1: `--print-schema` covers the `[ssh]` model.
    assert!(schema["properties"]["ssh"].is_object());
    // Spec R10-T1: `--print-schema` covers the `[redaction]` model.
    assert!(schema["properties"]["redaction"].is_object());
    // Spec R11-T1: `--print-schema` covers the `[input]` model.
    assert!(schema["properties"]["input"].is_object());
}

/// Spec R09-T1: `ait --init`'s template documents all four `[ssh]`
/// keys in a commented block.
#[test]
fn template_documents_the_ssh_section() {
    let template = default_template();
    for key in [
        "enable_integration",
        "hosts_allowlist",
        "hosts_denylist",
        "reuse_existing_control_master",
    ] {
        assert!(template.contains(key), "template missing [ssh] key {key}");
    }
    assert!(
        template.contains("# [ssh]"),
        "the [ssh] block stays commented"
    );
}

/// Spec R10-T1: `ait --init`'s template documents the `[redaction]`
/// section — the three lists plus the names they toggle — in a
/// commented block (§4.3 decided).
#[test]
fn template_documents_the_redaction_section() {
    let template = default_template();
    for key in ["extra", "enable", "disable"] {
        assert!(
            template.contains(key),
            "template missing [redaction] key {key}"
        );
    }
    for name in ["aws_access_key", "pii_email"] {
        assert!(
            template.contains(name),
            "template missing pattern name {name}"
        );
    }
    assert!(
        template.contains("# [redaction]"),
        "the [redaction] block stays commented"
    );
}

/// Spec R11-T1: `ait --init`'s template documents the `[input]`
/// section — the classifier key and both live modes — in a commented
/// block (§5.10.4).
#[test]
fn template_documents_the_input_section() {
    let template = default_template();
    for key in ["classifier", "heuristic", "off"] {
        assert!(
            template.contains(key),
            "template missing [input] vocabulary {key}"
        );
    }
    assert!(
        template.contains("# [input]"),
        "the [input] block stays commented"
    );
}

/// Spec R14 §5.13: `ait --init`'s template documents the `[gui]`
/// section — the explorer pane's editor override — in a commented
/// block.
#[test]
fn template_documents_the_gui_section() {
    let template = default_template();
    assert!(template.contains("editor"), "template missing [gui] key");
    assert!(
        template.contains("# [gui]"),
        "the [gui] block stays commented"
    );
    // `--print-schema` covers the `[gui]` model.
    let schema: serde_json::Value = serde_json::from_str(&json_schema()).unwrap();
    assert!(schema["properties"]["gui"].is_object());
    assert!(schema["$defs"]["GuiSettings"].is_object());
}
