//! The §5.9 remote-bootstrap renderer and the `AIT_SSH_*` wire
//! vocabulary (Spec R09). The integrated snippets' `ssh()` wrapper
//! rewrites an interactive, policy-allowed `ssh` into
//! `ssh -t <args> '<remote command>'` where the remote command is the
//! self-contained POSIX-sh script [`bootstrap_script`] renders at
//! spawn: it lands the same [`Injection::files`] set in a private
//! `mktemp -d` dir, exports the session env, traps EXIT/HUP so the
//! `exit_shell` hook fires on a clean quit AND a dropped connection,
//! and runs the remote `$SHELL` as a CHILD under the same
//! integration — `exec` would destroy the traps (ADR-0012). No
//! remote dotfile is ever touched.
//!
//! The script carries the session's armed policy verbatim: the env
//! names below are the contract between the spawn-time renderer
//! (ait's `integration::inject`), the local `ssh()` wrapper (which
//! reads them), and the bootstrapped remote shell (whose own `ssh()`
//! reads the re-exported copies for a nested hop — the wrapper
//! re-sends `AIT_SSH_BOOTSTRAP` itself, so the script never has to
//! embed a copy of itself).
//!
//! [`Injection::files`]: crate::Injection::files

use std::ffi::OsStr;
use std::path::PathBuf;

/// Per-session opt-out: `AIT_SSH_OFF=1` makes `ssh()` a plain
/// passthrough regardless of policy (Spec R09, "an explicit
/// `AIT_SSH_OFF=1`").
pub const ENV_SSH_OFF: &str = "AIT_SSH_OFF";

/// The armed `[ssh] enable_integration` decision, `1`/`0`.
pub const ENV_SSH_ENABLED: &str = "AIT_SSH_ENABLED";

/// Newline-separated allowlist globs — OpenSSH `Host`-pattern
/// semantics (`*`, `?`, leading `!` negates) matched against the
/// destination token. Empty means open.
pub const ENV_SSH_ALLOW: &str = "AIT_SSH_ALLOW";

/// Newline-separated denylist globs — deny beats allow.
pub const ENV_SSH_DENY: &str = "AIT_SSH_DENY";

/// Whether to honor the user's own ControlMaster/ControlPath config
/// (`1`/`0`) — the armed `reuse_existing_control_master`.
pub const ENV_SSH_REUSE_CM: &str = "AIT_SSH_REUSE_CM";

/// The ait control dir the wrapper's `ControlPath=…/%C` lands
/// sockets in (`$XDG_RUNTIME_DIR/ait/ssh`, temp-dir fallback).
pub const ENV_SSH_CONTROL_DIR: &str = "AIT_SSH_CONTROL_DIR";

/// The `ControlPersist` value the wrapper attaches (`10m`, …).
pub const ENV_SSH_CONTROL_PERSIST: &str = "AIT_SSH_CONTROL_PERSIST";

/// The rendered remote command `ssh()` sends — and re-exports
/// into the remote session so a nested `ssh` bootstraps the next
/// hop identically.
pub const ENV_SSH_BOOTSTRAP: &str = "AIT_SSH_BOOTSTRAP";

/// One hop's identity in a nested `ssh` chain: the wrapper mints a
/// fresh id per wrapped connect, sends it in `ssh_begin`/`ssh_end`
/// and exports it into the remote session, so the remote shell's own
/// hooks carry it back (`init_shell`, `exit_shell`, …). Every level
/// shares `AIT_SESSION_ID`, so `hop` is what attributes a hook to the
/// level that emitted it — without it a nested `ssh_end` or remote
/// `exit_shell` cannot be told apart from the outer session's.
pub const ENV_SSH_HOP: &str = "AIT_SSH_HOP";

/// The ssh client binary every consumer of this contract spawns —
/// the executor tiers' `ssh -S`/`-O check`/command forms and the
/// frontend's `-O exit` teardown (standard 17).
pub const SSH_PROGRAM: &str = "ssh";

/// The `ControlPersist` value the wrapper attaches when
/// [`ENV_SSH_CONTROL_PERSIST`] is unset — the one duration the spawn
/// side and the snippets must agree on.
pub const DEFAULT_CONTROL_PERSIST: &str = "10m";

/// Marker substituted inside the file set the script writes: the
/// heredocs are quoted (no expansion), so the literal text
/// `$__ait_dir` in each wrapper resolves when the REMOTE shell
/// sources the file.
const REMOTE_DIR: &str = "$__ait_dir";

/// The armed `[ssh]` policy plus the spawn-time values the remote
/// command bakes in — one session's worth (standard 17: the env
/// names above are the single source both sides read).
pub struct SshPolicy {
    /// The armed `enable_integration` (`AIT_SSH_ENABLED`).
    pub enabled: bool,
    /// Allowlist globs, one per line in `AIT_SSH_ALLOW`.
    pub allowlist: Vec<String>,
    /// Denylist globs, one per line in `AIT_SSH_DENY`.
    pub denylist: Vec<String>,
    /// The armed `reuse_existing_control_master` (`AIT_SSH_REUSE_CM`).
    pub reuse_control_master: bool,
    /// The ait control dir (`AIT_SSH_CONTROL_DIR`) — `None` when no
    /// `0700` dir could be offered: the pair is then not emitted at
    /// all rather than as an empty contract, and the wrapper never
    /// attaches a `ControlPath`.
    pub control_dir: Option<PathBuf>,
    /// The `ControlPersist` value (`AIT_SSH_CONTROL_PERSIST`).
    pub control_persist: String,
    /// The session id the remote snippet echoes back (`AIT_SESSION_ID`).
    pub session_id: String,
}

/// The `AIT_SSH_*` environment a spawned session carries (the
/// injection env seam): the armed policy the local `ssh()` reads,
/// plus the rendered [`bootstrap_script`] under
/// [`ENV_SSH_BOOTSTRAP`]. The opt-out [`ENV_SSH_OFF`] is never set
/// — it belongs to the user.
pub fn policy_env(policy: &SshPolicy) -> Vec<(String, String)> {
    let mut env = vec![
        (
            ENV_SSH_ENABLED.to_owned(),
            if policy.enabled { "1" } else { "0" }.to_owned(),
        ),
        (ENV_SSH_ALLOW.to_owned(), policy.allowlist.join("\n")),
        (ENV_SSH_DENY.to_owned(), policy.denylist.join("\n")),
        (
            ENV_SSH_REUSE_CM.to_owned(),
            if policy.reuse_control_master {
                "1"
            } else {
                "0"
            }
            .to_owned(),
        ),
        (
            ENV_SSH_CONTROL_PERSIST.to_owned(),
            policy.control_persist.clone(),
        ),
        (ENV_SSH_BOOTSTRAP.to_owned(), bootstrap_script(policy)),
    ];
    // An unset contract is never emitted — the wrappers test
    // `-n`, and `ControlPath=/%C` on an empty dir would be a broken
    // attach rather than a missing feature.
    if let Some(dir) = &policy.control_dir {
        env.push((ENV_SSH_CONTROL_DIR.to_owned(), dir.display().to_string()));
    }
    env
}

/// The self-contained POSIX-sh remote command `ssh()` sends
/// (Spec R09-T2): `mktemp -d` at mode `0700`, the whole
/// [`Injection::files`](crate::Injection::files) set for the three
/// families written inside it, the session env exported, EXIT/HUP
/// traps that emit `exit_shell` and remove the dir, then the remote
/// `$SHELL` run as a CHILD under its family's flag — an unsupported
/// or unset `$SHELL` falls back to a plain `sh -l` login shell.
///
/// The rendered script is byte-identical for every hop it is sent
/// down: `ssh()` re-exports [`ENV_SSH_BOOTSTRAP`] into the remote
/// session so a nested hop sends this same text again.
pub fn bootstrap_script(policy: &SshPolicy) -> String {
    let mut script = String::from(
        "# ait remote bootstrap (REQUIREMENTS.md §5.9 / Spec R09) — the\n\
         # remote command an integrated ssh() sends. Everything the remote\n\
         # shell needs rides this text: the snippet file set, the session\n\
         # env and the exit_shell emission. No dotfile is touched.\n\
         __ait_dir=$(mktemp -d \"${TMPDIR:-/tmp}/ait-ssh.XXXXXXXX\") || exit 1\n\
         chmod 700 \"$__ait_dir\" 2>/dev/null\n\
         # The injected rc/snippet files reference $__ait_dir inside\n\
         # themselves — the remote child shell must inherit it through\n\
         # env, not this script's lexical scope.\n\
         export __ait_dir\n",
    );
    let exports: &[(&str, String)] = &[
        ("AIT_SESSION_ID", policy.session_id.clone()),
        ("AIT_IS_SSH", "1".to_owned()),
        (
            ENV_SSH_ENABLED,
            if policy.enabled { "1" } else { "0" }.to_owned(),
        ),
        (ENV_SSH_ALLOW, policy.allowlist.join("\n")),
        (ENV_SSH_DENY, policy.denylist.join("\n")),
        (
            ENV_SSH_REUSE_CM,
            if policy.reuse_control_master {
                "1"
            } else {
                "0"
            }
            .to_owned(),
        ),
        (ENV_SSH_CONTROL_PERSIST, policy.control_persist.clone()),
    ];
    for (name, value) in exports {
        script.push_str(&format!("export {name}={}\n", sh_single_quote(value)));
    }
    // The control dir rides only when one exists — the remote
    // wrapper's nested-`ssh()` attach checks `-n` on it, and an
    // empty export would read as "attach to /%C".
    if let Some(dir) = &policy.control_dir {
        script.push_str(&format!(
            "export {ENV_SSH_CONTROL_DIR}={}\n",
            sh_single_quote(&dir.display().to_string())
        ));
    }
    script.push_str(
        "# exit_shell on BOTH a clean exit and a dropped connection (EXIT\n\
         # fires inside the ssh that stayed up, HUP inside the one the\n\
         # network killed) — the same hook the local snippet's EXIT trap\n\
         # emits, so the tab's lifecycle never forks. `hop` attributes the\n\
         # emission to this level when the wrapper exported one.\n\
         __ait_exit() {\n\
         \x20   __ait_p=\"{\\\"hook\\\":\\\"exit_shell\\\",\\\"value\\\":{\\\"session_id\\\":\\\"${AIT_SESSION_ID}\\\"${AIT_SSH_HOP:+,\\\"hop\\\":\\\"${AIT_SSH_HOP}\\\"}}}\"\n\
         \x20   __ait_h=$(printf '%s' \"$__ait_p\" | od -An -v -tx1 | tr -d ' \\n')\n\
         \x20   printf '\\033]7700;%s\\007' \"$__ait_h\"\n\
         }\n\
         __ait_cleanup() {\n\
         \x20   trap - EXIT HUP\n\
         \x20   __ait_exit\n\
         \x20   rm -rf \"$__ait_dir\"\n\
         }\n\
         trap __ait_cleanup EXIT\n\
         trap '__ait_cleanup; exit 129' HUP\n",
    );
    // The same file set `Injection::files` materializes locally, for all
    // three families — rendered against the literal `$__ait_dir` so the
    // remote shell's own expansion resolves the paths.
    for (name, contents) in remote_files() {
        script.push_str(&format!(
            "cat > \"$__ait_dir/{name}\" <<'AIT_EOF_{marker}'\n\
             {contents}AIT_EOF_{marker}\n",
            marker = heredoc_marker(&name),
        ));
    }
    script.push_str(
        "# Run the remote login shell as a CHILD — `exec` would destroy\n\
         # the EXIT/HUP traps, and this script's cleanup (rm + exit_shell)\n\
         # must run on every path, including a remote shell that never\n\
         # integrated (a dumb remote's `-l`). An unsupported $SHELL still\n\
         # runs — plainly — rather than refusing the session (§5.5's\n\
         # degrade rule); an UNSET $SHELL degrades to `sh -l` the same\n\
         # way instead of an empty exec.\n\
         __ait_sh=${SHELL:-/bin/sh}\n\
         case ${__ait_sh##*/} in\n\
         \x20   bash) \"$__ait_sh\" --rcfile \"$__ait_dir/bashrc\" -i ;;\n\
         \x20   zsh)\n\
         \x20       AIT_ZDOTDIR_ORIG=${ZDOTDIR:-$HOME}; export AIT_ZDOTDIR_ORIG\n\
         \x20       ZDOTDIR=$__ait_dir; export ZDOTDIR\n\
         \x20       \"$__ait_sh\" -i ;;\n\
         \x20   fish)\n\
         \x20       fish_features=no-mark-prompt; export fish_features\n\
         \x20       \"$__ait_sh\" --init-command \"source '$__ait_dir/snippet.fish'\" -i ;;\n\
         \x20   *) \"$__ait_sh\" -l ;;\n\
         esac\n",
    );
    script
}

/// `name`/`contents` pairs of every file the remote dir needs: the
/// three families' [`Injection::files`](crate::Injection::files)
/// union, rendered against the [`REMOTE_DIR`] placeholder.
fn remote_files() -> Vec<(String, String)> {
    let dir = PathBuf::from("@AIT_DIR@");
    let mut files = Vec::new();
    for basename in ["bash", "zsh", "fish"] {
        let plan = crate::plan_with_zdotdir(std::path::Path::new(basename), None)
            .expect("the three basenames always plan");
        for (name, contents) in plan.files(&dir) {
            files.push((name.to_owned(), contents.replace("@AIT_DIR@", REMOTE_DIR)));
        }
    }
    files
}

/// The heredoc end marker for file `name` — uppercase, file-scoped so
/// a stray `AIT_EOF` in snippet text can never terminate a heredoc
/// early.
fn heredoc_marker(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// POSIX single-quote a value for `name='value'` — embedded `'`
/// becomes `'\''` so nothing inside can close the string early.
fn sh_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The per-user runtime-dir rule every ait-owned runtime dir shares
/// (CODING_STANDARDS §17 — one definition, named once):
/// `$XDG_RUNTIME_DIR/ait` — plus `subdir` when given — while the
/// runtime dir is set and non-empty, else `$TMPDIR/ait[-<subdir>]-<uid>`.
/// A shared `/tmp/ait` is NOT safe: the first creator wins at `0700`
/// and every later user is silently shut out, so the fallback carries
/// the owner's uid in its name. `ssh_control_dir` (`subdir` `"ssh"`)
/// and the frontend's ctl socket dir (no subdir) both resolve here so
/// every consumer names the same place.
pub fn ait_runtime_dir(runtime_dir: Option<&OsStr>, subdir: Option<&str>) -> PathBuf {
    match runtime_dir.filter(|dir| !dir.is_empty()).map(PathBuf::from) {
        Some(dir) => match subdir {
            Some(subdir) => dir.join("ait").join(subdir),
            None => dir.join("ait"),
        },
        // The fallback folds `subdir` into the dir name — a bare
        // `$TMPDIR/ait` would lock every later user out at `0700`.
        None => std::env::temp_dir().join(match subdir {
            Some(subdir) => format!("ait-{subdir}-{}", owner_uid()),
            None => format!("ait-{}", owner_uid()),
        }),
    }
}

/// The per-user dir ait-managed control sockets live under —
/// `ssh()`'s `ControlPath` base and the frontend's "did WE create
/// this socket" check, so both sides name the same place. Prefers
/// `$XDG_RUNTIME_DIR/ait/ssh` (already private per-user tmpfs);
/// falls back to `$TMPDIR/ait-ssh-<uid>` — a shared `/tmp/ait/ssh`
/// is NOT safe (first creator wins at `0700`, everyone else is
/// silently shut out). Created at `0700` like the session dirs; a
/// failure returns `None` — no ControlMaster attach, the rest of
/// the integration still works (the same degrade every other
/// optional piece gets).
pub fn ssh_control_dir() -> Option<PathBuf> {
    let base = ait_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), Some("ssh"));
    if std::fs::create_dir_all(&base).is_err() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
    }
    Some(base)
}

/// The owning uid for [`ait_runtime_dir`]'s fallback name — the real
/// uid on unix; `$USER`/`$USERNAME` elsewhere (the path only needs a
/// stable per-user token, never a security check).
#[cfg(unix)]
fn owner_uid() -> u32 {
    // SAFETY: getuid is always safe to call.
    unsafe { libc::getuid() }
}

/// Non-unix uid source — a stable per-user token is all the dir name
/// asks for.
#[cfg(not(unix))]
fn owner_uid() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| String::from("user"))
}

#[cfg(test)]
mod tests {
    //! The pure renderer (Spec R09-T2): the script is POSIX text —
    //! asserts cover the wire contract (exports, traps, exec dispatch,
    //! the full file set) and `sh -n` parses it where a POSIX shell
    //! exists.

    use super::*;

    fn policy() -> SshPolicy {
        SshPolicy {
            enabled: true,
            allowlist: vec!["*.prod".to_owned(), "!bastion.prod".to_owned()],
            denylist: vec!["*.internal".to_owned()],
            reuse_control_master: true,
            control_dir: Some(PathBuf::from("/run/user/1000/ait/ssh")),
            control_persist: "10m".to_owned(),
            session_id: "42".to_owned(),
        }
    }

    /// The script is self-contained: every spawn-time value is baked
    /// in as an export, the file set covers all three families, both
    /// traps emit `exit_shell` and the dir is removed.
    #[test]
    fn the_script_is_self_contained() {
        let script = bootstrap_script(&policy());
        assert!(
            script.contains("mktemp -d"),
            "a private remote dir: {script}"
        );
        assert!(script.contains("export AIT_SESSION_ID='42'"));
        assert!(script.contains("export AIT_IS_SSH='1'"));
        assert!(script.contains("export AIT_SSH_ENABLED='1'"));
        assert!(script.contains("export AIT_SSH_ALLOW='*.prod\n!bastion.prod'"));
        assert!(script.contains("export AIT_SSH_DENY='*.internal'"));
        assert!(script.contains("export AIT_SSH_REUSE_CM='1'"));
        assert!(script.contains("export AIT_SSH_CONTROL_DIR='/run/user/1000/ait/ssh'"));
        assert!(script.contains("export AIT_SSH_CONTROL_PERSIST='10m'"));
        assert!(script.contains("trap __ait_cleanup EXIT"), "EXIT trap");
        assert!(
            script.contains("trap '__ait_cleanup; exit 129' HUP"),
            "HUP trap"
        );
        assert!(script.contains("rm -rf \"$__ait_dir\""), "the dir dies");
        assert!(script.contains("exit_shell"), "the hook rides the cleanup");
        for file in [
            "bashrc",
            "snippet.bash",
            ".zshenv",
            ".zprofile",
            ".zshrc",
            "snippet.zsh",
            "snippet.fish",
        ] {
            assert!(
                script.contains(&format!("cat > \"$__ait_dir/{file}\"")),
                "file set missing {file}"
            );
        }
        for dispatch in [
            "\"$__ait_sh\" --rcfile \"$__ait_dir/bashrc\" -i",
            "ZDOTDIR=$__ait_dir; export ZDOTDIR",
            "\"$__ait_sh\" --init-command",
            "\"$__ait_sh\" -l",
        ] {
            assert!(script.contains(dispatch), "dispatch missing {dispatch}");
        }
        // `exec` must NOT appear — it would destroy the cleanup traps.
        assert!(!script.contains("exec \"$__ait_sh\""), "no exec: {script}");
        // An unset SHELL resolves to /bin/sh instead of an empty
        // exec, and every family name the case doesn't know still
        // runs — plainly — through the `*)` login-shell arm.
        assert!(
            script.contains("__ait_sh=${SHELL:-/bin/sh}"),
            "unset-SHELL fallback: {script}"
        );
        assert!(
            script.contains("*) \"$__ait_sh\" -l ;;"),
            "unknown-shell degrade arm: {script}"
        );
    }

    /// The file bodies land REMOTE_DIR-relative: wrappers inside the
    /// heredocs reference the literal `$__ait_dir` the remote shell
    /// expands — never a baked host path.
    #[test]
    fn file_bodies_reference_the_remote_dir() {
        let script = bootstrap_script(&policy());
        assert!(script.contains(". \"$__ait_dir/snippet.bash\""));
        assert!(!script.contains("@AIT_DIR@"), "no placeholder survives");
        assert!(
            !script.contains("/run/user/1000/ait/ssh/snippet"),
            "the control dir is a host path — it must never appear as a file root"
        );
    }

    /// Single quotes inside exported values close-and-reopen safely —
    /// a pattern or path containing `'` cannot break the script.
    #[test]
    fn env_values_survive_quotes() {
        let mut policy = policy();
        policy.denylist = vec!["it's-a-host".to_owned()];
        let script = bootstrap_script(&policy);
        assert!(script.contains("export AIT_SSH_DENY='it'\\''s-a-host'"));
    }

    /// `policy_env` pairs every armed key with its rendered value and
    /// puts the script under AIT_SSH_BOOTSTRAP — the spawn-side half
    /// of the env contract.
    #[test]
    fn policy_env_covers_the_contract() {
        let env = policy_env(&policy());
        let map: std::collections::BTreeMap<_, _> = env.iter().cloned().collect();
        assert_eq!(map[ENV_SSH_ENABLED], "1");
        assert_eq!(map[ENV_SSH_ALLOW], "*.prod\n!bastion.prod");
        assert_eq!(map[ENV_SSH_DENY], "*.internal");
        assert_eq!(map[ENV_SSH_REUSE_CM], "1");
        assert_eq!(map[ENV_SSH_CONTROL_DIR], "/run/user/1000/ait/ssh");
        assert_eq!(map[ENV_SSH_CONTROL_PERSIST], "10m");
        assert_eq!(map[ENV_SSH_BOOTSTRAP], bootstrap_script(&policy()));
        assert!(!map.contains_key(ENV_SSH_OFF), "the opt-out is the user's");
    }

    /// No control dir means no contract at all — neither the env pair
    /// nor the remote export carries an empty `AIT_SSH_CONTROL_DIR`
    /// that would read as "attach to /%C".
    #[test]
    fn a_missing_control_dir_emits_no_contract() {
        let policy = SshPolicy {
            control_dir: None,
            ..policy()
        };
        let env = policy_env(&policy);
        let map: std::collections::BTreeMap<_, _> = env.iter().cloned().collect();
        assert!(!map.contains_key(ENV_SSH_CONTROL_DIR));
        let script = bootstrap_script(&policy);
        assert!(
            !script.contains("export AIT_SSH_CONTROL_DIR"),
            "no remote export either: {script}"
        );
    }

    /// `ssh_control_dir` lands under `XDG_RUNTIME_DIR` when set,
    /// creates it lazily at `0700` and never falls back to a shared
    /// `/tmp/ait/ssh`.
    #[cfg(unix)]
    #[test]
    fn ssh_control_dir_uses_the_runtime_dir_at_0700() {
        let temp = tempfile::tempdir().expect("tempdir");
        let xdg = temp.path().join("xdg");
        // SAFETY: the test process is single-purpose here — no other
        // test in this crate reads XDG_RUNTIME_DIR concurrently.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", &xdg) };
        let dir = ssh_control_dir().expect("dir offered");
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };
        assert_eq!(dir, xdg.join("ait").join("ssh"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir)
            .expect("created")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700, "private control dir");
    }

    /// The rendered script parses under a real POSIX shell where one
    /// exists — `sh -n` is a syntax gate, never an execution.
    #[test]
    fn the_script_is_valid_posix_sh() {
        let sh = ["/bin/sh", "/usr/bin/sh"]
            .iter()
            .find(|p| std::path::Path::new(p).exists());
        let Some(sh) = sh else { return };
        let script = bootstrap_script(&policy());
        let status = std::process::Command::new(sh)
            .args(["-n", "-c", &script])
            .status()
            .expect("spawn sh -n");
        assert!(status.success(), "sh -n rejected the bootstrap script");
    }
}
