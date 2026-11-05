//! e2e for `ait --headless "<task>"` (Spec R11 §5.10.5, ticket
//! R11-T6): the real binary as a plain child process — no TUI, no PTY
//! harness — driven against `ait_llm::testing`'s loopback [`Fixture`]
//! (one scripted SSE response per request, zero network). The
//! contracts: stdout is the assistant's answer stream only (pipe-safe,
//! no escape bytes), stderr the `ait: `-prefixed activity log, and the
//! exit code the terminal event's — `Done`→0, `Error`→1,
//! `BudgetReached`→2, `Cancelled`→130 (us-24/25/26).
//!
//! `XDG_CONFIG_HOME` points the settings load at a per-test fixture
//! dir (the R03/R04 pattern — the ephemeral port can't ride a TOML
//! `env`) and `XDG_DATA_HOME` pins `store.db` beside it — the run
//! persists like a TUI conversation (us-28) without touching the
//! developer's real store.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use ait_llm::testing::{error_status, sse, Fixture, Script};
use ait_testkit::{ait_bin, json_string};

#[cfg(unix)]
use std::io::Read;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::time::{Duration, Instant};

/// Write `contents` as `<xdg>/ait/settings.toml`, creating the `ait/`
/// directory first (the R03 pattern).
fn write_settings(xdg: &Path, contents: &str) {
    let dir = xdg.join("ait");
    fs::create_dir_all(&dir).expect("create the fixture config directory");
    fs::write(dir.join("settings.toml"), contents).expect("write fixture settings.toml");
}

/// A `settings.toml` binding `default`'s `coding_model` — the role the
/// headless run resolves (§4.2) — to the fixture endpoint, with
/// `profile_fields` appended inside `[agents.execution_profiles.default]`
/// (the agent tests' shape — `read_files = "always_ask"` and kin).
fn settings_with_fixture(base_url: &str, profile_fields: &str) -> String {
    format!(
        r#"
[agents.custom_endpoints.local]
name     = "Fixture"
base_url = "{base_url}"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "fixture-model"
alias      = "Fixture Model"
config_key = "coding-key"

[agents.execution_profiles.default]
coding_model = "coding-key"
{profile_fields}
"#
    )
}

/// A plain end-turn `openai_chat_completions` SSE transcript — one
/// content delta plus the `[DONE]` sentinel.
fn chat_text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        json_string(text)
    )
}

/// A `chat.completion.chunk` turn calling `name` once with `args` —
/// `id`/`name` on the announce fragment, the arguments as a string
/// fragment after it, `finish_reason: "tool_calls"` closing the turn
/// (the agent_panel.rs wire shape).
fn tool_turn(call_id: &str, name: &str, args: &str) -> String {
    let mut body = format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":{},\"type\":\"function\",\"function\":{{\"name\":{},\"arguments\":\"\"}}}}],\"finish_reason\":null}}}}]}}\n\n",
        json_string(call_id),
        json_string(name),
    );
    body.push_str(&format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":{}}}}}]}}}}]}}\n\n",
        json_string(args)
    ));
    body.push_str(
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    );
    body.push_str("data: [DONE]\n\n");
    body
}

/// `ait --headless <args>` under the fixture environment: settings
/// from `xdg`, `store.db` under `data`, `work` the task's cwd.
fn headless(xdg: &Path, data: &Path, work: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(ait_bin!());
    command
        .arg("--headless")
        .args(args)
        .env("XDG_CONFIG_HOME", xdg)
        .env("XDG_DATA_HOME", data)
        .env_remove("AIT_CTL_SOCKET")
        .current_dir(work);
    command
}

/// The fixture dirs every run gets: `(xdg, data, work)` — each a fresh
/// tempdir so the spawned process reads and writes only what the test
/// planted.
fn dirs() -> (tempfile::TempDir, tempfile::TempDir, tempfile::TempDir) {
    (
        tempfile::tempdir().expect("config tempdir"),
        tempfile::tempdir().expect("data tempdir"),
        tempfile::tempdir().expect("cwd tempdir"),
    )
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// us-17/18/24/25/26: a `Done` task — the `run_command` executes in the
/// hidden shell (`always_allow` clears its gate), the activity lands
/// on stderr `ait: `-prefixed, assistant text is stdout verbatim with
/// no escape bytes, and the exit code is 0.
#[test]
fn a_done_task_streams_the_answer_and_exits_zero() {
    let stub = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "run_command",
            r#"{"command":"echo HEADLESS_OK"}"#,
        )),
        sse(&chat_text_turn("ran it — done")),
    ]);
    let (xdg, data, work) = dirs();
    write_settings(
        xdg.path(),
        &settings_with_fixture(&stub.base_url, "execute_commands = \"always_allow\""),
    );

    let output = headless(xdg.path(), data.path(), work.path(), &["run the echo"])
        .output()
        .expect("the headless run completes");

    assert!(
        output.status.success(),
        "exit {:?}, stderr: {}",
        output.status.code(),
        stderr(&output)
    );
    assert_eq!(stdout(&output), "ran it — done");
    assert!(
        !stdout(&output).contains('\u{1b}'),
        "no TUI bytes on the answer stream: {:?}",
        stdout(&output)
    );
    let err = stderr(&output);
    assert!(err.contains("ait: tool run_command:"), "stderr: {err}");
    assert!(err.contains("ait: tool call_1 done"), "stderr: {err}");
    // The activity contract: every stderr line is `ait: `-prefixed.
    assert!(
        err.lines().all(|line| line.starts_with("ait: ")),
        "an unprefixed stderr line: {err}"
    );
}

/// us-26/29: a terminal `Error` exits 1 with the message on stderr —
/// stdout stays empty, pipe-safe even on failure. 400 skips the
/// adapter's bounded retry (the agent tests' precedent).
#[test]
fn an_endpoint_error_exits_one_with_stdout_clean() {
    let stub = Fixture::serve(vec![error_status(
        400,
        "{\"error\":{\"message\":\"provider down\"}}",
    )]);
    let (xdg, data, work) = dirs();
    write_settings(xdg.path(), &settings_with_fixture(&stub.base_url, ""));

    let output = headless(xdg.path(), data.path(), work.path(), &["do the thing"])
        .output()
        .expect("the headless run completes");

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stdout(&output).is_empty(),
        "an error never writes stdout: {}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("ait: error:"),
        "stderr: {}",
        stderr(&output)
    );
}

/// us-22/29: an unknown `--profile` is a startup failure — exit 1 with
/// `ait: ` on stderr and nothing on stdout, before anything spawns.
#[test]
fn an_unknown_profile_is_a_startup_failure() {
    let (xdg, data, work) = dirs();
    write_settings(xdg.path(), &settings_with_fixture("http://127.0.0.1:1", ""));

    let output = headless(
        xdg.path(),
        data.path(),
        work.path(),
        &["check", "--profile", "ghost"],
    )
    .output()
    .expect("the headless run completes");

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty(), "stdout: {}", stdout(&output));
    let err = stderr(&output);
    assert!(
        err.contains("ait: unknown execution profile"),
        "stderr: {err}"
    );
    assert!(err.contains("ghost"), "stderr: {err}");
}

/// us-26: `BudgetReached` maps to exit 2 — the fixed fifty-turn step
/// budget trips with every `read_files` call auto-denied under
/// `always_ask` (no shell round-trip), and `ait: step budget reached`
/// lands on stderr.
#[test]
fn the_step_budget_exits_two() {
    let scripts: Vec<Script> = (0..50)
        .map(|i| {
            sse(&tool_turn(
                &format!("call_{i}"),
                "read_files",
                r#"{"paths":["note.txt"]}"#,
            ))
        })
        .collect();
    let stub = Fixture::serve(scripts);
    let (xdg, data, work) = dirs();
    write_settings(
        xdg.path(),
        &settings_with_fixture(&stub.base_url, "read_files = \"always_ask\""),
    );

    let output = headless(
        xdg.path(),
        data.path(),
        work.path(),
        &["summarize the file"],
    )
    .output()
    .expect("the headless run completes");

    assert_eq!(output.status.code(), Some(2), "stderr: {}", stderr(&output));
    assert!(
        stderr(&output).contains("ait: step budget reached"),
        "stderr: {}",
        stderr(&output)
    );
}

/// us-27: SIGINT mid-run is the headless Esc — the armed flag calls
/// `session.cancel()`, the task reports `Cancelled` and the process
/// exits 130. The drip-fed fixture holds the model stream open so the
/// signal provably lands while a task is in flight: the captured
/// request is the arm.
#[cfg(unix)]
#[test]
fn sigint_cancels_the_in_flight_task() {
    let (drip_tx, drip_rx) = mpsc::channel::<Vec<u8>>();
    let stub = Fixture::serve(vec![Script::DripSse(drip_rx)]);
    let (xdg, data, work) = dirs();
    write_settings(xdg.path(), &settings_with_fixture(&stub.base_url, ""));

    let mut child = headless(xdg.path(), data.path(), work.path(), &["stall"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the headless run");

    // The request landing proves the run is armed and the task in
    // flight — the signal must reach a live task, not a queued one.
    let deadline = Instant::now() + Duration::from_secs(15);
    while stub.captured_count() == 0 {
        assert!(Instant::now() < deadline, "the model request never arrived");
        std::thread::sleep(Duration::from_millis(10));
    }
    // SAFETY: `kill` on our own child delivering SIGINT — the headless
    // run's own us-27 contract.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };

    // `drip_tx` stays alive past the wait — its drop would EOF the
    // stalled stream and race the cancel with a terminal event.
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait().expect("poll the headless run") {
            Some(status) => break status,
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            None => {
                let _ = child.kill();
                panic!("the run never exited");
            }
        }
    };
    assert_eq!(status.code(), Some(130));
    drop(drip_tx);

    let mut err = String::new();
    child
        .stderr
        .take()
        .expect("stderr was piped")
        .read_to_string(&mut err)
        .expect("read the child's stderr");
    assert!(err.contains("ait: cancelled"), "stderr: {err}");
}
