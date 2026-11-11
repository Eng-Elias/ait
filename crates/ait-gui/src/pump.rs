//! The GUI's drain/wakeup plumbing (Spec R13 §5.12): the headless
//! drain loop's counterpart for a windowed app.
//!
//! `ait-term`'s pull API gives the frontend no waker — `drain_events`
//! is a nonblocking channel read — so [`headless`](ait_app::headless)'s
//! answer is the model: a thread that drains the app's sources on a
//! ~5 ms cadence. The GUI's version drains the same set the TUI's tick
//! drains, in the same order — settings watcher → skills watcher →
//! suggester events → ctl → session events → agent events → transcripts
//! → persisted blocks —
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
/// settings watcher → skills → suggester → ctl → `drain_events` (which folds
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
        suggester,
        suggestion,
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

    // Suggester events on the same cadence (Spec R04): `Delta`s stream
    // into the pinned tab's session under one paste envelope (the
    // shell's own echo IS the live preview — the grid paints it), a
    // terminal `Done`/`Error` releases the pin, and a non-cancelled
    // `Error` toasts. `apply` per event (rather than the `drain`
    // batch) is what lets the pass flag the repaint it produced.
    while let Ok(event) = suggester.events().try_recv() {
        activity.session = true;
        suggestion.apply(tabs, event, toast);
    }

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
