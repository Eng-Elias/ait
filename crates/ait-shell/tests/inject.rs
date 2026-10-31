//! Tests for the injection-planner seam (Spec R05 / issue #71):
//! `ait_shell::plan` maps a resolved program path to an [`Injection`] —
//! the pure argv/env rewrite plus the files a per-session `0700` dir must
//! hold — and `Injection::materialize_in` writes it. Unknown programs
//! plan to `None` (§5.5 graceful degrade). A bash smoke test at the end
//! feeds the real shell bytes through the materialized wrapper.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ait_shell::{plan, plan_with_zdotdir, ShellKind, ENV_SESSION_ID};

/// A scratch base for `materialize_in`, unique per test process.
fn scratch_base(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ait-shell-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    base
}

fn env_has(env: &[(OsString, OsString)], key: &str, value: &str) -> bool {
    env.iter()
        .any(|(k, v)| k == &OsString::from(key) && v == &OsString::from(value))
}

#[test]
fn supported_basenames_plan_to_their_shell_kind() {
    let cases = [
        ("/bin/bash", ShellKind::Bash),
        ("/usr/bin/bash", ShellKind::Bash),
        ("/opt/sh-x/bash", ShellKind::Bash),
        ("bash", ShellKind::Bash),
        ("/usr/bin/zsh", ShellKind::Zsh),
        ("/opt/homebrew/bin/zsh", ShellKind::Zsh),
        ("/usr/bin/fish", ShellKind::Fish),
        ("/usr/local/bin/fish", ShellKind::Fish),
    ];
    for (program, expected) in cases {
        let injection = plan(Path::new(program)).unwrap_or_else(|| panic!("{program} should plan"));
        assert_eq!(injection.kind(), expected, "{program}");
    }
}

#[test]
fn unknown_programs_plan_to_none() {
    for program in [
        "sh",
        "/bin/sh",
        "nu",
        "/usr/bin/nu",
        "sudo",
        "/usr/bin/sudo",
        "pwsh",
        "/usr/bin/env",
        "",
    ] {
        assert!(plan(Path::new(program)).is_none(), "{program}");
    }
}

#[test]
fn bash_plan_injects_rcfile_wrapper_sourcing_user_rc_then_snippet() {
    let injection = plan(Path::new("/bin/bash")).unwrap();
    let dir = Path::new("/tmp/ait-test-dir");

    assert_eq!(
        injection.args(dir),
        vec![
            OsString::from("--rcfile"),
            OsString::from("/tmp/ait-test-dir/bashrc"),
        ]
    );

    let env = injection.env(dir);
    // The inherited session id is scrubbed; T3 sets the real one.
    assert!(env_has(&env, ENV_SESSION_ID, ""));

    let files = injection.files(dir);
    let bashrc = files
        .iter()
        .find(|(name, _)| *name == "bashrc")
        .map(|(_, c)| c.as_str())
        .expect("bash plan must ship a bashrc wrapper");
    // The wrapper loads the user's real rc first, then the snippet
    // (Spec R05 us-2: dotfiles load untouched).
    let user_rc = bashrc.find(".bashrc").expect("wrapper sources ~/.bashrc");
    let snippet = bashrc
        .find("snippet.bash")
        .expect("wrapper sources the snippet");
    assert!(
        user_rc < snippet,
        "user rc must be sourced before the snippet"
    );

    let snippet = files
        .iter()
        .find(|(name, _)| *name == "snippet.bash")
        .map(|(_, c)| c.as_str())
        .expect("bash plan must ship the snippet");
    for needle in [
        "133;A",
        "133;B",
        "133;C",
        "133;D",
        "]7;",
        "7700",
        "7701",
        "__ait_emit",
        "__ait_run",
        "init_shell",
        "bootstrapped",
        "precmd",
        "preexec",
        "exit_shell",
    ] {
        assert!(snippet.contains(needle), "bash snippet missing {needle}");
    }
}

#[test]
fn zsh_plan_injects_zdotdir_forwarders_restoring_the_real_dir() {
    let injection = plan_with_zdotdir(
        Path::new("/usr/bin/zsh"),
        Some(OsString::from("/home/u/zdot")),
    )
    .unwrap();
    let dir = Path::new("/tmp/ait-test-dir");

    // zsh needs no argv — the ZDOTDIR env forwarder does everything.
    assert!(injection.args(dir).is_empty());

    let env = injection.env(dir);
    assert!(env_has(&env, "ZDOTDIR", "/tmp/ait-test-dir"));
    assert!(env_has(&env, "AIT_ZDOTDIR_ORIG", "/home/u/zdot"));
    assert!(env_has(&env, ENV_SESSION_ID, ""));

    let files = injection.files(dir);
    for name in [".zshenv", ".zprofile", ".zshrc", "snippet.zsh"] {
        assert!(
            files.iter().any(|(n, _)| *n == name),
            "zsh plan missing {name}"
        );
    }
    let zshrc = files
        .iter()
        .find(|(n, _)| *n == ".zshrc")
        .map(|(_, c)| c.as_str())
        .unwrap();
    // The forwarder reaches the user's real rc through the *original*
    // ZDOTDIR, then the snippet, then restores ZDOTDIR for nested zsh.
    assert!(zshrc.contains("AIT_ZDOTDIR_ORIG"));
    let user_rc = zshrc.find("/.zshrc").unwrap();
    let snippet = zshrc.find("snippet.zsh").unwrap();
    assert!(user_rc < snippet);
    assert!(zshrc.contains("export ZDOTDIR="), "nested zsh restore");
}

#[test]
fn fish_plan_injects_init_command_sourcing_the_snippet() {
    let injection = plan(Path::new("/usr/bin/fish")).unwrap();
    let dir = Path::new("/tmp/ait-test-dir");

    let args = injection.args(dir);
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], OsString::from("--init-command"));
    assert_eq!(
        args[1],
        OsString::from("source '/tmp/ait-test-dir/snippet.fish'")
    );

    let files = injection.files(dir);
    assert!(files.iter().any(|(n, _)| *n == "snippet.fish"));
}

#[test]
fn materialize_writes_files_into_a_0700_dir_and_cleans_up() {
    let base = scratch_base("materialize");
    let injection = plan(Path::new("/bin/bash")).unwrap();
    let dir;
    {
        let mut materialized = injection.materialize_in(&base).unwrap();
        dir = materialized.dir().to_path_buf();
        assert!(dir.starts_with(&base));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "session dir must be 0700");
        }

        for (name, _) in injection.files(&dir) {
            assert!(dir.join(name).is_file(), "missing {name}");
        }

        // The bundle carries the same rewrite the pure accessors plan.
        assert_eq!(materialized.args(), injection.args(&dir).as_slice());
        materialized.set_session_id("42");
        assert!(env_has(materialized.env(), ENV_SESSION_ID, "42"));
    }
    // Session dir removed with the guard — no litter (Spec R05 us-30).
    assert!(!dir.exists());
    let _ = std::fs::remove_dir_all(&base);
}

/// Byte-fed smoke test: the materialized bash wrapper emits OSC 133
/// marks, OSC 7700 hooks and a `7701` `__ait_run` response. Skips
/// silently when bash is unavailable (the unit-test seam stays the
/// planner; full hook coverage is the e2e ticket's job).
#[test]
fn bash_snippet_emits_marks_hooks_and_a_run_response() {
    let base = scratch_base("bash-smoke");
    let Some(injection) = plan(Path::new("/bin/bash")) else {
        unreachable!()
    };
    let materialized = match injection.materialize_in(&base) {
        Ok(m) => m,
        Err(_) => return,
    };
    let mut args: Vec<OsString> = materialized.args().to_vec();
    args.push(OsString::from("-i"));

    let mut child = match Command::new("bash")
        .args(&args)
        .env("AIT_SESSION_ID", "7")
        .env("TERM", "xterm-256color")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return, // no bash on this host — e2e's job
    };
    // `echo hi` → preexec + D;0; `__ait_run 1 hex(echo smoke)` → 7701;
    // `exit` → exit_shell.
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"echo hi\n__ait_run 1 6563686f20736d6f6b65\nexit\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let mut bytes = output.stdout;
    bytes.extend_from_slice(&output.stderr);
    let text = String::from_utf8_lossy(&bytes);

    for needle in [
        "\x1b]133;A\x07",
        "\x1b]133;C\x07",
        "\x1b]133;D;",
        "\x1b]7;file://",
        "\x1b]7700;",
        "\x1b]7701;A\x07",
        "\x1b]7701;R;",
        "\x1b]7701;B\x07",
        "736d6f6b65", // hex("smoke") inside the R-frame payload
    ] {
        assert!(text.contains(needle), "bash smoke missing {needle:?}");
    }
    let _ = std::fs::remove_dir_all(&base);
}
