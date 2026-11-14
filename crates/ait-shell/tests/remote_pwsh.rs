//! The pwsh `ssh` function's scripted tests (Spec R16-T5, §5.9/§5.14):
//! a real `pwsh`/`powershell` spawned through the materialized
//! injection, armed with the `AIT_SSH_*` env, and a `ssh.cmd` stub
//! ahead of OpenSSH's `ssh.exe` on PATH recording every invocation's
//! argv. The Windows leg mirrors `remote.rs`'s unix legs minus the
//! multiplex surface — Windows OpenSSH cannot multiplex, so the
//! wrapper emits no `Control*` options and never spawns an `ssh -G`
//! probe — and minus remote-side exec: the bootstrap is POSIX sh for
//! the far side and is only logged here; its execution is the unix
//! legs' proof. Windows-only (`#![cfg(windows)]`): the hosts and the
//! PATHEXT dispatch exist nowhere else — the windows-latest CI leg
//! is the verifier.

#![cfg(windows)]

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ait_shell::{policy_env, SshPolicy};

/// A scratch dir unique per test process.
fn scratch_base(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ait-ssh-ps-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// The fake `ssh`: a `ssh.cmd` batch recording every invocation's raw
/// argv line — `%*` expands once, so the remote command's quotes,
/// redirects and newlines land as literal text without a second parse.
/// PowerShell's command resolution (`Get-Command -CommandType
/// Application` inside the wrapper) finds it through PATHEXT ahead of
/// System32's real `ssh.exe`, exactly where the stub dir sits on PATH.
/// `-G` answers a resolved config for completeness, though the
/// Windows wrapper never probes; anything else just echoes a marker —
/// the remote command is logged, never executed.
const SSH_STUB_CMD: &str = r#"@echo off
>>"%AIT_SSH_STUB_ARGV%" echo ssh-stub: %*
if "%~1"=="-G" (
    echo hostname stub
    if "%AIT_SSH_STUB_MASTER%"=="" (echo controlmaster false) else (echo controlmaster %AIT_SSH_STUB_MASTER%)
    if "%AIT_SSH_STUB_CP%"=="" (echo controlpath none) else (echo controlpath %AIT_SSH_STUB_CP%)
    exit /b 0
)
echo stub-ssh-interactive
"#;

/// Writes the stub into `base/bin` and returns the argv log path.
fn install_stub(base: &Path) -> PathBuf {
    let bin = base.join("bin");
    let log = base.join("ssh.argv");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("ssh.cmd"), SSH_STUB_CMD).unwrap();
    let _ = std::fs::remove_file(&log);
    log
}

/// Every `7700` hook payload decoded to its JSON text, in order —
/// the stream the frontend's fold reads (same decoder as remote.rs).
fn emitted_payloads(output: &str) -> Vec<String> {
    let mut payloads = Vec::new();
    let mut rest = output;
    while let Some(start) = rest.find("\x1b]7700;") {
        rest = &rest[start + "\x1b]7700;".len()..];
        let end = rest.find(['\x07', '\x1b']).unwrap_or(rest.len());
        let hex = &rest[..end];
        rest = &rest[end..];
        let bytes: Vec<u8> = hex
            .as_bytes()
            .chunks(2)
            .filter_map(|pair| {
                std::str::from_utf8(pair)
                    .ok()
                    .and_then(|s| u8::from_str_radix(s, 16).ok())
            })
            .collect();
        let json = String::from_utf8_lossy(&bytes);
        if json.contains("\"hook\"") {
            payloads.push(json.into_owned());
        }
    }
    payloads
}

/// Extract `"hook":"name"` from the small hook JSON.
fn hook_name(json: &str) -> Option<String> {
    let key = "\"hook\":\"";
    let start = json.find(key)? + key.len();
    let end = json[start..].find('"')? + start;
    Some(json[start..end].to_owned())
}

/// What one scripted scenario produced: the stub's argv record and
/// the decoded hook payloads.
struct Outcome {
    /// Every `ssh-stub:` argv record, split into calls.
    calls: Vec<String>,
    /// The decoded hook JSON texts, in emission order.
    payloads: Vec<String>,
}

impl Outcome {
    /// The payload texts carrying `"hook":"<name>"`.
    fn hooks(&self, name: &str) -> Vec<&String> {
        self.payloads
            .iter()
            .filter(|payload| hook_name(payload).as_deref() == Some(name))
            .collect()
    }
}

/// A shell name runnable via PATH lookup (the spawn probe's own view).
fn on_path(name: &str) -> bool {
    Command::new(name)
        .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// A policy with integration on, no allowlist, one denied glob and a
/// control dir — armed-but-unread on Windows (Spec R16-T5: the env is
/// still exported; the wrapper must simply never act on it).
fn policy_for(control_dir: &Path) -> SshPolicy {
    SshPolicy {
        enabled: true,
        allowlist: vec![],
        denylist: vec!["*.denied".to_owned()],
        reuse_control_master: true,
        control_dir: Some(control_dir.to_path_buf()),
        control_persist: "10m".to_owned(),
        session_id: "7".to_owned(),
    }
}

/// The shared wrapper scenario: spawn `shell` through its materialized
/// injection armed with `policy` + `stub_env`, feed `commands`, and
/// return the stub's argv record plus the decoded `7700` payloads.
fn run_scenario(
    shell: &str,
    tag: &str,
    commands: &str,
    policy: &SshPolicy,
    stub_env: &[(&str, &str)],
) -> Option<Outcome> {
    let base = scratch_base(tag);
    let argv_log = install_stub(&base);
    let injection = ait_shell::plan(Path::new(shell))?;
    let materialized = injection.materialize_in(&base.join("inj")).ok()?;

    // The stub dir wins PATH order over System32's OpenSSH.
    let mut dirs = vec![base.join("bin")];
    dirs.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(dirs).ok()?;

    let mut env: Vec<(OsString, OsString)> = materialized.env().to_vec();
    env.extend(
        policy_env(policy)
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
    );
    env.push((OsString::from("PATH"), path));
    env.push((
        OsString::from("AIT_SSH_STUB_ARGV"),
        argv_log.clone().into_os_string(),
    ));
    env.push((OsString::from("AIT_SESSION_ID"), OsString::from("7")));
    for (key, value) in stub_env {
        env.push((OsString::from(key), OsString::from(value)));
    }

    // The materialized argv already carries
    // `-NoExit -ExecutionPolicy Bypass -Command ". '<dir>\snippet.ps1'"`;
    // commands pipe in on stdin like the inject.rs smoke leg.
    let mut child = Command::new(shell)
        .args(materialized.args())
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(commands.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let mut bytes = output.stdout;
    bytes.extend_from_slice(&output.stderr);
    let text = String::from_utf8_lossy(&bytes);

    let argv = std::fs::read_to_string(&argv_log).unwrap_or_default();
    let calls = argv
        .split("ssh-stub:")
        .filter(|call| !call.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let payloads = emitted_payloads(&text);
    let _ = std::fs::remove_dir_all(&base);
    Some(Outcome { calls, payloads })
}

/// A logged call's local argv — the remote command (which starts
/// `export AIT_SSH_HOP=`) is stripped so `Control`-free assertions
/// never read the POSIX bootstrap text it legitimately carries.
fn local_argv(call: &str) -> &str {
    call.split("export AIT_SSH_HOP").next().unwrap_or(call)
}

/// Spec R16-T5, both hosts: an allowed interactive `ssh` is rewritten
/// to `ssh -t <args> <remote bootstrap>`, bracketed by
/// `ssh_begin`/`ssh_end` — with no `Control*` options and no `-G`
/// probe. Denied hosts, remote commands and `AIT_SSH_OFF` all pass
/// through plain.
#[test]
fn pwsh_ssh_wrapper_wraps_allowed_and_passthroughs_the_rest() {
    for shell in ["pwsh", "powershell"] {
        if !on_path(shell) {
            continue; // host absent — the CI matrix supplies both
        }
        let tag = format!("{shell}-wrap");
        let control_dir = scratch_base(&tag).join("ctl");
        std::fs::create_dir_all(&control_dir).unwrap();
        let policy = policy_for(&control_dir);
        let Some(outcome) = run_scenario(
            shell,
            &tag,
            "ssh testhost\n\
             ssh host.denied\n\
             ssh testhost Write-Output REMOTE\n\
             $env:AIT_SSH_OFF='1'\n\
             ssh off.host\n\
             Remove-Item Env:\\AIT_SSH_OFF\n\
             exit\n",
            &policy,
            &[],
        ) else {
            continue;
        };
        let calls = &outcome.calls;

        // The wrap: `-t`, the destination, then the remote bootstrap
        // command — and no `Control*` option in OUR argv (the remote
        // command legitimately carries AIT_SSH_* exports).
        let wrapped = calls
            .iter()
            .find(|call| local_argv(call).contains("-t") && call.contains("testhost"))
            .unwrap_or_else(|| panic!("{shell}: no wrapped ssh call in {calls:?}"));
        let argv = local_argv(wrapped);
        assert!(argv.contains("-t"), "{shell}: {wrapped}");
        assert!(argv.contains("testhost"), "{shell}: {wrapped}");
        assert!(
            !argv.contains("Control"),
            "{shell}: no Control* options on Windows: {argv}"
        );
        assert!(
            wrapped.contains("export AIT_SSH_BOOTSTRAP="),
            "{shell}: the remote command re-exports the bootstrap: {wrapped}"
        );

        // No `ssh -G` probe was ever spawned — there is no socket to
        // find on Windows.
        assert!(
            calls.iter().all(|call| !call.contains("-G")),
            "{shell}: never probes -G: {calls:?}"
        );

        // The hook order the frontend folds (standard 16): begin →
        // end → re-integration.
        let hooks: Vec<String> = outcome
            .payloads
            .iter()
            .filter_map(|payload| hook_name(payload))
            .collect();
        let begin = hooks.iter().position(|h| h == "ssh_begin");
        let end = hooks.iter().position(|h| h == "ssh_end");
        let boot = hooks.iter().rposition(|h| h == "bootstrapped");
        assert!(
            begin.is_some() && end.is_some() && boot.is_some(),
            "{shell}: hook order {hooks:?}"
        );
        assert!(begin.unwrap() < end.unwrap() && end.unwrap() < boot.unwrap());

        // Passthroughs: the denied host, `ssh host cmd` and
        // `AIT_SSH_OFF='1'` produced no second ssh_begin, reached the
        // stub plain and carried no `-t`.
        assert_eq!(
            outcome.hooks("ssh_begin").len(),
            1,
            "{shell}: passthroughs must not wrap: {hooks:?} / {calls:?}"
        );
        let plain: Vec<&String> = calls
            .iter()
            .filter(|call| !local_argv(call).contains("-t"))
            .collect();
        assert!(
            plain.iter().any(|l| l.contains("host.denied")),
            "{shell}: denied passthrough in {calls:?}"
        );
        assert!(
            plain.iter().any(|l| l.contains("REMOTE")),
            "{shell}: remote-command passthrough in {calls:?}"
        );
        assert!(
            plain.iter().any(|l| l.contains("off.host")),
            "{shell}: AIT_SSH_OFF passthrough in {calls:?}"
        );
        assert!(
            plain.iter().all(|l| !l.contains("Control")),
            "{shell}: passthroughs carry no Control* options: {calls:?}"
        );
    }
}

/// Spec R16-T5 us-27 parity: the wrapper's `ssh_begin`/`ssh_end`
/// attribute to the same minted `session.pid.seq` hop, `ssh_begin`
/// names the typed target, reports `bootstrapped` and never carries
/// a `control_path`, and the remote command re-exports that hop id.
#[test]
fn a_wrapped_pwsh_session_attributes_hooks_to_its_hop() {
    for shell in ["pwsh", "powershell"] {
        if !on_path(shell) {
            continue;
        }
        let tag = format!("{shell}-hop");
        let control_dir = scratch_base(&tag).join("ctl");
        std::fs::create_dir_all(&control_dir).unwrap();
        let policy = policy_for(&control_dir);
        let Some(outcome) = run_scenario(shell, &tag, "ssh testhost\nexit\n", &policy, &[]) else {
            continue;
        };
        let begin = &outcome.hooks("ssh_begin")[0];
        let hop = begin
            .split("\"hop\":\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("ssh_begin carries a hop id");
        assert!(
            hop.starts_with("7."),
            "{shell}: hop is session.pid.seq — got {hop:?}"
        );
        assert!(
            begin.contains("\"target\":\"testhost\""),
            "{shell}: ssh_begin names the target: {begin}"
        );
        assert!(
            begin.contains("\"bootstrapped\":true"),
            "{shell}: ssh_begin reports the bootstrap: {begin}"
        );
        assert!(
            !begin.contains("control_path"),
            "{shell}: Windows never reports a control_path: {begin}"
        );
        for payload in outcome.hooks("ssh_end") {
            assert!(
                payload.contains(&format!("\"hop\":\"{hop}\"")),
                "{shell}: ssh_end carries the same hop: {payload}"
            );
        }
        // The remote command exports this hop id and re-exports the
        // bootstrap for a nested hop.
        let wrapped = outcome
            .calls
            .iter()
            .find(|call| local_argv(call).contains("-t"))
            .expect("the wrapped call");
        assert!(
            wrapped.contains(&format!("export AIT_SSH_HOP='{hop}'")),
            "{shell}: the remote env carries the hop: {wrapped}"
        );
    }
}

/// Spec R16-T5 (`[ssh]` parity): the allowlist narrows the wrap —
/// only matching destinations integrate; the rest pass through plain.
#[test]
fn the_pwsh_allowlist_narrows_the_wrap() {
    for shell in ["pwsh", "powershell"] {
        if !on_path(shell) {
            continue;
        }
        let tag = format!("{shell}-allow");
        let control_dir = scratch_base(&tag).join("ctl");
        std::fs::create_dir_all(&control_dir).unwrap();
        let policy = SshPolicy {
            allowlist: vec!["*.prod".to_owned()],
            ..policy_for(&control_dir)
        };
        let Some(outcome) = run_scenario(
            shell,
            &tag,
            "ssh a.prod\nssh testhost\nexit\n",
            &policy,
            &[],
        ) else {
            continue;
        };
        assert_eq!(
            outcome.hooks("ssh_begin").len(),
            1,
            "{shell}: only the allowlisted host wraps: {:?}",
            outcome.payloads
        );
        let plain = outcome
            .calls
            .iter()
            .find(|call| call.contains("testhost") && !local_argv(call).contains("-t"))
            .expect("the unlisted host passed through plain");
        assert!(!local_argv(plain).contains("Control"), "{plain}");
        assert!(outcome.hooks("ssh_begin")[0].contains("\"target\":\"a.prod\""));
    }
}

/// `AIT_SSH_OFF` is value-sensitive: set-but-empty does NOT opt out
/// (POSIX `-n` semantics) — while a non-empty value like `'0'` does.
#[test]
fn pwsh_ait_ssh_off_only_opts_out_when_non_empty() {
    for shell in ["pwsh", "powershell"] {
        if !on_path(shell) {
            continue;
        }
        let tag = format!("{shell}-off");
        let control_dir = scratch_base(&tag).join("ctl");
        std::fs::create_dir_all(&control_dir).unwrap();
        let policy = policy_for(&control_dir);
        let Some(outcome) = run_scenario(
            shell,
            &tag,
            "$env:AIT_SSH_OFF=''\n\
             ssh testhost\n\
             $env:AIT_SSH_OFF='0'\n\
             ssh still.off\n\
             Remove-Item Env:\\AIT_SSH_OFF\n\
             exit\n",
            &policy,
            &[],
        ) else {
            continue;
        };
        assert_eq!(
            outcome.hooks("ssh_begin").len(),
            1,
            "{shell}: an empty AIT_SSH_OFF must not opt out, '0' must: {:?}",
            outcome.payloads
        );
        assert!(
            outcome
                .calls
                .iter()
                .any(|call| call.contains("still.off") && !local_argv(call).contains("-t")),
            "{shell}: the '0' leg passed through plain: {:?}",
            outcome.calls
        );
    }
}

/// `ssh host -v` — a flag AFTER the destination — is remote-command
/// argv, not an interactive login: the dest parser must passthrough.
#[test]
fn pwsh_flags_after_the_destination_pass_through() {
    for shell in ["pwsh", "powershell"] {
        if !on_path(shell) {
            continue;
        }
        let tag = format!("{shell}-flags");
        let control_dir = scratch_base(&tag).join("ctl");
        std::fs::create_dir_all(&control_dir).unwrap();
        let policy = policy_for(&control_dir);
        let Some(outcome) = run_scenario(shell, &tag, "ssh testhost -v\nexit\n", &policy, &[])
        else {
            continue;
        };
        assert_eq!(
            outcome.hooks("ssh_begin").len(),
            0,
            "{shell}: `ssh host -v` is remote-command argv: {:?}",
            outcome.payloads
        );
        assert!(
            outcome
                .calls
                .iter()
                .any(|call| call.contains("testhost") && call.contains("-v")),
            "{shell}: the plain call reached the stub: {:?}",
            outcome.calls
        );
    }
}
