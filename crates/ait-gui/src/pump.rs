//! The GUI's drain/wakeup plumbing (Spec R13 §5.12): the headless
//! drain loop's counterpart for a windowed app.
//!
//! `ait-term`'s pull API gives the frontend no waker — `drain_events`
//! is a nonblocking channel read — so [`headless`](ait_app::headless)'s
//! answer is the model: a thread that drains the app's sources on a
//! ~5 ms cadence. The GUI's version drains the same set the TUI's tick
//! drains, in the same order — settings watcher → skills watcher → ctl
//! → session events → agent events → transcripts → persisted blocks —
//! and its "paint" step is `ctx.request_repaint()`, fired only when a
//! pass actually observed something (a folded event, grid damage beyond
//! the cursor cell, a moved cursor, a serviced ctl request, a bumped
//! agent-panel revision, a config/skills change). An idle window
//! therefore paints no frames: egui sleeps, the pump sleeps, only the
//! PTY reader works.
//!
//! [`Shared`](crate::app::Shared) is the single mutex: the UI locks it
//! for one `logic`/`ui` call, the pump per drain pass — never held
//! across the sleep or the `request_repaint`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ait_app::config::{self, LiveSettings};
use ait_app::ctl::{CtlServer, FollowUp};
use ait_app::profile;
use ait_app::status::{notification_text, Toast};
use ait_config::SettingsWatcher;
use ait_term::{Cursor, LiveEvent, SessionEvent};
use eframe::egui::Context;

use crate::app::Shared;
use crate::grid::damage_needs_repaint;

/// The pump's drain cadence — the same 5 ms `headless::DRAIN_POLL`
/// order that bounds `TabBridge` latency in the TUI.
const DRAIN_POLL: Duration = Duration::from_millis(5);

/// Whether a drain pass produced anything the window should paint —
/// the pump's only contract with egui.
#[derive(Debug, Default)]
pub struct PumpActivity {
    /// Sessions folded events, reported damage beyond the cursor, or
    /// changed cursor state this pass.
    pub session: bool,
    /// A ctl request was serviced (`FollowUp` arrived).
    pub ctl: bool,
    /// An agent panel produced an event (revision bump → panel region).
    pub agent: bool,
    /// Settings or skills changed (reload adopt, catalog refresh, or
    /// an error toast).
    pub config: bool,
}

impl PumpActivity {
    /// Whether anything this pass needs a frame.
    pub fn any(&self) -> bool {
        self.session || self.ctl || self.agent || self.config
    }
}

/// The pump thread's handle — spawned by [`Pump::start`], joined on
/// window close. `Drop` stops it too, so a failed `eframe` shutdown
/// can't orphan a drain thread holding a dead `Context`.
pub struct Pump {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Pump {
    /// Start the drain thread against the shared [`Shared`].
    ///
    /// The ctl server and the settings watcher are *owned* by the pump
    /// — only the drain pass ever touches them (the UI's ctl socket
    /// path went into the spawn template before the manager started).
    /// `ctx` is cloned onto the thread: `egui::Context` is an
    /// internally-shared handle built for exactly this — the wake side
    /// of `request_repaint` from a worker.
    pub fn start(
        ctx: Context,
        shared: Arc<Mutex<Shared>>,
        ctl: Option<CtlServer>,
        watcher: Option<SettingsWatcher>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("ait-gui-pump".into())
            .spawn(move || pump_loop(ctx, shared, ctl, watcher, thread_stop))
            .expect("spawn ait-gui-pump");
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// Signal the thread and join it.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The drain loop: one pass over every source, a repaint request iff
/// the pass was productive, then sleep. The lock is held for the pass
/// only — never across the sleep or the repaint request (egui wakes
/// the UI thread on the caller's stack).
fn pump_loop(
    ctx: Context,
    shared: Arc<Mutex<Shared>>,
    mut ctl: Option<CtlServer>,
    watcher: Option<SettingsWatcher>,
    stop: Arc<AtomicBool>,
) {
    // Per-tab snapshots keyed by the stable `Tab::id` — positional
    // indexes shift on close.
    let mut cursors: HashMap<u64, Cursor> = HashMap::new();
    let mut revisions: HashMap<u64, u64> = HashMap::new();
    while !stop.load(Ordering::Relaxed) {
        let activity = drain_pass(
            &ctx,
            &shared,
            ctl.as_mut(),
            watcher.as_ref(),
            &mut cursors,
            &mut revisions,
        );
        if activity.any() {
            ctx.request_repaint();
        }
        std::thread::sleep(DRAIN_POLL);
    }
}

/// One drain pass — the TUI tick's drain order, verbatim:
/// settings watcher → skills → ctl → `drain_events` (which folds
/// `observe_event` and services each tab's `ShellBridge` internally)
/// → `drain_agent_events` → `drain_transcripts` →
/// `take_persisted_blocks` → the per-tab damage/cursor/revision diff.
fn drain_pass(
    ctx: &Context,
    shared: &Arc<Mutex<Shared>>,
    ctl: Option<&mut CtlServer>,
    watcher: Option<&SettingsWatcher>,
    cursors: &mut HashMap<u64, Cursor>,
    revisions: &mut HashMap<u64, u64>,
) -> PumpActivity {
    let mut activity = PumpActivity::default();
    let mut shared = match shared.lock() {
        Ok(shared) => shared,
        // A poisoned lock means the other side panicked mid-pass —
        // report activity so the UI thread's own recovery at least
        // gets a frame.
        Err(_) => {
            return PumpActivity {
                session: true,
                ..PumpActivity::default()
            };
        }
    };
    let Shared {
        tabs,
        config,
        store,
        toast,
        skills,
        profile,
        picker,
        ..
    } = &mut *shared;

    // Settings reload (Spec R03 us-10/11): a good save adopts the new
    // `LiveSettings` and replaces the NEW-session template — existing
    // tabs keep spawn-time values; a broken save toasts and keeps
    // last-good.
    if let Some(watcher) = watcher {
        while let Ok(event) = watcher.receiver().try_recv() {
            activity.config = true;
            match event {
                Ok(settings) => {
                    tabs.set_session_template(config::session_template(&settings));
                    let vanished = profile::active_missing(config.settings(), &settings, profile)
                        .then(|| profile.clone());
                    *config = LiveSettings::adopt(settings);
                    *picker = None;
                    if let Some(name) = vanished {
                        *toast = Some(Toast::new(
                            format!("execution profile \"{name}\" no longer declared"),
                            Instant::now(),
                        ));
                    }
                }
                Err(error) => {
                    *toast = Some(Toast::new(
                        format!("config error: {}", error.message()),
                        Instant::now(),
                    ));
                }
            }
        }
    }
    // The skills catalog drains beside it (§5.10.2): each debounced
    // burst refreshes what the next `AgentRequest` snapshots.
    let toast_before = toast.is_some();
    skills.drain(toast);
    activity.config |= toast.is_some() != toast_before;

    // ctl (Spec R11 §5.10.5): completed requests dispatch over the
    // manager; `OpenedTab` registers the spawned tab's store row like
    // `Alt+T` does.
    if let Some(server) = ctl {
        for follow in server.drain(tabs) {
            activity.ctl = true;
            match follow {
                FollowUp::OpenedTab => {
                    if let Some(tab) = tabs.active_mut() {
                        if let Some(id) = store.session_started(toast, tab.title(), tab.cwd()) {
                            tab.set_store_session(id);
                        }
                    }
                }
            }
        }
    }

    // Sessions: `drain_events` folds each `LiveEvent` into the tab's
    // bookkeeping AND services its `ShellBridge` — the headless drain's
    // `observe → service_bridge`, folded into the manager's pass so
    // the ordering can't drift. The returned stream still carries the
    // events the FRONTEND renders: `Notification`s toast (OSC 9/777)
    // and `ClipboardWrite`s honor the OSC 52 request the session
    // opted into.
    for tab_event in tabs.drain_events() {
        activity.session = true;
        match tab_event.event {
            // OSC 9/777: the child asked for a notification. A
            // background tab's toast is prefixed with its `N:title`
            // label so it surfaces with attribution (Spec R02 user
            // story 16) — the TUI's status-line rule, verbatim.
            LiveEvent::Osc(SessionEvent::Notification { title, message }) => {
                let source = if tab_event.tab == tabs.active_index() {
                    None
                } else {
                    tabs.tabs()
                        .get(tab_event.tab)
                        .map(|tab| tab.label(tab_event.tab))
                };
                *toast = Some(Toast::new(
                    notification_text(source.as_deref(), title.as_deref(), &message),
                    Instant::now(),
                ));
            }
            // Child-app OSC 52 (`[terminal] osc52` opt-in, Spec R02):
            // the GUI IS the host, so the request writes the one
            // system clipboard egui exposes — `Selection` (X11
            // primary) has no egui counterpart and collapses onto it.
            LiveEvent::Osc(SessionEvent::ClipboardWrite { contents, .. }) => {
                ctx.copy_text(contents);
            }
            _ => {}
        }
    }
    // Agent events on the same cadence (Spec R06) — a streaming
    // request repaints the panel region even while the grid is idle.
    tabs.drain_agent_events();
    // The transcript mirror and the sealed-block insert — Spec R08's
    // two store drains, on the same pass.
    tabs.drain_transcripts(store, toast);
    let rows = tabs.take_persisted_blocks();
    if !rows.is_empty() {
        activity.session = true;
        store.insert_blocks(toast, &rows);
    }

    // The repaint decision: damage beyond the cursor cell, a moved
    // cursor, or a bumped panel revision — the same
    // cursor-only-damage filter the TUI's `damaged_beyond_cursor`
    // applies, because the underlying terminal always damages the
    // cursor cell and trusting that alone is the idle-spin trap.
    cursors.retain(|id, _| tabs.tabs().iter().any(|tab| tab.id == *id));
    revisions.retain(|id, _| tabs.tabs().iter().any(|tab| tab.id == *id));
    for tab in tabs.tabs_mut() {
        let cursor = tab.cursor();
        let last = cursors.insert(tab.id, cursor);
        // `session.damage()` consumes the report — this pass is its
        // only consumer (the UI repaints whole panes; egui's immediate
        // mode has no per-cell patch pass to feed it).
        if damage_needs_repaint(&tab.session.damage(), cursor) || last != Some(cursor) {
            activity.session = true;
        }
        let revision = tab.agent_panel().map(|panel| panel.revision()).unwrap_or(0);
        if revisions
            .insert(tab.id, revision)
            .is_some_and(|old| old != revision)
        {
            activity.agent = true;
        }
    }

    activity
}

#[cfg(all(test, unix))]
mod tests {
    //! The drain-pass seams a headless check can honestly cover (Spec
    //! R13 §5.12, R13-T7): a real `CtlServer` socket answering real
    //! `ait ctl` requests through [`drain_pass`], the settings
    //! watcher adopting/broken-toasting through the same pass, and
    //! sealed blocks landing in a real SQLite store. The windowed
    //! half — `GuiApp::new` needing a `CreationContext` — has no
    //! headless seam; these tests drive the pump's `Shared` exactly
    //! the way `new`+`Pump::start` wire it.
    use super::*;
    use ait_app::ctl::{decode_response, encode_request, Request, Response};
    use ait_app::skills::LiveSkills;
    use ait_app::store::StoreHandle;
    use ait_app::tabs::{SessionTemplate, Tab, TabManager};
    use ait_config::Settings;
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;

    /// Options spawning an interactive `/bin/sh` — the cheap real
    /// session the ait-app ctl tests also drive.
    fn sh() -> ait_term::LiveSessionOptions {
        ait_term::LiveSessionOptions {
            program: Some(std::ffi::OsString::from("/bin/sh")),
            ..Default::default()
        }
    }

    /// A `sh -c` script emitting one `A B C D` block (`echo hi`,
    /// output `hi`, exit `3`) — the same shape ait-app's ctl and
    /// tabs tests stage.
    fn marks_script() -> ait_term::LiveSessionOptions {
        ait_term::LiveSessionOptions {
            program: Some(std::ffi::OsString::from("/bin/sh")),
            args: vec![
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from(
                    "printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
                     printf 'echo hi'; sleep 0.2; \
                     printf '\\n\\033]133;C\\007'; sleep 0.2; \
                     printf 'hi\\n'; sleep 0.2; \
                     printf '\\033]133;D;3\\007'; sleep 0.5",
                ),
            ],
            ..Default::default()
        }
    }

    /// A manager with one interactive `/bin/sh` tab, `template`'s
    /// remaining fields filled in — `ctl_socket` included.
    fn manager(options: ait_term::LiveSessionOptions, ctl_socket: Option<&Path>) -> TabManager {
        TabManager::spawn_with(
            ait_term::Size::new(120, 10),
            SessionTemplate {
                options,
                ctl_socket: ctl_socket.map(|path| path.to_path_buf()),
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// The `Shared` `GuiApp::new` builds, minus the eframe context —
    /// fields in the same order, `store`/`tabs` injected.
    fn shared(tabs: TabManager, store: StoreHandle) -> Arc<Mutex<Shared>> {
        Arc::new(Mutex::new(Shared {
            tabs,
            config: LiveSettings::adopt(Settings::default()),
            store,
            toast: None,
            skills: LiveSkills::new(),
            profile: profile::DEFAULT_PROFILE.to_string(),
            picker: None,
            copied_text: None,
        }))
    }

    /// `GuiApp::new`'s `terminal_sessions` registration for a just-
    /// spawned active tab — the same fold `drain_pass` runs on
    /// `FollowUp::OpenedTab`.
    fn register_session(shared: &Arc<Mutex<Shared>>) {
        let mut shared = shared.lock().unwrap();
        let Shared {
            tabs, store, toast, ..
        } = &mut *shared;
        if let Some(tab) = tabs.active_mut() {
            if let Some(id) = store.session_started(toast, tab.title(), tab.cwd()) {
                tab.set_store_session(id);
            }
        }
    }

    /// A `StoreHandle` on a tempdir database — `Store::open`'s seam
    /// keeps the real XDG path untouched.
    fn temp_store() -> (tempfile::TempDir, std::path::PathBuf, StoreHandle) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("store.db");
        let store = StoreHandle::from_store(ait_store::Store::open(&db).unwrap());
        (dir, db, store)
    }

    /// Generous bound for every wait below (the ait-app live-test
    /// convention): tiny `/bin/sh` runs finish far under it.
    const TIMEOUT: Duration = Duration::from_secs(15);

    /// All text in a session's grid (scrollback then viewport).
    fn grid_text(tab: &Tab) -> String {
        let grid = tab.session.grid();
        grid.scrollback_rows()
            .chain(grid.visible_rows())
            .map(|row| row.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One pass over the test's `Shared` — `drain_pass`'s per-tab
    /// snapshots bundled so each call site reads like the pump loop's.
    struct Pass {
        cursors: HashMap<u64, Cursor>,
        revisions: HashMap<u64, u64>,
    }

    impl Pass {
        fn new() -> Self {
            Self {
                cursors: HashMap::new(),
                revisions: HashMap::new(),
            }
        }

        fn run(
            &mut self,
            shared: &Arc<Mutex<Shared>>,
            ctl: Option<&mut CtlServer>,
            watcher: Option<&SettingsWatcher>,
        ) -> PumpActivity {
            drain_pass(shared, ctl, watcher, &mut self.cursors, &mut self.revisions)
        }

        /// Passes until `pred` observes the shared state, or the
        /// timeout kills the test.
        fn until(
            &mut self,
            shared: &Arc<Mutex<Shared>>,
            ctl: Option<&mut CtlServer>,
            watcher: Option<&SettingsWatcher>,
            pred: impl Fn(&mut Shared) -> bool,
        ) {
            let mut ctl = ctl;
            let deadline = Instant::now() + TIMEOUT;
            loop {
                self.run(shared, ctl.as_deref_mut(), watcher);
                if shared.lock().is_ok_and(|mut shared| pred(&mut shared)) {
                    return;
                }
                assert!(Instant::now() < deadline, "timed out waiting");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    /// The client half of `ait ctl`: connect, write one request line,
    /// keep draining passes until the server's reply flushes (the
    /// request's flight time can span a pass under load) and decode
    /// it — the socket round trip `run_client` makes, without the TUI
    /// binary's process boundary.
    fn ctl_round_trip(
        shared: &Arc<Mutex<Shared>>,
        server: &mut CtlServer,
        pass: &mut Pass,
        request: &Request,
    ) -> Response {
        let mut conn = UnixStream::connect(server.socket_path()).unwrap();
        conn.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        writeln!(conn, "{}", encode_request(request)).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        let mut buf = String::new();
        loop {
            match conn.read_to_string(&mut buf) {
                Ok(_) => break,
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    assert!(Instant::now() < deadline, "no ctl reply in {buf:?}");
                    pass.run(shared, Some(server), None);
                }
                Err(error) => panic!("ctl read failed: {error}"),
            }
        }
        decode_response(&buf).unwrap_or_else(|error| panic!("{error}: {buf:?}"))
    }

    /// §5.10.5 against the GUI frontend (R13-T7's AC): the bound
    /// socket a `Pump` would drain answers `open_tab` (with the
    /// `OpenedTab` follow-up registering the new tab's
    /// `terminal_sessions` row), `send` lands input in the target's
    /// PTY — and the spawned session carries `AIT_CTL_SOCKET` — and
    /// `last_block` routes through dispatch (a blockless tab earns the
    /// same `{"ok":false}` the TUI answers). One test owns the ONE
    /// `CtlServer::bind()` this process can make — the socket path is
    /// pid-scoped, so a second bind would unlink the first's file.
    #[test]
    fn ctl_verbs_round_trip_through_the_drain_pass() {
        let mut server = CtlServer::bind().expect("bind the control socket");
        let (_dir, _db, store) = temp_store();
        let shared = shared(manager(sh(), Some(server.socket_path())), store);
        register_session(&shared);
        assert!(
            shared
                .lock()
                .unwrap()
                .tabs
                .active()
                .unwrap()
                .store_session()
                .is_some(),
            "the boot tab's store row lands like GuiApp::new's"
        );
        let mut pass = Pass::new();

        // `ait ctl open-tab` — dispatch spawns, the response carries
        // the new `AIT_SESSION_ID`, and the pass's `OpenedTab`
        // follow-up registers the new active tab's store row.
        let opened = ctl_round_trip(&shared, &mut server, &mut pass, &Request::OpenTab);
        assert!(opened.ok, "{opened:?}");
        let new_id = opened.session_id.expect("open_tab answers the new id");
        {
            let shared = shared.lock().unwrap();
            assert_eq!(shared.tabs.len(), 2);
            let tab = shared.tabs.active().unwrap();
            assert_eq!(tab.id.to_string(), new_id, "the spawned tab is active");
            assert!(
                tab.store_session().is_some(),
                "the OpenedTab follow-up bound its terminal_sessions row"
            );
        }

        // `ait ctl send` — bytes land in the (newly opened, now
        // active) tab's PTY and run: the echo proves the input, and
        // `$AIT_CTL_SOCKET` proves the spawn exported the socket path
        // the template carried.
        let sent = ctl_round_trip(
            &shared,
            &mut server,
            &mut pass,
            &Request::Send {
                tab: None,
                text: "echo G$((6*7)):${AIT_CTL_SOCKET##*/}".to_owned(),
                enter: true,
            },
        );
        assert!(sent.ok, "{sent:?}");
        let socket_name = server
            .socket_path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        pass.until(&shared, None, None, |shared| {
            grid_text(&shared.tabs.tabs()[1]).contains("G42:")
        });
        let text = grid_text(&shared.lock().unwrap().tabs.tabs()[1]);
        assert!(text.contains("G42:"), "{text}");
        assert!(
            text.contains(&socket_name),
            "AIT_CTL_SOCKET exported into the spawn: {text}"
        );

        // `ait ctl last-block` — routed to dispatch; an interactive
        // sh seals no blocks, so the answer is the TUI's same
        // `{"ok":false}` (ait-app's dispatch tests cover the happy
        // path over a marks-staged tab).
        let block = ctl_round_trip(
            &shared,
            &mut server,
            &mut pass,
            &Request::LastBlock { tab: None },
        );
        assert!(!block.ok);
        assert!(
            block
                .error
                .as_deref()
                .is_some_and(|e| e.contains("no sealed blocks")),
            "{block:?}"
        );
    }

    /// §5.12 us-17 inside the GUI: a `settings.toml` write debounces
    /// onto the watcher's channel and the next pass adopts it — the
    /// status line's `LiveSettings` is the observable half — while a
    /// broken write toasts `config error:` and keeps last-good. The
    /// template half (`set_session_template`, including the
    /// `ctl_socket` carry-over) is ait-app's tested contract.
    #[test]
    fn a_settings_write_hot_reloads_through_the_drain_pass() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        std::fs::write(&path, "[terminal]\nscrollback_lines = 5000\n").unwrap();
        let watcher = ait_config::watch_from(&path).expect("watch the temp settings");
        let shared = shared(manager(sh(), None), StoreHandle::disabled());
        let mut pass = Pass::new();

        std::fs::write(&path, "[terminal]\nscrollback_lines = 1234\n").unwrap();
        pass.until(&shared, None, Some(&watcher), |shared| {
            shared.config.settings().terminal.scrollback_lines == Some(1234)
        });

        // A broken save keeps last-good and surfaces the same
        // `config error:` toast the TUI's reload shows.
        std::fs::write(&path, "not = [valid\n").unwrap();
        pass.until(&shared, None, Some(&watcher), |shared| {
            shared
                .toast
                .as_ref()
                .is_some_and(|toast| toast.text().contains("config error:"))
        });
        assert_eq!(
            shared
                .lock()
                .unwrap()
                .config
                .settings()
                .terminal
                .scrollback_lines,
            Some(1234),
            "the broken write never replaced last-good"
        );
    }

    /// §5.12 us-18 inside the GUI: a sealed block flows
    /// `take_persisted_blocks` → `insert_blocks` in the same pass and
    /// lands as a real `blocks` row keyed to the tab's
    /// `terminal_sessions` row — read back through a second store
    /// connection after the handle drops (transcript rows ride the
    /// same pass's `drain_transcripts`; ait-app covers their store
    /// contract).
    #[test]
    fn sealed_blocks_persist_through_the_drain_pass() {
        let (_dir, db, mut store) = temp_store();
        let mut tabs = manager(marks_script(), None);
        let mut toast = None;
        let row = {
            let tab = tabs.active_mut().unwrap();
            let id = store
                .session_started(&mut toast, tab.title(), tab.cwd())
                .expect("the session row inserts");
            tab.set_store_session(id);
            id
        };
        let shared = shared(tabs, store);
        let mut pass = Pass::new();

        // The marks script seals one block; the same pass that sees it
        // is the pass that persists it.
        pass.until(&shared, None, None, |shared| {
            shared.tabs.tabs()[0].blocks().iter().any(|b| b.completed())
        });
        assert!(
            shared.lock().unwrap().toast.is_none(),
            "a healthy store never toasts"
        );
        drop(shared);

        // Read the rows back through a fresh connection — persistence,
        // not in-memory bookkeeping.
        let store = ait_store::Store::open(&db).unwrap();
        assert!(store.terminal_session(row).unwrap().is_some());
        let blocks = store.list_blocks(row).unwrap();
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert_eq!(blocks[0].command.as_deref(), Some("echo hi"));
        assert_eq!(blocks[0].exit, Some(3));
    }
}
