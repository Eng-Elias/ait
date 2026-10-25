//! `Command` driver self-tests (Spec E01, ticket E01-T3): stub driver
//! scripts — real subprocesses written to temp files — resolve `act` and
//! `assert` steps through the versioned stdin/stdout JSON protocol, and
//! every failure mode (non-zero exit, malformed JSON, wrong protocol
//! version, timeout) fails the step with the child's stderr in the
//! message. No model anywhere.

mod common;

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use ait_testkit::{Command, E2e, Size, SpawnConfig};
use common::{step_file, TestDir};

const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};

/// Interactive `/bin/sh` (pinned `PS1="$ "`) at the default size.
fn sh() -> E2e {
    E2e::spawn(SpawnConfig::new("/bin/sh", &[], SIZE)).expect("spawn /bin/sh")
}

/// Wait for the `$ ` prompt — the shell is up and idle.
fn wait_for_prompt(app: &mut E2e) {
    app.wait_for_screen("the shell prompt", |app| app.screen_text().contains('$'));
}

/// Write the stub driver `body` into `dir` and return its path. Tests
/// invoke it as `sh '<path>'` — no exec bit needed — while the
/// env-selection test quotes the raw path to prove a plain program
/// path works.
fn write_script(dir: &TestDir, body: &str) -> PathBuf {
    dir.write("driver.sh", body)
}

/// The canned-answer stub: `assert` requests get a `pass` verdict, `act`
/// requests get the mark-echo inputs. Kind detection greps the request's
/// `"kind":"…"` field — this is a stub, not a JSON parser (the wire
/// format is compact, so the field always appears verbatim).
const STUB_DRIVER: &str = r#"#!/bin/sh
request=$(cat)
case "$request" in
  *'"kind":"assert"'*)
    printf '%s' '{"v":1,"verdict":"pass"}'
    ;;
  *)
    printf '%s' '{"v":1,"inputs":[{"text":"echo AGENT-MARK"},{"key":"Enter"}]}'
    ;;
esac
"#;

/// `Command::new` + `set_driver`: the stub subprocess resolves the step,
/// the inputs it returns are sent through the real encoding path, and a
/// normal recording lands on disk.
#[test]
fn act_resolves_through_the_stub_script() {
    let dir = TestDir::fresh();
    let script = write_script(&dir, STUB_DRIVER);
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(Command::new(format!("sh '{}'", script.display())));

    app.agent().act("show the agent mark");

    assert!(
        app.screen_text().contains("AGENT-MARK"),
        "the subprocess's inputs were sent:\n{}",
        app.screen_text()
    );
    let recording =
        fs::read_to_string(step_file(dir.path(), 0)).expect("a normal recording was written");
    assert!(recording.contains(r#""op":"act""#), "header: {recording}");
    assert!(
        recording.contains(r#"{"text":"echo AGENT-MARK"}"#),
        "semantic inputs in the recording: {recording}"
    );
}

/// `assert` requests carry `kind`/`claim`; the stub's `pass` verdict is
/// recorded the same way any driver's is.
#[test]
fn assert_resolves_through_the_stub_script() {
    let dir = TestDir::fresh();
    let script = write_script(&dir, STUB_DRIVER);
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(Command::new(format!("sh '{}'", script.display())));

    app.agent().assert("the mark is on screen");

    let recording =
        fs::read_to_string(step_file(dir.path(), 0)).expect("the assert recording was written");
    assert!(
        recording.contains(r#""op":"assert""#),
        "header: {recording}"
    );
    assert!(
        recording.contains(r#""verdict":"pass""#),
        "header: {recording}"
    );
}

/// `AIT_E2E_DRIVER` at spawn replaces the replay-only `Authored` driver:
/// record mode resolves through the env-named command and produces a
/// normal recording, which a later driverless spawn then replays.
#[test]
fn ait_e2e_driver_env_selects_the_command_backing() {
    let dir = TestDir::fresh();
    let script = write_script(&dir, STUB_DRIVER);
    // The env var is read once at spawn; remove it promptly so the
    // brief set window can't leak into a concurrently-running test's
    // spawn (which would be benign anyway — replay never consults the
    // driver, and every record-mode test here sets its own).
    std::env::set_var("AIT_E2E_DRIVER", format!("sh '{}'", script.display()));
    let mut app = sh();
    std::env::remove_var("AIT_E2E_DRIVER");
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);

    app.agent().act("show the agent mark");

    let recording = fs::read_to_string(step_file(dir.path(), 0))
        .expect("the env-selected driver produced a normal recording");
    assert!(recording.contains(r#""op":"act""#), "header: {recording}");
    drop(app);

    // Round trip: the recording replays with zero driver involvement.
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(false);

    app.agent().act("show the agent mark");

    assert!(
        app.screen_text().contains("AGENT-MARK"),
        "the recorded inputs replayed:\n{}",
        app.screen_text()
    );
}

/// A recorded `fail` verdict shape — `{"fail":{"reason":…}}` — surfaces
/// the driver's reason as the step failure.
#[test]
#[should_panic(expected = "no such thing on the screen")]
fn a_fail_verdict_surfaces_the_drivers_reason() {
    let dir = TestDir::fresh();
    let script = write_script(
        &dir,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"v\":1,\"verdict\":{\"fail\":{\"reason\":\"no such thing on the screen\"}}}'\n",
    );
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(Command::new(format!("sh '{}'", script.display())));

    app.agent().assert("an impossible claim");
}

/// Non-zero exit: a step failure carrying the subprocess's stderr —
/// the message a wrapper writes when its endpoint refuses.
#[test]
#[should_panic(expected = "endpoint refused")]
fn nonzero_exit_fails_the_step_with_stderr() {
    let dir = TestDir::fresh();
    let script = write_script(&dir, "#!/bin/sh\necho 'endpoint refused' >&2\nexit 3\n");
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(Command::new(format!("sh '{}'", script.display())));

    app.agent().act("anything");
}

/// Output that isn't the protocol JSON is a step failure, not a
/// misparsed half-answer.
#[test]
#[should_panic(expected = "not valid protocol JSON")]
fn malformed_json_fails_the_step() {
    let dir = TestDir::fresh();
    let script = write_script(&dir, "#!/bin/sh\nprintf 'this is not json'\n");
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(Command::new(format!("sh '{}'", script.display())));

    app.agent().act("anything");
}

/// A wrapper speaking a newer protocol fails loudly instead of
/// silently misbehaving — the version field's whole point.
#[test]
#[should_panic(expected = "protocol v2")]
fn wrong_protocol_version_fails_the_step() {
    let dir = TestDir::fresh();
    let script = write_script(
        &dir,
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"v\":2,\"inputs\":[]}'\n",
    );
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(Command::new(format!("sh '{}'", script.display())));

    app.agent().act("anything");
}

/// A wrapper that never finishes is killed at the timeout bound and
/// fails the step — `with_timeout` exists so this test doesn't sit on
/// the real 60s bound.
#[test]
#[should_panic(expected = "timed out")]
fn a_hung_driver_is_killed_at_the_timeout() {
    let dir = TestDir::fresh();
    let script = write_script(&dir, "#!/bin/sh\nsleep 30\n");
    let mut app = sh();
    wait_for_prompt(&mut app);
    app.set_recordings_dir(dir.path());
    app.set_record_mode(true);
    app.set_driver(Command::with_timeout(
        format!("sh '{}'", script.display()),
        Duration::from_millis(300),
    ));

    app.agent().act("anything");
}
