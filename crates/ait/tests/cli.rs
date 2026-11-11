//! CLI-level tests for `ait`'s config commands (Spec R03, user stories
//! 2–5): `ait --init` writes the commented default `settings.toml`
//! under `$XDG_CONFIG_HOME/ait/` and refuses to overwrite it without
//! `--force`; `ait --print-schema` emits the settings JSON Schema.
//! Spec R12 §5.11 adds the `--help`/`--version` surface: man-page
//! sections covering every arm, and the workspace version string.
//! The real binary runs as a plain child process — no TTY needed —
//! with `XDG_CONFIG_HOME` pointed at a fresh tempdir per test.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::net::UnixListener;
#[cfg(unix)]
use std::sync::mpsc::{channel, Receiver};
#[cfg(unix)]
use std::time::Duration;

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
/// errors on stderr, never a silent TUI launch (Spec R12 us-16): the
/// message names the offender and the full help block follows on
/// stderr.
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
        let err = stderr(&output);
        assert!(err.contains("ait:"), "{args:?} stderr: {err}");
        // The usage error appends the same help `--help` prints.
        assert!(
            err.contains("SYNOPSIS"),
            "{args:?} stderr carries the help block: {err}"
        );
    }
}

/// Spec R12 us-12: `ait --version` prints the built package version —
/// the `[workspace.package]` single source of truth (us-24), which
/// this test crate shares through `env!("CARGO_PKG_VERSION")`.
#[test]
fn version_prints_the_package_version() {
    let dir = tempfile::tempdir().unwrap();

    for flag in ["--version", "-V"] {
        let output = ait(dir.path(), &[flag]);
        assert!(
            output.status.success(),
            "{flag} stderr: {}",
            stderr(&output)
        );
        assert_eq!(
            stdout(&output),
            format!("ait {}\n", env!("CARGO_PKG_VERSION")),
            "{flag} stdout"
        );
    }
}

/// Spec R12 us-13–15: `ait --help` reads like a man page — NAME,
/// SYNOPSIS, DESCRIPTION, OPTIONS — with one synopsis line per CLI
/// arm, and the headless stdout/stderr/exit-code contract spelled
/// out so scripting ait needs no trial and error.
#[test]
fn help_is_man_page_style_and_covers_every_arm() {
    let dir = tempfile::tempdir().unwrap();

    let output = ait(dir.path(), &["--help"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    let help = stdout(&output);
    for section in ["NAME", "SYNOPSIS", "DESCRIPTION", "OPTIONS"] {
        assert!(
            help.contains(section),
            "help is missing the {section} section: {help}"
        );
    }
    for synopsis in [
        "    ait\n",
        "    ait ctl open-tab\n",
        "    ait ctl send [--tab <id>] [--no-enter] <text>\n",
        "    ait ctl last-block [--tab <id>] [--command|--exit]\n",
        "    ait --headless \"<task>\" [--profile <name>]\n",
        "    ait --init [--force]\n",
        "    ait --print-schema\n",
        "    ait --version\n",
        "    ait --help\n",
    ] {
        assert!(
            help.contains(synopsis),
            "help is missing the synopsis line {synopsis:?}: {help}"
        );
    }
    // us-15: the headless contract — answer text to stdout, activity
    // to stderr, the non-zero exit vocabulary — is on the page.
    for needle in ["stdout", "stderr", "130"] {
        assert!(help.contains(needle), "help is missing {needle:?}: {help}");
    }
}

// ------------------------------------------------------------------
// `ait ctl` (Spec R11 §5.10.5): the client arm against a fake control
// socket — socket resolution, the one-request wire shape, and the
// stderr/exit-code contract. A running ait itself needs a TTY; the
// client half provably doesn't.
// ------------------------------------------------------------------

/// `ait ctl <args>` under a controlled environment: `runtime` is the
/// `XDG_RUNTIME_DIR` the socket scan reads and `socket` the
/// `AIT_CTL_SOCKET` override (`None` removes an inherited one).
#[cfg(unix)]
fn ctl(runtime: &Path, socket: Option<&Path>, args: &[&str]) -> Output {
    let mut command = Command::new(ait_bin!());
    command
        .arg("ctl")
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime)
        .env_remove("AIT_CTL_SOCKET");
    if let Some(socket) = socket {
        command.env("AIT_CTL_SOCKET", socket);
    }
    command.output().expect("spawn the ait binary")
}

/// A fake instance: bind `<dir>/fake.sock`, answer its one connection
/// with `reply`, and hand the request line it received back on the
/// returned receiver — the wire shape is part of what these tests
/// assert.
#[cfg(unix)]
fn fake_socket(dir: &Path, reply: &'static str) -> (PathBuf, Receiver<String>) {
    let socket = dir.join("fake.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("the ctl client connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut request = String::new();
        let mut buf = [0u8; 4096];
        while !request.contains('\n') {
            let n = stream.read(&mut buf).expect("the request reads");
            if n == 0 {
                break;
            }
            request.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        stream
            .write_all(reply.as_bytes())
            .expect("the reply writes");
        tx.send(request).expect("the harness collects the request");
    });
    (socket, rx)
}

/// Zero sockets under the runtime dir — every verb fails with the
/// documented message on stderr and a nonzero exit (us-32/41).
#[cfg(unix)]
#[test]
fn ctl_reports_no_running_instance() {
    let dir = tempfile::tempdir().unwrap();
    for args in [&["open-tab"][..], &["send", "hi"][..], &["last-block"][..]] {
        let output = ctl(dir.path(), None, args);
        assert!(!output.status.success(), "{args:?} must fail");
        assert_eq!(
            stderr(&output),
            "ait ctl: no running ait instance\n",
            "{args:?}"
        );
    }
}

/// Several `*.sock` candidates refuse to guess — the error lists each
/// and names `AIT_CTL_SOCKET` as the disambiguator. Resolution probes
/// liveness, so the candidates are real bound listeners, not files.
#[cfg(unix)]
#[test]
fn ctl_lists_candidates_when_several_instances_exist() {
    let dir = tempfile::tempdir().unwrap();
    let ait_dir = dir.path().join("ait");
    std::fs::create_dir_all(&ait_dir).unwrap();
    let _one = UnixListener::bind(ait_dir.join("111.sock")).unwrap();
    let _two = UnixListener::bind(ait_dir.join("222.sock")).unwrap();

    let output = ctl(dir.path(), None, &["last-block"]);

    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(err.contains("ait ctl: several ait instances"), "{err}");
    assert!(
        err.contains("111.sock") && err.contains("222.sock"),
        "{err}"
    );
    assert!(err.contains("AIT_CTL_SOCKET"), "{err}");
}

/// `AIT_CTL_SOCKET` wins over the dir scan: `open-tab`'s request is
/// the verb's single-line JSON and the response line is the client's
/// stdout — jq-composable as us-41 asks.
#[cfg(unix)]
#[test]
fn ctl_open_tab_round_trips_through_the_env_socket() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let (socket, requests) = fake_socket(dir.path(), "{\"ok\":true,\"session_id\":\"7\"}\n");

    let output = ctl(runtime.path(), Some(&socket), &["open-tab"]);

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "{\"ok\":true,\"session_id\":\"7\"}\n");
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(15)).unwrap(),
        "{\"verb\":\"open_tab\"}\n",
        "the wire request is one JSON line"
    );
}

/// `send`'s flags encode on the wire — `--tab` and `--no-enter`
/// included — and a bare `{"ok":true}` still prints.
#[cfg(unix)]
#[test]
fn ctl_send_encodes_its_flags_on_the_wire() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let (socket, requests) = fake_socket(dir.path(), "{\"ok\":true}\n");

    let output = ctl(
        runtime.path(),
        Some(&socket),
        &["send", "--tab", "4", "--no-enter", "echo", "hi"],
    );

    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "{\"ok\":true}\n");
    assert_eq!(
        requests.recv_timeout(Duration::from_secs(15)).unwrap(),
        "{\"verb\":\"send\",\"tab\":\"4\",\"text\":\"echo hi\",\"enter\":false}\n"
    );
}

/// `last-block --exit` selects the field client-side (us-38); a
/// server `{"ok":false}` relays its message to stderr and fails.
#[cfg(unix)]
#[test]
fn ctl_last_block_selects_and_relays_errors() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let (socket, _) = fake_socket(
        dir.path(),
        "{\"ok\":true,\"block\":{\"command\":\"echo hi\",\"output\":\"hi\",\"exit\":3}}\n",
    );
    let output = ctl(runtime.path(), Some(&socket), &["last-block", "--exit"]);
    assert!(output.status.success(), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), "3\n");

    let dir = tempfile::tempdir().unwrap();
    let (socket, _) = fake_socket(
        dir.path(),
        "{\"ok\":false,\"error\":\"the active tab has no sealed blocks\"}\n",
    );
    let output = ctl(runtime.path(), Some(&socket), &["last-block"]);
    assert!(!output.status.success());
    assert_eq!(
        stderr(&output),
        "ait ctl: the active tab has no sealed blocks\n"
    );
}

/// A socket path nothing listens on fails naming the path — the
/// client never hangs or reports success (us-41).
#[cfg(unix)]
#[test]
fn ctl_fails_clearly_when_the_socket_is_unreachable() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let dead = dir.path().join("dead.sock");

    let output = ctl(runtime.path(), Some(&dead), &["open-tab"]);

    assert!(!output.status.success());
    let err = stderr(&output);
    assert!(err.contains("ait ctl: can't reach"), "{err}");
    assert!(err.contains("dead.sock"), "{err}");
}
