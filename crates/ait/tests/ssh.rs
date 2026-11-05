//! e2e for §5.9 SSH integration (Spec R09-T8): the whole remote loop
//! against a REAL `sshd` — opt-in only, so the suite stays hermetic by
//! default. Two target shapes share one scenario:
//!
//! - the container `scripts/ssh-e2e.sh` brings up exports
//!   `AIT_E2E_SSH_{PORT,KEY,USER,HOST}` pointing at it — the test then
//!   just connects;
//! - bare `AIT_E2E_SSH=1` with no target env provisions a throwaway
//!   sshd on a loopback port from the local host keys — handy where a
//!   system `sshd` exists but docker does not.
//!
//! Either way the gate is the same: `AIT_E2E_SSH` unset skips before
//! anything spawns, keeping `cargo test --workspace` unchanged.

use std::ffi::OsString;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use ait_llm::testing::{assert_post, sse, Fixture};
use ait_testkit::{
    ait_bin, json_string, panel_has_line, terminal_half, wait_fresh_prompt, E2e, Size, SpawnConfig,
};

/// 80x24: row 0 is the tab bar, row 23 the status line.
const SIZE: Size = Size {
    columns: 80,
    lines: 24,
};
const STATUS_ROW: usize = 23;

/// Who to ssh as and with what — either the container env contract or
/// a locally provisioned sshd (`guard` keeps both alive).
struct SshTarget {
    /// The destination token the wrapper reports (`user@host`).
    dest: String,
    /// The full interactive argv minus `ssh` itself — flags included.
    command: String,
    /// The local-sshd case's tempdir + daemon child — dropped last.
    _guard: Option<(tempfile::TempDir, Child)>,
}

/// `scripts/ssh-e2e.sh`'s env contract — returns `None` when the
/// target vars aren't all present.
fn container_target() -> Option<SshTarget> {
    let (port, key) = (
        std::env::var("AIT_E2E_SSH_PORT").ok()?,
        std::env::var("AIT_E2E_SSH_KEY").ok()?,
    );
    let user = std::env::var("AIT_E2E_SSH_USER").unwrap_or_else(|_| "testuser".into());
    let host = std::env::var("AIT_E2E_SSH_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let dest = format!("{user}@{host}");
    let command = format!(
        "ssh -p {port} -i {key} \
         -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
         {dest}"
    );
    Some(SshTarget {
        dest,
        command,
        _guard: None,
    })
}

/// The first sshd on the usual paths/PATH — `None` on hosts without
/// one (the test skips; e2e coverage is best-effort by design).
fn sshd_bin() -> Option<PathBuf> {
    ["/usr/sbin/sshd", "/usr/bin/sshd", "/sbin/sshd"]
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .or_else(|| {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .map(|dir| dir.join("sshd"))
                .find(|path| path.exists())
        })
}

/// An ephemeral loopback port.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind a probe port")
        .local_addr()
        .unwrap()
        .port()
}

/// Bring up a throwaway sshd on a loopback port: generated host/user
/// keys, pubkey auth only, `PermitRootLogin yes` so a root-run suite
/// can connect. `None` when any piece is missing — the caller skips.
fn provision_local_sshd() -> Option<SshTarget> {
    let sshd = sshd_bin()?;
    let dir = tempfile::tempdir().ok()?;
    let d = dir.path();

    // sshd's privilege-separation dir must exist even for a private
    // daemon; /run/sshd is what stock installs use.
    if std::fs::create_dir_all("/run/sshd").is_err() {
        return None;
    }

    let run = |args: &[&str]| {
        Command::new(args[0])
            .args(&args[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()
            .filter(|status| status.success())
    };
    let host_key = d.join("host_key");
    let user_key = d.join("user_key");
    if run(&[
        "ssh-keygen",
        "-t",
        "ed25519",
        "-f",
        host_key.to_str()?,
        "-N",
        "",
        "-q",
    ])
    .is_none()
        || run(&[
            "ssh-keygen",
            "-t",
            "ed25519",
            "-f",
            user_key.to_str()?,
            "-N",
            "",
            "-q",
        ])
        .is_none()
    {
        return None;
    }
    let pubkey = std::fs::read_to_string(d.join("user_key.pub")).ok()?;
    std::fs::write(d.join("authorized_keys"), pubkey).ok()?;

    let port = free_port();
    std::fs::write(
        d.join("sshd_config"),
        format!(
            "Port {port}\n\
             ListenAddress 127.0.0.1\n\
             HostKey {host}\n\
             PidFile {pid}\n\
             PasswordAuthentication no\n\
             PubkeyAuthentication yes\n\
             AuthorizedKeysFile {keys}\n\
             PermitRootLogin yes\n\
             UsePAM no\n\
             StrictModes no\n\
             LogLevel ERROR\n",
            host = host_key.display(),
            pid = d.join("sshd.pid").display(),
            keys = d.join("authorized_keys").display(),
        ),
    )
    .ok()?;

    let mut daemon = Command::new(&sshd)
        .args(["-f"])
        .arg(d.join("sshd_config"))
        .args(["-D", "-e"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    // The daemon is ready once it accepts the loopback port; a couple
    // of seconds covers even a cold-starting sshd.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut listening = false;
    while Instant::now() < deadline {
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => {
                drop(stream);
                listening = true;
                break;
            }
            Err(_) if daemon.try_wait().ok()?.is_some() => break,
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    if !listening {
        let _ = daemon.kill();
        return None;
    }

    // The login user: $USER, else `id -un`.
    let user = std::env::var("USER").ok().or_else(|| {
        Command::new("id")
            .arg("-un")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_owned())
    })?;
    let known_hosts = d.join("known_hosts");
    let dest = format!("{user}@127.0.0.1");
    let command = format!(
        "ssh -p {port} -i {key} \
         -o StrictHostKeyChecking=no -o UserKnownHostsFile={known} \
         {dest}",
        key = user_key.display(),
        known = known_hosts.display(),
    );
    Some(SshTarget {
        dest,
        command,
        _guard: Some((dir, daemon)),
    })
}

/// Spawn `ait` on real bash (the wrapper lives in the snippet) with a
/// `[ssh]` deny fixture for the passthrough case.
fn spawn_ait(dir: &Path) -> E2e {
    spawn_ait_with_settings(dir, "[ssh]\nhosts_denylist = [\"*.internal\"]\n")
}

/// [`spawn_ait`] with the whole `settings.toml` body supplied — the
/// evidence generator layers the scripted-provider `[agents]` tables
/// on top of the `[ssh]` fixture.
fn spawn_ait_with_settings(dir: &Path, settings: &str) -> E2e {
    let ait_dir = dir.join("ait");
    std::fs::create_dir_all(&ait_dir).expect("create the fixture config dir");
    std::fs::write(ait_dir.join("settings.toml"), settings).expect("write fixture settings.toml");

    let mut config = SpawnConfig::new(ait_bin!(), &[], SIZE);
    config.env.push((
        OsString::from("XDG_CONFIG_HOME"),
        dir.as_os_str().to_os_string(),
    ));
    config
        .env
        .push((OsString::from("SHELL"), OsString::from("/bin/bash")));
    E2e::spawn(config).expect("spawn the ait binary")
}

/// Type `command` + Enter and wait out its execution — gated on
/// `output_probe`, text only execution produces (std.15: never the
/// typed echo).
fn run_line(app: &mut E2e, command: &str, output_probe: &str) {
    app.type_text(&format!("{command}\n"));
    app.wait_for_screen(&format!("the output of `{command}`"), |app| {
        app.screen_text()
            .lines()
            .any(|line| line.contains(output_probe))
    });
    wait_fresh_prompt(app, "the prompt back");
}

/// Spec R09-T8: the real remote loop on a real sshd — `ssh` connects
/// through the wrapper (ssh_begin → remote bootstrap → remote
/// `init_shell`/`Integrated`), a remote block closes with its exit
/// code while the `ssh:` status segment is up, `exit` unwinds
/// (ssh_end → remote ctx closes → `bootstrapped` re-integrates the
/// local shell) — plus the denied host's plain passthrough.
#[test]
#[ignore = "opt-in real-SSH test — AIT_E2E_SSH=1 (scripts/ssh-e2e.sh)"]
fn ssh_remote_loop_against_a_real_sshd() {
    if std::env::var_os("AIT_E2E_SSH").is_none() {
        return;
    }
    if !Path::new("/bin/bash").exists() {
        return;
    }
    let Some(target) = container_target().or_else(provision_local_sshd) else {
        eprintln!("AIT_E2E_SSH set but no sshd target: see scripts/ssh-e2e.sh");
        return;
    };

    let dir = tempfile::tempdir().expect("fixture tempdir");
    let mut app = spawn_ait(dir.path());
    app.wait_for_screen("the bash chrome", |app| {
        app.line(STATUS_ROW).contains("· bash ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");

    // Denied passthrough first: `ssh host.internal` matches the
    // armed `*.internal` deny — plain `command ssh`, no wrap, no
    // segment, just ssh's own resolution error (the probe is the
    // resolver output — execution-produced, never the typed echo).
    run_line(
        &mut app,
        "ssh host.internal",
        "Could not resolve hostname host.internal",
    );
    assert!(
        !app.line(STATUS_ROW).contains("ssh:"),
        "a denied host never opens remote context: {}",
        app.line(STATUS_ROW)
    );

    // The real connect: ssh_begin lands as the wrapper fires, then the
    // remote bootstrap integrates the remote shell — the `ssh:` status
    // segment is the screen-visible proof the context is open. A probe
    // echo is the "remote prompt is up" gate: stdin typed ahead of it
    // would buffer through and execute anyway, but the marker's output
    // only appears once the remote shell is really reading.
    app.type_text(&format!("{}\n", target.command));
    app.wait_for_screen("the ssh: status segment", |app| {
        app.line(STATUS_ROW)
            .contains(&format!("ssh:{}", target.dest))
    });
    app.type_text("echo R09PROBE\n");
    app.wait_for_screen("the remote shell answered the probe", |app| {
        app.screen_text()
            .lines()
            .any(|line| line.trim() == "R09PROBE")
    });

    // A remote block: `echo` runs ON the remote host, its `D` mark
    // flows back through the channel and the status picks up `✓ 0`
    // beside the ssh segment.
    app.type_text("true\n");
    app.wait_for_screen("the remote block's exit code", |app| {
        app.line(STATUS_ROW).contains("✓ 0")
            && app
                .line(STATUS_ROW)
                .contains(&format!("ssh:{}", target.dest))
    });
    app.type_text("echo R09REMOTE\n");
    app.wait_for_screen("remote echo output", |app| {
        // `trim() == ` — the marker as a whole line, so the typed
        // `echo R09REMOTE` echo can never satisfy the wait (std.15).
        app.screen_text()
            .lines()
            .any(|line| line.trim() == "R09REMOTE")
    });

    // `exit` unwinds: the remote shell's exit_shell + the wrapper's
    // ssh_end close the context, `bootstrapped` re-integrates the
    // local shell — the segment is gone and local blocks resume.
    app.type_text("exit\n");
    app.wait_for_screen("the local session back", |app| {
        !app.line(STATUS_ROW).contains("ssh:")
    });
    app.type_text("echo R09LOCAL\n");
    app.wait_for_screen("local echo after the return", |app| {
        app.screen_text()
            .lines()
            .any(|line| line.trim() == "R09LOCAL")
    });
}

/// One `chat.completion.chunk` `data:` frame carrying a content
/// delta — a plain end turn (the `agent_panel.rs` helpers' shape,
/// duplicated here so this file stays self-contained).
fn chat_text_turn(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        json_string(text)
    )
}

/// A `chat.completion.chunk` turn calling `name` once with `args` —
/// the wire shape the `openai_chat_completions` adapter decodes into
/// one complete tool call (same helper as `agent_panel.rs`).
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

/// Manual PR-evidence generator (the `pr_evidence_*` precedent in
/// `agent_panel.rs`): the real-sshd loop PLUS the agent's remote
/// `read_files` — the scripted fixture model calls the tool while the
/// user's shell sits inside the wrapped `ssh`, so the remote tier
/// (`RemoteState::Remote` → `ToolCwd::new_remote` → `select_executor`)
/// serves it through the live ControlMaster socket while the user
/// keeps typing. Prints the screen at each beat:
///
/// ```text
/// AIT_E2E_SSH=1 cargo test -p ait --test ssh pr_evidence -- --ignored --nocapture
/// ```
#[test]
#[ignore = "manual evidence generator — needs a real sshd (AIT_E2E_SSH=1)"]
fn pr_evidence_ssh_remote_transcript() {
    if std::env::var_os("AIT_E2E_SSH").is_none() || !Path::new("/bin/bash").exists() {
        return;
    }
    let Some(target) = container_target().or_else(provision_local_sshd) else {
        eprintln!("AIT_E2E_SSH set but no sshd target: see scripts/ssh-e2e.sh");
        return;
    };

    let fixture = Fixture::serve(vec![
        sse(&tool_turn(
            "call_1",
            "read_files",
            r#"{"paths":["remote-marker.txt"]}"#,
        )),
        sse(&chat_text_turn("the remote file reads R09-REMOTE-FILE-OK")),
    ]);
    let dir = tempfile::tempdir().expect("fixture tempdir");
    let settings = format!(
        "[ssh]\nhosts_denylist = [\"*.internal\"]\n\n\
         [agents.custom_endpoints.local]\n\
         name     = \"Fixture\"\n\
         base_url = \"{}\"\n\
         schema   = \"openai_chat_completions\"\n\n\
         [[agents.custom_endpoints.local.models]]\n\
         name       = \"fixture-model\"\n\
         alias      = \"Fixture Model\"\n\
         config_key = \"coding-key\"\n\n\
         [agents.execution_profiles.default]\n\
         coding_model = \"coding-key\"\n",
        fixture.base_url
    );
    let mut app = spawn_ait_with_settings(dir.path(), &settings);
    app.wait_for_screen("the bash chrome", |app| {
        app.line(STATUS_ROW).contains("· bash ·")
    });
    wait_fresh_prompt(&mut app, "the first prompt");

    // Beat 1: the wrapped connect — `ssh_begin` fires, the bootstrap
    // lands, the remote shell integrates and the `ssh:` status
    // segment opens the remote context.
    app.type_text(&format!("{}\n", target.command));
    app.wait_for_screen("the ssh: status segment", |app| {
        app.line(STATUS_ROW)
            .contains(&format!("ssh:{}", target.dest))
    });
    println!(
        "--- beat 1: wrapped ssh, remote context open ---\n{}",
        app.screen_text()
    );

    // Beat 2: remote blocks — commands run on the remote host, their
    // marks flow back and the status carries the exit code beside the
    // ssh segment. The second block seeds the marker file on the
    // REMOTE side of the connection — under the remote cwd the task's
    // `ToolCwd::new_remote` confines to — so `read_files` has bytes
    // only the remote channel can serve.
    app.type_text("echo R09REMOTE\n");
    app.wait_for_screen("the remote block's output + exit", |app| {
        app.screen_text()
            .lines()
            .any(|line| line.trim() == "R09REMOTE")
            && app.line(STATUS_ROW).contains("✓ 0")
    });
    app.type_text("echo R09-REMOTE-FILE-OK > remote-marker.txt && echo R09SEEDED\n");
    app.wait_for_screen("the marker seeded on the remote host", |app| {
        app.screen_text()
            .lines()
            .any(|line| line.trim() == "R09SEEDED")
    });
    println!(
        "--- beat 2: remote blocks sealed on the remote host ---\n{}",
        app.screen_text()
    );

    // Beat 3: the panel opens while the shell is still remote.
    app.press("Alt+A");
    app.wait_for_screen("the open, focused agent panel", |app| {
        app.screen_text().contains('│') && app.screen_text().contains(" agent")
    });

    // Beat 4: the task submits; the fixture calls `read_files`, which
    // parks on the approve card (the fixture profile leaves
    // `read_files` unset → Ask). The input line is ~30 columns, so
    // the probe keys on what stays visible.
    app.type_text("read remote-marker.txt");
    app.wait_for_screen("the task on the input line", |app| {
        app.screen_text().contains("remote-marker.txt") // sentinel-ok: gates the typed task's render in the ~30-col input — the echo IS the wait
    });
    app.press("Enter");
    app.wait_for_screen("the parked read_files approve card", |app| {
        app.screen_text().contains("approve read_files")
    });
    println!(
        "--- beat 4: read_files approve card, session still remote ---\n{}",
        app.screen_text()
    );

    // Beat 5: approved — the remote tier serves the read through the
    // ControlMaster socket; the file's bytes land in the result card
    // and the loop ends `done`.
    app.type_text("a");
    app.wait_for_screen("the remote read's result + done", |app| {
        app.screen_text().contains("✓ read_files") && panel_has_line(app, "done")
    });
    println!(
        "--- beat 5: agent read the remote file; task done ---\n{}",
        app.screen_text()
    );

    // The bytes' proof: the second request the adapter sent carries
    // the tool result — the marker that exists only on the remote
    // side — back into the conversation.
    let first = fixture.captured();
    assert_post(&first);
    let second = fixture.captured();
    assert_post(&second);
    assert!(
        second.body.contains("R09-REMOTE-FILE-OK"), // sentinel-ok: a fixture-body assert, not a screen probe
        "the remote read's bytes should round-trip in the turn-2 body: {}",
        second.body
    );
    println!(
        "--- turn-2 request body carries the remote file's bytes ---\n{}",
        second.body
    );

    // Beat 6: `Alt+A` on the focused panel closes it (the documented
    // three-state cycle) — a deterministic return of keystrokes to
    // the PTY that `Esc`-unfocus can't give the very next bytes —
    // then `exit` reaches the remote shell: remote `exit_shell` +
    // `ssh_end` close the context.
    app.press("Alt+A");
    app.wait_for_screen("the panel closed, terminal full-width", |app| {
        !app.screen_text().contains('│')
    });
    app.type_text("exit\n");
    app.wait_for_screen("the local session back", |app| {
        !app.line(STATUS_ROW).contains("ssh:")
    });
    println!(
        "--- beat 6: exit unwound the remote context ---\n{}",
        app.screen_text()
    );

    app.type_text("echo R09LOCAL\n");
    app.wait_for_screen("local echo after the return", |app| {
        app.screen_text()
            .lines()
            .any(|line| terminal_half(line).trim() == "R09LOCAL")
    });
    println!("--- beat 7: local blocks resume ---\n{}", app.screen_text());
}
