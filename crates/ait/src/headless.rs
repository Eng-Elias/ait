//! The hidden shell half of headless mode (REQUIREMENTS.md §5.10.5,
//! Spec R11 / issue #140, ticket R11-T4, user stories 18/19/30): a
//! [`LiveSession`] spawned off-screen through the same integration
//! materialization a visible tab gets — rc snippets land, a vetted
//! `init_shell`/`bootstrapped` proves `Integrated` — with a dedicated
//! **drain thread** standing in for the app tick: `drain_events` →
//! [`Tab::observe_event`] → [`Tab::service_bridge`] on a fixed cadence.
//!
//! The session IS a real [`Tab`]: the bookkeeping [`Tab::observe_event`]
//! folds — integration state, the `__ait_run` accumulator, the remote
//! context stack, block assembly, `dropped_lines` — is literally the
//! tab's, so the bridge's absolute-line answers and integration
//! semantics cannot drift from the TUI's (standard 17). Nothing
//! renders: no TUI, no painter, no input path touches it.
//!
//! The [`ShellBridge`] consumers get is the same channel-proxy shape
//! `TabBridge` uses in the TUI — `shell_bridge::pair()` — answered on
//! the drain thread instead of a `TabManager` tick. `run_in_band`
//! resolves through the hidden session's in-band accumulator,
//! `send_bytes` writes its PTY, `grid_lines`/`output_tail` snapshot its
//! capture surfaces, `remote_state` reads its `ssh()` context, and
//! `subscribe` forwards its decoded events. Every failure mode is a
//! [`BridgeError`], never a wedge: a dropped drain disconnects the
//! service channel and `ask` reports `Gone`.

// The whole module is forward surface: R11-T5's `ait --headless`
// driver constructs `HiddenSession`; until that arm lands only the
// tests below exercise it.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use ait_agent::{AgentContext, BridgeError};
use ait_term::{LiveSessionOptions, SpawnError};

use crate::remote::SshSpawn;
use crate::shell_bridge::{self, TabBridge};
use crate::tabs::{SessionTemplate, Tab};

/// How often the drain thread pumps the session — the same order as
/// the app's ~8 ms input tick that bounds `TabBridge` latency in the
/// TUI (tighter is wasted cycles; the hidden session has no render
/// pass to wait on).
const DRAIN_POLL: Duration = Duration::from_millis(5);

/// `AIT_SESSION_ID`s for hidden sessions, allocated downward from
/// `u64::MAX` so they can never collide with `TabManager`'s upward tab
/// ids — the ctl surface (Spec R11) addresses tabs by this id, and a
/// hidden session must never shadow a visible one.
static NEXT_HIDDEN_ID: AtomicU64 = AtomicU64::new(u64::MAX);

/// A hidden, fully integrated shell an `AgentSession` can drive without
/// the TUI (Spec R11 §5.10.5): owns the spawned `Tab` on its drain
/// thread; hands out [`TabBridge`] clones (`ShellBridge` impls) that
/// reach the thread's service queue. `Drop` stops the drain — the
/// `Tab`'s own drop then kills the child, joins the session's
/// reader/waiter threads and frees the snippet dir (us-30).
pub(crate) struct HiddenSession {
    /// The `ShellBridge` handle every consumer clones —
    /// `AgentRequest::bridge`'s `Box<dyn ShellBridge>` source.
    bridge: TabBridge,
    /// The drain thread's stop flag — `Drop` raises it, then joins.
    stop: Arc<AtomicBool>,
    /// The dedicated drain thread — the hidden session's app tick.
    drain: Option<JoinHandle<()>>,
    /// The `AIT_SESSION_ID` allocated at spawn — hook payloads echo it
    /// back, and the `ssh()` wrapper's hop ids prefix it.
    session_id: String,
}

impl HiddenSession {
    /// Spawn `options` as a hidden, integration-materialized session
    /// and start its drain thread. `options.session.size` is the grid
    /// size as given (no window shapes it); `options.cwd` unset means
    /// the process's own cwd. `ssh` is the armed `[ssh]` policy — the
    /// same `AIT_SSH_*` env a tab spawn renders (Spec R09-T2). A
    /// `None` injection plan or materialize failure still spawns the
    /// session — the bridge then reports `NotIntegrated`, the
    /// standard-1 degrade.
    pub(crate) fn spawn(options: LiveSessionOptions, ssh: &SshSpawn) -> Result<Self, SpawnError> {
        let cwd = options
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let id = NEXT_HIDDEN_ID.fetch_sub(1, Ordering::Relaxed);
        let session_id = id.to_string();
        let template = SessionTemplate {
            shell_integration: true,
            ssh: ssh.clone(),
            ..SessionTemplate::default()
        };
        let mut tab = Tab::spawn(options, cwd, id, &template)?;
        let (bridge, service) = shell_bridge::pair();
        tab.bridge = Some(service);
        let stop = Arc::new(AtomicBool::new(false));
        let drain = std::thread::Builder::new()
            .name("ait-headless-drain".to_string())
            .spawn({
                let stop = Arc::clone(&stop);
                move || drain_loop(tab, stop)
            })
            .map_err(SpawnError::Thread)?;
        Ok(Self {
            bridge,
            stop,
            drain: Some(drain),
            session_id,
        })
    }

    /// A `ShellBridge` handle for this hidden session — cloneable and
    /// `Send`, serviced by the drain thread; `Box<dyn ShellBridge>` it
    /// into `AgentRequest::bridge` and `run_command`, `write_to_pty`
    /// (via `send_bytes`), `grid_lines`/`output_tail` and
    /// `remote_state` all answer against the hidden session.
    pub(crate) fn bridge(&self) -> TabBridge {
        self.bridge.clone()
    }

    /// The `AIT_SESSION_ID` this session's spawn allocated — the
    /// identity its hooks echo (standard 14).
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The §5.6 `AgentContext` of the hidden session — cwd, shell, OS
    /// and the same `output_lines` `context_for` assembles for a
    /// visible tab (Spec R11 us-30: the model sees its own commands'
    /// results the way it does in a tab). Answered on the drain like
    /// every bridge request; `Gone` once the session is dropped.
    pub(crate) fn context(&self) -> Result<AgentContext, BridgeError> {
        self.bridge.context()
    }
}

impl Drop for HiddenSession {
    /// Stop the drain thread and join it — bounded: the loop wakes at
    /// most `DRAIN_POLL` late, then the `Tab` it owns drops (kills the
    /// child, frees the snippet dir). Bridge clones outliving `self`
    /// report `Gone` on their next call — never a wedge.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(drain) = self.drain.take() {
            let _ = drain.join();
        }
    }
}

/// The drain thread's body — the hidden session's entire event loop:
/// feed PTY output through the tab's bookkeeping, drain the sealed
/// block queue so it can't grow unpersisted (`store_session` is always
/// `None` here — the same rule `TabManager::take_persisted_blocks`
/// documents), then answer every queued `ShellBridge` request. Runs
/// until `stop` (the owning `HiddenSession`'s drop). A session exit
/// keeps the loop alive — the bridge still answers
/// `integration_state`/`grid_lines`/`output_tail`/`remote_state` and
/// refuses `run_in_band`/`send_bytes` with `SessionExited`, exactly as
/// `drain_events` does on a dead tab.
fn drain_loop(mut tab: Tab, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        for event in tab.session.drain_events() {
            tab.observe_event(&event);
        }
        tab.take_persisted_blocks();
        tab.service_bridge();
        std::thread::sleep(DRAIN_POLL);
    }
}

#[cfg(test)]
mod tests {
    //! The integration seam the ticket pins: a REAL spawned session —
    //! injected `/bin/bash`, real PTY, real `7701` frames — driven only
    //! through the `ShellBridge` surface. No TUI, no renderer (prior
    //! art: `tabs.rs`'s `bridge_*` tests, which stage the same service
    //! against `TabManager`'s drain).
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::Instant;

    use ait_agent::{BridgeEvent, RemoteState, ShellBridge, ShellIntegration};

    /// Generous bound for every wait below (mirrors `tabs.rs`'s live
    /// tests): tiny `/bin/bash` runs finish far under it even on a
    /// loaded CI runner.
    const TIMEOUT: Duration = Duration::from_secs(15);

    /// Options spawning an interactive `/bin/bash` — the injected real
    /// shell the ticket requires.
    fn bash_options() -> LiveSessionOptions {
        LiveSessionOptions {
            program: Some(OsString::from("/bin/bash")),
            ..Default::default()
        }
    }

    /// A hidden bash session, or `None` where bash isn't installed.
    fn spawn_bash() -> Option<HiddenSession> {
        Path::new("/bin/bash")
            .exists()
            .then(|| HiddenSession::spawn(bash_options(), &SshSpawn::default()).unwrap())
    }

    /// Poll `f(bridge)` until it holds — read-only bridge calls only —
    /// or the timeout kills the test. The drain thread does all the
    /// work; the caller never touches the session.
    fn wait_until(bridge: &TabBridge, f: impl Fn(&TabBridge) -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        while !f(bridge) {
            assert!(Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the hidden shell proves `Integrated` — a vetted
    /// `init_shell`/`bootstrapped` landed through the injected rc.
    fn wait_integrated(bridge: &TabBridge) {
        wait_until(bridge, |b| {
            matches!(b.integration_state(), Ok(ShellIntegration::Integrated))
        });
    }

    /// `OSC 7700 ; <hex(json)>` as a line the shell prints — the same
    /// hand-emit `tabs.rs` tests use.
    fn hook_line(json: &str) -> Vec<u8> {
        let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
        format!("printf '\\033]7700;{hex}\\007'\n").into_bytes()
    }

    /// The drain collects `events` until `f` accepts one or the timeout
    /// kills the test.
    fn recv_until(events: &Receiver<BridgeEvent>, f: impl Fn(&BridgeEvent) -> bool) -> BridgeEvent {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            match events.recv_timeout(Duration::from_millis(200)) {
                Ok(event) if f(&event) => return event,
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("subscription dropped unexpectedly")
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting");
        }
    }

    /// Acceptance: the spawn reaches `Integrated` (rc snippets landed,
    /// `init_shell`/`bootstrapped` proved it) and `run_in_band` returns
    /// real `RunResult`s through the hidden session's in-band
    /// accumulator — the `__ait_run` trace and `7701` reply included.
    #[test]
    fn hidden_session_integrates_and_runs_in_band() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);

        let rx = bridge
            .run_in_band(b"printf 'agent-marker'")
            .expect("an Integrated hidden session accepts the request");
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the run resolved");
        assert_eq!(result.output, b"agent-marker".to_vec());
        assert_eq!(result.exit_code, 0);

        // A second run resolves by id — requests serialize on the wire.
        let rx = bridge
            .run_in_band(b"echo second")
            .expect("a second request");
        let result = rx.recv_timeout(Duration::from_secs(5)).expect("resolved");
        assert_eq!(result.id, "1");
        assert!(result.output.windows(6).any(|w| w == b"second"));
    }

    /// Acceptance: shell state persists across commands — a `cd` and an
    /// `export` typed through `send_bytes` carry into the next
    /// `run_in_band`, exactly as in a tab (us-18); and the typed input
    /// round-trips: `grid_lines`/`output_tail` reflect its output
    /// (us-19/30).
    #[test]
    fn send_bytes_state_persists_and_surfaces() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);

        // Typed input round-trips through the hidden PTY — standard 15:
        // wait on computed output, never the echoed command.
        bridge
            .send_bytes(b"export AIT_KEEP=41; cd /\n")
            .expect("send_bytes on a live session");
        bridge
            .send_bytes(b"echo HX$((100+23))\n")
            .expect("send_bytes");
        wait_until(&bridge, |b| {
            String::from_utf8_lossy(&b.output_tail().unwrap()).contains("HX123")
        });
        let lines = bridge.grid_lines(0, 10_000).expect("grid snapshot");
        assert!(
            lines.iter().any(|line| line.contains("HX123")),
            "the output reached the grid: {lines:?}"
        );

        // The typed `cd`/`export` persisted — `run_in_band` answers
        // against THAT shell's cwd and env.
        let rx = bridge
            .run_in_band(b"printf '%s:%s' \"$PWD\" \"$AIT_KEEP\"")
            .expect("run accepted");
        let result = rx.recv_timeout(Duration::from_secs(5)).expect("resolved");
        assert!(
            result.output.windows(4).any(|w| w == b"/:41"),
            "cd and export carried into the in-band run: {:?}",
            result.output
        );
        assert_eq!(result.exit_code, 0);
    }

    /// The subscription feed and `remote_state` mirror `TabBridge`: a
    /// vetted `ssh_begin` opens the context the bridge reports, the
    /// remote shell's own `init_shell` marks it `bootstrapped`,
    /// `ssh_end` closes it — and `OSC 133` marks reach subscribers with
    /// their absolute line.
    #[test]
    fn subscription_and_remote_state_mirror_tabbridge() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);
        let events = bridge.subscribe().expect("subscribe");
        assert!(
            matches!(bridge.remote_state(), Ok(RemoteState::Local)),
            "a fresh hidden session is local"
        );

        // A vetted ssh_begin: the session's allocated id must echo.
        let sid = hidden.session_id().to_owned();
        bridge
            .send_bytes(&hook_line(&format!(
                r#"{{"hook":"ssh_begin","value":{{"session_id":"{sid}","target":"u@prod","hop":"{sid}.1.1","control_path":"/tmp/ait-hl/%C"}}}}"#
            )))
            .unwrap();
        wait_until(&bridge, |b| {
            matches!(b.remote_state(), Ok(RemoteState::Remote { .. }))
        });
        // The remote shell's OWN integration lands through a
        // hop-attributed init_shell.
        bridge
            .send_bytes(&hook_line(&format!(
                r#"{{"hook":"init_shell","value":{{"session_id":"{sid}","hop":"{sid}.1.1"}}}}"#
            )))
            .unwrap();
        wait_until(&bridge, |b| {
            matches!(
                b.remote_state(),
                Ok(RemoteState::Remote {
                    bootstrapped: true,
                    ..
                })
            )
        });
        match bridge.remote_state().unwrap() {
            RemoteState::Remote {
                target,
                control_path,
                ..
            } => {
                assert_eq!(target, "u@prod");
                assert_eq!(control_path.as_deref(), Some("/tmp/ait-hl/%C"));
            }
            state => panic!("expected Remote, got {state:?}"),
        }

        // Marks reach the subscriber with their absolute line — feed
        // `grid_lines` the same coordinate space.
        bridge.send_bytes(b"printf '\\033]133;D;0\\007'\n").unwrap();
        let mark = recv_until(&events, |event| {
            matches!(event, BridgeEvent::PromptMark { kind: 'D', .. })
        });
        let BridgeEvent::PromptMark { line, payload, .. } = mark else {
            unreachable!()
        };
        assert_eq!(payload.as_deref(), Some("0"));
        let rows = bridge
            .grid_lines(line as i32, line as i32 + 1)
            .expect("the mark's row resolves");
        assert_eq!(rows.len(), 1, "the absolute line still resolves");

        // ssh_end closes the context — back to Local.
        bridge
            .send_bytes(&hook_line(&format!(
                r#"{{"hook":"ssh_end","value":{{"session_id":"{sid}","target":"u@prod","hop":"{sid}.1.1"}}}}"#
            )))
            .unwrap();
        wait_until(&bridge, |b| {
            matches!(b.remote_state(), Ok(RemoteState::Local))
        });
    }

    /// `context()` answers the §5.6 `AgentContext` — the same assembly
    /// a visible tab's panel runs (us-30): real cwd, the spawned
    /// program's basename, and the session's own output lines.
    #[test]
    fn context_reports_cwd_shell_and_output() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);
        bridge.send_bytes(b"echo CTX$((200+24))\n").unwrap();
        wait_until(&bridge, |b| {
            String::from_utf8_lossy(&b.output_tail().unwrap()).contains("CTX224")
        });

        let context = hidden.context().expect("context answers");
        assert!(!context.cwd.is_empty(), "a real cwd: {context:?}");
        assert_eq!(context.shell, "bash");
        assert_eq!(context.os, std::env::consts::OS);
        assert!(
            context
                .output_lines
                .iter()
                .any(|line| line.contains("CTX224")),
            "the session's own output feeds context: {:?}",
            context.output_lines
        );
    }

    /// Acceptance: a dead or exited session reports through
    /// `BridgeError` exactly as `TabBridge` does — subscribers get
    /// `Exited`, `run_in_band`/`send_bytes` refuse `SessionExited`, and
    /// a dropped `HiddenSession` leaves clones reporting `Gone` — never
    /// a wedge.
    #[test]
    fn exited_and_dropped_sessions_report_bridge_errors() {
        let Some(hidden) = spawn_bash() else { return };
        let bridge = hidden.bridge();
        wait_integrated(&bridge);
        let events = bridge.subscribe().expect("subscribe");

        bridge.send_bytes(b"exit\n").unwrap();
        recv_until(&events, |event| matches!(event, BridgeEvent::Exited));

        // The drain observed the exit — both typed paths refuse.
        wait_until(&bridge, |b| {
            matches!(b.run_in_band(b"echo hi"), Err(BridgeError::SessionExited))
        });
        assert!(
            matches!(bridge.send_bytes(b"x"), Err(BridgeError::SessionExited)),
            "send_bytes on a dead session"
        );

        // Snapshots still answer against the dead session's grid — the
        // drain keeps servicing until the owner drops.
        assert!(bridge.output_tail().is_ok());
        assert!(bridge.integration_state().is_ok());

        drop(hidden);
        assert!(
            matches!(bridge.integration_state(), Err(BridgeError::Gone)),
            "a dropped service reports Gone"
        );
        assert!(matches!(bridge.run_in_band(b"x"), Err(BridgeError::Gone)));
    }
}
