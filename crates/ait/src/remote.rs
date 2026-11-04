//! The §5.9 remote-session plumbing the frontend owns (Spec R09):
//! [`SshSpawn`] — the armed `[ssh]` policy rendered into each spawned
//! session's `AIT_SSH_*` environment — plus [`RemoteContext`], the
//! per-tab tracking the `ssh_begin`/`ssh_end`/remote `exit_shell`
//! hooks fold, and the control-socket teardown that retires ait's own
//! multiplexed sockets when a remote session ends.
//!
//! The wire vocabulary (`AIT_SSH_*`, `ssh_begin`/`ssh_end`) lives in
//! ait-shell (standard 17): this module only maps `[ssh]` settings and
//! decoded hook payloads onto it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ait_config::SshSettings;
use ait_shell::HookName;

/// The `ControlPersist` value the ait-managed attach uses — long
/// enough for a just-finished session's socket to answer the agent's
/// follow-up commands, short enough to die on its own if teardown
/// never runs.
const CONTROL_PERSIST: &str = "10m";

/// The armed `[ssh]` policy a spawn renders into `AIT_SSH_*` env —
/// one per [`SessionTemplate`](crate::tabs::SessionTemplate), so a
/// settings reload reaches only sessions spawned from then on (the
/// `[terminal]` precedent).
#[derive(Debug, Clone)]
pub struct SshSpawn {
    /// The armed `enable_integration`.
    enabled: bool,
    /// `hosts_allowlist` globs.
    allowlist: Vec<String>,
    /// `hosts_denylist` globs.
    denylist: Vec<String>,
    /// The armed `reuse_existing_control_master`.
    reuse: bool,
    /// The ait control dir — `None` when none could be created, so
    /// `ssh()` simply never attaches a `ControlPath` instead of
    /// breaking an otherwise-working connection.
    control_dir: Option<PathBuf>,
}

impl Default for SshSpawn {
    /// The armed `[ssh]` defaults — `SessionTemplate::default()`'s
    /// ssh half: integration on, open allowlist, no denies, Control-
    /// Master reuse on.
    fn default() -> Self {
        Self::armed(&SshSettings::default())
    }
}

impl SshSpawn {
    /// Arm the `[ssh]` section's documented defaults.
    pub fn armed(settings: &SshSettings) -> Self {
        Self {
            enabled: settings.integration_enabled(),
            allowlist: settings.hosts_allowlist.clone(),
            denylist: settings.hosts_denylist.clone(),
            reuse: settings.reuses_control_master(),
            control_dir: control_dir(),
        }
    }

    /// The `AIT_SSH_*` env pairs this spawn adds through the injection
    /// env seam: the armed policy the snippet's `ssh()` reads, plus
    /// `AIT_SSH_BOOTSTRAP` — the remote command rendered against this
    /// session's id.
    pub fn env(&self, session_id: &str) -> Vec<(OsString, OsString)> {
        ait_shell::policy_env(&ait_shell::SshPolicy {
            enabled: self.enabled,
            allowlist: self.allowlist.clone(),
            denylist: self.denylist.clone(),
            reuse_control_master: self.reuse,
            control_dir: self.control_dir.clone().unwrap_or_default(),
            control_persist: CONTROL_PERSIST.to_owned(),
            session_id: session_id.to_owned(),
        })
        .into_iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)))
        .collect()
    }

    /// The ait control dir, when one was created at arm time —
    /// [`RemoteContext::fold`] uses it to tell ait-managed sockets
    /// from the user's own before retiring them.
    pub fn control_dir(&self) -> Option<&Path> {
        self.control_dir.as_deref()
    }
}

/// `$XDG_RUNTIME_DIR/ait/ssh` — `0700`, created on demand — falling
/// back to `$TMPDIR/ait-ssh-<uid>` when no runtime dir exists. `None`
/// when neither can be made: an unusable ControlPath would break a
/// working `ssh`, so a missing dir opts out instead.
fn control_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ait")
        .join("ssh");
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir)
}

/// One tab's remote context — `Some` while an `ssh()`-wrapped session
/// is live (Spec R09-T4). Tracks the destination token the user typed
/// and the negotiated ControlPath so the status line, the executor
/// tiers and the socket teardown share one view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteContext {
    /// The destination token (`user@host`, `host`, config alias).
    target: String,
    /// The ControlPath pattern `ssh -G` resolved (`-` when none) —
    /// passed to the executor tiers and `-O exit` on teardown.
    control_path: Option<String>,
    /// `true` when `control_path` lands under the ait control dir —
    /// only those sockets are ours to retire.
    managed: bool,
}

impl RemoteContext {
    /// The destination token — the status line's `ssh:<target>` and
    /// `blocks.host`'s value.
    pub fn target(&self) -> &str {
        &self.target
    }

    /// The negotiated ControlPath pattern, when `ssh -G` named one.
    pub fn control_path(&self) -> Option<&str> {
        self.control_path.as_deref()
    }

    /// `true` while the socket sits under the ait control dir — only
    /// a managed socket is retired on teardown (a user-owned
    /// ControlPath survives: other sessions share it).
    pub fn managed(&self) -> bool {
        self.managed
    }

    /// Fold a session-id-vetted ssh lifecycle hook: `ssh_begin` opens
    /// the context, `ssh_end` and a remote `exit_shell` close it.
    /// `control_dir` marks sockets ait owns. Returns the context that
    /// just closed, so the caller can retire its socket.
    pub fn fold(
        current: &mut Option<Self>,
        name: &HookName,
        value: &serde_json::Value,
        control_dir: Option<&Path>,
    ) -> Option<Self> {
        match name {
            HookName::SshBegin => {
                let target = value
                    .get("target")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let control_path = value
                    .get("control_path")
                    .and_then(|v| v.as_str())
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned);
                let managed = control_path
                    .as_deref()
                    .zip(control_dir)
                    .is_some_and(|(path, dir)| Path::new(path).starts_with(dir));
                current.replace(Self {
                    target,
                    control_path,
                    managed,
                })
            }
            HookName::SshEnd | HookName::ExitShell => current.take(),
            _ => None,
        }
    }
}

/// Retire an ait-managed ControlMaster socket — `ssh -S <path> -O
/// exit <target>` best-effort (Spec R09-T4: "on `ssh_end` or tab
/// close"). Fires only for [`RemoteContext::managed`] sockets; a
/// failure is silent (the socket dies with ControlPersist anyway) and
/// a user-owned path is never offered here.
pub fn retire_control_socket(context: &RemoteContext) {
    if !context.managed() {
        return;
    }
    let Some(path) = context.control_path() else {
        return;
    };
    let _ = Command::new("ssh")
        .args(["-S", path, "-O", "exit", context.target()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fold(
        current: &mut Option<RemoteContext>,
        name: HookName,
        value: serde_json::Value,
    ) -> Option<RemoteContext> {
        RemoteContext::fold(current, &name, &value, Some(Path::new("/run/ait/ssh")))
    }

    /// The lifecycle fold (Spec R09-T4): ssh_begin opens the context
    /// with target + control path, ssh_end or a remote exit_shell
    /// closes it.
    #[test]
    fn begin_opens_end_and_exit_close() {
        let mut ctx = None;
        assert!(fold(
            &mut ctx,
            HookName::SshBegin,
            json!({"session_id": "1", "target": "prod", "bootstrapped": true,
                   "control_path": "/run/ait/ssh/%C"}),
        )
        .is_none());
        let open = ctx.as_ref().expect("ssh_begin opens the context");
        assert_eq!(open.target(), "prod");
        assert_eq!(open.control_path(), Some("/run/ait/ssh/%C"));
        assert!(open.managed(), "under the ait dir = managed");

        let closed = fold(&mut ctx, HookName::ExitShell, json!({"session_id": "1"}));
        assert!(ctx.is_none(), "a remote exit_shell closes it");
        assert_eq!(closed.unwrap().target(), "prod");

        fold(
            &mut ctx,
            HookName::SshBegin,
            json!({"session_id": "1", "target": "prod"}),
        );
        assert!(fold(&mut ctx, HookName::SshEnd, json!({"target": "prod"})).is_some());
        assert!(ctx.is_none());
    }

    /// A user-managed ControlPath is never `managed` — its socket is
    /// shared and must survive our teardown.
    #[test]
    fn foreign_sockets_are_not_managed() {
        let mut ctx = None;
        fold(
            &mut ctx,
            HookName::SshBegin,
            json!({"target": "prod", "control_path": "/home/u/.ssh/%C"}),
        );
        assert!(!ctx.as_ref().unwrap().managed());
    }
}
