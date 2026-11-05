//! The `ssh()` wrapper's scripted tests (Spec R09-T2/T3): a real
//! bash/zsh/fish spawned through the materialized injection, armed
//! with the `AIT_SSH_*` env, and a fake `ssh` on PATH that records
//! its argv, answers `-G` and executes the remote command locally.
//! No network, no real sshd — the wrapper's policy decisions, hook
//! ordering, hop attribution and argv rewrite are all observable
//! through the stub and the OSC 7700 stream. Each family's leg
//! gates on the shell being installed; bash is the required one
//! (Spec R09's test decision).

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use ait_shell::{policy_env, SshPolicy};

/// A scratch dir unique per test process.
fn scratch_base(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ait-ssh-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    base
}

/// The fake `ssh`: records every invocation's argv one line per call;
/// `ssh -G …` answers a resolved config — `AIT_SSH_STUB_MASTER` /
/// `AIT_SSH_STUB_CP` steer the controlmaster/controlpath lines so a
/// test can pose a user-managed socket, while our `-o ControlPath=`
/// adds still win over the stub default; anything else treats the
/// first positional token as the destination and everything after it
/// as the remote command, which it runs through POSIX `sh` — the
/// remote bootstrap executes "locally" this way.
const SSH_STUB: &str = r#"#!/bin/sh
{
    printf 'ssh-stub:'
    for a in "$@"; do printf ' <%s>' "$a"; done
    printf '\n'
} >>"$AIT_SSH_STUB_ARGV"
if [ "$1" = "-G" ]; then
    cp=${AIT_SSH_STUB_CP:-none}
    for a in "$@"; do
        case $a in ControlPath=*) cp=${a#ControlPath=};; esac
    done
    printf 'hostname stub\ncontrolmaster %s\ncontrolpath %s\n' "${AIT_SSH_STUB_MASTER:-false}" "$cp"
    exit 0
fi
dest="" cmd="" skip=0
for a in "$@"; do
    if [ $skip = 1 ]; then skip=0; continue; fi
    case $a in
        -o|-F|-p|-l|-J|-b|-c|-D|-E|-e|-I|-i|-L|-m|-O|-Q|-R|-S|-W|-w|-B) skip=1;;
        -*) ;;
        *) if [ -z "$dest" ]; then dest=$a; else cmd="$cmd $a"; fi;;
    esac
done
cmd=${cmd# }
if [ -n "$cmd" ]; then
    exec sh -c "$cmd"
fi
printf 'stub-ssh-interactive\n'
"#;

/// The `(dir, argv-log)` pair the test installs the stub into.
fn install_stub(base: &std::path::Path) -> PathBuf {
    let bin = base.join("bin");
    let log = base.join("ssh.argv");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("ssh"), SSH_STUB).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(bin.join("ssh")).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(bin.join("ssh"), permissions).unwrap();
    }
    let _ = std::fs::remove_file(&log);
    log
}

/// Every `7700` hook payload decoded to its JSON text, in order —
/// the stream the frontend's fold reads.
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

/// Extract `"hook":"name"` from the small hook JSON — no serde
/// dependency in this test target, so a scan it is.
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

/// The shared wrapper scenario: spawn `shell` through its
/// materialized injection armed with `policy` + `stub_env`, feed
/// `commands`, and return the stub's argv record plus the decoded
/// `7700` payloads. `None` when the shell or materializer isn't
/// available on this host — e2e's job then.
fn run_scenario(
    shell: &str,
    tag: &str,
    commands: &str,
    policy: &SshPolicy,
    stub_env: &[(&str, &str)],
) -> Option<Outcome> {
    let shell_path = std::path::Path::new(shell);
    if !shell_path.exists() {
        return None; // the shell isn't installed here — e2e's job
    }
    let base = scratch_base(tag);
    let argv_log = install_stub(&base);
    let Some(injection) = ait_shell::plan(shell_path) else {
        unreachable!()
    };
    let Ok(materialized) = injection.materialize_in(&base.join("inj")) else {
        return None;
    };
    let mut args = materialized.args().to_vec();
    args.push(OsString::from("-i"));

    let path = std::env::var_os("PATH").unwrap_or_default();
    // The materialized env first — zsh's ZDOTDIR forwarder, fish's
    // feature flag, the empty AIT_SESSION_ID slot — then the test's
    // own overrides.
    let mut env: Vec<(OsString, OsString)> = materialized.env().to_vec();
    env.extend(
        policy_env(policy)
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
    );
    env.push((
        OsString::from("PATH"),
        OsString::from(format!(
            "{}:{}",
            base.join("bin").display(),
            path.to_string_lossy()
        )),
    ));
    env.push((OsString::from("AIT_SSH_STUB_ARGV"), argv_log.clone().into()));
    env.push((OsString::from("AIT_SESSION_ID"), OsString::from("7")));
    env.push((OsString::from("TERM"), OsString::from("xterm")));
    // The remote bootstrap's shell dispatch: /bin/true accepts the
    // family flags and exits — the remote cleanup trap still fires
    // (exit_shell + rm), which is what the hook assertions observe.
    env.push((OsString::from("SHELL"), OsString::from("/bin/true")));
    for (key, value) in stub_env {
        env.push((OsString::from(key), OsString::from(value)));
    }

    let mut child = Command::new(shell)
        .args(&args)
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
    let _ = materialized;

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

/// A policy with integration on, no allowlist, one denied glob and
/// this test's own control dir.
fn policy_for(control_dir: &std::path::Path) -> SshPolicy {
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

/// A shell that exists on this host, or `None` to skip the leg.
fn installed(names: &[&'static str]) -> Option<&'static str> {
    names
        .iter()
        .copied()
        .find(|name| std::path::Path::new(name).exists())
}

/// Spec R09-T2's bash leg, end to end: an allowed interactive `ssh`
/// is rewritten to `ssh -t -o ControlMaster=… … host <bootstrap>`,
/// bracketed by `ssh_begin`/`ssh_end`, with the remote bootstrap
/// materializing and integrating the remote shell (its `init_shell`
/// hook lands through the stub's own stdout — the real session's
/// would arrive through the SSH channel). Denied hosts, remote
/// commands, `AIT_SSH_OFF` and `command ssh` all pass through plain.
#[test]
fn bash_ssh_wrapper_wraps_allowed_and_passthroughs_the_rest() {
    wrapper_case(
        installed(&["/bin/bash", "/usr/bin/bash"]),
        "bash-ssh",
        "ssh testhost\n\
         ssh host.denied\n\
         ssh testhost echo REMOTE\n\
         AIT_SSH_OFF=1 ssh testhost\n\
         command ssh testhost\n\
         exit\n",
    );
}

/// Spec R09-T3's zsh leg — the same wrapper contract through the
/// snippet's zsh-family parser (`${~pattern}` globs on the argv).
#[test]
fn zsh_ssh_wrapper_wraps_allowed_and_passthroughs_the_rest() {
    wrapper_case(
        installed(&["/bin/zsh", "/usr/bin/zsh"]),
        "zsh-ssh",
        "ssh testhost\n\
         ssh host.denied\n\
         ssh testhost echo REMOTE\n\
         AIT_SSH_OFF=1 ssh testhost\n\
         command ssh testhost\n\
         exit\n",
    );
}

/// Spec R09-T3's fish leg — fish has no `VAR=1 cmd` prefix, so the
/// per-session opt-out rides `set -lx`/`set -e` instead.
#[test]
fn fish_ssh_wrapper_wraps_allowed_and_passthroughs_the_rest() {
    wrapper_case(
        installed(&["/bin/fish", "/usr/bin/fish"]),
        "fish-ssh",
        "ssh testhost\n\
         ssh host.denied\n\
         ssh testhost echo REMOTE\n\
         set -lx AIT_SSH_OFF 1\n\
         ssh testhost\n\
         set -e AIT_SSH_OFF\n\
         command ssh testhost\n\
         exit\n",
    );
}

/// The shared wrap/passthrough assertions every family answers to:
/// one wrapped call carrying the ait options and the bootstrap, the
/// hook order begin → remote lifecycle → end → re-integration, and
/// every passthrough shape reaching the stub plain.
fn wrapper_case(shell: Option<&str>, tag: &str, commands: &str) {
    let Some(shell) = shell else { return };
    let control_dir = scratch_base(tag).join("ctl");
    std::fs::create_dir_all(&control_dir).unwrap();
    let policy = policy_for(&control_dir);
    let Some(outcome) = run_scenario(shell, tag, commands, &policy, &[]) else {
        return;
    };
    let calls = &outcome.calls;

    // The wrap: `-t`, our ControlMaster options, the destination and
    // the remote bootstrap command as the last argv.
    let wrapped = calls
        .iter()
        .find(|call| call.contains("<-t>") && call.contains("testhost"))
        .unwrap_or_else(|| panic!("no wrapped ssh call in {calls:?}"));
    assert!(wrapped.contains("ControlMaster=auto"), "{wrapped}");
    let ctl = format!("ControlPath={}/%C", control_dir.display());
    assert!(wrapped.contains(&ctl), "{wrapped}");
    assert!(wrapped.contains("ControlPersist=10m"), "{wrapped}");
    assert!(
        wrapped.contains("export AIT_SSH_BOOTSTRAP="),
        "the remote command is the bootstrap: {wrapped}"
    );

    // The hook order the frontend folds (standard 16): begin → the
    // remote shell's own lifecycle (init…exit through the stub's
    // local exec) → end → re-integration.
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
        "hook order {hooks:?}"
    );
    assert!(begin.unwrap() < end.unwrap() && end.unwrap() < boot.unwrap());

    // Passthroughs: the denied host, `ssh host cmd`, `AIT_SSH_OFF`
    // and `command ssh` never produced a second ssh_begin, and the
    // stub saw them plain (no -t, no ControlMaster).
    assert_eq!(
        outcome.hooks("ssh_begin").len(),
        1,
        "passthroughs must not wrap: {hooks:?} / {calls:?}"
    );
    let plain: Vec<&String> = calls
        .iter()
        .filter(|call| !call.contains("<-G>") && !call.contains("<-t>"))
        .collect();
    assert!(
        plain.iter().any(|l| l.contains("<host.denied>")),
        "denied passthrough in {calls:?}"
    );
    assert!(
        plain
            .iter()
            .any(|l| l.contains("REMOTE") || l.contains("<echo>")),
        "remote-command passthrough in {calls:?}"
    );
    assert!(
        plain.iter().all(|l| !l.contains("ControlMaster=")),
        "passthroughs carry no ait options: {calls:?}"
    );
}

/// Spec R09 us-27: the wrapped session attributes itself — the
/// wrapper's `ssh_begin`/`ssh_end` and the remote shell's own
/// `exit_shell` all carry the same hop id the remote command
/// exported.
#[test]
fn a_wrapped_session_attributes_every_hook_to_its_hop() {
    let Some(shell) = installed(&["/bin/bash", "/usr/bin/bash"]) else {
        return;
    };
    let control_dir = scratch_base("hop").join("ctl");
    std::fs::create_dir_all(&control_dir).unwrap();
    let policy = policy_for(&control_dir);
    let Some(outcome) = run_scenario(shell, "hop", "ssh testhost\nexit\n", &policy, &[]) else {
        return;
    };
    let begin = outcome.hooks("ssh_begin");
    let hop = begin
        .first()
        .and_then(|payload| payload.split("\"hop\":\"").nth(1))
        .and_then(|rest| rest.split('"').next())
        .expect("ssh_begin carries a hop id");
    assert!(
        hop.starts_with("7."),
        "hop is session.pid.seq — got {hop:?}"
    );
    for payload in outcome.hooks("ssh_end") {
        assert!(
            payload.contains(&format!("\"hop\":\"{hop}\"")),
            "ssh_end carries the same hop: {payload}"
        );
    }
    // The remote exit_shell — emitted by the bootstrap the stub ran
    // locally — attributes to the same hop.
    let remote_exit = outcome
        .hooks("exit_shell")
        .into_iter()
        .find(|payload| payload.contains("\"hop\""))
        .expect("the remote exit_shell carries a hop");
    assert!(
        remote_exit.contains(&format!("\"hop\":\"{hop}\"")),
        "remote exit_shell hop: {remote_exit}"
    );
    // A local hook (the local shell's own precmd/init) carries no hop.
    assert!(
        outcome
            .hooks("init_shell")
            .iter()
            .all(|payload| !payload.contains("\"hop\"")),
        "local hooks carry no hop field"
    );
}

/// Spec R09-T3: the user's own `controlmaster`/`controlpath` config
/// suppresses our attach entirely — `reuse_existing_control_master=1`
/// adds no `-o` options, the reported path is the USER's socket, and
/// the single `ssh -G` probe is all the session ever spawns (no
/// redundant second resolution).
#[test]
fn a_user_managed_controlmaster_suppresses_the_attach() {
    let Some(shell) = installed(&["/bin/bash", "/usr/bin/bash"]) else {
        return;
    };
    let control_dir = scratch_base("reuse").join("ctl");
    std::fs::create_dir_all(&control_dir).unwrap();
    let policy = policy_for(&control_dir);
    let Some(outcome) = run_scenario(
        shell,
        "reuse",
        "ssh testhost\nexit\n",
        &policy,
        &[
            ("AIT_SSH_STUB_MASTER", "auto"),
            ("AIT_SSH_STUB_CP", "/tmp/user.sock"),
        ],
    ) else {
        return;
    };
    let wrapped = outcome
        .calls
        .iter()
        .find(|call| call.contains("<-t>"))
        .expect("the call still wraps (bootstrap + -t)");
    // Only the argv BEFORE the remote command is ours — the remote
    // command itself embeds the snippet set, `ControlMaster` strings
    // and all.
    let wrapped_argv = wrapped.split("<export").next().unwrap_or(wrapped);
    assert!(
        !wrapped_argv.contains("ControlMaster=auto"),
        "the user's socket is reused — no ait attach: {wrapped_argv}"
    );
    assert!(!wrapped_argv.contains("ControlPath="), "{wrapped_argv}");
    let begin = &outcome.hooks("ssh_begin")[0];
    assert!(
        begin.contains("\"control_path\":\"/tmp/user.sock\""),
        "the reported path is the user's socket: {begin}"
    );
    // Reuse answers from ONE `ssh -G`: the config probe already told
    // the wrapper everything a resolve would.
    let probes = outcome
        .calls
        .iter()
        .filter(|call| call.contains("<-G>"))
        .count();
    assert_eq!(probes, 1, "one -G per wrapped session: {:?}", outcome.calls);
}

/// Spec R09-T1: the allowlist narrows the wrap — only matching
/// destinations integrate; everything else passes through plain.
#[test]
fn the_allowlist_narrows_the_wrap() {
    let Some(shell) = installed(&["/bin/bash", "/usr/bin/bash"]) else {
        return;
    };
    let control_dir = scratch_base("allow").join("ctl");
    std::fs::create_dir_all(&control_dir).unwrap();
    let policy = SshPolicy {
        allowlist: vec!["*.prod".to_owned()],
        ..policy_for(&control_dir)
    };
    let Some(outcome) = run_scenario(
        shell,
        "allow",
        "ssh a.prod\nssh testhost\nexit\n",
        &policy,
        &[],
    ) else {
        return;
    };
    assert_eq!(
        outcome.hooks("ssh_begin").len(),
        1,
        "only the allowlisted host wraps: {:?}",
        outcome.payloads
    );
    let plain = outcome
        .calls
        .iter()
        .find(|call| call.contains("<testhost>") && !call.contains("<-t>"))
        .expect("the unlisted host passed through plain");
    assert!(!plain.contains("ControlMaster="), "{plain}");
    // The wrapped one named the allowlisted host.
    assert!(outcome.hooks("ssh_begin")[0].contains("\"target\":\"a.prod\""));
}

/// ssh's `controlpath none` is its "no socket" literal — it must
/// never land in `ssh_begin` as a path the executor or teardown
/// would try.
#[test]
fn controlpath_none_never_reads_as_a_socket() {
    let Some(shell) = installed(&["/bin/bash", "/usr/bin/bash"]) else {
        return;
    };
    // No control dir → no attach → the stub's `controlpath none` is
    // all `-G` can say.
    let policy = SshPolicy {
        control_dir: None,
        ..policy_for(&scratch_base("none").join("ctl"))
    };
    let Some(outcome) = run_scenario(shell, "none", "ssh testhost\nexit\n", &policy, &[]) else {
        return;
    };
    let begin = &outcome.hooks("ssh_begin")[0];
    assert!(
        !begin.contains("control_path"),
        "`none` is filtered out of the payload: {begin}"
    );
}

/// `AIT_SSH_OFF` is value-sensitive: set-but-empty does NOT opt out —
/// only a non-empty value passthroughs (the spec's `AIT_SSH_OFF=1`).
#[test]
fn an_empty_ait_ssh_off_still_wraps() {
    let Some(shell) = installed(&["/bin/bash", "/usr/bin/bash"]) else {
        return;
    };
    let control_dir = scratch_base("off").join("ctl");
    std::fs::create_dir_all(&control_dir).unwrap();
    let policy = policy_for(&control_dir);
    let Some(outcome) = run_scenario(
        shell,
        "off",
        "AIT_SSH_OFF= ssh testhost\nexit\n",
        &policy,
        &[],
    ) else {
        return;
    };
    assert_eq!(
        outcome.hooks("ssh_begin").len(),
        1,
        "an empty AIT_SSH_OFF must not opt out: {:?}",
        outcome.payloads
    );
}

/// `ssh host -v` — a flag AFTER the destination — is remote-command
/// argv, not an interactive login: the dest parser must passthrough.
#[test]
fn flags_after_the_destination_pass_through() {
    let Some(shell) = installed(&["/bin/bash", "/usr/bin/bash"]) else {
        return;
    };
    let control_dir = scratch_base("flags").join("ctl");
    std::fs::create_dir_all(&control_dir).unwrap();
    let policy = policy_for(&control_dir);
    let Some(outcome) = run_scenario(shell, "flags", "ssh testhost -v\nexit\n", &policy, &[])
    else {
        return;
    };
    assert_eq!(
        outcome.hooks("ssh_begin").len(),
        0,
        "`ssh host -v` is remote-command argv: {:?}",
        outcome.payloads
    );
    assert!(
        outcome
            .calls
            .iter()
            .any(|call| call.contains("<testhost>") && call.contains("<-v>")),
        "the plain call reached the stub: {:?}",
        outcome.calls
    );
}

/// Spec R09-T3's zsh argv quirk: a bare `"${extra[@]}"` expands an
/// EMPTY array to one empty word in zsh — the wrapped argv must go
/// `<-t> <dest>` directly, no `""` mangling the destination.
#[test]
fn zsh_an_empty_extra_array_never_mangles_the_argv() {
    let Some(shell) = installed(&["/bin/zsh", "/usr/bin/zsh"]) else {
        return;
    };
    let policy = SshPolicy {
        control_dir: None, // no attach → `extra` stays empty
        ..policy_for(&scratch_base("zsh-empty").join("ctl"))
    };
    let Some(outcome) = run_scenario(shell, "zsh-empty", "ssh testhost\nexit\n", &policy, &[])
    else {
        return;
    };
    let wrapped = outcome
        .calls
        .iter()
        .find(|call| call.contains("<-t>"))
        .expect("the call still wraps");
    assert!(
        wrapped.contains("<-t> <testhost>"),
        "no empty word between -t and the destination: {wrapped}"
    );
    assert_eq!(outcome.hooks("ssh_begin").len(), 1);
}

/// Spec R09-T3's fish JSON: a `controlpath` carrying a `"` must land
/// in `ssh_begin` escaped — an unescaped splice would emit invalid
/// JSON the codec then drops silently.
#[test]
fn fish_control_path_json_is_escaped() {
    let Some(shell) = installed(&["/bin/fish", "/usr/bin/fish"]) else {
        return;
    };
    let control_dir = scratch_base("fish-json").join("ctl");
    std::fs::create_dir_all(&control_dir).unwrap();
    let policy = policy_for(&control_dir);
    let Some(outcome) = run_scenario(
        shell,
        "fish-json",
        "ssh testhost\nexit\n",
        &policy,
        &[
            ("AIT_SSH_STUB_MASTER", "auto"),
            ("AIT_SSH_STUB_CP", "we\"ird/sock"),
        ],
    ) else {
        return;
    };
    let begin = &outcome.hooks("ssh_begin")[0];
    assert!(
        begin.contains(r#"we\"ird/sock"#),
        "the path lands JSON-escaped: {begin}"
    );
}
