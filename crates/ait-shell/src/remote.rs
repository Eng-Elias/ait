//! The §5.9 remote-bootstrap renderer and the `AIT_SSH_*` wire
//! vocabulary (Spec R09). The integrated snippets' `ssh()` wrapper
//! rewrites an interactive, policy-allowed `ssh` into
//! `ssh -t <args> '<remote command>'` where the remote command is the
//! self-contained POSIX-sh script [`bootstrap_script`] renders at
//! spawn: it lands the same [`Injection::files`] set in a private
//! `mktemp -d` dir, exports the session env, traps EXIT/HUP so the
//! `exit_shell` hook fires on a clean quit AND a dropped connection,
//! and `exec`s the remote `$SHELL` under the same integration — no
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
    /// The ait control dir (`AIT_SSH_CONTROL_DIR`).
    pub control_dir: PathBuf,
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
    vec![
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
            ENV_SSH_CONTROL_DIR.to_owned(),
            policy.control_dir.display().to_string(),
        ),
        (
            ENV_SSH_CONTROL_PERSIST.to_owned(),
            policy.control_persist.clone(),
        ),
        (ENV_SSH_BOOTSTRAP.to_owned(), bootstrap_script(policy)),
    ]
}

/// The self-contained POSIX-sh remote command `ssh()` sends
/// (Spec R09-T2): `mktemp -d` at mode `0700`, the whole
/// [`Injection::files`](crate::Injection::files) set for the three
/// families written inside it, the session env exported, EXIT/HUP
/// traps that emit `exit_shell` and remove the dir, then `exec` of
/// the remote `$SHELL` under its family's flag — an unsupported
/// remote shell falls back to a plain login shell.
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
        (
            ENV_SSH_CONTROL_DIR,
            policy.control_dir.display().to_string(),
        ),
        (ENV_SSH_CONTROL_PERSIST, policy.control_persist.clone()),
    ];
    for (name, value) in exports {
        script.push_str(&format!("export {name}={}\n", sh_single_quote(value)));
    }
    script.push_str(
        "# exit_shell on BOTH a clean exit and a dropped connection (EXIT\n\
         # fires inside the ssh that stayed up, HUP inside the one the\n\
         # network killed) — the same hook the local snippet's EXIT trap\n\
         # emits, so the tab's lifecycle never forks.\n\
         __ait_exit() {\n\
         \x20   __ait_p=\"{\\\"hook\\\":\\\"exit_shell\\\",\\\"value\\\":{\\\"session_id\\\":\\\"${AIT_SESSION_ID}\\\"}}\"\n\
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
         # degrade rule).\n\
         case ${SHELL##*/} in\n\
         \x20   bash) \"$SHELL\" --rcfile \"$__ait_dir/bashrc\" -i ;;\n\
         \x20   zsh)\n\
         \x20       AIT_ZDOTDIR_ORIG=${ZDOTDIR:-$HOME}; export AIT_ZDOTDIR_ORIG\n\
         \x20       ZDOTDIR=$__ait_dir; export ZDOTDIR\n\
         \x20       \"$SHELL\" -i ;;\n\
         \x20   fish)\n\
         \x20       fish_features=no-mark-prompt; export fish_features\n\
         \x20       \"$SHELL\" --init-command \"source '$__ait_dir/snippet.fish'\" -i ;;\n\
         \x20   *) \"$SHELL\" -l ;;\n\
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
            control_dir: PathBuf::from("/run/user/1000/ait/ssh"),
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
            "\"$SHELL\" --rcfile \"$__ait_dir/bashrc\" -i",
            "ZDOTDIR=$__ait_dir; export ZDOTDIR",
            "\"$SHELL\" --init-command",
            "\"$SHELL\" -l",
        ] {
            assert!(script.contains(dispatch), "dispatch missing {dispatch}");
        }
        // `exec` must NOT appear — it would destroy the cleanup traps.
        assert!(!script.contains("exec \"$SHELL\""), "no exec: {script}");
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
