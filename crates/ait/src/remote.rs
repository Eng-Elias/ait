//! The §5.9 remote-session plumbing the frontend owns (Spec R09):
//! [`SshSpawn`] — the armed `[ssh]` policy rendered into each spawned
//! session's `AIT_SSH_*` environment — plus [`RemoteContext`], the
//! per-tab tracking stack the `ssh_begin`/`ssh_end`/remote
//! `exit_shell` hooks fold, and the control-socket teardown that
//! retires ait's own multiplexed sockets when a remote session ends.
//!
//! The wire vocabulary (`AIT_SSH_*`, `ssh_begin`/`ssh_end`, the `hop`
//! attribution field, the `ssh` program name) lives in ait-shell
//! (standard 17): this module only maps `[ssh]` settings and decoded
//! hook payloads onto it.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ait_config::SshSettings;
use ait_shell::HookName;

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
    /// The ait control dir — `None` when the integration is off or
    /// none could be created, so `ssh()` simply never attaches a
    /// `ControlPath` instead of breaking an otherwise-working
    /// connection. Materialized lazily: a disabled policy never
    /// creates the dir.
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
        let enabled = settings.integration_enabled();
        Self {
            enabled,
            allowlist: settings.hosts_allowlist.clone(),
            denylist: settings.hosts_denylist.clone(),
            reuse: settings.reuses_control_master(),
            // Only an enabled integration owns a control dir — an
            // armed-off template must create nothing.
            control_dir: enabled.then(ait_shell::ssh_control_dir).flatten(),
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
            control_dir: self.control_dir.clone(),
            control_persist: ait_shell::DEFAULT_CONTROL_PERSIST.to_owned(),
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

/// One hop of a possibly-nested `ssh()` session (Spec R09-T4):
/// pushed by `ssh_begin`, closed by a hop-attributed `ssh_end` or a
/// remote `exit_shell` — either close event removes the level it
/// names AND every level above it, since a dead shell takes its
/// nested sessions down with it. `target` feeds the `ssh:` status
/// segment and `blocks.host`; `control_path` feeds `remote_state()`
/// and — when it lands inside our control dir — the managed-socket
/// teardown at close; `integrated` records whether the remote shell
/// proved its own integration — the bridge reports it as
/// `RemoteState::Remote.bootstrapped`, the executor's guard against
/// typing `__ait_run` at a remote shell that never integrated
/// (Spec R09 us-18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteContext {
    /// The destination token (`user@host`, `host`, config alias).
    target: String,
    /// The ControlPath pattern `ssh -G` resolved — passed to the
    /// executor tiers and `-O exit` on teardown.
    control_path: Option<String>,
    /// `true` when `control_path` lands under the ait control dir —
    /// only those sockets are ours to retire.
    managed: bool,
    /// The wrapper-minted hop id — the only thing that attributes a
    /// nested level's `ssh_end`/`exit_shell`/lifecycle hooks to their
    /// own context, since every level shares `session_id`.
    hop: Option<String>,
    /// The remote shell confirmed integrated — set when one of ITS
    /// lifecycle hooks lands.
    integrated: bool,
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

    /// `true` once the remote shell's own hooks proved it integrated
    /// — the `bootstrapped` leg of `remote_state()`.
    pub fn integrated(&self) -> bool {
        self.integrated
    }

    /// Fold a session-id-vetted hook into the context `stack`
    /// (innermost last), returning every closed context in close
    /// order so the caller can retire their sockets. `control_dir`
    /// marks sockets ait owns.
    ///
    /// `ssh_begin` pushes a level. `ssh_end`/`exit_shell` close the
    /// level their `hop` names plus every level above it — a dead
    /// shell kills its nested sessions — which also makes the
    /// `exit_shell`-then-`ssh_end` pair a clean session produces
    /// naturally idempotent: the second event finds no level. A
    /// hop-less close pops the top (pre-hop emissions can only be
    /// the innermost's). Any other vetted lifecycle hook
    /// (`init_shell`/`bootstrapped`/`precmd`/`preexec`) marks its
    /// level integrated — the innermost when it carries no `hop`,
    /// since only the top's shell can be emitting.
    pub fn fold(
        stack: &mut Vec<Self>,
        name: &HookName,
        value: &serde_json::Value,
        control_dir: Option<&Path>,
    ) -> Vec<Self> {
        match name {
            HookName::SshBegin => {
                let Some(target) = value.get("target").and_then(|v| v.as_str()) else {
                    return Vec::new();
                };
                // `controlpath none` is ssh's "no socket" literal —
                // never report it as a path.
                let control_path = value
                    .get("control_path")
                    .and_then(|v| v.as_str())
                    .filter(|p| !p.is_empty() && *p != "none")
                    .map(str::to_owned);
                let managed = control_path
                    .as_deref()
                    .zip(control_dir)
                    .is_some_and(|(path, dir)| Path::new(path).starts_with(dir));
                let hop = value
                    .get("hop")
                    .and_then(|v| v.as_str())
                    .filter(|hop| !hop.is_empty())
                    .map(str::to_owned);
                stack.push(Self {
                    target: target.to_owned(),
                    control_path,
                    managed,
                    hop,
                    integrated: false,
                });
                Vec::new()
            }
            HookName::SshEnd | HookName::ExitShell => {
                let hop = value.get("hop").and_then(|v| v.as_str());
                match hop {
                    Some(hop) => match stack.iter().position(|ctx| ctx.hop.as_deref() == Some(hop))
                    {
                        // The dead shell's nested sessions die with
                        // it — the level and everything above.
                        Some(at) => stack.drain(at..).collect(),
                        // Already folded — the paired exit_shell /
                        // ssh_end beat this one to it.
                        None => Vec::new(),
                    },
                    None => stack.pop().into_iter().collect(),
                }
            }
            HookName::InitShell | HookName::Bootstrapped | HookName::Precmd | HookName::Preexec => {
                let hop = value.get("hop").and_then(|v| v.as_str());
                let context = match hop {
                    Some(hop) => stack.iter_mut().find(|ctx| ctx.hop.as_deref() == Some(hop)),
                    None => stack.last_mut(),
                };
                if let Some(context) = context {
                    context.integrated = true;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }
}

/// Retire an ait-managed ControlMaster socket — `ssh -S <path> -O
/// exit <target>` best-effort (Spec R09-T4: "on `ssh_end` or tab
/// close"). Fires only for [`RemoteContext::managed`] sockets; a
/// failure is silent (the socket dies with ControlPersist anyway) and
/// a user-owned path is never offered here. Fire-and-forget —
/// `-O exit` against a dead socket can block on the connect timeout,
/// so neither the app tick nor `Tab::drop` waits on it; a detached
/// wait thread keeps the child reaped.
pub fn retire_control_socket(context: &RemoteContext) {
    if !context.managed() {
        return;
    }
    let Some(path) = context.control_path() else {
        return;
    };
    let spawned = Command::new(ait_shell::SSH_PROGRAM)
        .args(["-S", path, "-O", "exit", context.target()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DIR: &str = "/run/ait/ssh";

    fn fold(
        stack: &mut Vec<RemoteContext>,
        name: HookName,
        value: serde_json::Value,
    ) -> Vec<RemoteContext> {
        RemoteContext::fold(stack, &name, &value, Some(Path::new(DIR)))
    }

    fn begin(target: &str, hop: &str) -> serde_json::Value {
        json!({"session_id": "1", "target": target, "hop": hop,
               "control_path": format!("{DIR}/%C")})
    }

    /// The lifecycle fold (Spec R09-T4): ssh_begin pushes the context
    /// with target + control path, a hop-matched ssh_end or a remote
    /// exit_shell closes it — and the pair of them for one hop is
    /// idempotent.
    #[test]
    fn begin_opens_end_and_exit_close() {
        let mut stack = Vec::new();
        assert!(fold(&mut stack, HookName::SshBegin, begin("prod", "h1")).is_empty());
        let open = stack.last().expect("ssh_begin pushes the context");
        assert_eq!(open.target(), "prod");
        assert_eq!(open.control_path(), Some("/run/ait/ssh/%C"));
        assert!(open.managed(), "under the ait dir = managed");
        assert!(!open.integrated(), "nothing remote integrated yet");

        let closed = fold(
            &mut stack,
            HookName::ExitShell,
            json!({"session_id": "1", "hop": "h1"}),
        );
        assert!(stack.is_empty(), "a remote exit_shell closes it");
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].target(), "prod");
        // The paired ssh_end finds nothing — idempotent, not a
        // second pop of an outer level.
        assert!(fold(
            &mut stack,
            HookName::SshEnd,
            json!({"target": "prod", "hop": "h1"})
        )
        .is_empty());

        fold(&mut stack, HookName::SshBegin, begin("prod", "h2"));
        let closed = fold(
            &mut stack,
            HookName::SshEnd,
            json!({"target": "prod", "hop": "h2"}),
        );
        assert_eq!(closed.len(), 1);
        assert!(stack.is_empty());
    }

    /// A user-managed ControlPath is never `managed` — its socket is
    /// shared and must survive our teardown.
    #[test]
    fn foreign_sockets_are_not_managed() {
        let mut stack = Vec::new();
        fold(
            &mut stack,
            HookName::SshBegin,
            json!({"target": "prod", "hop": "h", "control_path": "/home/u/.ssh/%C"}),
        );
        assert!(!stack.last().unwrap().managed());
    }

    /// `controlpath none` — ssh's no-socket literal — is filtered out,
    /// never reported as a path (it would read as a relative path
    /// under cwd otherwise).
    #[test]
    fn controlpath_none_is_no_socket() {
        let mut stack = Vec::new();
        fold(
            &mut stack,
            HookName::SshBegin,
            json!({"target": "prod", "hop": "h", "control_path": "none"}),
        );
        assert_eq!(stack.last().unwrap().control_path(), None);
        assert!(!stack.last().unwrap().managed());
    }

    /// us-27 (Spec R09): a nested ssh pushes its own level; a remote
    /// `init_shell` attributed by hop marks THAT level integrated and
    /// the inner `ssh_end` pops it, restoring the outer context —
    /// never clearing it.
    #[test]
    fn nested_sessions_stack_and_restore() {
        let mut stack = Vec::new();
        fold(&mut stack, HookName::SshBegin, begin("outer", "h1"));
        fold(&mut stack, HookName::SshBegin, begin("inner", "h2"));
        assert_eq!(stack.len(), 2);
        assert_eq!(stack.last().unwrap().target(), "inner");

        // The inner remote shell's init arrives attributed to h2.
        fold(
            &mut stack,
            HookName::InitShell,
            json!({"session_id": "1", "hop": "h2"}),
        );
        assert!(stack[1].integrated());
        assert!(!stack[0].integrated());

        // The inner hop's exit_shell + ssh_end close only h2.
        let closed = fold(&mut stack, HookName::ExitShell, json!({"hop": "h2"}));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].target(), "inner");
        assert!(fold(&mut stack, HookName::SshEnd, json!({"hop": "h2"})).is_empty());
        assert_eq!(stack.len(), 1);
        assert_eq!(stack.last().unwrap().target(), "outer");
    }

    /// An outer level dying mid-nesting takes every level above it —
    /// the outer shell's exit_shell can only mean the whole chain is
    /// gone.
    #[test]
    fn a_dead_outer_shell_closes_the_whole_chain() {
        let mut stack = Vec::new();
        fold(&mut stack, HookName::SshBegin, begin("outer", "h1"));
        fold(&mut stack, HookName::SshBegin, begin("inner", "h2"));
        let closed = fold(&mut stack, HookName::ExitShell, json!({"hop": "h1"}));
        assert_eq!(closed.len(), 2, "outer + inner both retire");
        assert!(stack.is_empty());
        // Their ssh_ends fold to nothing after the fact.
        assert!(fold(&mut stack, HookName::SshEnd, json!({"hop": "h2"})).is_empty());
        assert!(fold(&mut stack, HookName::SshEnd, json!({"hop": "h1"})).is_empty());
    }

    /// Pre-hop emissions (a hop-less `exit_shell` from an older
    /// wrapper, or the local shell's own) pop the top — the only
    /// level they can be attributed to.
    #[test]
    fn hopless_close_pops_the_top() {
        let mut stack = Vec::new();
        fold(&mut stack, HookName::SshBegin, begin("outer", "h1"));
        fold(&mut stack, HookName::SshBegin, begin("inner", "h2"));
        let closed = fold(&mut stack, HookName::ExitShell, json!({"session_id": "1"}));
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].target(), "inner");
        assert_eq!(stack.len(), 1);
    }
}
