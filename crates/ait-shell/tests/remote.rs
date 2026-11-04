//! The `ssh()` wrapper's scripted tests (Spec R09-T2/T3): a real
//! bash/zsh/fish spawned through the materialized injection, armed
//! with the `AIT_SSH_*` env, and a fake `ssh` on PATH that records
//! its argv, answers `-G` and executes the remote command locally.
//! No network, no real sshd — the wrapper's policy decisions, hook
//! ordering and argv rewrite are all observable through the stub and
//! the OSC 7700 stream. Each family's leg gates on the shell being
//! installed; bash is the required one (Spec R09's test decision).

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
/// `ssh -G …` answers a resolved config (honoring our `-o
/// ControlPath=` adds); anything else treats the first positional
/// token as the destination and everything after it as the remote
/// command, which it runs through POSIX `sh` — the remote bootstrap
/// executes "locally" this way.
const SSH_STUB: &str = r#"#!/bin/sh
{
    printf 'ssh-stub:'
    for a in "$@"; do printf ' <%s>' "$a"; done
    printf '\n'
} >>"$AIT_SSH_STUB_ARGV"
if [ "$1" = "-G" ]; then
    cp=none
    for a in "$@"; do
        case $a in ControlPath=*) cp=${a#ControlPath=};; esac
    done
    printf 'hostname stub\ncontrolmaster false\ncontrolpath %s\n' "$cp"
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

/// Every `{"hook":"…"}` name emitted on the `7700` channel, in order.
fn emitted_hooks(output: &str) -> Vec<String> {
    let mut names = Vec::new();
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
        if let Some(value) = serde_json_lite(&json) {
            names.push(value);
        }
    }
    names
}

/// Extract `"hook":"name"` from the small hook JSON — no serde
/// dependency in this test target, so a scan it is.
fn serde_json_lite(json: &str) -> Option<String> {
    let key = "\"hook\":\"";
    let start = json.find(key)? + key.len();
    let end = json[start..].find('"')? + start;
    Some(json[start..end].to_owned())
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
    run_wrapper_case(
        "/bin/bash",
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
    if !std::path::Path::new("/usr/bin/zsh").exists() && !std::path::Path::new("/bin/zsh").exists()
    {
        return; // zsh isn't installed everywhere tests run
    }
    let shell = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
        .unwrap();
    run_wrapper_case(
        shell,
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
    if !std::path::Path::new("/usr/bin/fish").exists()
        && !std::path::Path::new("/bin/fish").exists()
    {
        return; // fish isn't installed everywhere tests run
    }
    let shell = ["/bin/fish", "/usr/bin/fish"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
        .unwrap();
    run_wrapper_case(
        shell,
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

/// The shared wrapper scenario: spawn `shell` through its
/// materialized injection, feed `commands`, and assert the argv the
/// stub recorded plus the hook sequence on the `7700` stream.
fn run_wrapper_case(shell: &str, tag: &str, commands: &str) {
    let shell_path = std::path::Path::new(shell);
    if !shell_path.exists() {
        return; // the shell isn't installed here — e2e's job
    }
    let base = scratch_base(tag);
    let argv_log = install_stub(&base);
    let control_dir = base.join("ctl");
    std::fs::create_dir_all(&control_dir).unwrap();
    let Some(injection) = ait_shell::plan(shell_path) else {
        unreachable!()
    };
    let Ok(materialized) = injection.materialize_in(&base.join("inj")) else {
        return;
    };
    let mut args = materialized.args().to_vec();
    args.push(OsString::from("-i"));

    let policy = SshPolicy {
        enabled: true,
        allowlist: vec![],
        denylist: vec!["*.denied".to_owned()],
        reuse_control_master: true,
        control_dir: control_dir.clone(),
        control_persist: "10m".to_owned(),
        session_id: "7".to_owned(),
    };
    let path = std::env::var_os("PATH").unwrap_or_default();
    // The materialized env first — zsh's ZDOTDIR forwarder, fish's
    // feature flag, the empty AIT_SESSION_ID slot — then the test's
    // own overrides.
    let mut env: Vec<(OsString, OsString)> = materialized.env().to_vec();
    env.extend(
        policy_env(&policy)
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
    // The remote bootstrap's `$SHELL` exec: /bin/true accepts the
    // family flags and exits — the remote cleanup trap still fires
    // (exit_shell + rm), which is what the hook assertions observe.
    env.push((OsString::from("SHELL"), OsString::from("/bin/true")));

    let mut child = match Command::new(shell)
        .args(&args)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return, // no shell on this host — e2e's job
    };
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

    // One record per invocation — the remote-command arg is a
    // multi-line script, so the file can't be read line-wise.
    let calls: Vec<&str> = argv
        .split("ssh-stub:")
        .filter(|call| !call.trim().is_empty())
        .collect();

    // The wrap: `-t`, our ControlMaster options, the destination and
    // the remote bootstrap command as the last argv.
    let wrapped = calls
        .iter()
        .find(|call| call.contains("<-t>") && call.contains("testhost"))
        .unwrap_or_else(|| panic!("no wrapped ssh call in {argv}"));
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
    let hooks = emitted_hooks(&text);
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
        hooks.iter().filter(|h| *h == "ssh_begin").count(),
        1,
        "passthroughs must not wrap: {hooks:?} / {argv}"
    );
    let plain: Vec<&&str> = calls
        .iter()
        .filter(|call| !call.contains("<-G>") && !call.contains("<-t>"))
        .collect();
    assert!(
        plain.iter().any(|l| l.contains("<host.denied>")),
        "denied passthrough in {argv}"
    );
    assert!(
        plain
            .iter()
            .any(|l| l.contains("REMOTE") || l.contains("<echo>")),
        "remote-command passthrough in {argv}"
    );
    assert!(
        plain.iter().all(|l| !l.contains("ControlMaster=")),
        "passthroughs carry no ait options: {argv}"
    );
    let _ = std::fs::remove_dir_all(&base);
}
