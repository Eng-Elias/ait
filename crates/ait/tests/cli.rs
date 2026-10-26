//! CLI-level tests for `ait`'s config commands (Spec R03, user stories
//! 2–5): `ait --init` writes the commented default `settings.toml`
//! under `$XDG_CONFIG_HOME/ait/` and refuses to overwrite it without
//! `--force`; `ait --print-schema` emits the settings JSON Schema.
//! The real binary runs as a plain child process — no TTY needed —
//! with `XDG_CONFIG_HOME` pointed at a fresh tempdir per test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ait_testkit::ait_bin;

/// Run `ait <args>` with `XDG_CONFIG_HOME` set to `xdg`.
fn ait(xdg: &Path, args: &[&str]) -> Output {
    Command::new(ait_bin!())
        .args(args)
        .env("XDG_CONFIG_HOME", xdg)
        .output()
        .expect("spawn the ait binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Where `ait --init` writes under a given `XDG_CONFIG_HOME`.
fn settings_file(xdg: &Path) -> PathBuf {
    xdg.join("ait").join("settings.toml")
}

/// Stories 2–4: `--init` creates `ait/settings.toml` under
/// `$XDG_CONFIG_HOME` — parents included — and the written template is
/// immediately valid.
#[test]
fn init_writes_a_valid_template_under_xdg() {
    let dir = tempfile::tempdir().unwrap();

    let output = ait(dir.path(), &["--init"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let text = std::fs::read_to_string(settings_file(dir.path()))
        .expect("--init must create ait/settings.toml");
    assert!(text.contains("# AIT settings"), "not the template");
    ait_config::Settings::from_toml(&text)
        .expect("the written file must parse through the crate seam (story 4)");
    assert!(
        stdout(&output).contains("settings.toml"),
        "stdout names the written path: {}",
        stdout(&output)
    );
}

/// Story 3: a second `--init` fails naming the file and leaves it
/// untouched; `--force` — in either argument order — overrides.
#[test]
fn init_refuses_overwrite_until_forced() {
    let dir = tempfile::tempdir().unwrap();
    assert!(ait(dir.path(), &["--init"]).status.success());
    let before = std::fs::read_to_string(settings_file(dir.path())).unwrap();

    let refused = ait(dir.path(), &["--init"]);
    assert!(!refused.status.success(), "second --init must fail");
    assert!(
        stderr(&refused).contains("already exists"),
        "stderr: {}",
        stderr(&refused)
    );
    assert_eq!(
        std::fs::read_to_string(settings_file(dir.path())).unwrap(),
        before,
        "a refused init must not touch the file"
    );

    for args in [&["--init", "--force"][..], &["--force", "--init"][..]] {
        let output = ait(dir.path(), args);
        assert!(
            output.status.success(),
            "{args:?} stderr: {}",
            stderr(&output)
        );
        ait_config::Settings::from_toml(
            &std::fs::read_to_string(settings_file(dir.path())).unwrap(),
        )
        .expect("the forced file must parse");
    }
}

/// Story 5: `--print-schema` prints the JSON Schema — the closed
/// settings model editors validate against.
#[test]
fn print_schema_emits_the_json_schema() {
    let dir = tempfile::tempdir().unwrap();

    let output = ait(dir.path(), &["--print-schema"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let stdout = stdout(&output);
    for needle in [
        "\"$schema\"",
        "additionalProperties",
        "custom_endpoints",
        "execution_profiles",
    ] {
        assert!(stdout.contains(needle), "schema is missing {needle:?}");
    }
}

/// Unknown arguments — and a `--force` with no `--init` — are usage
/// errors on stderr, never a silent TUI launch.
#[test]
fn bad_arguments_fail_with_usage() {
    let dir = tempfile::tempdir().unwrap();

    for args in [
        &["--bogus"][..],
        &["--force"][..],
        &["--version", "--init"][..],
    ] {
        let output = ait(dir.path(), args);
        assert!(!output.status.success(), "{args:?} must fail");
        assert!(
            stderr(&output).contains("ait:"),
            "{args:?} stderr: {}",
            stderr(&output)
        );
    }
}
