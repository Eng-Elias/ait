//! e2e tests for the `ait` binary (Spec E01, ticket E01-T1): the real
//! binary in a real PTY via `ait-testkit`'s [`E2e`] harness. The harness
//! pins `SHELL=/bin/sh`, a fresh temp cwd and `TERM=xterm-256color`, so
//! the spawned app's chrome — tab bar (`N:sh` titles) and status line
//! (`cwd · sh · — · running`) — reproduces identically on every machine.
//!
//! The Round-3 config tests below are code-first rather than TOML
//! scenarios (Spec R03, ticket R03-T8): a scenario's `env` values are
//! literal strings, so `XDG_CONFIG_HOME` cannot be interpolated to a
//! per-test tempdir holding a fixture `settings.toml` — a testkit gap.
//! Here each test writes its own settings file under a `tempfile` dir
//! and passes the path through [`SpawnConfig::env`], which applies after
//! the harness's pinned `XDG_CONFIG_HOME` and therefore overrides it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc;
use std::thread;

use ait_testkit::{ait_bin, E2e, Size, SpawnConfig};
use tiny_http::{Header, Response, Server, StatusCode};

/// 80x24: row 0 is the tab bar, row 23 the status line.
const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};
const STATUS_ROW: usize = 23;

fn spawn_ait() -> E2e {
    E2e::spawn(SpawnConfig::new(ait_bin!(), &[], SIZE)).expect("spawn the ait binary")
}

/// Wait until the app's chrome has rendered: ` 1:sh ` in the tab bar,
/// the `… · sh · — · running` summary on the status line.
fn wait_for_chrome(app: &mut E2e) {
    app.wait_for_screen("the tab bar and status line", |app| {
        app.line(0).contains("1:sh") && app.line(STATUS_ROW).contains("· sh · — · running")
    });
}

/// Manual PR-evidence generator (Spec E01, ticket E01-T5: "the PR
/// description includes a transcript of an e2e run"). Excluded from the
/// suite — run it on demand:
///
/// ```text
/// cargo test -p ait --test e2e pr_evidence_transcript -- --ignored --nocapture
/// ```
///
/// It prints a spawn → keystroke → screen assertion → exit transcript
/// and leaves the full evidence dump under
/// `target/e2e-failures/pr-evidence/`.
#[test]
#[ignore = "manual evidence generator for PR bodies"]
fn pr_evidence_transcript() {
    let mut app = spawn_ait();
    wait_for_chrome(&mut app);
    println!("--- screen after spawn ---\n{}", app.screen_text());

    app.press("Alt+T");
    app.wait_for_screen("a second tab in the bar", |app| {
        app.line(0).contains("2:sh")
    });
    println!("--- tab bar after Alt+T ---\n{}", app.line(0));

    app.press("Alt+Q");
    let status = app.wait_for_exit();
    assert!(status.success(), "expected a clean exit, got {status:?}");
    println!("--- exited: {status:?} ---");

    let dir = app.dump_evidence("pr-evidence").expect("evidence dump");
    println!("--- evidence dump: {} ---", dir.display());
}

/// One text snapshot of the initial screen (Spec E01: plain-text diffs
/// for visual regressions). The pinned dash prompt is `$ `;
/// [`E2e::snapshot`] redacts the temp cwd so the snapshot is
/// machine-independent.
#[test]
fn initial_screen_snapshot() {
    let mut app = spawn_ait();
    wait_for_chrome(&mut app);
    app.wait_for_screen("the shell prompt", |app| app.line(1) == "$");

    insta::assert_snapshot!(app.snapshot());
}

// ---------------------------------------------------------------------------
// Spec R03: settings.toml driving the live app (ticket R03-T8).
// ---------------------------------------------------------------------------

/// Write `contents` as `<xdg>/ait/settings.toml`, creating the `ait/`
/// directory first — the app's hot-reload watcher attaches to the
/// settings file's *parent* directory at startup, so the directory must
/// exist before spawn for reload coverage.
fn write_settings(xdg: &Path, contents: &str) {
    let dir = xdg.join("ait");
    fs::create_dir_all(&dir).expect("create the fixture config directory");
    fs::write(dir.join("settings.toml"), contents).expect("write fixture settings.toml");
}

/// Spawn `ait` with `XDG_CONFIG_HOME` pointing at `xdg` — applied after
/// the harness's pinned env, so it overrides the missing-dir default.
fn spawn_ait_with_xdg(xdg: &Path) -> E2e {
    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config.env.push((
        OsString::from("XDG_CONFIG_HOME"),
        xdg.as_os_str().to_os_string(),
    ));
    E2e::spawn(config).expect("spawn the ait binary")
}

/// A valid `settings.toml`: one local endpoint whose model carries
/// `alias`, and the `default` profile's `base_model` resolving to it.
fn settings_with_model(alias: &str) -> String {
    format!(
        r#"
[agents.custom_endpoints.local]
name     = "Local"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "wire-name"
alias      = "{alias}"
config_key = "key-1"

[agents.execution_profiles.default]
base_model = "key-1"
"#
    )
}

/// Stories 1 + 24: a `settings.toml` under `$XDG_CONFIG_HOME/ait/` is
/// picked up at startup and the status line's model slot shows the
/// resolved `base_model`'s alias instead of the `—` placeholder.
#[test]
fn settings_file_sets_the_status_line_model() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_model("Qwen Coder"));

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the resolved model alias on the status line", |app| {
        app.line(STATUS_ROW).contains("· sh · Qwen Coder · running")
    });
    assert_eq!(app.line(0), " 1:sh");
}

/// Stories 7–8 + strict unknown-key rejection (ADR-0006): an invalid
/// `settings.toml` never costs the terminal — `ait` boots on built-in
/// defaults, toasts the `ConfigError` (`config error: <key-path>: …`),
/// and reverts to the normal `—` status summary once the toast expires.
#[test]
fn invalid_settings_still_boots_with_a_config_error_toast() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    // An unknown key is rejected by `deny_unknown_fields` — the error
    // names the offending key path.
    write_settings(dir.path(), "[agents]\nbogus_key = true\n");

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the config-error toast on the status line", |app| {
        app.line(STATUS_ROW).contains("config error:")
    });
    assert!(
        app.line(STATUS_ROW).contains("bogus_key"),
        "the toast names the offending key: {}",
        app.line(STATUS_ROW)
    );
    // Boots anyway — past the ~3 s toast the default summary returns.
    app.wait_for_screen("the default status summary after the toast", |app| {
        app.line(STATUS_ROW).contains("· sh · — · running")
    });
    assert_eq!(app.line(0), " 1:sh");
}

/// Story 12: `[terminal] shell` overrides the program NEW sessions
/// spawn — the tab title and the status line's shell slot are the
/// spawned program's basename, so `cat` shows up as `1:cat` / `· cat ·`.
#[test]
fn terminal_shell_spawns_the_configured_program() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), "[terminal]\nshell = \"cat\"\n");

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the configured shell in the chrome", |app| {
        app.line(0).contains("1:cat") && app.line(STATUS_ROW).contains("· cat · — · running")
    });
}

/// Story 13: `[terminal] scrollback_lines` bounds the scrollback a new
/// session keeps — with a 5-line capacity the `seq 1 100` command echo
/// falls out of retained history entirely (a default-capacity session
/// would still show it at the scroll top, per scrollback-keys.toml).
#[test]
fn terminal_scrollback_lines_bounds_retained_history() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), "[terminal]\nscrollback_lines = 5\n");

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("the shell prompt", |app| app.line(1) == "$");

    app.type_text("seq 1 100\n");
    // Gate on the fresh prompt at the content band's bottom row, so the
    // scroll accounting below is settled (output finished, prompt back).
    app.wait_for_screen("seq's output and a fresh prompt", |app| app.line(22) == "$");

    app.press("Shift+Home");
    // At the scroll top only the last ~5 scrolled-off lines survive:
    // the top row is a seq number near the end, and the command echo —
    // line 1 of the session's history — is gone entirely.
    app.wait_for_screen("a truncated scroll top", |app| {
        !app.screen_text().contains("seq 1 100") && app.line(1).chars().all(|c| c.is_ascii_digit())
    });
    let oldest: u32 = app
        .line(1)
        .parse()
        .expect("the scroll top is a seq output line");
    assert!(
        oldest >= 50,
        "5-line scrollback keeps only the tail of 100 lines, got line {oldest}"
    );
}

/// Stories 10–11 + 14: edits to `settings.toml` hot-reload live —
/// the model slot re-renders, and reloaded `[terminal]` options apply
/// to NEW sessions only (the live `sh` tab keeps its spawn-time
/// program; a post-reload `Alt+T` tab gets the configured `cat`).
/// A broken save keeps the last-good settings and toasts the error.
#[test]
fn hot_reload_swaps_the_model_and_keeps_last_good() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let settings_path = dir.path().join("ait").join("settings.toml");
    write_settings(dir.path(), &settings_with_model("First Model"));

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("the first model on the status line", |app| {
        app.line(STATUS_ROW)
            .contains("· sh · First Model · running")
    });

    // A good edit swaps the live settings: the model slot re-renders on
    // the existing tab, and the new `[terminal]` becomes the template
    // for sessions spawned from now on.
    let reloaded = format!(
        "[terminal]\nshell = \"cat\"\n{}",
        settings_with_model("Second Model")
    );
    fs::write(&settings_path, reloaded).expect("rewrite settings.toml");
    app.wait_for_screen("the reloaded model on the status line", |app| {
        app.line(STATUS_ROW)
            .contains("· sh · Second Model · running")
    });
    app.press("Alt+T");
    app.wait_for_screen("a new tab on the reloaded shell", |app| {
        app.line(0).contains("2:cat")
            && app
                .line(STATUS_ROW)
                .contains("· cat · Second Model · running")
    });

    // A broken save keeps last-good: the error toasts, then the summary
    // returns with the previous model still resolved.
    fs::write(&settings_path, "[agents]\nbogus_key = true\n").expect("break settings.toml");
    app.wait_for_screen("the reload-error toast", |app| {
        app.line(STATUS_ROW).contains("config error:")
    });
    app.wait_for_screen("last-good settings after the toast", |app| {
        app.line(STATUS_ROW)
            .contains("· cat · Second Model · running")
    });
}

/// Story 24's fallback chain, middle step: a model entry with an EMPTY
/// `alias` renders its wire `name` in the status line's model slot
/// (`alias → wire name → —`). `alias` is a required field, so `""` —
/// not an absent key — is what drives the fallback.
#[test]
fn model_slot_falls_back_to_wire_name() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(
        dir.path(),
        r#"
[agents.custom_endpoints.local]
name     = "Local"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "qwen2.5-coder:14b"
alias      = ""
config_key = "key-1"

[agents.execution_profiles.default]
base_model = "key-1"
"#,
    );

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the wire name on the status line", |app| {
        app.line(STATUS_ROW)
            .contains("· sh · qwen2.5-coder:14b · running")
    });
}

/// Endpoints declared but no `default` profile (or no `base_model` on
/// it): `resolve_model` yields `None` and the model slot renders `—` —
/// a quiet empty state, not a config error.
#[test]
fn model_slot_is_dash_without_a_default_profile() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(
        dir.path(),
        r#"
[agents.custom_endpoints.local]
name     = "Local"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "wire-name"
config_key = "key-1"
"#,
    );

    let mut app = spawn_ait_with_xdg(dir.path());

    app.wait_for_screen("the dash model slot", |app| {
        app.line(STATUS_ROW).contains("· sh · — · running")
    });
    // A toast renders on the same tick the status does — by the time the
    // summary is up, an error would already be on screen.
    assert!(
        !app.line(STATUS_ROW).contains("config error:"),
        "no error toast for a merely-unconfigured profile: {}",
        app.line(STATUS_ROW)
    );
}

/// Story 10's creation case: the watcher attaches to the settings
/// file's parent directory — creating it on the first-run path where
/// `$XDG_CONFIG_HOME/ait/` doesn't exist yet — so the user's first-ever
/// `settings.toml` save flips `—` to the resolved model without a
/// restart.
#[test]
fn settings_created_after_spawn_hot_loads() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    // No `ait/` directory at all — the true first-run state.

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("defaults before any settings file", |app| {
        app.line(STATUS_ROW).contains("· sh · — · running")
    });

    write_settings(dir.path(), &settings_with_model("Late Model"));

    app.wait_for_screen("the hot-loaded model on the status line", |app| {
        app.line(STATUS_ROW).contains("· sh · Late Model · running")
    });
}

/// Deleting `settings.toml` mid-session reloads to built-in defaults —
/// `load()` treats a missing file as defaults and the watcher reports
/// the deletion, so the model slot returns to `—`.
#[test]
fn settings_deleted_reverts_to_defaults() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let settings_path = dir.path().join("ait").join("settings.toml");
    write_settings(dir.path(), &settings_with_model("Going Away"));

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("the model before deletion", |app| {
        app.line(STATUS_ROW).contains("· sh · Going Away · running")
    });

    fs::remove_file(&settings_path).expect("delete settings.toml");

    app.wait_for_screen("defaults after the file is deleted", |app| {
        app.line(STATUS_ROW).contains("· sh · — · running")
    });
}

/// Story 14's scrollback half: a reloaded `scrollback_lines` bounds NEW
/// sessions only — the pre-reload tab keeps its full spawn-time
/// history (the `seq 1 100` echo survives at its scroll top), while a
/// tab spawned after the reload is capped at 5 lines.
#[test]
fn existing_tabs_keep_spawn_time_scrollback_after_reload() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let settings_path = dir.path().join("ait").join("settings.toml");
    // No [terminal] — the spawn-time scrollback is the default 10k.
    write_settings(dir.path(), &settings_with_model("First Model"));

    let mut app = spawn_ait_with_xdg(dir.path());
    app.wait_for_screen("the first model and a prompt", |app| {
        app.line(STATUS_ROW).contains("First Model") && app.line(1) == "$"
    });
    app.type_text("seq 1 100\n");
    app.wait_for_screen("seq's output and a fresh prompt", |app| app.line(22) == "$");

    // Reload: a 5-line scrollback cap plus a new model — the model swap
    // on the status line gates that the reload landed.
    let reloaded = format!(
        "[terminal]\nscrollback_lines = 5\n{}",
        settings_with_model("Second Model")
    );
    fs::write(&settings_path, reloaded).expect("rewrite settings.toml");
    app.wait_for_screen("the reloaded model on the status line", |app| {
        app.line(STATUS_ROW)
            .contains("· sh · Second Model · running")
    });

    // The live tab keeps its spawn-time capacity: Shift+Home reaches
    // the command echo at the very top of its history.
    app.press("Shift+Home");
    app.wait_for_screen("the echo still at the old tab's scroll top", |app| {
        app.screen_text().contains("seq 1 100")
    });

    // A post-reload tab gets the 5-line cap — the bounded-history
    // assertion from terminal_scrollback_lines_bounds_retained_history.
    app.press("Alt+T");
    app.wait_for_screen("a second tab in the bar", |app| {
        app.line(0).contains("2:sh")
    });
    app.wait_for_screen("the new tab's prompt", |app| app.line(1) == "$");
    app.type_text("seq 1 100\n");
    app.wait_for_screen("seq's output and a fresh prompt", |app| app.line(22) == "$");
    app.press("Shift+Home");
    app.wait_for_screen("a truncated scroll top on the new tab", |app| {
        !app.screen_text().contains("seq 1 100") && app.line(1).chars().all(|c| c.is_ascii_digit())
    });
    let oldest: u32 = app
        .line(1)
        .parse()
        .expect("the scroll top is a seq output line");
    assert!(
        oldest >= 50,
        "the reloaded 5-line cap bounds only the new tab, got line {oldest}"
    );
}

// ---------------------------------------------------------------------------
// Spec R04: natural language → command, end to end (ticket R04-T7).
//
// The real path, no internals: a verified `# `+Enter or `Alt+Enter`
// keypress queues a `SuggestRequest`, the `CommandSuggester` worker
// POSTs to a loopback stub speaking the `openai_chat_completions` SSE
// wire format, and the sanitized deltas type themselves onto the input
// line. Code-first for the same reason as the R03 tests, plus one more:
// the stub's ephemeral port cannot be interpolated into a scenario's
// `env` either (backlog #37). The stub is a trimmed copy of the
// ait-llm/ait-agent `tests/support` fixture — one scripted response per
// incoming request, on a daemon thread.
// ---------------------------------------------------------------------------

/// One scripted response — one entry per incoming request, consumed in
/// order (the fixture's own convention).
enum Script {
    /// A complete SSE body, sent in one response.
    Sse(String),
    /// A body fed chunk-by-chunk through a channel — the mid-stream
    /// stall the Esc test needs: send what must arrive, hold the rest,
    /// and the stream pauses exactly there (the test's pacing knob;
    /// nothing sleeps).
    Drip(mpsc::Receiver<Vec<u8>>),
}

/// A `Read` fed by a channel: `Ok(chunk)` appends, a dropped sender is
/// EOF, a live-but-silent sender blocks. Copied from the fixture's
/// `ChannelRead`.
struct ChannelRead {
    rx: mpsc::Receiver<Vec<u8>>,
    buf: VecDeque<u8>,
}

impl Read for ChannelRead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.buf.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.buf.extend(chunk),
                Err(_) => return Ok(0),
            }
        }
        let n = self.buf.len().min(out.len());
        for slot in out.iter_mut().take(n) {
            *slot = self.buf.pop_front().expect("n is bounded by buf len");
        }
        Ok(n)
    }
}

/// The in-process SSE stub: a `tiny_http` server on
/// `127.0.0.1:<ephemeral>` replaying `Script`s, one per request.
struct SseStub {
    /// `http://127.0.0.1:<port>` — the fixture `base_url` the settings
    /// file points `[agents.custom_endpoints.local]` at.
    base_url: String,
    /// The daemon serving thread — exits with the test process.
    _thread: thread::JoinHandle<()>,
}

impl SseStub {
    /// Start the stub replaying `scripts`, one response per request.
    fn serve(scripts: Vec<Script>) -> Self {
        let server = Server::http("127.0.0.1:0").expect("bind the SSE stub on loopback");
        let addr = server.server_addr().to_ip().expect("the stub has an IP");
        let mut scripts: VecDeque<Script> = scripts.into();
        let thread = thread::spawn(move || {
            while let Some(script) = scripts.pop_front() {
                let Ok(mut request) = server.recv() else {
                    break;
                };
                // Drain the POST body so the reply goes out clean.
                let mut payload = String::new();
                let _ = request.as_reader().read_to_string(&mut payload);
                match script {
                    Script::Sse(body) => {
                        let mut response =
                            Response::from_string(body).with_status_code(StatusCode(200));
                        response.add_header(
                            Header::from_bytes("Content-Type", "text/event-stream")
                                .expect("ASCII header"),
                        );
                        let _ = request.respond(response);
                    }
                    Script::Drip(rx) => {
                        let _ = request.respond(Response::new(
                            StatusCode(200),
                            vec![Header::from_bytes("Content-Type", "text/event-stream")
                                .expect("ASCII header")],
                            ChannelRead {
                                rx,
                                buf: VecDeque::new(),
                            },
                            None,
                            None,
                        ));
                    }
                }
            }
        });
        SseStub {
            base_url: format!("http://{addr}"),
            _thread: thread,
        }
    }
}

/// A JSON string literal (`"…"`) for the chunk body — the same
/// hand-rolled escaping ait-agent's suggester tests use; the transcripts
/// here are built inline.
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// One `chat.completion.chunk` `data:` frame carrying a content delta —
/// the wire shape the `openai_chat_completions` adapter decodes (fields
/// the IR doesn't read stay out; `choices`/`delta`/`content` are the
/// whole contract).
fn sse_delta(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\n",
        json_string(text)
    )
}

/// A complete `openai_chat_completions` SSE transcript: the deltas,
/// then the `[DONE]` sentinel.
fn sse_transcript(deltas: &[&str]) -> String {
    let mut body: String = deltas.iter().map(|d| sse_delta(d)).collect();
    body.push_str("data: [DONE]\n\n");
    body
}

/// A `settings.toml` whose `command_model` resolves to the stub — the
/// same fixture shape as [`settings_with_model`], with `base_url`
/// pointed at the loopback stub. No `api_key_env` and no keyring entry
/// resolve to no secret, so the request goes out unauthenticated
/// (us-21).
fn settings_with_stub(base_url: &str) -> String {
    format!(
        r#"
[agents.custom_endpoints.local]
name     = "Stub"
base_url = "{base_url}"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "stub-model"
alias      = "Stub Model"
config_key = "cmd-key"

[agents.execution_profiles.default]
command_model = "cmd-key"
"#
    )
}

/// The live input line: the viewport row the cursor sits on — `$ `
/// prompt plus whatever the shell has echoed. Asserting on the
/// cursor's own row is the honest "what's on the input line" check and
/// needs no hard-coded row for post-submission prompts.
fn input_line(app: &E2e) -> String {
    app.line(app.cursor().line)
}

/// Gate on the shell's echo of `text` BEFORE the submission keypress —
/// the trigger's shadow+grid verification needs the echoed line on the
/// grid when Enter/Alt+Enter arrives, or it cannot fire (desync is
/// inert, us-13). Waiting here is what makes the trigger keypress
/// deterministic.
fn wait_for_typed_echo(app: &mut E2e, text: &str) {
    app.wait_for_screen("the echoed input line", |app| {
        input_line(app).ends_with(text)
    });
}

/// Stories 1–3: `# <request>` + Enter submits the line to the shell as
/// a comment — it stays in scrollback as a normal submitted line — and
/// the stub's command streams onto the FRESH prompt like typing, never
/// executed (the Enter the user presses is the only one).
#[test]
fn hash_enter_streams_the_suggested_command() {
    let stub = SseStub::serve(vec![Script::Sse(sse_transcript(&[
        "find ",
        ". -type f -size +1G",
    ]))]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_stub(&stub.base_url));

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    app.wait_for_screen("the shell prompt", |app| app.line(1) == "$");

    app.type_text("# list files larger than 1GB");
    wait_for_typed_echo(&mut app, "# list files larger than 1GB");
    app.press("Enter");

    app.wait_for_screen("the streamed command on the fresh prompt", |app| {
        input_line(app) == "$ find . -type f -size +1G"
    });
    // The `# ` line went to the shell as a comment no-op — it sits in
    // scrollback one row up as a submitted line, and the suggestion
    // only ever typed (us-2/4).
    assert_eq!(app.line(1), "$ # list files larger than 1GB");
    assert_eq!(input_line(&app), "$ find . -type f -size +1G");
}

/// Stories 11–12: `Alt+Enter` on a bare typed line — no `# ` prefix —
/// takes the whole verified line as the request, erases it, and the
/// suggestion streams onto the same prompt row.
#[test]
fn alt_enter_suggests_for_a_bare_line() {
    let stub = SseStub::serve(vec![Script::Sse(sse_transcript(&[
        "git reset --soft ",
        "HEAD~1",
    ]))]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_stub(&stub.base_url));

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    app.wait_for_screen("the shell prompt", |app| app.line(1) == "$");

    app.type_text("undo my last commit");
    wait_for_typed_echo(&mut app, "undo my last commit");
    app.press("Alt+Enter");

    app.wait_for_screen("the suggestion replacing the typed line", |app| {
        input_line(app) == "$ git reset --soft HEAD~1"
    });
}

/// Story 8: Esc mid-stream cancels the request and wipes the partial
/// command. The stub's drip is the timing knob — one chunk carries the
/// first delta past tiny_http's ~8 KiB buffer, then the stream stalls;
/// Esc's `Ctrl+U` clears the line, and a second `# ` request afterwards
/// proves the pin released and the terminal is back to normal (us-10).
#[test]
fn escape_mid_stream_wipes_the_partial_command() {
    let (drip_tx, drip_rx) = mpsc::channel::<Vec<u8>>();
    let stub = SseStub::serve(vec![
        Script::Drip(drip_rx),
        // The post-cancel request: proof the trigger refires.
        Script::Sse(sse_transcript(&["du -sh ."])),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    write_settings(dir.path(), &settings_with_stub(&stub.base_url));

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    app.wait_for_screen("the shell prompt", |app| app.line(1) == "$");

    app.type_text("# list log files");
    wait_for_typed_echo(&mut app, "# list log files");
    app.press("Enter");

    // One chunk, then silence: the delta first, then a `:`-comment pad
    // pushing past tiny_http's ~8 KiB chunked-encoder buffer (the
    // fixture's own caveat — smaller writes sit in that buffer until
    // EOF and never reach the client before the stall). The comment
    // line is left unterminated so the SSE parser holds it silently.
    let mut chunk = sse_delta("ls -la /var/lo").into_bytes();
    chunk.extend_from_slice(format!(": {}\n", "x".repeat(10 * 1024)).as_bytes());
    drip_tx.send(chunk).expect("drip feed");

    app.wait_for_screen("the partial suggestion on the input line", |app| {
        input_line(app) == "$ ls -la /var/lo"
    });

    app.press("Escape");
    app.wait_for_screen("the wiped input line", |app| input_line(app) == "$");

    // The cancelled request ended silently — the status line keeps the
    // normal summary (the model slot resolves `base_model`, which this
    // fixture leaves unset), never a toast for the user's own Esc.
    app.wait_for_screen("no cancel toast on the status line", |app| {
        app.line(STATUS_ROW).contains("· sh · — · running") && input_line(app) == "$"
    });
    // A post-cancel chunk can never re-type onto the wiped line — the
    // pin flags `cancelled` until the worker's terminal event, and a
    // late `Delta` drops against it. The second request below is the
    // real proof: it can only fire after the pin released, and it lands
    // on a line that must be exactly the bare prompt first.
    let _ = drip_tx.send(sse_delta("g").into_bytes());
    drop(drip_tx);

    // A fresh `# ` request fires the next suggestion — the pin released
    // and the trigger is armed again.
    app.type_text("# disk usage");
    wait_for_typed_echo(&mut app, "# disk usage");
    app.press("Enter");
    app.wait_for_screen("the post-cancel request streaming", |app| {
        input_line(app) == "$ du -sh ."
    });
}

/// Story 22: a request with no `command_model` (and no `base_model`
/// fallback) surfaces as a status-line toast — never a wedge, never a
/// crash — and the terminal takes the next command like nothing
/// happened (§6 degrade-gracefully).
#[test]
fn unconfigured_model_toasts_and_the_terminal_keeps_working() {
    let dir = tempfile::tempdir().expect("fixture tempdir");
    // The `default` profile exists but binds no model roles — the
    // `resolve_model` `Ok(None)` path, "unconfigured" in the literal
    // sense (us-22).
    write_settings(dir.path(), "[agents.execution_profiles.default]\n");

    let mut app = spawn_ait_with_xdg(dir.path());
    wait_for_chrome(&mut app);
    app.wait_for_screen("the shell prompt", |app| app.line(1) == "$");

    app.type_text("# list files");
    wait_for_typed_echo(&mut app, "# list files");
    app.press("Enter");

    app.wait_for_screen("the unconfigured-model toast", |app| {
        app.line(STATUS_ROW).contains("command_model")
    });

    // The terminal never wedged: the comment submitted, the fresh
    // prompt took it, and the next command runs normally.
    app.type_text("echo ok\n");
    app.wait_for_screen("the terminal still running commands", |app| {
        app.screen_text().lines().any(|line| line == "ok")
    });
}
