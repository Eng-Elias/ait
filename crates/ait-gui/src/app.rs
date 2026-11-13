//! The GUI application (Spec R13 §5.12): [`GuiApp`] is the `eframe::App`
//! — the same five regions the TUI carves (`layout`), the same
//! keymap-before-PTY input dispatch the TUI's `handle_event` runs, and
//! the same `Shared` app state the pump's drain thread owns between
//! frames.
//!
//! Ownership split: [`Shared`] is everything a drain pass or an input
//! event can mutate (tabs, live settings, store, toast, skills, the
//! armed profile, the open picker, the copied-selection buffer) — it
//! lives behind one `Arc<Mutex<…>>` the UI and pump threads share.
//! [`GuiApp`] itself holds only frontend-local state: the theme, the
//! zoom/cell metrics, the chrome heights, the last-computed band size
//! the resize detector diffs, the last grid-pane rect the wheel/selection
//! hit-tests use, and the quit flag.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ait_agent::ApprovalVerdict;
use ait_agent::CommandSuggester;
use ait_app::agent_panel::{self, AgentPanel};
use ait_app::blocks::BlockStatus;
use ait_app::config::{self, LiveSettings};
use ait_app::conversations::{self, ConversationPicker};
use ait_app::ctl::{self, CtlServer};
use ait_app::dispatch;
use ait_app::integration::IntegrationState;
use ait_app::keymap::{self, AppAction, CapabilityTier, Resolution};
use ait_app::profile::{self, ProfilePicker};
use ait_app::scroll::WHEEL_LINES;
use ait_app::search::{FindBar, SearchHit};
use ait_app::selection::{self, GridPoint};
use ait_app::skills::LiveSkills;
use ait_app::status::{self, block_text, status_text, StatusLine, Toast};
use ait_app::store::StoreHandle;
use ait_app::suggest::{self, SuggestionPin};
use ait_app::tabs::TabManager;
use ait_config::Settings;
use ait_term::{Key, KeyEvent, Modifiers, Size};
use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2, ViewportCommand,
};

use crate::browser::BrowserState;
use crate::detect;
use crate::explorer::ExplorerState;
use crate::grid::{chrome, GridTheme};
use crate::keys::{translate_key, translate_text};
use crate::layout::{
    chrome_regions, grid_point, grid_size, plan_block_chrome, session_size, split_content,
    tab_chips, BlockDivider, BlockLabel, CellMetrics, Chrome, GuiMetrics, CHIP_PAD, FIND_BAR_H,
};
use crate::paint::{paint_grid, DetectOverlay, FindOverlay, Overlays};
use crate::pane::{pane, pane_chord, PaneHost, PaneId, SidePane, PANES};
use crate::panel::{self, Gate, Tone};
use crate::pump::Pump;
use crate::webview::WebSlot;

/// How long before a live toast's expiry the UI asks for one more
/// frame — the only *timed* repaint the GUI schedules (everything
/// else is activity-driven; an idle window with a toast showing still
/// needs exactly one frame when the toast ages out).
const TOAST_REFRESH: Duration = Duration::from_millis(500);

/// How still the content band must sit before a winsize reflow
/// applies — a reflow walks each session's full scrollback, so
/// resizing on every frame of a window drag is the resize jank.
/// `pending_band` applies once the band is unchanged this long;
/// a `request_repaint_after` wakes the frame that applies it even
/// after the pointer stops (no input, no frame).
const RESIZE_SETTLE: Duration = Duration::from_millis(120);

/// The app state the pump's drain thread and the UI thread share —
/// the GUI's `AppState` minus the frontend-local fields. One mutex:
/// the drains and the paint are both short passes, and sharing one
/// lock keeps the "drains never run mid-paint" invariant free.
pub struct Shared {
    /// The session tabs — same `TabManager` the TUI drives.
    pub tabs: TabManager,
    /// The last-good settings (hot reload lands via the pump).
    pub config: LiveSettings,
    /// The §5.8 store facade — `disabled()` on an open failure.
    pub store: StoreHandle,
    /// The live toast (status-line priority).
    pub toast: Option<Toast>,
    /// The §5.10.2 skills catalog — drained by the pump.
    pub skills: LiveSkills,
    /// The armed execution profile's name (Spec R07 §5.7).
    pub profile: String,
    /// The open `Alt+P` profile switcher, if any — app-global and
    /// ephemeral like the TUI's (dropped on settings reload).
    pub picker: Option<ProfilePicker>,
    /// The open `Alt+R` conversation picker, if any (Spec R08 §5.8) —
    /// a snapshot of the store's rows taken at open, like the TUI's.
    pub conversations: Option<ConversationPicker>,
    /// ait's own copied selection — `Shift+Insert`'s paste source,
    /// mirroring the TUI (the host clipboard rides `ctx.copy_text`
    /// alongside, since the GUI has a real clipboard API).
    pub copied_text: Option<String>,
    /// The channel-based command suggester (Spec R04): the `# `+Enter
    /// trigger, `Alt+Enter` and the §5.10.4 classifier queue a
    /// [`SuggestRequest`](ait_agent::SuggestRequest) on it; its event
    /// channel is the pump's streamed-suggestion source.
    pub suggester: CommandSuggester,
    /// The in-flight suggestion, pinned to the tab that fired it
    /// (Spec R04 us-14) — the same one-request state machine the TUI
    /// delegates to.
    pub suggestion: SuggestionPin,
    /// The find bar's state while it's open (Spec R13-T4) — `Some` IS
    /// the bar's visibility. It lives in `Shared` (not `GuiApp`) so the
    /// pump's drain pass can rescan hits when session output shifts
    /// the grid under them.
    pub find: Option<FindBar>,
    /// The side region's pane host (Spec R14 §5.13): which auxiliary
    /// pane is selected — app-global and ephemeral (us-31), following
    /// the active tab while each tab's own agent claim resolves per
    /// tab.
    pub panes: PaneHost,
    /// The file explorer's model (Spec R14 §5.13, R14-T2) — the
    /// listing, selection, preview and navigation the `explorer`
    /// pane's `key`/`paint` drive; re-synced to the active tab's
    /// `cwd_for` answer on every touch.
    pub explorer: ExplorerState,
    /// The browser pane's model (Spec R14 §5.13, R14-T3): the address
    /// entry, the navigation history, the queued `NavCommand`s the
    /// frame's webview sync drains and the embed verdict the probe
    /// reports back — app-global and ephemeral like the selection.
    /// The `!Send` wry handle itself is `GuiApp`'s, never this
    /// struct's (the pump's drain thread shares `Shared`).
    pub browser: BrowserState,
    /// Test-driver input queued for the next frame's dispatch (Spec
    /// R15): the pump's test-socket drain pushes translated verbs here
    /// and `dispatch_events` prepends them to the frame's native
    /// events. Feature-gated — a default build carries no injection
    /// path.
    #[cfg(feature = "test-driver")]
    pub injected: std::collections::VecDeque<crate::test_driver::Injected>,
    /// The hit-target registry (Spec R15-T3): the paint pass's
    /// name → rect map a `pointer`/`click` `target` resolves against
    /// and the `dump` reports. Always present so the paint sites stay
    /// unconditional — the map inside only exists (and only fills)
    /// in `test-driver` builds.
    pub hits: HitTargets,
    /// The Ctrl-hover affordance's last-painted resolution (Spec
    /// R15-T4): `paint_grid_pane` records the plain-Ctrl gate and
    /// the detected token so `dump` answers "what would Ctrl+click
    /// do" without driving one. Feature-gated like `injected` — a
    /// release build has no socket to read it.
    #[cfg(feature = "test-driver")]
    pub detect_hover: crate::test_driver::DetectHover,
    /// The cell metrics `paint_grid_pane` last laid out (Spec
    /// R15-T4) — the dump's `grid.cell`, so a scenario turns a
    /// `(row, column)` into a pointer position without guessing.
    /// Feature-gated like `detect_hover`.
    #[cfg(feature = "test-driver")]
    pub cell_metrics: Option<(f32, f32)>,
    /// The `wait_dump` barrier's queued half (Spec R15): `dispatch`
    /// bumps this for every request that queues frame-folded input
    /// — `dump`'s `input_seq` thus counts everything serviced before
    /// it. Feature-gated like `injected`.
    #[cfg(feature = "test-driver")]
    pub input_seq: u64,
    /// The `wait_dump` barrier's delivered half: `ui` sets this to
    /// `input_seq` at a frame's end once `injected` is drained AND
    /// the pointer plugin's queue emptied — `input_done >=
    /// input_seq` in a dump IS "every accepted request folded
    /// through a completed frame", the barrier negative assertions
    /// wait on instead of a guessed sleep.
    #[cfg(feature = "test-driver")]
    pub input_done: u64,
}

/// The test-driver's hit-target registry (Spec R15-T3) — semantic
/// name → the rect its `interact` owned on the last painted frame.
/// `register`/`clear` are inline no-ops outside `test-driver` builds
/// (no map exists to populate), so a production build pays nothing
/// for the names the paint pass carries anyway.
#[derive(Debug, Default)]
pub struct HitTargets {
    /// The names registered so far this frame — the dump's `hits`
    /// and `target` resolution read exactly this.
    #[cfg(feature = "test-driver")]
    map: std::collections::BTreeMap<String, Rect>,
}

impl HitTargets {
    /// Register `name` at `rect` — the `interact` site owns both, so
    /// a name's rect IS the hit-tested rect. `name` is a thunk: only
    /// `test-driver` builds have a map to fill, so release builds
    /// never run the `format!`/clone the name costs.
    #[inline]
    pub fn register(&mut self, name: impl FnOnce() -> String, rect: Rect) {
        #[cfg(feature = "test-driver")]
        self.map.insert(name(), rect);
        #[cfg(not(feature = "test-driver"))]
        let _ = (name, rect);
    }

    /// Forget the last frame's targets — `ui` clears at paint start
    /// so `hits` is always exactly one frame's registration.
    #[inline]
    pub fn clear(&mut self) {
        #[cfg(feature = "test-driver")]
        self.map.clear();
    }

    /// `name` → its last-painted rect — the `target` resolution seam
    /// (test-driver only).
    #[cfg(feature = "test-driver")]
    pub fn get(&self, name: &str) -> Option<Rect> {
        self.map.get(name).copied()
    }

    /// The registry's entries — the dump's `hits` field (test-driver
    /// only).
    #[cfg(feature = "test-driver")]
    pub fn entries(&self) -> impl Iterator<Item = (&String, &Rect)> {
        self.map.iter()
    }
}

impl Shared {
    /// The [`dispatch::KeyCtx`] view of this state — one bundle for
    /// the shared dispatch fns' per-event session-model access.
    fn key_ctx(&mut self) -> dispatch::KeyCtx<'_> {
        dispatch::KeyCtx {
            picker: &mut self.picker,
            conversations: &mut self.conversations,
            tabs: &mut self.tabs,
            store: &mut self.store,
            profile: &mut self.profile,
            toast: &mut self.toast,
            suggester: &self.suggester,
            suggestion: &mut self.suggestion,
            settings: self.config.settings(),
            skills: &mut self.skills,
        }
    }

    /// Whether the active tab's agent panel claims the region — the
    /// precedence input `PaneHost` resolves against; the dispatch,
    /// the paint pass and the chord toggle all ask the same question.
    pub(crate) fn agent_claiming(&self) -> bool {
        self.tabs.active().is_some_and(|tab| tab.agent_panel_open())
    }

    /// What the side region shows on the active tab right now — the
    /// agent panel's per-tab claim first, else the app-global aux
    /// selection, else the region is closed.
    pub(crate) fn shown_pane(&self) -> Option<SidePane> {
        self.panes.visible(self.agent_claiming())
    }
}

/// The GUI frontend's `eframe::App` — T3's agent/suggest wiring,
/// T4's find bar and T5's panel content all extend this struct and
/// its `paint_*` regions; the seams are named in the paint methods.
pub struct GuiApp {
    /// The shared app state — see [`Shared`].
    shared: Arc<Mutex<Shared>>,
    /// The drain/wakeup thread — `Option` so `stop` can join it.
    pump: Option<Pump>,
    /// The grid theme.
    theme: GridTheme,
    /// Zoom + base font size — the cell metrics' source.
    metrics: GuiMetrics,
    /// The chrome strips' heights.
    chrome: Chrome,
    /// The content band's cell size last frame — the resize detector:
    /// a changed band means zoom or layout moved, so `resize_all`
    /// re-derives every tab's winsize (the winsize = grid pane rule).
    last_band: Size,
    /// The last frame's grid pane — the wheel-scroll and drag-selection
    /// hit-test region (events arriving this frame test against it).
    grid_pane: Option<Rect>,
    /// The last frame's side-panel region — the wheel hit-test routes
    /// hovers over it to the panel's own conversation scroll (the
    /// layout contract's "panel wheel events must not scroll the
    /// terminal grid" rule, from the other side).
    panel_pane: Option<Rect>,
    /// The conversation rows the panel's last paint fit — a Page-unit
    /// wheel's scroll distance over the panel region.
    panel_rows: usize,
    /// Sub-line wheel distance carried between deltas, per region —
    /// touchpads emit Point deltas far under a row each; truncating
    /// every event to whole lines dropped them to zero (touchpad
    /// scroll felt dead), so each region accumulates its own
    /// remainder and scrolls the whole lines it crosses.
    grid_wheel_rem: f32,
    /// The panel arm's own carry — same reason.
    panel_wheel_rem: f32,
    /// The winsize a resize left pending — a reflow walks each
    /// session's full scrollback, so applying every frame of a window
    /// drag is the resize jank; the band applies once it sits still
    /// for `RESIZE_SETTLE` instead.
    pending_band: Option<(Size, Instant)>,
    /// The cross-frame row-galley cache — one `LayoutJob` per row,
    /// shaped once and reused while the row's plan is unchanged
    /// (REQUIREMENTS §6; the pump's damage filter decides whether a
    /// frame runs at all, this keeps the frame cheap).
    row_galleys: crate::paint::RowGalleys,
    /// The Ctrl+hover detection cache — [`detect::DetectCache`]
    /// memoizes the probe on its whole input so the filesystem
    /// `exists()` checks run once per hover position+content, not
    /// once per frame while Ctrl is held (§6).
    detect_cache: detect::DetectCache,
    /// The test-driver's memoized detection-token scan (Spec R15-T3)
    /// — the `detect.*` hit names → cell rects. Keyed on the whole
    /// input (tab, scroll, pane, cell, remote flag, every visible
    /// row's text) so the fs probes inside `detect::tokens` run only
    /// on a real change — the same §6 guard `detect_cache` applies
    /// to the Ctrl+hover, applied to the scan.
    #[cfg(feature = "test-driver")]
    detect_hits: Option<DetectHitScan>,
    /// The browser pane's webview slot (Spec R14 §5.13, R14-T3): the
    /// `!Send` wry child once the probe embeds it — frontend-local
    /// like the galley cache, since `Shared` is touched by the pump's
    /// drain thread and the webview is UI-thread-bound. Its per-frame
    /// [`WebSlot::sync`] runs post-paint.
    web: WebSlot,
    /// Set when the input dispatch decided the app should close —
    /// applied after `ui` returns so the frame can't see a
    /// half-drained state.
    quit: bool,
}

impl GuiApp {
    /// Boot: load settings, open the store, bind the ctl socket, spawn
    /// the first tab at the window's real cell geometry (the
    /// `CreationContext` already knows the viewport size and fonts), and
    /// start the pump thread against the shared state.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Result<Self, BootError> {
        let ctx = &cc.egui_ctx;
        let metrics = GuiMetrics {
            base_px: 15.0,
            zoom: 1.0,
        };
        let chrome = Chrome::default();
        // egui installs fonts on the first `Context::run` — the
        // creation callback runs BEFORE it, so `fonts_mut` (what
        // `cell_metrics` reads) panics here (eframe 0.37 calls the
        // creator pre-run). Spawn the first tab at a provisional cell
        // — typical monospace ratios, nothing the user sees: the
        // first `ui()` frame computes the real band, and the
        // `last_band` diff drives `resize_all` before a frame ever
        // paints.
        let cell = provisional_cell(&metrics);
        let regions = chrome_regions(ctx.content_rect(), chrome);
        let band = grid_size(regions.content, cell);

        // Same boot order as the TUI's `run`: settings (toasting a bad
        // file, running on defaults), then the store (degrade to
        // unpersisted), then the ctl bind so `AIT_CTL_SOCKET` lands in
        // the spawn template, then the manager.
        let (config, load_error) = match ait_config::load() {
            Ok(settings) => (LiveSettings::adopt(settings), None),
            Err(error) => (
                LiveSettings::adopt(Settings::default()),
                Some(error.message().to_string()),
            ),
        };
        let (store, store_error) = match StoreHandle::open() {
            Ok(store) => (store, None),
            Err(error) => (StoreHandle::disabled(), Some(error.to_string())),
        };
        let (ctl, ctl_error) = match CtlServer::bind() {
            Ok(server) => (Some(server), None),
            Err(error) => (None, Some(ctl::socket_error_text(error))),
        };
        // The test-driver socket (Spec R15): bound beside ctl only
        // when the feature is compiled AND the env var names its
        // path — a bind failure degrades to a boot toast exactly
        // like ctl's.
        #[cfg(feature = "test-driver")]
        let (driver, driver_error) = match crate::test_driver::TestDriver::bind_env() {
            Some(Ok(server)) => (Some(server), None),
            Some(Err(error)) => (None, Some(crate::test_driver::socket_error_text(error))),
            None => (None, None),
        };
        let mut template = config::session_template(config.settings());
        template.ctl_socket = ctl
            .as_ref()
            .map(|server| server.socket_path())
            .filter(|path| !path.as_os_str().is_empty())
            .map(|path| path.to_path_buf());
        let mut tabs = TabManager::spawn_with(session_size(band, false), template)
            .map_err(|error| BootError(error.to_string()))?;
        // The first tab's `terminal_sessions` row (Spec R08) — same
        // registration `Alt+T` performs per spawned tab.
        let mut toast = status::boot_toast(
            [
                load_error.map(|text| config::error_text(&text)),
                store_error.map(StoreHandle::unavailable_text),
                ctl_error,
                #[cfg(feature = "test-driver")]
                driver_error,
            ]
            .into_iter()
            .flatten()
            .collect(),
            Instant::now(),
        );
        let mut store = store;
        dispatch::register_active_session(&mut tabs, &mut store, &mut toast);

        let shared = Arc::new(Mutex::new(Shared {
            tabs,
            config,
            store,
            toast,
            skills: LiveSkills::new(),
            profile: profile::DEFAULT_PROFILE.to_string(),
            picker: None,
            conversations: None,
            copied_text: None,
            suggester: CommandSuggester::new(),
            suggestion: SuggestionPin::new(),
            find: None,
            panes: PaneHost::default(),
            explorer: ExplorerState::default(),
            browser: BrowserState::default(),
            #[cfg(feature = "test-driver")]
            injected: std::collections::VecDeque::new(),
            hits: HitTargets::default(),
            #[cfg(feature = "test-driver")]
            detect_hover: crate::test_driver::DetectHover::default(),
            #[cfg(feature = "test-driver")]
            cell_metrics: None,
            #[cfg(feature = "test-driver")]
            input_seq: 0,
            #[cfg(feature = "test-driver")]
            input_done: 0,
        }));
        // The pointer-injection plugin (Spec R15-T3): its `input_hook`
        // splices queued `pointer`/`click` batches into `RawInput` at
        // `begin_pass` — the only seam upstream of egui's PointerState
        // fold, so clicks hit-test like real input.
        #[cfg(feature = "test-driver")]
        ctx.add_plugin(crate::test_driver::DriverInputPlugin::default());
        let watcher = config::watcher();
        let pump = Pump::start(
            ctx.clone(),
            Arc::clone(&shared),
            ctl,
            watcher,
            #[cfg(feature = "test-driver")]
            driver,
        )
        .map_err(|error| BootError(format!("pump spawn failed: {error}")))?;
        Ok(Self {
            shared,
            pump: Some(pump),
            theme: GridTheme::default(),
            metrics,
            chrome,
            last_band: band,
            grid_pane: None,
            panel_pane: None,
            panel_rows: 0,
            grid_wheel_rem: 0.0,
            panel_wheel_rem: 0.0,
            pending_band: None,
            row_galleys: crate::paint::RowGalleys::default(),
            detect_cache: detect::DetectCache::default(),
            #[cfg(feature = "test-driver")]
            detect_hits: None,
            web: WebSlot::new(),
            quit: false,
        })
    }

    // -----------------------------------------------------------------
    // Input dispatch — the TUI `handle_event`'s GUI counterpart: native
    // events translate to `ait_term::KeyEvent`, resolve through the
    // shared keymap, and only `PassThrough` may reach the PTY.

    /// One frame's input events, in order — keys, composed text,
    /// paste, wheel. Selection drags ride `interact` in the paint pass
    /// (they need the pane rect, which is layout state).
    fn dispatch_events(&mut self, ctx: &egui::Context, shared: &mut Shared) {
        // Chord chars whose `Event::Text` twin would be the same input
        // twice: some platforms emit BOTH `Key{A, alt}` and `Text("a")`
        // for `Alt+A`, which typed `a` into the PTY beside the panel
        // the chord opened. The Key event carries the whole chord to
        // the resolver — a matching Text char in the ADJACENT Text
        // event is its twin, eaten there (winit emits the pair from
        // one KeyboardInput, so nothing else can interleave); any
        // other event closes the twin window, or a burst that types
        // right behind a chord would lose real characters.
        let mut eaten: Vec<char> = Vec::new();
        // Spec R15: the pump's test-socket drain queued injected input
        // between frames — it lands AHEAD of the frame's native events
        // (it is the older input), dispatched item by item so the
        // queue's own order holds (a `scroll` between two `key`s
        // dispatches between them). `Quit` takes the same close path
        // a resolved `AppAction::Quit` does.
        #[cfg(feature = "test-driver")]
        for item in shared.injected.drain(..).collect::<Vec<_>>() {
            match item {
                crate::test_driver::Injected::Event(event) => {
                    self.dispatch_event(ctx, shared, event, &mut eaten);
                }
                // A wheel gesture carries its own hover: real
                // `MouseWheel` events route on `pointer.hover_pos`,
                // which an injected event can't reach — egui consumed
                // the pointer state at `begin_pass` — so the verb's
                // region resolves against last frame's pane rects.
                crate::test_driver::Injected::Wheel {
                    unit,
                    delta,
                    target,
                } => {
                    let hover = match target {
                        crate::test_driver::ScrollTarget::Grid => {
                            self.grid_pane.map(|pane| pane.center())
                        }
                        crate::test_driver::ScrollTarget::Panel => {
                            self.panel_pane.or(self.grid_pane).map(|pane| pane.center())
                        }
                        crate::test_driver::ScrollTarget::At(pos) => Some(pos),
                    };
                    // Before the first painted pane the content rect
                    // stands in — a wheel still lands somewhere real.
                    let hover = hover.or_else(|| Some(ctx.content_rect().center()));
                    self.wheel(shared, ctx, unit, delta, hover);
                }
                // A viewport resize rides the real path: the window
                // manager round trip, then the band's `RESIZE_SETTLE`
                // debounce — the settle a scenario asserts IS the
                // production one.
                crate::test_driver::Injected::Resize(size) => {
                    ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
                }
                // A staged `AgentEvent` — the same `apply` fold the
                // pump's `drain_agent_events` feeds real worker
                // events through, so the parked gate card is the
                // panel's real approval state.
                crate::test_driver::Injected::AgentEvent(event) => {
                    if let Some(panel) = shared.tabs.active_mut().and_then(|tab| tab.agent.as_mut())
                    {
                        panel.apply(event);
                    }
                }
                // The pointer verbs' frame wake — the batch already
                // sits on the plugin; the marker only earns the
                // frame that delivers it.
                crate::test_driver::Injected::Repaint => {}
                crate::test_driver::Injected::Quit => self.quit = true,
            }
        }
        for event in ctx.input(|input| input.events.clone()) {
            self.dispatch_event(ctx, shared, event, &mut eaten);
        }
    }

    /// One `egui::Event` through the input dispatch — the per-event
    /// arm `dispatch_events` runs for native and (Spec R15) injected
    /// events alike; `eaten` is the frame's twin-char filter, shared
    /// across the whole stream.
    fn dispatch_event(
        &mut self,
        ctx: &egui::Context,
        shared: &mut Shared,
        event: egui::Event,
        eaten: &mut Vec<char>,
    ) {
        // Only a `Text` event consults the twin filter: winit emits
        // `Key` + `Text` adjacently from one `KeyboardInput`, so any
        // other event — the chord's own release, the next press, a
        // paste, a wheel — closes the twin window. Without this a
        // burst that types right behind a chord (injected input, a
        // scripted macro) loses real characters to a stale `eaten`.
        if !matches!(event, egui::Event::Text(_)) {
            eaten.clear();
        }
        match event {
            egui::Event::Key {
                key,
                pressed: true,
                repeat,
                modifiers,
                ..
            } => {
                // GUI-local bindings the keymap can't own — zoom is
                // a frontend concern and these chords are unbound in
                // the table (Ctrl+0/=/- type nothing a shell wants).
                if self.zoom_key(key, modifiers) {
                    return;
                }
                // The find bar's open chord — Ctrl+Shift+F, or
                // ⌘F on macOS (Spec R13-T4). The keymap can't own
                // it: the TUI has no find bar, so the chord is
                // GUI-local like zoom. Bare Ctrl+F is readline's
                // forward-char — it falls through to the PTY.
                if find_open_chord(key, modifiers) {
                    self.open_find(shared);
                    return;
                }
                // The pane host's toggle chords — `Ctrl+Shift+E`
                // explorer, `Ctrl+Shift+B` browser (Spec R14
                // §5.13). GUI-local like the find chord: the
                // keymap can't own them and they never reach
                // the PTY. They hold while an overlay owns focus
                // — the find bar or a modal picker would lose the
                // region under it mid-interaction; the chord
                // falls through to the overlay's swallow arm, so
                // nothing reaches the PTY either way.
                let overlay_owns_focus = shared.find.is_some()
                    || shared.picker.is_some()
                    || shared.conversations.is_some();
                if !overlay_owns_focus {
                    if let Some(id) = pane_chord(key, modifiers) {
                        // A held chord's autorepeat must not
                        // strobe the toggle — one flip per
                        // physical press; the repeat is still
                        // swallowed, never PTY input.
                        if !repeat {
                            self.toggle_pane(shared, id);
                        }
                        return;
                    }
                }
                let translated = translate_key(key, modifiers);
                if let Some(KeyEvent {
                    key: Key::Char(c), ..
                }) = translated
                {
                    eaten.push(c);
                }
                self.dispatch_key(ctx, shared, translated);
            }
            // Releases carry no input — same as the TUI's
            // `KeyEventKind::Release` guard.
            egui::Event::Key { pressed: false, .. } => {}
            // Composed text: one Char event per character through
            // the same resolver (every Char resolves PassThrough —
            // the trip costs nothing and keeps ONE dispatch path).
            // While the find bar is open, text is the query —
            // nothing reaches the PTY mid-search.
            egui::Event::Text(text) => {
                // The twin/meta guard runs BEFORE any routing: on
                // twin-emitting platforms an `Alt+<letter>` chord
                // arrives as `Key{.., alt}` + `Text("a")`, and the
                // Text twin must land nowhere — not the PTY, not
                // the find query, not an aux pane's address entry.
                // `meta_held` also eats the twin whose Key event
                // lands after this Text (a Meta chord still held
                // while Text composes wants the chord's bytes —
                // Option-as-Alt — never a literal).
                let meta_held = ctx.input(|input| input.modifiers.alt || input.modifiers.mac_cmd);
                let text: String = text
                    .chars()
                    .filter(|c| {
                        !meta_held
                            && !eaten
                                .iter()
                                .position(|e| e.eq_ignore_ascii_case(c))
                                .map(|pos| eaten.remove(pos))
                                .is_some()
                    })
                    .collect();
                if text.is_empty() {
                    return;
                }
                if shared.find.is_some() {
                    self.find_text(shared, &text);
                    return;
                }
                // A shown aux pane owns composed text while it's
                // up (the find bar's focus rule) — nothing
                // reaches the PTY mid-pane.
                if let Some(SidePane::Aux(id)) = shared.shown_pane() {
                    pane(id).text(shared, &text);
                    return;
                }
                for key in translate_text(&text) {
                    self.dispatch_key(ctx, shared, Some(key));
                }
            }
            // A native paste (Ctrl+V / ⌘V, menu paste): encode the
            // payload through the shared `encode_paste` so
            // bracketed-paste children see their mode honored.
            // While the find bar is open it pastes into the query
            // — the browser convention.
            egui::Event::Paste(text) => {
                if shared.find.is_some() {
                    self.find_text(shared, &text);
                } else if let Some(SidePane::Aux(id)) = shared.shown_pane() {
                    pane(id).text(shared, &text);
                } else {
                    self.paste(shared, &text);
                }
            }
            egui::Event::MouseWheel { unit, delta, .. } => {
                let hover = ctx.input(|input| input.pointer.hover_pos());
                self.wheel(shared, ctx, unit, delta, hover);
            }
            _ => {}
        }
    }

    /// Ctrl+=/+-/0 — the terminal-text zoom (never `set_zoom_factor`:
    /// only the grid font grows, the tabs/status/panel stay put — the
    /// spec's terminal-text-only zoom rule). The spec's chord is
    /// literal Ctrl (US21); ⌘± also works on macOS — egui's `command`
    /// — as the desktop convention. Returns whether the key was a
    /// zoom chord.
    fn zoom_key(&mut self, key: egui::Key, modifiers: egui::Modifiers) -> bool {
        if !((modifiers.command || modifiers.ctrl) && !modifiers.alt) {
            return false;
        }
        match key {
            egui::Key::Plus | egui::Key::Equals => self.metrics.zoom_in(),
            egui::Key::Minus => self.metrics.zoom_out(),
            egui::Key::Num0 => self.metrics.zoom_reset(),
            _ => return false,
        }
        true
    }

    /// One translated key event through the shared resolver —
    /// `None` (untranslatable key) falls to the PassThrough arm and is
    /// swallowed exactly like an unbound key, same as the TUI.
    fn dispatch_key(&mut self, ctx: &egui::Context, shared: &mut Shared, key: Option<KeyEvent>) {
        let resolution = key.map_or(Resolution::PassThrough, |key| {
            keymap::resolve(&key, CapabilityTier::Enhanced)
        });
        match resolution {
            Resolution::Action(AppAction::Quit) => self.quit = true,
            Resolution::Action(AppAction::NewTab) => self.open_tab(shared),
            Resolution::Action(AppAction::CloseTab) => self.close_active_tab(shared),
            Resolution::Action(AppAction::NextTab) => shared.tabs.activate_next(),
            Resolution::Action(AppAction::PrevTab) => shared.tabs.activate_prev(),
            Resolution::Action(AppAction::JumpToTab(n)) => shared.tabs.jump_to(n),
            // `Ctrl+Shift+←/→` reorder — a MOVE keeps the tab's own
            // selection/viewport (the manager carries it with the
            // slot), so unlike a switch nothing is cleared.
            Resolution::Action(AppAction::MoveTabLeft) => {
                shared.tabs.move_active_prev();
            }
            Resolution::Action(AppAction::MoveTabRight) => {
                shared.tabs.move_active_next();
            }
            // `Alt+A` (Spec R06): the shared toggle — open focuses,
            // a second press while focused closes — then `resize_active`
            // re-derives THIS tab's winsize through the GUI's
            // `session_size` (panels are per-tab).
            Resolution::Action(AppAction::AgentPanel) => {
                let session_size = region_session_size(&shared.panes);
                dispatch::toggle_agent_panel(&mut shared.tabs, session_size);
            }
            // `Alt+P` (Spec R07 §5.7): the shared switch — a second
            // press dismisses the open picker; otherwise the declared
            // count picks picker/cycle/unavailable.
            Resolution::Action(AppAction::SwitchProfile) => {
                profile::switch_profile(
                    shared.config.settings(),
                    &mut shared.picker,
                    &mut shared.profile,
                    &mut shared.toast,
                );
            }
            // `Alt+Enter` (Spec R04 us-11/12) — the TUI's arm verbatim:
            // the input line itself is the request, verified through
            // the same shadow+grid echo agreement the `# ` path runs;
            // once the request is accepted the typed line is erased so
            // request text and the streamed command never interleave.
            Resolution::Action(AppAction::SuggestCommand) => {
                let Some(tab) = shared.tabs.active_mut() else {
                    return;
                };
                // Input-adjacent: drop the highlight and snap to the
                // live viewport like typing does.
                tab.clear_selection();
                tab.snap_to_bottom();
                shared.suggestion.suggest_from_line(
                    &shared.suggester,
                    shared.config.settings(),
                    &shared.profile,
                    tab,
                    &mut shared.toast,
                );
            }
            // `Alt+R` (Spec R08 §5.8): the conversation picker over
            // the store's rows — newest-first, Enter resumes into the
            // active tab's panel, `Ctrl+D` deletes behind a one-key
            // confirm, Esc closes. A second `Alt+R` toggles it closed,
            // and opening it drops the profile picker — one overlay at
            // a time. The list is a snapshot taken here; an empty
            // store toasts instead of opening nothing.
            Resolution::Action(AppAction::Conversations) => {
                conversations::toggle(
                    &mut shared.conversations,
                    &mut shared.picker,
                    &shared.store,
                    &mut shared.toast,
                    SystemTime::now(),
                );
            }
            Resolution::Action(action) => {
                let Some(tab) = shared.tabs.active_mut() else {
                    return;
                };
                tab.clear_selection();
                match action {
                    AppAction::ScrollPageUp => tab.page_up(),
                    AppAction::ScrollPageDown => tab.page_down(),
                    AppAction::ScrollTop => tab.scroll_to_top(),
                    AppAction::ScrollBottom => tab.snap_to_bottom(),
                    // Same no-blocks rule as the TUI: on a tab with NO
                    // blocks the chords are ordinary input again —
                    // encoded to the PTY rather than eaten inertly.
                    AppAction::PrevBlockStart
                    | AppAction::NextBlockStart
                    | AppAction::SelectBlock
                        if !tab.has_blocks() =>
                    {
                        tab.snap_to_bottom();
                        if tab.exit_status().is_none() {
                            if let Some(key) = key {
                                tab.shadow.update(&key);
                                let _ = tab.session.send_key(key);
                            }
                        }
                    }
                    AppAction::PrevBlockStart => tab.prev_block_start(),
                    AppAction::NextBlockStart => tab.next_block_start(),
                    AppAction::SelectBlock => {
                        if let Some(range) = tab.select_block() {
                            let text = selection::extract(&tab.grid(), range);
                            shared.copied_text = Some(text.clone());
                            ctx.copy_text(text);
                        }
                    }
                    AppAction::Paste => {
                        tab.snap_to_bottom();
                        if tab.exit_status().is_none() {
                            if let Some(text) = shared.copied_text.clone() {
                                let bytes =
                                    selection::encode_paste(&text, tab.session.bracketed_paste());
                                let _ = tab.session.send_bytes(&bytes);
                                tab.shadow.update_pasted(&text);
                            }
                        }
                    }
                    AppAction::Quit
                    | AppAction::NewTab
                    | AppAction::CloseTab
                    | AppAction::NextTab
                    | AppAction::PrevTab
                    | AppAction::JumpToTab(_)
                    | AppAction::MoveTabLeft
                    | AppAction::MoveTabRight
                    | AppAction::AgentPanel
                    | AppAction::SwitchProfile
                    | AppAction::SuggestCommand
                    | AppAction::Conversations => {} // dispatched above
                }
            }
            Resolution::PassThrough => {
                // An open picker owns unbound keys (Spec R07 §5.7 /
                // R08-T6) — the shared overlay routing: ↑/↓ move the
                // highlight, Enter arms/resumes the highlighted row,
                // `Ctrl+D` deletes behind its one-key confirm, plain
                // Esc dismisses, and everything else is swallowed so
                // nothing leaks to the PTY mid-pick.
                if shared.picker.is_some() || shared.conversations.is_some() {
                    if let Some(key) = key {
                        let session_size = region_session_size(&shared.panes);
                        dispatch::overlay_key(&key, &mut shared.key_ctx(), session_size);
                    }
                    return;
                }
                // An open find bar owns unbound keys the same way —
                // Enter/Shift+Enter/arrows step matches, Backspace
                // edits, Esc closes; everything else is swallowed so
                // nothing reaches the PTY mid-search.
                if shared.find.is_some() {
                    if let Some(key) = key {
                        self.find_key(shared, key);
                    }
                    return;
                }
                // A shown aux pane owns unbound keys the same way
                // (Spec R14 §5.13 — the find bar's focus rule): Esc
                // returns focus to the grid, everything else routes
                // to the pane; nothing reaches the PTY mid-pane.
                if let Some(SidePane::Aux(id)) = shared.shown_pane() {
                    if let Some(key) = key {
                        self.pane_key(shared, id, key);
                    }
                    return;
                }
                // The pass-through tail — a focused agent panel owns
                // the keystroke (Spec R06); an in-flight suggestion
                // swallows Enter/cancels on Esc scoped to the pinned
                // tab (Spec R04 us-8/9/10); the `# `+Enter and
                // classifier triggers arm the suggester; anything
                // left encodes through the input shadow to the PTY —
                // the shared fold the TUI runs.
                dispatch::passthrough_key(key, &mut shared.key_ctx());
            }
        }
    }

    /// A native `Event::Paste` — the payload went through the OS
    /// clipboard, so it pastes directly (unlike `Shift+Insert`, which
    /// pastes ait's own copied selection).
    fn paste(&mut self, shared: &mut Shared, text: &str) {
        let Some(tab) = shared.tabs.active_mut() else {
            return;
        };
        tab.clear_selection();
        tab.snap_to_bottom();
        if tab.exit_status().is_none() && !text.is_empty() {
            let bytes = selection::encode_paste(text, tab.session.bracketed_paste());
            let _ = tab.session.send_bytes(&bytes);
            tab.shadow.update_pasted(text);
        }
    }

    /// Wheel over the grid pane scrolls the terminal — wheel over the
    /// panel region scrolls the PANEL's conversation (the layout
    /// contract's "panel wheel events must not scroll the terminal
    /// grid" rule, from the other side: terminal wheel events must
    /// not scroll the panel either — the two regions route to their
    /// own scroll state).
    fn wheel(
        &mut self,
        shared: &mut Shared,
        ctx: &egui::Context,
        unit: egui::MouseWheelUnit,
        delta: Vec2,
        hover: Option<Pos2>,
    ) {
        let Some(hover) = hover else {
            return;
        };
        if self.panel_pane.is_some_and(|pane| pane.contains(hover)) {
            let Some(tab) = shared.tabs.active_mut() else {
                return;
            };
            let cell = self.cell_height(ctx);
            let pages = self.panel_rows.max(1);
            let lines = wheel_lines(unit, delta.y, cell, pages, &mut self.panel_wheel_rem);
            if lines > 0 {
                tab.agent_scroll_up(lines as usize);
            } else if lines < 0 {
                tab.agent_scroll_down((-lines) as usize);
            }
            return;
        }
        if !self.grid_pane.is_some_and(|pane| pane.contains(hover)) {
            return;
        }
        let Some(tab) = shared.tabs.active_mut() else {
            return;
        };
        match unit {
            egui::MouseWheelUnit::Page => {
                if delta.y > 0.0 {
                    tab.page_up();
                } else {
                    tab.page_down();
                }
            }
            _ => {
                let cell = self.cell_height(ctx);
                let lines = wheel_lines(unit, delta.y, cell, 0, &mut self.grid_wheel_rem);
                if lines > 0 {
                    tab.scroll_up(lines as usize);
                } else if lines < 0 {
                    tab.scroll_down((-lines) as usize);
                }
            }
        }
    }

    /// The current cell height — the wheel's pixel→lines divisor.
    fn cell_height(&self, ctx: &egui::Context) -> f32 {
        let font = FontId::monospace(self.metrics.font_px());
        ctx.fonts_mut(|fonts| fonts.row_height(&font)).max(1.0)
    }

    // -----------------------------------------------------------------
    // Find bar (Spec R13-T4) — `Shared.find` being `Some` IS the open
    // state; the pump rescans its hits as output lands, these handlers
    // own the input side. Unbound keys route here while it's open
    // (text arrives through `Event::Text`/`Paste` → `find_text`).

    /// `Ctrl+Shift+F`/`⌘F`: open the bar over the active tab.
    /// Idempotent — a second press keeps the bar and its query (the
    /// browser rule).
    fn open_find(&mut self, shared: &mut Shared) {
        if shared.find.is_some() {
            return;
        }
        let Some(tab) = shared.tabs.active() else {
            return;
        };
        // The anchor is the view's top row in grid-line space —
        // `-scroll_offset` — so the first match is measured from what's
        // on screen, not from the oldest scrollback line.
        let anchor = -(i32::try_from(tab.scroll_offset()).unwrap_or(i32::MAX));
        shared.find = Some(FindBar::new(tab.id, anchor));
    }

    /// Composed text or a paste while the bar is open: append to the
    /// query, rescan against the active tab's grid, and reveal the
    /// match it lands on — the find-as-you-type jump.
    fn find_text(&mut self, shared: &mut Shared, text: &str) {
        let hit = {
            let Some(tab) = shared.tabs.active() else {
                return;
            };
            let Some(find) = shared.find.as_mut() else {
                return;
            };
            find.push_str(&tab.grid(), text);
            find.current()
        };
        if let Some(hit) = hit {
            reveal_hit(shared, hit);
        }
    }

    /// One unbound key while the bar is open — named keys only (plain
    /// text arrives through `Event::Text`, so any `Char` here is a
    /// modifier chord and falls to the swallow arm like everything
    /// else). Esc closes; Enter/Shift+Enter (and ↓/↑) step matches.
    fn find_key(&mut self, shared: &mut Shared, key: KeyEvent) {
        match keymap::normalize(key.key) {
            Key::Escape if key.modifiers.is_empty() => {
                shared.find = None;
            }
            Key::Enter if key.modifiers.is_empty() => self.find_step(shared, false),
            Key::Enter if key.modifiers == Modifiers::SHIFT => self.find_step(shared, true),
            Key::Backspace if key.modifiers.is_empty() => {
                let hit = {
                    let Some(tab) = shared.tabs.active() else {
                        return;
                    };
                    let Some(find) = shared.find.as_mut() else {
                        return;
                    };
                    find.pop(&tab.grid());
                    find.current()
                };
                if let Some(hit) = hit {
                    reveal_hit(shared, hit);
                }
            }
            // ↑/↓ ride along as second prev/next chords.
            Key::Up if key.modifiers.is_empty() => self.find_step(shared, true),
            Key::Down if key.modifiers.is_empty() => self.find_step(shared, false),
            _ => {}
        }
    }

    /// Enter / Shift+Enter / ↓ / ↑: step the current match and reveal
    /// the one the bar landed on.
    fn find_step(&mut self, shared: &mut Shared, backward: bool) {
        let hit = shared.find.as_mut().and_then(|find| {
            if backward {
                find.prev_match()
            } else {
                find.next_match()
            }
        });
        if let Some(hit) = hit {
            reveal_hit(shared, hit);
        }
    }

    // -----------------------------------------------------------------
    // Pane host (Spec R14 §5.13, R14-T1) — `Shared.panes` holds the
    // app-global, ephemeral aux-pane selection; the region shows the
    // agent panel while a tab's claim holds, else the selection.
    // Selection changes resize EVERY tab (the open flag is global),
    // claim changes resize one — [`PaneHost::region_open`] ORs them.

    /// A pane chord's or switcher chip's toggle: the shown pane's
    /// press deselects (the region falls back to the agent claim or
    /// closes); any other press runs the shared [`select_pane`] arm.
    fn toggle_pane(&mut self, shared: &mut Shared, id: PaneId) {
        if shared.panes.showing(id, shared.agent_claiming()) {
            shared.panes.clear();
        } else {
            self.select_pane(shared, id);
        }
        self.resize_for_panes(shared);
    }

    /// The select arm `toggle_pane` and `show_pane` share: pick the
    /// pane, and close an open agent panel outright so its claim
    /// yields — [`Tab::close_agent_panel`] is a close, never a
    /// cycle; the conversation survives and `Alt+A` reopens it.
    fn select_pane(&mut self, shared: &mut Shared, id: PaneId) {
        shared.panes.select(id);
        if let Some(tab) = shared.tabs.active_mut() {
            tab.close_agent_panel();
        }
    }

    /// One unbound key while an aux pane shows: plain Esc returns
    /// focus to the grid (the toggle chord is the other way out —
    /// the find bar's own rule), everything else routes to the pane.
    fn pane_key(&mut self, shared: &mut Shared, id: PaneId, key: KeyEvent) {
        if key.key == Key::Escape && key.modifiers.is_empty() {
            shared.panes.clear();
            self.resize_for_panes(shared);
            return;
        }
        pane(id).key(shared, key);
    }

    /// Re-derive EVERY tab's winsize after the aux selection moved —
    /// region-open is `agent claim OR app-global selection`, so a
    /// selection change is every tab's resize, not just the active
    /// one's (a background tab must already be narrow when a switch
    /// lands it under an open pane).
    fn resize_for_panes(&mut self, shared: &mut Shared) {
        let session_size = region_session_size(&shared.panes);
        shared.tabs.resize_all(self.last_band, session_size);
    }

    /// Make `id` the region's pane — the shared [`select_pane`] arm
    /// plus the every-tab resize, minus `toggle_pane`'s off half (an
    /// explicit open never closes the pane).
    fn show_pane(&mut self, shared: &mut Shared, id: PaneId) {
        if !shared.panes.showing(id, shared.agent_claiming()) {
            self.select_pane(shared, id);
        }
        self.resize_for_panes(shared);
    }

    /// "Open the browser pane at this URL" — the Ctrl+click-on-URL
    /// route (R14-T4) and any other caller that names a destination:
    /// queue the navigation (the address entry follows it) and show
    /// the browser pane. Whether the page embeds or hands to the
    /// system browser is the probe's verdict, not this caller's
    /// concern.
    fn open_browser_at(&mut self, shared: &mut Shared, url: &str) {
        shared.browser.navigate(url);
        self.show_pane(shared, PaneId::Browser);
    }

    /// The Ctrl+click route (Spec R14 §5.13 us-25/26): a detected
    /// token sends its target to the right pane — a URL navigates the
    /// browser, a file path selects and previews in the explorer, a
    /// directory navigates the explorer to it — each through
    /// [`show_pane`], so a route carries the same claim-yield and
    /// winsize a chord's open arm does and can never toggle one off.
    /// Detection already gated remotes to URLs, so a `Path` target
    /// always names a local entry `sync_explorer` can serve.
    fn open_detection(&mut self, shared: &mut Shared, target: detect::Target) {
        match target {
            detect::Target::Url(url) => self.open_browser_at(shared, &url),
            detect::Target::Path(path) => {
                crate::explorer::sync_explorer(shared);
                if path.is_dir() {
                    shared.explorer.navigate_to_dir(&path);
                } else {
                    shared.explorer.select_file(&path);
                }
                self.show_pane(shared, PaneId::Explorer);
            }
        }
    }

    // -----------------------------------------------------------------
    // Tab lifecycle — shared by the keymap actions above and the tab
    // bar's click targets below, so binding and button can never drift.

    /// `Alt+T` / the bar's "+": the shared spawn — a tab plus its
    /// `terminal_sessions` row; a spawn failure toasts instead of
    /// taking the window down (graceful degrade, like the TUI).
    fn open_tab(&mut self, shared: &mut Shared) {
        dispatch::new_tab(&mut shared.tabs, &mut shared.store, &mut shared.toast);
        // The spawn sized the session for a closed region (the
        // manager's stored band); an app-global aux selection holds
        // the region open on the new tab too — re-derive now. A fresh
        // tab has no agent claim, so `region_open(false)` IS the aux
        // selection's `is_some`.
        if shared.panes.region_open(false) {
            shared
                .tabs
                .resize_active(|size, _agent_open| session_size(size, true));
        }
    }

    /// `Alt+W` / a chip's "×": the shared close — store bookkeeping
    /// (`ended_at`, `closed`) lands BEFORE the tab goes, then the
    /// manager drops it. `false` = the set is now empty: exit rather
    /// than ever indexing it (CODING_STANDARDS §6).
    fn close_active_tab(&mut self, shared: &mut Shared) {
        if !dispatch::close_active_tab(&mut shared.tabs, &mut shared.store, &mut shared.toast) {
            self.quit = true;
        }
    }

    // -----------------------------------------------------------------
    // Paint — the five regions, painted in order; the modal layer is
    // last so its overlay always wins.

    /// The tab-bar strip: one chip per tab — the body activates, the
    /// "×" closes — plus the trailing "+" spawn button (Spec R13 §5.12:
    /// open/close/switch by binding AND by click). [`tab_chips`] owns
    /// the geometry (it and the painter share one layout, so a click
    /// can never land beside the chip it was painted on); this
    /// measures the titles, paints the chips and dispatches the
    /// clicks. `jump_to` is 1-based — the `N:title` labels ARE the
    /// jump indices. T4's find bar slots into the modal layer, not
    /// here.
    fn paint_tab_bar(&mut self, ui: &mut egui::Ui, shared: &mut Shared, rect: Rect) {
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, chrome::STRIP);
        let font = FontId::proportional(12.0);
        let titles = shared.tabs.titles();
        let widths: Vec<f32> = titles
            .iter()
            .map(|title| {
                painter
                    .layout_no_wrap(title.clone(), font.clone(), Color32::WHITE)
                    .size()
                    .x
            })
            .collect();
        let bar = tab_chips(rect, &widths);
        let active = shared.tabs.active_index();
        for (index, chip) in bar.tabs.iter().enumerate() {
            // `click_and_drag`: a release inside the same chip is the
            // activate click; a drag released over ANOTHER chip is the
            // reorder — `move_tab` slides the others over, so the chip
            // lands where the pointer dropped it (Spec R13 §5.12).
            let chip_hit = ui.interact(
                chip.rect,
                egui::Id::unique(("tab", index)),
                Sense::click_and_drag(),
            );
            let close_hit = ui.interact(
                chip.close,
                egui::Id::unique(("tab-close", index)),
                Sense::click(),
            );
            // The registry names mirror the chip's role: the body
            // activates, the "×" closes (Spec R15-T3).
            shared.hits.register(|| format!("tab.{index}"), chip.rect);
            shared
                .hits
                .register(|| format!("tab.{index}.close"), chip.close);
            let bg = if index == active {
                chrome::FIELD_HI
            } else if chip_hit.hovered() || close_hit.hovered() {
                chrome::FIELD
            } else {
                Color32::TRANSPARENT
            };
            painter.rect_filled(chip.rect, 4.0, bg);
            painter.text(
                Pos2::new(chip.rect.left() + CHIP_PAD, chip.rect.center().y),
                Align2::LEFT_CENTER,
                &titles[index],
                font.clone(),
                Color32::WHITE,
            );
            painter.text(
                chip.close.center(),
                Align2::CENTER_CENTER,
                "×",
                font.clone(),
                if close_hit.hovered() {
                    Color32::WHITE
                } else {
                    chrome::IDLE
                },
            );
            if close_hit.clicked() {
                // Closing by click activates first (the user's target
                // is unambiguous) — `jump_to` is 1-based — then the
                // shared close path runs, quitting on the last tab.
                shared.tabs.jump_to(index + 1);
                self.close_active_tab(shared);
            } else if chip_hit.drag_stopped() {
                // The drop slot: the chip the pointer is over, clamped
                // to the set's edges so a drop past either end lands at
                // that end. `move_tab` no-ops on a same-slot drop.
                if let Some(pos) = ui.ctx().input(|input| input.pointer.interact_pos()) {
                    let to = bar
                        .tabs
                        .iter()
                        .position(|slot| pos.x <= slot.rect.right())
                        .unwrap_or(bar.tabs.len().saturating_sub(1));
                    shared.tabs.move_tab(index, to);
                }
            } else if chip_hit.clicked() {
                shared.tabs.jump_to(index + 1);
            }
        }
        if let Some(new_rect) = bar.new_tab {
            let new_hit = ui.interact(new_rect, egui::Id::unique("tab-new"), Sense::click());
            shared.hits.register(|| "tab.new".to_owned(), new_rect);
            if new_hit.hovered() {
                painter.rect_filled(new_rect, 4.0, chrome::FIELD);
            }
            painter.text(
                new_rect.center(),
                Align2::CENTER_CENTER,
                "+",
                font.clone(),
                if new_hit.hovered() {
                    Color32::WHITE
                } else {
                    chrome::SUBTEXT
                },
            );
            if new_hit.clicked() {
                self.open_tab(shared);
            }
        }
        // The pane host's always-on click affordance (Spec R14 §5.13
        // us-1 — "a chord AND a clickable affordance"): the switcher
        // chips only exist once the region is open, so this is the
        // mouse path onto the host. It toggles the last selection,
        // explorer by default — the same state move the pane's own
        // chord makes, so click and chord can never disagree.
        if let Some(pane_rect) = bar.pane_button {
            let pane_hit = ui.interact(pane_rect, egui::Id::unique("pane-open"), Sense::click());
            shared
                .hits
                .register(|| "panes.button".to_owned(), pane_rect);
            if pane_hit.hovered() {
                painter.rect_filled(pane_rect, 4.0, chrome::FIELD);
            }
            painter.text(
                pane_rect.center(),
                Align2::CENTER_CENTER,
                "panes",
                font,
                if pane_hit.hovered() {
                    Color32::WHITE
                } else {
                    chrome::SUBTEXT
                },
            );
            if pane_hit.clicked() {
                let id = shared.panes.selected().unwrap_or(PaneId::Explorer);
                self.toggle_pane(shared, id);
            }
        }
    }

    /// The grid pane: terminal painting plus the pointer interactions
    /// that need the pane rect — drag-select (begin/extend/finish +
    /// copy on release) and wheel scroll (handled in `wheel` via the
    /// stored rect).
    fn paint_grid_pane(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        shared: &mut Shared,
        pane: Rect,
        cell: CellMetrics,
        font: &FontId,
    ) {
        self.grid_pane = Some(pane);
        // `grid` — the whole pane is the selection/click surface.
        shared.hits.register(|| "grid".to_owned(), pane);
        let Some(tab) = shared.tabs.active_mut() else {
            return;
        };
        let response = ui.interact(pane, egui::Id::unique("grid"), Sense::click_and_drag());
        let scroll = tab.scroll_offset();
        // Ctrl+hover detection (Spec R14 §5.13 us-24): while plain
        // Ctrl is held and the pointer is over the grid, the token
        // under it underlines and the pointer turns to the hand —
        // Ctrl+click routes the same hit. Plain Ctrl only, so chords
        // that add a modifier keep their other meanings and the
        // affordance never fights them. The probe rides the frame's
        // [`detect::DetectCache`]: the row's text/column map still
        // rebuilds each frame (cheap, O(row)) but the filesystem
        // probes run only when the position, the text or the cwd
        // moved — never once per held frame (§6).
        let ctrl = ctx.input(|input| {
            let modifiers = input.modifiers;
            modifiers.ctrl && !modifiers.alt && !modifiers.shift && !modifiers.mac_cmd
        });
        let link = if ctrl {
            response.hover_pos().and_then(|pos| {
                let (line, column) = grid_point(pos, pane, cell, scroll);
                // Scrollback rows don't afford (Spec R15-T4): a
                // scrollback path was written under whatever cwd the
                // session held then — resolving it against today's
                // cwd can afford a wrong or since-deleted file, so
                // only the live viewport's tokens resolve.
                if line < 0 {
                    return None;
                }
                let remote = !tab.remote.is_empty();
                let (text, cols) = tab
                    .grid()
                    .line(line)
                    .map(|row| detect::line_cells(&row))
                    .unwrap_or_default();
                // The expensive halves — the `/proc` readlink
                // `cwd_for` can cost and `home_dir`'s env resolve —
                // ride the cache's `env` closure: a held Ctrl at an
                // unchanged position never evaluates either (§6).
                self.detect_cache
                    .resolve(
                        &detect::HoverAt {
                            tab: tab.id,
                            line,
                            column,
                            text: &text,
                            cols: &cols,
                            remote,
                        },
                        || (PathBuf::from(suggest::cwd_for(tab)), home_dir()),
                    )
                    .map(|hit| (line, hit))
            })
        } else {
            None
        };
        if link.is_some() {
            ctx.set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let detect = link.as_ref().map(|(line, hit)| DetectOverlay {
            line: *line,
            column: hit.column,
            cells: hit.cells,
        });
        // The dump's Ctrl-hover + cell-metric probes (Spec R15-T4):
        // record what the affordance resolved — `ctrl` only counts
        // over the grid — and this frame's cell geometry, so a dump
        // answers "what Ctrl+click would route" without driving one.
        #[cfg(feature = "test-driver")]
        {
            shared.cell_metrics = Some((cell.width, cell.height));
            shared.detect_hover.ctrl = ctrl && response.hover_pos().is_some();
            shared.detect_hover.token = link.clone();
        }
        // `clicked` is press AND release inside the pane with no drag
        // — the affordance's release half; the route lands after the
        // paint pass, once `tab`'s borrow ends.
        let route = if response.clicked() && ctrl {
            link.map(|(_, hit)| hit.target)
        } else {
            None
        };
        // A Ctrl+press is the affordance's, never a selection start —
        // `extend_selection` no-ops without a `begin`, so the drag
        // arm needs no gate of its own (us-27: ordinary click-drag
        // selection is untouched).
        if let Some(pos) = response.interact_pointer_pos() {
            let (line, column) = grid_point(pos, pane, cell, scroll);
            if response.drag_started() && !ctrl {
                tab.begin_selection(GridPoint::new(line, column));
            } else if response.dragged() {
                tab.extend_selection(GridPoint::new(line, column));
            }
        }
        if response.drag_stopped() {
            if let Some(range) = tab.finish_selection() {
                let text = selection::extract(&tab.grid(), range);
                shared.copied_text = Some(text.clone());
                ctx.copy_text(text);
            }
        }
        // The find bar rides the tab it opened on: a tab switch
        // retargets its hits to the new session (the query carries
        // over — the browser convention); the pump's rescan keeps
        // them fresh as output lands.
        if let Some(find) = shared.find.as_mut() {
            if find.tab() != tab.id {
                let anchor = -(i32::try_from(scroll).unwrap_or(i32::MAX));
                find.retarget(tab.id, anchor, &tab.grid());
            }
        }
        // Block chrome is `Integrated`-only: MarksOnly/Absent sessions
        // skip the resolve pass entirely and render identically to
        // plain output — no chrome, no errors (Spec R05's degrade
        // rule; R13-T4's acceptance). The label is `block_text`'s —
        // the same `✓/✗ <code> · <dur>` the status line renders.
        let blocks = if tab.integration_state() == IntegrationState::Integrated {
            tab.blocks()
        } else {
            Vec::new()
        };
        let dividers: Vec<BlockDivider> = blocks
            .iter()
            .map(|block| BlockDivider {
                start: *block.range().start(),
                label: block.exit().map(|exit| BlockLabel {
                    text: block_text(BlockStatus {
                        exit,
                        duration: block.duration().unwrap_or_default(),
                    }),
                    ok: exit == 0,
                }),
            })
            .collect();
        let pane_rows = (pane.height() / cell.height).floor() as usize;
        let chips = plan_block_chrome(&dividers, tab.scrollback(), scroll, pane_rows);
        let painter = ui.painter().with_clip_rect(pane);
        paint_grid(
            &crate::paint::GridSurface {
                painter: &painter,
                pane,
                cell,
                theme: self.theme,
                font,
            },
            &mut self.row_galleys,
            &tab.grid(),
            scroll,
            tab.cursor(),
            tab.selection_range(),
            Overlays {
                find: shared.find.as_ref().map(|find| FindOverlay {
                    hits: find.hits(),
                    current: find.current_index(),
                }),
                block_chips: Some(&chips),
                detect,
            },
        );
        // Scrolled into scrollback: the jump-to-bottom affordance
        // over the pane's corner — the wheel's trip back is the depth
        // itself, and the typing-snap rule means the button only ever
        // offers a no-op when the view is already live.
        if scroll > 0 {
            let mut snap = false;
            let mut pill_rect = Rect::NOTHING;
            egui::Area::new(egui::Id::unique("scroll-bottom"))
                .order(egui::Order::Foreground)
                .pivot(Align2::RIGHT_BOTTOM)
                .fixed_pos(pane.right_bottom() - Vec2::new(8.0, 8.0))
                .show(ctx, |ui| {
                    let button = egui::Button::new(egui::RichText::new("⤓").monospace())
                        .fill(Color32::from_rgba_premultiplied(0x2A, 0x2A, 0x2A, 0xDC))
                        .stroke(Stroke::new(1.0, chrome::EDGE));
                    let response = ui.add(button).on_hover_text("jump to the live output");
                    pill_rect = response.rect;
                    snap = response.clicked();
                });
            shared
                .hits
                .register(|| "pill.scroll_bottom".to_owned(), pill_rect);
            if snap {
                tab.snap_to_bottom();
            }
        }
        // The detection-token targets (Spec R15-T3): every
        // `detect.*` name maps the span's cell rect, so a scenario
        // clicks a URL like a user would — through the grid's own
        // hit-test, not a bypass.
        #[cfg(feature = "test-driver")]
        self.register_detect_targets(&mut shared.hits, tab, pane, cell, scroll);
        // The Ctrl+click route — applied last so `tab`'s borrow has
        // ended: `open_detection` drives the pane host, the explorer
        // model and the browser queue off the whole `Shared`.
        if let Some(target) = route {
            self.open_detection(shared, target);
        }
    }

    /// Register the visible rows' detected URL/path tokens as
    /// `detect.{n}` hit targets (scan order) plus content-keyed
    /// `detect.url.<u>`/`detect.path.<p>` aliases — so a scenario
    /// clicks a link by name through the grid's own hit-test. The
    /// scan rides `detect_hits`' memo: `line_cells` is the cheap
    /// half (it rebuilds for the hover every frame), the token
    /// scan's filesystem probes are the expensive one and re-run
    /// only when the key — tab, scroll, pane, cell, remote flag, the
    /// rows' text — moved.
    #[cfg(feature = "test-driver")]
    fn register_detect_targets(
        &mut self,
        hits: &mut HitTargets,
        tab: &ait_app::tabs::Tab,
        pane: Rect,
        cell: CellMetrics,
        scroll: usize,
    ) {
        let remote = !tab.remote.is_empty();
        let lines: Vec<(String, Vec<usize>)> = tab
            .grid()
            .visible_rows()
            .map(|row| detect::line_cells(&row))
            .collect();
        let texts: Vec<String> = lines.iter().map(|(text, _)| text.clone()).collect();
        let fresh = self.detect_hits.as_ref().is_none_or(|scan| {
            scan.tab != tab.id
                || scan.scroll != scroll
                || scan.remote != remote
                || scan.pane != pane
                || scan.cell != cell
                || scan.rows != texts
        });
        if fresh {
            let mut out: Vec<(String, Rect)> = Vec::new();
            // `cwd_for`/`home_dir` resolve once per scan, not per
            // row — the same laziness `DetectCache`'s `env` closure
            // buys the hover probe.
            let mut env: Option<(PathBuf, PathBuf)> = None;
            let mut ordinal = 0usize;
            for (row_index, (text, cols)) in lines.iter().enumerate() {
                let (cwd, home) =
                    env.get_or_insert_with(|| (PathBuf::from(suggest::cwd_for(tab)), home_dir()));
                let tokens = if remote {
                    detect::url_tokens(text)
                } else {
                    detect::tokens(text, cwd, home)
                };
                for token in tokens {
                    let (Some(&first), Some(&last)) =
                        (cols.get(token.start), cols.get(token.end.saturating_sub(1)))
                    else {
                        continue;
                    };
                    let rect = Rect::from_min_max(
                        Pos2::new(
                            pane.left() + first as f32 * cell.width,
                            pane.top() + row_index as f32 * cell.height,
                        ),
                        Pos2::new(
                            pane.left() + (last + 1) as f32 * cell.width,
                            pane.top() + (row_index + 1) as f32 * cell.height,
                        ),
                    );
                    match &token.target {
                        detect::Target::Url(url) => {
                            out.push((format!("detect.url.{url}"), rect));
                        }
                        detect::Target::Path(path) => {
                            out.push((format!("detect.path.{}", path.display()), rect));
                        }
                    }
                    out.push((format!("detect.{ordinal}"), rect));
                    ordinal += 1;
                }
            }
            self.detect_hits = Some(DetectHitScan {
                tab: tab.id,
                scroll,
                remote,
                pane,
                cell,
                rows: texts,
                out,
            });
        }
        if let Some(scan) = &self.detect_hits {
            for (name, rect) in &scan.out {
                hits.register(|| name.clone(), *rect);
            }
        }
    }

    /// The side region (Spec R14 §5.13): the pane host's frame — the
    /// region background, the grid-side separator, and the switcher
    /// strip that is the host's visible way to switch — then the one
    /// pane `shown` names: the agent panel's body while its claim
    /// holds, else the selected registry entry's own paint.
    ///
    /// The switcher shows one chip for the agent claim plus one per
    /// registered pane, the shown chip reversed; clicks queue a
    /// [`SidePane`] target applied after the pass (same queued-action
    /// pattern as the gate card — the responses borrow `ui`, the
    /// mutation borrows `shared`). Clicking the agent chip is
    /// `Alt+A`'s mouse form; clicking a pane chip is its chord's.
    fn paint_side_region(
        &mut self,
        ui: &mut egui::Ui,
        shared: &mut Shared,
        rect: Rect,
        shown: SidePane,
    ) {
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, PANEL_BG);
        painter.line_segment(
            [rect.left_top(), rect.left_bottom()],
            Stroke::new(1.0, chrome::SEP),
        );
        // The switcher strip across the region's top — the host's
        // visible switch affordance.
        let bar = Rect::from_min_size(rect.left_top(), Vec2::new(rect.width(), PANE_BAR_H));
        painter.line_segment(
            [bar.left_bottom(), bar.right_bottom()],
            Stroke::new(1.0, chrome::SEP),
        );
        let font = FontId::monospace(12.0);
        let chips: Vec<(SidePane, &str)> = std::iter::once((SidePane::Agent, "agent"))
            .chain(
                PANES
                    .iter()
                    .map(|entry| (SidePane::Aux(entry.id()), entry.title())),
            )
            .collect();
        let mut action = None;
        let mut x = bar.left() + PANEL_PAD / 2.0;
        for (index, (target, label)) in chips.iter().enumerate() {
            let label_w = painter
                .layout_no_wrap((*label).to_string(), font.clone(), Color32::WHITE)
                .size()
                .x;
            let chip = Rect::from_min_size(
                Pos2::new(x, bar.top() + 2.0),
                Vec2::new(label_w + 12.0, bar.height() - 4.0),
            );
            if chip.right() > bar.right() {
                break;
            }
            let hit = ui.interact(chip, egui::Id::unique(("pane-chip", index)), Sense::click());
            // `panes.chip.<label>` — the name IS the chip's label
            // (`agent`/`explorer`/…), so a click reads like the UI.
            shared.hits.register(|| format!("panes.chip.{label}"), chip);
            let active = *target == shown;
            let (bg, fg) = if active {
                (self.theme.fg, PANEL_BG)
            } else if hit.hovered() {
                (chrome::FIELD, self.theme.fg)
            } else {
                (Color32::TRANSPARENT, chrome::SUBTEXT)
            };
            painter.rect_filled(chip, 3.0, bg);
            painter.text(
                chip.center(),
                Align2::CENTER_CENTER,
                *label,
                font.clone(),
                fg,
            );
            if hit.clicked() {
                action = Some(*target);
            }
            x = chip.right() + 4.0;
        }

        // The body: everything under the switcher strip.
        let body = Rect::from_min_max(Pos2::new(rect.left(), bar.bottom()), rect.right_bottom());
        match shown {
            SidePane::Agent => self.paint_panel(ui, shared, body),
            SidePane::Aux(id) => pane(id).paint(ui, shared, body, self.theme),
        }

        if let Some(target) = action {
            match target {
                // The agent chip is `Alt+A`'s mouse form — the shared
                // open→focus→close cycle, never a direct flip.
                SidePane::Agent => {
                    let session_size = region_session_size(&shared.panes);
                    dispatch::toggle_agent_panel(&mut shared.tabs, session_size);
                }
                SidePane::Aux(id) => self.toggle_pane(shared, id),
            }
        }
    }

    /// The side-panel region (Spec R13-T5): the agent conversation
    /// panel's egui paint over [`AgentPanel`]'s shared state — the
    /// same regions the TUI's `paint_agent_panel` carves: the title
    /// row (reversed while the panel owns the keyboard — the focus
    /// affordance), the pinned `update_plan` block under it, the
    /// wrapped conversation between, the parked gate's interactive
    /// card above the input row, and the `› ` input line on the
    /// last row. Line planning is [`crate::panel`]'s pure half; the
    /// scroll window publishes back through `set_painted_view` so
    /// the wheel bounds match the real layout.
    ///
    /// Clicks resolve to [`PanelAction`]s applied after painting:
    /// the verdict buttons call the same `decide`/`begin_deny_reason`
    /// the `a`/`t`/`d` keys reach through `agent_key`, and choice
    /// rows answer through `answer_choice` — the click and the
    /// keyboard can never disagree about what a gate does.
    /// `rect` is the pane BODY — [`GuiApp::paint_side_region`] owns
    /// the region's frame (background, separator, switcher strip).
    fn paint_panel(&mut self, ui: &mut egui::Ui, shared: &mut Shared, rect: Rect) {
        let ctx = ui.ctx().clone();
        let painter = ui.painter().with_clip_rect(rect);
        let font = FontId::monospace(13.0);
        let (line_h, cell_w) = ctx.fonts_mut(|fonts| {
            (
                fonts.row_height(&font).max(1.0),
                fonts.glyph_width(&font, 'M').max(1.0),
            )
        });
        let mut action: Option<PanelAction> = None;
        // The whole region is the focus affordance — a click
        // anywhere that isn't a button re-focuses an unfocused
        // panel. Registered BEFORE the gate card's buttons so they
        // win the overlap in egui's hit test (last registered =
        // topmost) — registered after, this rect would swallow
        // every verdict/choice click.
        let body = ui.interact(rect, egui::Id::unique("agent-focus"), Sense::click());
        // `agent.focus` — the whole body is the focus affordance.
        shared.hits.register(|| "agent.focus".to_owned(), rect);
        {
            let Some(panel) = shared.tabs.active().and_then(|tab| tab.agent_panel()) else {
                return;
            };
            let left = rect.left() + PANEL_PAD;
            let right = rect.right() - PANEL_PAD / 2.0;
            let width_cols = ((right - left) / cell_w).floor().max(1.0) as usize;

            // Title row — reversed while focused, the TUI's focus
            // affordance — plus the in-flight hint while a task runs.
            let title_rect = Rect::from_min_size(rect.left_top(), Vec2::new(rect.width(), line_h));
            if panel.is_focused() {
                painter.rect_filled(title_rect, 0.0, self.theme.fg);
            }
            painter.text(
                title_rect.left_top(),
                Align2::LEFT_TOP,
                panel::title(panel),
                font.clone(),
                if panel.is_focused() {
                    PANEL_BG
                } else {
                    self.theme.fg
                },
            );
            let content_top = rect.top() + line_h;

            // The input row owns the last line; the parked gate's card
            // anchors directly above it (never hidden — the
            // conversation loses those rows for the duration).
            let input_rect = Rect::from_min_max(
                Pos2::new(rect.left(), rect.bottom() - line_h),
                rect.right_bottom(),
            );
            let card_height = self.paint_gate_card(
                ui,
                &painter,
                panel,
                &mut shared.hits,
                &GateCardLayout {
                    font: &font,
                    line_h,
                    left,
                    right,
                    input_top: input_rect.top(),
                },
                &mut action,
            );
            let card_top = input_rect.top() - card_height;

            // The pinned plan block directly under the title — capped
            // so a long plan folds rather than crowding the
            // scrollback out.
            let available_rows = ((card_top - content_top) / line_h).floor().max(0.0) as usize;
            let plan = panel::plan_lines(
                panel.plan(),
                panel::PLAN_BLOCK_MAX.min(available_rows.saturating_sub(1)),
            );
            for (index, row) in plan.iter().enumerate() {
                let y = content_top + index as f32 * line_h;
                painter.text(
                    Pos2::new(left, y),
                    Align2::LEFT_TOP,
                    &row.text,
                    font.clone(),
                    tone_color(row.tone, self.theme),
                );
            }
            let conv_top = content_top + plan.len() as f32 * line_h;

            // The conversation, pinned to the tail unless the user
            // scrolled — the paint owns the wrap, so it also owns the
            // scroll clamp: `top`/`painted` are what the wheel
            // handlers bound against.
            let conv_height = (card_top - conv_top).max(0.0);
            let conv_rows = (conv_height / line_h).floor() as usize;
            self.panel_rows = conv_rows;
            let lines = panel::display_lines(panel.entries(), width_cols);
            let (top, max_top) = panel::view_window(panel, lines.len(), conv_rows);
            panel.set_painted_view((top, max_top));
            let conv_rect = Rect::from_min_max(
                Pos2::new(rect.left(), conv_top),
                Pos2::new(rect.right(), card_top),
            );
            let conv_painter = painter.with_clip_rect(conv_rect);
            for row in 0..conv_rows {
                let y = conv_top + row as f32 * line_h;
                if let Some(row) = lines.get(top + row) {
                    conv_painter.text(
                        Pos2::new(left, y),
                        Align2::LEFT_TOP,
                        &row.text,
                        font.clone(),
                        tone_color(row.tone, self.theme),
                    );
                }
            }

            // The input line: `› ` plus the tail of the buffer — a
            // long draft clips from the left, the end being edited
            // stays visible — and a beam caret while the panel owns
            // the keyboard.
            let room = width_cols.saturating_sub(panel::PROMPT_WIDTH);
            let shown = panel::tail(panel.input(), room);
            painter.text(
                Pos2::new(left, input_rect.top()),
                Align2::LEFT_TOP,
                format!("{}{}", panel::INPUT_PROMPT, shown),
                font.clone(),
                self.theme.fg,
            );
            if panel.is_focused() {
                let caret_x = (left
                    + (panel::PROMPT_WIDTH + shown.chars().count()) as f32 * cell_w)
                    .min(right - 1.0);
                painter.line_segment(
                    [
                        Pos2::new(caret_x, input_rect.top() + 1.0),
                        Pos2::new(caret_x, input_rect.bottom() - 1.0),
                    ],
                    Stroke::new(1.5, self.theme.fg),
                );
            }
        }
        if body.clicked()
            && shared
                .tabs
                .active()
                .and_then(|tab| tab.agent_panel())
                .is_some_and(|panel| panel.is_open() && !panel.is_focused())
        {
            action = action.or(Some(PanelAction::Focus));
        }
        if let Some(action) = action {
            self.panel_action(shared, action);
        }
    }

    /// The parked gate's card above the input row — the approve
    /// card's verdict buttons (allow once / allow for task / deny)
    /// or its deny-reason prompt, or the ask card's clickable
    /// choices / free-text hint. Paints inside `painter`, queues the
    /// click's [`PanelAction`] into `action`, and returns the card's
    /// height in points (`0.0` while no gate is parked).
    fn paint_gate_card(
        &self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        panel: &AgentPanel,
        hits: &mut HitTargets,
        layout: &GateCardLayout<'_>,
        action: &mut Option<PanelAction>,
    ) -> f32 {
        let GateCardLayout {
            font,
            line_h,
            left,
            right,
            input_top,
        } = *layout;
        let font = font.clone();
        let gate = panel::gate(panel);
        // Rows of card content: the prompt row, plus the buttons row
        // for a waiting approve or one row per choice for a choices
        // ask (a long list folds like the plan block's `… N more`).
        let content_rows = match &gate {
            Some(Gate::Approve { denying, .. }) => {
                if *denying {
                    1
                } else {
                    2
                }
            }
            Some(Gate::Ask(card)) if !card.choices.is_empty() => {
                1 + card.choices.len().min(ASK_CHOICES_MAX + 1)
            }
            Some(Gate::Ask(_)) => 1,
            None => 0,
        };
        if content_rows == 0 {
            return 0.0;
        }
        let height = content_rows as f32 * line_h + 8.0;
        let card = Rect::from_min_max(
            Pos2::new(left - 4.0, input_top - height),
            Pos2::new(right, input_top - 2.0),
        );
        painter.rect_filled(card, 4.0, chrome::WELL);
        painter.rect_stroke(
            card,
            4.0,
            Stroke::new(1.0, chrome::EDGE),
            egui::StrokeKind::Inside,
        );
        let mut y = card.top() + 4.0;
        match gate {
            Some(Gate::Approve { name, denying }) => {
                painter.text(
                    Pos2::new(left, y),
                    Align2::LEFT_TOP,
                    if denying {
                        agent_panel::deny_prompt(name)
                    } else {
                        agent_panel::approve_title(name)
                    },
                    font.clone(),
                    tone_color(Tone::Alert, self.theme),
                );
                y += line_h;
                if !denying {
                    // The three verdict buttons — the `a`/`t`/`d`
                    // keys' click counterparts, same semantics.
                    let buttons = [
                        (
                            "a: once",
                            egui::Id::unique("agent-allow-once"),
                            "gate.approve",
                            PanelAction::Verdict(ApprovalVerdict::AllowOnce),
                        ),
                        (
                            "t: task",
                            egui::Id::unique("agent-allow-task"),
                            "gate.approve_task",
                            PanelAction::Verdict(ApprovalVerdict::AllowForTask),
                        ),
                        (
                            "d: deny",
                            egui::Id::unique("agent-deny"),
                            "gate.deny",
                            PanelAction::BeginDenyReason,
                        ),
                    ];
                    let button_w = (card.width() - 16.0 - 8.0 * 2.0) / 3.0;
                    for (index, (label, id, name, verdict)) in buttons.into_iter().enumerate() {
                        let button = Rect::from_min_size(
                            Pos2::new(card.left() + 8.0 + index as f32 * (button_w + 8.0), y),
                            Vec2::new(button_w, line_h),
                        );
                        let hit = ui.interact(button, id, Sense::click());
                        hits.register(|| name.to_owned(), button);
                        painter.rect_filled(
                            button,
                            3.0,
                            if hit.hovered() {
                                chrome::FIELD_HI
                            } else {
                                chrome::FIELD
                            },
                        );
                        painter.text(
                            button.center(),
                            Align2::CENTER_CENTER,
                            label,
                            font.clone(),
                            self.theme.fg,
                        );
                        if hit.clicked() {
                            *action = Some(verdict);
                        }
                    }
                }
            }
            Some(Gate::Ask(ask)) => {
                if ask.choices.is_empty() {
                    painter.text(
                        Pos2::new(left, y),
                        Align2::LEFT_TOP,
                        agent_panel::ASK_TEXT_PROMPT,
                        font.clone(),
                        tone_color(Tone::Alert, self.theme),
                    );
                } else {
                    painter.text(
                        Pos2::new(left, y),
                        Align2::LEFT_TOP,
                        format!(" ask — {} ", ask.question),
                        font.clone(),
                        tone_color(Tone::Alert, self.theme),
                    );
                    y += line_h;
                    let shown = ask.choices.len().min(ASK_CHOICES_MAX);
                    for (index, choice) in ask.choices.iter().take(shown).enumerate() {
                        let row = Rect::from_min_size(
                            Pos2::new(card.left() + 4.0, y),
                            Vec2::new(card.width() - 8.0, line_h),
                        );
                        let hit = ui.interact(
                            row,
                            egui::Id::unique(("agent-ask", index)),
                            Sense::click(),
                        );
                        hits.register(|| format!("gate.ask.{index}"), row);
                        let selected = index == ask.selected;
                        if selected || hit.hovered() {
                            painter.rect_filled(row, 2.0, chrome::SELECT);
                        }
                        painter.text(
                            Pos2::new(left, y),
                            Align2::LEFT_TOP,
                            agent_panel::choice_line(choice, selected),
                            font.clone(),
                            if selected {
                                tone_color(Tone::Strong, self.theme)
                            } else {
                                self.theme.fg
                            },
                        );
                        if hit.clicked() {
                            *action = Some(PanelAction::AnswerChoice(index));
                        }
                        y += line_h;
                    }
                    if ask.choices.len() > shown {
                        painter.text(
                            Pos2::new(left, y),
                            Align2::LEFT_TOP,
                            format!("    … {} more · ↑/↓ to reach", ask.choices.len() - shown),
                            font.clone(),
                            tone_color(Tone::Dim, self.theme),
                        );
                    }
                }
            }
            None => {}
        }
        height
    }

    /// Apply one queued [`PanelAction`] — the click's shared-state
    /// mutation, routed to the same methods `agent_key` reaches.
    fn panel_action(&self, shared: &mut Shared, action: PanelAction) {
        let Some(tab) = shared.tabs.active_mut() else {
            return;
        };
        match action {
            PanelAction::Focus => {
                tab.toggle_agent_panel();
            }
            PanelAction::Verdict(verdict) => {
                if let Some(panel) = tab.agent.as_mut() {
                    panel.decide(verdict, &mut shared.toast);
                }
            }
            PanelAction::BeginDenyReason => {
                if let Some(panel) = tab.agent.as_mut() {
                    panel.begin_deny_reason();
                }
            }
            PanelAction::AnswerChoice(index) => {
                if let Some(panel) = tab.agent.as_mut() {
                    panel::answer_choice(panel, index, &mut shared.toast);
                }
            }
        }
    }

    /// The status strip: `status_text` over the same `StatusLine` the
    /// TUI composes (toast wins while live). A live suggestion adds a
    /// `spinner · suggesting` segment — the gap between the trigger
    /// and the first streamed Delta is otherwise silent.
    fn paint_status(&self, ui: &mut egui::Ui, shared: &Shared, rect: Rect) {
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, chrome::STRIP);
        let now = Instant::now();
        let toast_live = shared.toast.as_ref().is_some_and(|t| t.is_live(now));
        let text = status_line_text(shared, now);
        if shared.suggestion.is_some() && !toast_live {
            // The spinner animates one frame per ~100 ms — the repaint
            // keeps the glyph turning through the request's quiet
            // model-call stretch, not just while Delta bytes land.
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        let highlighted = toast_live;
        let color = if highlighted {
            Color32::BLACK
        } else {
            chrome::SUBTEXT
        };
        if highlighted {
            painter.rect_filled(rect, 0.0, self.theme.fg);
        }
        painter.text(
            rect.left_center() + Vec2::new(8.0, 0.0),
            Align2::LEFT_CENTER,
            text,
            FontId::monospace(12.0),
            color,
        );
    }

    /// The modal layer — painted last so overlays always win. Hosts
    /// the `Alt+P` profile picker and the `Alt+R` conversation picker
    /// (the same slots the TUI's `paint_profile_picker` and
    /// `paint_conversation_picker` occupy); T4's find bar anchors
    /// here too.
    fn paint_modals(&self, ui: &mut egui::Ui, shared: &Shared, area: Rect) {
        let painter = ui.painter();
        let font = FontId::monospace(13.0);
        let row_h = 18.0;
        if let Some(picker) = shared.picker.as_ref() {
            let h = picker.names().len() as f32 * row_h + 16.0;
            let rect = Rect::from_center_size(
                Pos2::new(
                    area.center().x,
                    area.bottom() - h / 2.0 - self.chrome.status - 8.0,
                ),
                Vec2::new(260.0, h),
            );
            painter.rect_filled(rect, 4.0, chrome::WELL);
            painter.rect_stroke(
                rect,
                4.0,
                Stroke::new(1.0, chrome::EDGE),
                egui::StrokeKind::Inside,
            );
            for (index, _) in picker.names().iter().enumerate() {
                let y = rect.top() + 8.0 + index as f32 * row_h;
                let selected = index == picker.selected();
                if selected {
                    painter.rect_filled(
                        Rect::from_min_size(
                            Pos2::new(rect.left() + 4.0, y - 1.0),
                            Vec2::new(rect.width() - 8.0, row_h),
                        ),
                        2.0,
                        chrome::SELECT,
                    );
                }
                painter.text(
                    Pos2::new(rect.left() + 12.0, y),
                    Align2::LEFT_TOP,
                    picker.row_text(index),
                    font.clone(),
                    Color32::WHITE,
                );
            }
        }
        // `Alt+R`'s conversation picker (Spec R08 §5.8): the stored
        // rows newest-first — `▸ title  age · N items` on the
        // highlighted row — over a bottom-anchored card, the hint row
        // doubling as `Ctrl+D`'s one-key confirm prompt. The list
        // windows around the highlight when it outgrows the card,
        // same as the TUI's band.
        if let Some(picker) = shared.conversations.as_ref() {
            let shown = picker.rows().len().min(CONVERSATION_ROWS);
            let start = picker
                .selected()
                .saturating_add(1)
                .saturating_sub(shown)
                .min(picker.rows().len().saturating_sub(shown));
            let h = shown as f32 * row_h + row_h + 16.0;
            let rect = Rect::from_center_size(
                Pos2::new(
                    area.center().x,
                    area.bottom() - h / 2.0 - self.chrome.status - 8.0,
                ),
                Vec2::new(440.0, h),
            );
            painter.rect_filled(rect, 4.0, chrome::WELL);
            painter.rect_stroke(
                rect,
                4.0,
                Stroke::new(1.0, chrome::EDGE),
                egui::StrokeKind::Inside,
            );
            for (offset, row) in picker.rows().iter().skip(start).take(shown).enumerate() {
                let index = start + offset;
                let y = rect.top() + 8.0 + offset as f32 * row_h;
                let selected = index == picker.selected();
                if selected {
                    painter.rect_filled(
                        Rect::from_min_size(
                            Pos2::new(rect.left() + 4.0, y - 1.0),
                            Vec2::new(rect.width() - 8.0, row_h),
                        ),
                        2.0,
                        chrome::SELECT,
                    );
                }
                painter.text(
                    Pos2::new(rect.left() + 12.0, y),
                    Align2::LEFT_TOP,
                    row.display_line(selected),
                    font.clone(),
                    Color32::WHITE,
                );
            }
            // The bottom row is the keys hint — `Ctrl+D`'s armed
            // state replaces it with the confirm prompt (us-11).
            let (hint, color) = if picker.confirming() {
                (picker.hint_text(), tone_color(Tone::Alert, self.theme))
            } else {
                (picker.hint_text(), tone_color(Tone::Dim, self.theme))
            };
            painter.text(
                Pos2::new(rect.left() + 12.0, rect.top() + 8.0 + shown as f32 * row_h),
                Align2::LEFT_TOP,
                hint,
                font.clone(),
                color,
            );
        }
        // The find bar (Spec R13-T4) — anchored above the status
        // strip, same modal slot as the picker.
        if let Some(find) = shared.find.as_ref() {
            let bar = Rect::from_min_max(
                Pos2::new(area.left(), area.bottom() - self.chrome.status - FIND_BAR_H),
                Pos2::new(area.right(), area.bottom() - self.chrome.status),
            );
            let painter = ui.painter().with_clip_rect(bar);
            painter.rect_filled(bar, 0.0, chrome::WELL);
            painter.line_segment(
                [bar.left_top(), bar.right_top()],
                Stroke::new(1.0, chrome::EDGE),
            );
            let font = FontId::monospace(13.0);
            let y = bar.center().y;
            painter.text(
                bar.left_center() + Vec2::new(8.0, 0.0),
                Align2::LEFT_CENTER,
                "find:",
                font.clone(),
                chrome::IDLE,
            );
            // The query text, then a caret — the bar's input model is
            // append/backspace, so the caret always sits at the end.
            let query_x = bar.left() + 52.0;
            let galley =
                painter.layout_no_wrap(find.query().to_string(), font.clone(), Color32::WHITE);
            painter.galley(
                Pos2::new(query_x, y - galley.size().y / 2.0),
                galley.clone(),
                Color32::WHITE,
            );
            painter.rect_filled(
                Rect::from_center_size(
                    Pos2::new(query_x + galley.size().x + 3.0, y),
                    Vec2::new(2.0, 14.0),
                ),
                0.0,
                self.theme.fg,
            );
            // Right edge: the `n/m` count (or "no matches") plus the
            // bar's key hints.
            let count = match find.position() {
                Some((n, total)) => format!("{n}/{total}"),
                None if find.query().is_empty() => String::new(),
                None => "no matches".to_string(),
            };
            let hint = if count.is_empty() {
                "type to search · Esc close".to_string()
            } else {
                format!("{count} · Enter/↓ next · Shift+Enter/↑ prev · Esc close")
            };
            painter.text(
                bar.right_center() + Vec2::new(-8.0, 0.0),
                Align2::RIGHT_CENTER,
                hint,
                font,
                chrome::IDLE,
            );
        }
    }
}

impl eframe::App for GuiApp {
    /// Pump-side bookkeeping that must run even while hidden: nothing
    /// heavy — the pump thread owns the drains; `logic` just schedules
    /// the one timed repaint a live toast needs to age out.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The Arc is cloned first so the guard never borrows `self` —
        // the same pattern `ui` needs for `&mut self` dispatch.
        let live_toast = Arc::clone(&self.shared).lock().ok().is_some_and(|shared| {
            shared
                .toast
                .as_ref()
                .is_some_and(|toast| toast.is_live(Instant::now()))
        });
        if live_toast {
            ctx.request_repaint_after(TOAST_REFRESH);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Clone the Arc BEFORE locking: a `MutexGuard` borrows the
        // Mutex, which would hold `self.shared` immutably for the
        // guard's life and block every `&mut self` call below.
        let shared_arc = Arc::clone(&self.shared);
        let mut shared = match shared_arc.lock() {
            Ok(shared) => shared,
            Err(poisoned) => poisoned.into_inner(),
        };

        // 1. Input first — the frame below paints post-input state.
        self.dispatch_events(&ctx, &mut shared);

        // The hit registry repaints with the frame (Spec R15-T3):
        // cleared at the top, every `interact` site re-registers its
        // name → rect below — the dump's `hits` is always exactly the
        // last painted frame, never a stale union of them.
        shared.hits.clear();

        // 2. Regions + cell metrics (zoom included), then the band→
        //    winsize resize — settled, not per-frame: a reflow walks
        //    each session's scrollback, so mid-drag reflows are the
        //    resize jank. The band applies once it's still for
        //    `RESIZE_SETTLE` (a dragged window keeps moving the pending
        //    value until release, then the repaint timer fires it).
        let area = ctx.content_rect();
        let regions = chrome_regions(area, self.chrome);
        let font = FontId::monospace(self.metrics.font_px());
        let cell = cell_metrics(&ctx, &self.metrics);
        let band = grid_size(regions.content, cell);
        if band != self.last_band {
            let now = Instant::now();
            match self.pending_band {
                Some((pending, since)) if pending == band => {
                    let elapsed = now.saturating_duration_since(since);
                    if elapsed >= RESIZE_SETTLE {
                        let session_size = region_session_size(&shared.panes);
                        shared.tabs.resize_all(band, session_size);
                        self.last_band = band;
                        self.pending_band = None;
                    } else {
                        ctx.request_repaint_after(RESIZE_SETTLE - elapsed);
                    }
                }
                _ => {
                    self.pending_band = Some((band, now));
                    ctx.request_repaint_after(RESIZE_SETTLE);
                }
            }
        } else if self
            .pending_band
            .is_some_and(|(pending, _)| pending == band)
        {
            self.pending_band = None;
        }

        // 3. The content split — the pane host's share comes off the
        //    band exactly like `session_size` told the PTY (the region
        //    is open while the agent claims OR an aux pane is
        //    selected). The browser's staged webview rect clears here
        //    — the pane's paint re-stages it while embedded; a `None`
        //    surviving to the sync is the hide signal.
        let shown = shared.shown_pane();
        shared.browser.set_surface(None);
        let (grid_pane, region_rect) = split_content(regions.content, shown.is_some(), cell);

        self.paint_tab_bar(ui, &mut shared, regions.tab_bar);
        self.paint_grid_pane(&ctx, ui, &mut shared, grid_pane, cell, &font);
        // The wheel hit-test mirrors THIS frame's split — a stale
        // rect would deaden the region the grid just reclaimed. Only
        // the agent claim routes wheel to its conversation scroll;
        // aux panes own their own wheel handling (T2/T3).
        self.panel_pane = if shown == Some(SidePane::Agent) {
            region_rect
        } else {
            None
        };
        if let (Some(rect), Some(shown)) = (region_rect, shown) {
            self.paint_side_region(ui, &mut shared, rect, shown);
        }
        self.paint_status(ui, &shared, regions.status);
        self.paint_modals(ui, &shared, area);
        // The webview sync — post-paint so the browser pane's staged
        // rect is this frame's: probe once while the pane shows,
        // drain the queued navigations, keep the child's bounds on
        // the staged rect (or hide it), pump glib on X11, and —
        // while a live webview is staged — schedule the bounded
        // repaint that keeps the pump fed. No blocking calls, and
        // with no staged webview the idle window still paints zero
        // frames.
        let browser_shown = shown == Some(SidePane::Aux(PaneId::Browser));
        self.web.sync(&ctx, frame, &mut shared, browser_shown);

        // The `wait_dump` barrier's delivered half (Spec R15): this
        // frame folded the pointer plugin's batch at `begin_pass` and
        // `dispatch_events` drained `injected` — with both queues
        // empty at the frame's end, every request the socket accepted
        // has run through a completed frame, so `input_done` catches
        // up. A multi-batch gesture (press→release split across
        // frames) only catches up after its LAST batch's frame.
        #[cfg(feature = "test-driver")]
        {
            let plugin_drained =
                ctx.with_plugin(|plugin: &mut crate::test_driver::DriverInputPlugin| {
                    plugin.pending() == 0
                });
            if shared.injected.is_empty() && plugin_drained == Some(true) {
                shared.input_done = shared.input_seq;
            }
        }
        drop(shared);

        if self.quit {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
    }

    fn on_exit(&mut self) {
        if let Some(mut pump) = self.pump.take() {
            pump.stop();
        }
    }
}

/// The agent panel's palette slots: its own bg and the pad its text
/// keeps off the separator.
const PANEL_BG: Color32 = Color32::from_rgb(0x1B, 0x1B, 0x1B);
const PANEL_PAD: f32 = 8.0;
/// The pane host's switcher strip height — fixed like the other
/// chrome (never cell-tied) so zoom doesn't reflow it.
const PANE_BAR_H: f32 = 22.0;
/// How many choice rows a parked ask card shows before folding —
/// the plan block's `… N more` idiom applied to the card.
const ASK_CHOICES_MAX: usize = 6;
/// How many rows the conversation picker lists before windowing —
/// the card stays a card, never a screen takeover.
const CONVERSATION_ROWS: usize = 8;

/// A click on the panel's interactive surfaces, resolved after the
/// paint pass (the responses borrow `ui`, the mutation borrows
/// `shared` — queuing keeps the two from overlapping).
enum PanelAction {
    /// Re-focus an open-but-unfocused panel (the `Alt+A` refocus
    /// arm's mouse counterpart — never a close).
    Focus,
    /// Answer the parked approve card — `AgentPanel::decide`.
    Verdict(ApprovalVerdict),
    /// The `d` key: switch the card into denial-reason mode.
    BeginDenyReason,
    /// Answer the parked choices ask as if choice `index` had been
    /// highlighted and Enter pressed.
    AnswerChoice(usize),
}

/// The gate card's paint geometry — the row metrics, the
/// horizontal span it draws inside, and the input row's top edge
/// the card anchors above. Bundles what the paint pass needs in
/// one type rather than trailing five scalars through the call.
struct GateCardLayout<'a> {
    font: &'a FontId,
    line_h: f32,
    left: f32,
    right: f32,
    input_top: f32,
}

/// A panel row's [`Tone`] → the paint color — `Strong` lifts the
/// theme foreground to full white (the same bright-foreground
/// degrade `bold_color` documents for the grid, since egui's
/// bundled monospace has no bold face).
fn tone_color(tone: Tone, theme: GridTheme) -> Color32 {
    match tone {
        Tone::Plain => theme.fg,
        Tone::Strong => Color32::WHITE,
        Tone::Dim => chrome::DIM,
        Tone::Alert => chrome::WARN,
        Tone::Good => chrome::GOOD,
        Tone::Bad => chrome::ERR,
        Tone::Info => theme.link,
    }
}

/// The creation-time cell estimate — fonts don't exist until the
/// first `Context::run`, so `GuiApp::new` can't read the glyph table.
/// One detection-token scan's cache key and result — see
/// [`GuiApp::detect_hits`].
#[cfg(feature = "test-driver")]
struct DetectHitScan {
    /// The tab the rows were scanned on.
    tab: u64,
    /// The scroll offset they were read at.
    scroll: usize,
    /// Remote sessions scan URLs only (no local fs probes).
    remote: bool,
    /// The pane the rects were computed against.
    pane: Rect,
    /// The cell metrics the rects were computed against.
    cell: CellMetrics,
    /// Every visible row's text — the scan's input.
    rows: Vec<String>,
    /// The `(name, rect)` pairs in scan order.
    out: Vec<(String, Rect)>,
}

/// Typical monospace ratios (advance ~0.6×em, line ~1.2×em) seed the
/// first tab's winsize; the first frame's real `cell_metrics` +
/// `resize_all` correct it before anything paints.
fn provisional_cell(metrics: &GuiMetrics) -> CellMetrics {
    CellMetrics {
        width: metrics.font_px() * 0.6,
        height: metrics.font_px() * 1.2,
    }
}

/// The `~` expansion root, resolved once per process — `home_dir`
/// reads the env on every call and the detect cache's `env` closure
/// asks it on each miss (§6's never-per-frame rule).
fn home_dir() -> PathBuf {
    static HOME: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    HOME.get_or_init(|| std::env::home_dir().unwrap_or_default())
        .clone()
}

/// One wheel delta → signed scroll lines, carrying the sub-line
/// remainder in `rem`: touchpads emit Point deltas well under a row
/// each and truncating every event to `usize` dropped them all to
/// zero — touchpad scroll felt dead. Line units scale by the shared
/// `WHEEL_LINES` so a tick scrolls the same 3 lines the TUI's wheel
/// binding does (CODING_STANDARDS §17 — one constant, both
/// frontends); Point divides by `cell_h`; a Page unit is `page_rows`
/// whole lines, unaccumulated (the grid arm pages through
/// `page_up`/`page_down` itself and never calls this for Page).
fn wheel_lines(
    unit: egui::MouseWheelUnit,
    delta_y: f32,
    cell_h: f32,
    page_rows: usize,
    rem: &mut f32,
) -> i32 {
    let lines = match unit {
        egui::MouseWheelUnit::Line => delta_y * WHEEL_LINES as f32,
        egui::MouseWheelUnit::Point => delta_y / cell_h,
        egui::MouseWheelUnit::Page => {
            *rem = 0.0;
            return (delta_y.signum() * page_rows.max(1) as f32) as i32;
        }
    };
    *rem += lines;
    let whole = rem.trunc() as i32;
    *rem -= whole as f32;
    whole
}

/// `char`-less cell metrics from egui's font tables — 'M' is the
/// monospace advance in every font egui bundles (Hack's is uniform).
fn cell_metrics(ctx: &egui::Context, metrics: &GuiMetrics) -> CellMetrics {
    let font = FontId::monospace(metrics.font_px());
    ctx.fonts_mut(|fonts| CellMetrics {
        width: fonts.glyph_width(&font, 'M').max(1.0),
        height: fonts.row_height(&font).max(1.0),
    })
}

/// The find bar's open chord: `Ctrl+Shift+F`, or `⌘F` on macOS —
/// with `Alt` excluded so `Option`/`Alt` chords keep passing through.
/// Bare `Ctrl+F` is deliberately NOT the chord: it is readline's
/// `C-f` forward-char, and stealing it would leave shell users no way
/// to move the cursor a character — it falls through to the PTY like
/// every unbound key. GUI-local like zoom: the TUI has no find bar
/// so the keymap can't own the chord.
fn find_open_chord(key: egui::Key, modifiers: egui::Modifiers) -> bool {
    if key != egui::Key::F || modifiers.alt {
        return false;
    }
    modifiers.mac_cmd || (modifiers.ctrl && modifiers.shift)
}

/// The status strip's text at `now` — the same composition
/// `paint_status` draws: a live toast wins outright, else the
/// `StatusLine` session summary, and a live suggestion appends a
/// `· <spinner> suggesting` segment (Spec R04; the repaint cadence
/// stays the painter's concern). Factored so the test-driver `dump`
/// ships the exact string the user reads (Spec R15-T6) instead of a
/// re-derived near-copy.
pub(crate) fn status_line_text(shared: &Shared, now: Instant) -> String {
    let toast_live = shared.toast.as_ref().is_some_and(|t| t.is_live(now));
    let mut text = match shared.tabs.active() {
        Some(tab) => {
            let model = shared.config.model(&shared.profile);
            status_text(
                &StatusLine {
                    cwd: tab.cwd(),
                    shell: tab.title(),
                    model: &model,
                    profile: profile::status_slot(&shared.profile),
                    block: tab.last_block_status(),
                    remote: tab.remote_target(),
                    exit: tab.exit_status(),
                },
                shared.toast.as_ref(),
                now,
            )
        }
        None => String::new(),
    };
    if shared.suggestion.is_some() && !toast_live {
        // Braille spinner — one frame per ~100 ms.
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let millis = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as usize;
        let frame = FRAMES[(millis / 100) % FRAMES.len()];
        text = format!("{text} · {frame} suggesting");
    }
    text
}

/// The `session_size` the shared dispatch fns take under the pane
/// host (Spec R14 §5.13): ORs the app-global aux selection into each
/// tab's agent-open flag, so every session's winsize stays the GRID
/// PANE's cells — never the window's — while the region hosts ANY
/// pane. `resize_all`/`resize_active` read the claim per tab
/// internally; the aux half is app-global, so it applies to every
/// tab uniformly.
fn region_session_size(panes: &PaneHost) -> impl Fn(Size, bool) -> Size {
    let aux_open = panes.selected().is_some();
    move |size, agent_open| session_size(size, agent_open || aux_open)
}

/// Scroll the view the minimum amount that puts `hit`'s row on screen:
/// a visible match doesn't move the view, one above tops at its line
/// (the block-nav rule `scroll_to_line` applies), one below lands on
/// the bottom row.
fn reveal_hit(shared: &mut Shared, hit: SearchHit) {
    let Some(tab) = shared.tabs.active_mut() else {
        return;
    };
    let scrollback = i64::try_from(tab.scrollback()).unwrap_or(i64::MAX);
    let offset = i64::try_from(tab.scroll_offset()).unwrap_or(i64::MAX);
    let rows = tab.grid().lines() as i64;
    // Grid line → live coordinate: `live = line + scrollback` — the
    // same conversion `select_block`'s `to_grid` runs in reverse.
    let live = hit.line as i64 + scrollback;
    let screen = hit.line as i64 + offset;
    if screen < 0 {
        tab.scroll_to_line(live.max(0) as usize);
    } else if screen >= rows {
        tab.scroll_to_line((live - rows + 1).max(0) as usize);
    }
}

/// Boot failures that abort the window — spawn errors only; config,
/// store and ctl failures degrade to toasts like the TUI.
#[derive(Debug)]
pub struct BootError(String);

impl std::fmt::Display for BootError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BootError {}

#[cfg(test)]
mod tests {
    //! Dispatch tests at the same seams the TUI's `handle_event` tests
    //! exercise (Spec R04, R11-T2 — `dispatch_key` IS the GUI's
    //! `handle_event`): post-translation [`KeyEvent`]s over real
    //! `/bin/sh` tabs, where the shadow, the pin and the PTY byte
    //! stream are the observables. `pump: None` keeps the drain
    //! thread out — `wait_until` drives the same drains by hand, the
    //! way the pump's `drain_pass` does.

    use super::*;
    use ait_agent::SuggestEvent;
    use ait_app::suggest;
    use ait_app::tabs::SessionTemplate;
    use ait_term::{LiveSessionOptions, Modifiers};

    /// A manager on `/bin/sh` — deterministic tty echo for the
    /// trigger tests (the TUI's `sh_manager` rationale: an
    /// interactive `$SHELL`'s prompt timing can bury typed text
    /// mid-line and `ends_with` could never hold).
    fn sh_manager() -> TabManager {
        TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(std::ffi::OsString::from("/bin/sh")),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// The dispatch harness: a `GuiApp` minus its pump and window,
    /// sharing one `Arc<Mutex<Shared>>` with the test body — the same
    /// shape `ui` locks per frame.
    struct Harness {
        app: GuiApp,
        shared: Arc<Mutex<Shared>>,
        ctx: egui::Context,
    }

    impl Harness {
        fn new(tabs: TabManager) -> Self {
            let shared = Arc::new(Mutex::new(Shared {
                tabs,
                config: LiveSettings::adopt(Settings::default()),
                store: StoreHandle::disabled(),
                toast: None,
                // No watcher and no roots — tests never touch the
                // filesystem for skills.
                skills: LiveSkills::unwatched(),
                profile: profile::DEFAULT_PROFILE.to_string(),
                picker: None,
                conversations: None,
                copied_text: None,
                // `NoSecrets` keeps keyring lookups out of tests —
                // resolution never blocks on the Secret Service.
                suggester: CommandSuggester::with_secret_store(config::NoSecrets),
                suggestion: SuggestionPin::new(),
                find: None,
                panes: PaneHost::default(),
                explorer: ExplorerState::default(),
                browser: BrowserState::default(),
                #[cfg(feature = "test-driver")]
                injected: std::collections::VecDeque::new(),
                hits: HitTargets::default(),
                #[cfg(feature = "test-driver")]
                detect_hover: crate::test_driver::DetectHover::default(),
                #[cfg(feature = "test-driver")]
                cell_metrics: None,
                #[cfg(feature = "test-driver")]
                input_seq: 0,
                #[cfg(feature = "test-driver")]
                input_done: 0,
            }));
            Self {
                app: GuiApp {
                    shared: Arc::clone(&shared),
                    pump: None,
                    theme: GridTheme::default(),
                    metrics: GuiMetrics {
                        base_px: 15.0,
                        zoom: 1.0,
                    },
                    chrome: Chrome::default(),
                    last_band: Size::new(40, 10),
                    grid_pane: None,
                    panel_pane: None,
                    panel_rows: 0,
                    grid_wheel_rem: 0.0,
                    panel_wheel_rem: 0.0,
                    pending_band: None,
                    row_galleys: crate::paint::RowGalleys::default(),
                    detect_cache: detect::DetectCache::default(),
                    #[cfg(feature = "test-driver")]
                    detect_hits: None,
                    web: WebSlot::new(),
                    quit: false,
                },
                shared,
                ctx: egui::Context::default(),
            }
        }

        /// One post-translation key through the dispatch — the seam
        /// `egui::Event::Key` + `translate_key` feed it in `ui`.
        fn press(&mut self, key: Key, modifiers: Modifiers) {
            let mut shared = self.shared.lock().unwrap();
            self.app
                .dispatch_key(&self.ctx, &mut shared, Some(KeyEvent::new(key, modifiers)));
        }

        /// The shared state under the same lock the UI holds —
        /// short-lived guards only, `pred`s lock per probe.
        fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
            self.shared.lock().unwrap()
        }

        /// `GuiApp::toggle_pane` under the same lock `press` takes —
        /// the pane chord's and the switcher chip's dispatch seam.
        fn toggle_pane(&mut self, id: PaneId) {
            let mut shared = self.shared.lock().unwrap();
            self.app.toggle_pane(&mut shared, id);
        }
    }

    /// Drain sessions and suggester events until `pred` holds, or
    /// fail after a generous bound — the drains `drain_pass` runs,
    /// minus the repaint decision.
    fn wait_until(harness: &Harness, pred: impl Fn(&Harness) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !pred(harness) {
            {
                let mut shared = harness.shared();
                let Shared {
                    tabs,
                    suggester,
                    suggestion,
                    toast,
                    ..
                } = &mut *shared;
                tabs.drain_events();
                suggestion.drain(suggester.events(), tabs, toast);
            }
            assert!(Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The logical input line at the active tab's cursor.
    fn logical(harness: &Harness) -> String {
        let shared = harness.shared();
        let tab = shared.tabs.active().unwrap();
        suggest::logical_line(&tab.grid(), tab.cursor())
    }

    /// Every grid row — scrollback then viewport — as trimmed text.
    fn rows(harness: &Harness) -> Vec<String> {
        let shared = harness.shared();
        let tab = shared.tabs.active().unwrap();
        tab.grid()
            .scrollback_rows()
            .chain(tab.grid().visible_rows())
            .map(|row| row.text().trim_end().to_owned())
            .collect()
    }

    /// Wait for the prompt to exist BEFORE typing (the TUI's race
    /// note: the kernel echo must land after the prompt), then press
    /// `text`'s characters.
    fn type_keys(harness: &mut Harness, text: &str) {
        wait_until(harness, |h| !logical(h).is_empty());
        for c in text.chars() {
            harness.press(Key::Char(c), Modifiers::NONE);
        }
    }

    // ---- The `# `+Enter and Alt+Enter triggers (Spec R04) ----

    /// The `# `+Enter trigger end to end through `dispatch_key`:
    /// the shadow built by typed keys verifies against the PTY's own
    /// echo, the request fires into the suggester pinned to its tab —
    /// and Enter is forwarded normally, so the comment lands in the
    /// shell's line discipline like any input (us-2). The
    /// unconfigured default settings answer `Error`, which the drain
    /// surfaces as a toast and releases the pin (us-22).
    #[test]
    fn hash_enter_fires_request_and_forwards_enter() {
        let mut harness = Harness::new(sh_manager());
        type_keys(&mut harness, "# ls");
        // The verification reads the grid — wait for the PTY echo.
        wait_until(&harness, |h| logical(h).ends_with("# ls"));
        assert!(!harness.shared().tabs.active().unwrap().shadow.is_stale());

        harness.press(Key::Enter, Modifiers::NONE);

        // The verified request fired, pinned to tab 0's id (us-14).
        {
            let shared = harness.shared();
            assert_eq!(
                shared.suggestion.pinned_id(),
                Some(shared.tabs.tabs()[0].id),
                "the '# ' request fired into the suggester"
            );
            // The shadow reset for the next line.
            assert_eq!(shared.tabs.active().unwrap().shadow.text(), "");
        }

        // Enter reached the PTY normally: the kernel echoes the
        // submitted line and the cursor advances past it.
        wait_until(&harness, |h| {
            h.shared().tabs.active().unwrap().cursor().line > 0
        });

        // Default settings hold no model — the worker answers `Error`,
        // the drain toasts it and releases the pin.
        wait_until(&harness, |h| {
            let shared = h.shared();
            shared.toast.is_some() && shared.suggestion.is_none()
        });
        let shared = harness.shared();
        let toast = shared.toast.as_ref().unwrap();
        assert!(
            toast.text().contains("profile"),
            "the toast names the model-resolution failure: {}",
            toast.text()
        );
    }

    /// us-11/12/14: `Alt+Enter` on a verified bare line fires the
    /// request pinned to its tab, erases the typed line via DELs (the
    /// grid returns to the bare prompt), resets the shadow, and the
    /// suggestion streams into the cleared input line — the shell's
    /// own echo IS the live preview.
    #[test]
    fn alt_enter_fires_erases_and_streams_into_the_cleared_line() {
        let mut harness = Harness::new(sh_manager());
        wait_until(&harness, |h| !logical(h).is_empty());
        let prompt = logical(&harness);
        type_keys(&mut harness, "git undo");
        wait_until(&harness, |h| logical(h).ends_with("git undo"));

        harness.press(Key::Enter, Modifiers::ALT);

        {
            let shared = harness.shared();
            assert_eq!(
                shared.suggestion.pinned_id(),
                Some(shared.tabs.tabs()[0].id),
                "the bare line fired as a request, pinned to its tab"
            );
            let shadow = &shared.tabs.active().unwrap().shadow;
            assert_eq!(shadow.text(), "", "the erased line resets the shadow");
            assert!(!shadow.is_stale());
        }

        // us-12: the typed line is gone from the grid — the request
        // text and the incoming command can never interleave.
        wait_until(&harness, |h| logical(h) == prompt);

        // The real request answers `Error` (default settings hold no
        // model) — drain it out of the channel so it can't race the
        // manual pin the stream seam is driven through below.
        wait_until(&harness, |h| h.shared().suggestion.is_none());

        // The shared stream path types into the cleared line — the
        // tty echo makes the preview visible on the grid (us-3).
        pin_suggestion(&harness, 0);
        fire(&harness, SuggestEvent::Delta("git reset HEAD~1".into()));
        wait_until(&harness, |h| logical(h).ends_with("git reset HEAD~1"));
        fire(&harness, SuggestEvent::Done);
        assert!(harness.shared().suggestion.is_none());
    }

    /// us-13: a desynced shadow (history recall via `Up`) cannot fire
    /// — `Alt+Enter` leaves the line, the shadow and the suggester
    /// alone, the desync-is-inert rule.
    #[test]
    fn alt_enter_on_a_desynced_shadow_does_not_fire() {
        let mut harness = Harness::new(sh_manager());
        type_keys(&mut harness, "git undo");
        wait_until(&harness, |h| logical(h).ends_with("git undo"));
        harness.press(Key::Up, Modifiers::NONE);
        assert!(harness.shared().tabs.active().unwrap().shadow.is_stale());

        harness.press(Key::Enter, Modifiers::ALT);

        assert!(
            harness.shared().suggestion.is_none(),
            "stale shadow → no trigger"
        );
        assert!(harness.shared().toast.is_none());
    }

    // ---- The §5.10.4 classifier trigger (Spec R11-T2) ----

    /// A verified natural-language line + Enter is swallowed and
    /// rewritten — `Ctrl+U`, then `# <line>` + `\r` — while the
    /// `SuggestRequest` path arms the stream the fresh prompt
    /// receives. The raw sentence never ran.
    #[test]
    fn natural_language_enter_rewrites_to_a_hash_request() {
        let mut harness = Harness::new(sh_manager());
        wait_until(&harness, |h| !logical(h).is_empty());
        let prompt = logical(&harness);
        type_keys(&mut harness, "what is the largest file");
        wait_until(&harness, |h| {
            logical(h).ends_with("what is the largest file")
        });
        assert!(!harness.shared().tabs.active().unwrap().shadow.is_stale());

        harness.press(Key::Enter, Modifiers::NONE);

        // The request armed the suggester pinned to tab 0 — the same
        // in-flight contract the `# ` trigger honors.
        {
            let shared = harness.shared();
            assert_eq!(
                shared.suggestion.pinned_id(),
                Some(shared.tabs.tabs()[0].id),
                "the classifier's request armed the suggestion path"
            );
            let shadow = &shared.tabs.active().unwrap().shadow;
            assert_eq!(shadow.text(), "");
            assert!(!shadow.is_stale());
        }

        // The rewrite reached the shell: the `# ` comment echoed and
        // submitted — a fresh prompt row follows it.
        wait_until(&harness, |h| {
            rows(h)
                .iter()
                .any(|row| row.ends_with("# what is the largest file"))
        });
        wait_until(&harness, |h| logical(h) == prompt);
        // …and the typed line never executed: a forwarded Enter would
        // have run it and `what` resolves nowhere.
        assert!(
            !rows(&harness).iter().any(|row| row.contains("not found")),
            "the raw sentence never reached the shell: {:?}",
            rows(&harness)
        );
    }

    /// `input.classifier = "off"` submits every line verbatim — no
    /// pin, no rewrite, the shell's own error for the non-command.
    #[test]
    fn classifier_off_submits_verbatim() {
        let mut harness = Harness::new(sh_manager());
        {
            let mut shared = harness.shared();
            shared.config = LiveSettings::adopt(
                Settings::from_toml("[input]\nclassifier = \"off\"\n").unwrap(),
            );
        }
        type_keys(&mut harness, "what is the largest file");
        wait_until(&harness, |h| {
            logical(h).ends_with("what is the largest file")
        });

        harness.press(Key::Enter, Modifiers::NONE);

        assert!(harness.shared().suggestion.is_none());
        wait_until(&harness, |h| {
            rows(h).iter().any(|row| row.contains("not found"))
        });
        // The `# ` prompt makes a verbatim `# what …` row
        // indistinguishable from the text alone — the rewrite's tell
        // is the doubled hash.
        assert!(
            !rows(&harness)
                .iter()
                .any(|row| row.contains("# # what is the largest file")),
            "the line was never rewritten: {:?}",
            rows(&harness)
        );
    }

    // ---- Streaming insertion + in-flight keys (Spec R04 T5) ----
    //
    // The insertion seam is byte-exact like the TUI's: the pinned tab
    // runs `stty raw; head -c N > <file>` so the capture holds the
    // exact bytes the PTY received — envelope markers, `Ctrl+U` and
    // all. `SuggestEvent`s are injected through `SuggestionPin::apply`
    // (the seam the pump's drain uses) instead of a real model.

    /// A unique temp path for one test's capture file.
    fn capture_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ait-gui-t6-{name}-{}", std::process::id()))
    }

    /// Run `line` on the active tab's shell once its prompt exists —
    /// the shell must be up before `stty`/`head` can be started.
    fn run_line(harness: &Harness, line: &str) {
        wait_until(harness, |h| !logical(h).is_empty());
        harness
            .shared()
            .tabs
            .active()
            .unwrap()
            .session
            .send_bytes(format!("{line}\n").as_bytes())
            .unwrap();
    }

    /// Run `line` and wait until `path` exists — the `> path`
    /// redirect proves the preceding `stty raw` applied; without that
    /// sync, bytes sent early hit the still-canonical tty.
    fn run_capture(harness: &Harness, line: &str, path: &std::path::Path) {
        run_line(harness, line);
        wait_until(harness, |_h| path.exists());
    }

    /// The state a fired `SuggestRequest` leaves behind — a pin on
    /// `tab`, re-resolved through the tab's stable id.
    fn pin_suggestion(harness: &Harness, tab: usize) {
        let mut shared = harness.shared();
        shared.suggestion = SuggestionPin::staged(shared.tabs.tabs()[tab].id);
    }

    /// Inject one suggester event through the pump's drain seam.
    fn fire(harness: &Harness, event: SuggestEvent) {
        let mut shared = harness.shared();
        let Shared {
            tabs,
            suggestion,
            toast,
            ..
        } = &mut *shared;
        suggestion.apply(tabs, event, toast);
    }

    /// Wait for the capture file to hold `len` bytes, then read it.
    fn wait_file(harness: &Harness, path: &std::path::Path, len: usize) -> Vec<u8> {
        wait_until(harness, |_h| {
            std::fs::read(path).is_ok_and(|bytes| bytes.len() >= len)
        });
        std::fs::read(path).unwrap()
    }

    /// us-3/4: a `Delta` sequence lands byte-for-byte as typed text on
    /// the pinned session — the streamed preview is the shell's own
    /// echo of those bytes (no newline can ever arrive, so nothing
    /// executes).
    #[test]
    fn deltas_type_byte_for_byte() {
        let path = capture_path("deltas");
        let harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!("stty raw; head -c 10 > {}", path.display()),
            &path,
        );
        pin_suggestion(&harness, 0);

        for delta in ["git ", "stat", "us"] {
            fire(&harness, SuggestEvent::Delta(delta.to_string()));
        }
        fire(&harness, SuggestEvent::Done);

        assert_eq!(wait_file(&harness, &path, 10), b"git status");
        assert!(
            harness.shared().suggestion.is_none(),
            "Done released the pin"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// us-8: plain Esc on the pinned tab cancels — the session sees
    /// the envelope close and `Ctrl+U` (in that order) wiping the
    /// partial line. The pin survives until the worker's terminal
    /// event so a post-cancel `Delta` can't retype onto the wiped
    /// line and the cancellation `Error` arrives silently.
    #[test]
    fn esc_cancels_closes_the_envelope_and_wipes_the_line() {
        let path = capture_path("esc");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!(
                "stty raw; printf '\\033[?2004h'; head -c 20 > {}",
                path.display()
            ),
            &path,
        );
        wait_until(&harness, |h| {
            h.shared().tabs.active().unwrap().session.bracketed_paste()
        });
        pin_suggestion(&harness, 0);
        fire(&harness, SuggestEvent::Delta("partial".into()));
        // Shadow text a mid-flight keystroke could have left — the
        // Ctrl+U wipe makes it untrue, so the cancel resets it.
        harness
            .shared()
            .tabs
            .active_mut()
            .unwrap()
            .shadow
            .update(&KeyEvent::new(Key::Char('x'), Modifiers::NONE));

        harness.press(Key::Escape, Modifiers::NONE);

        assert_eq!(
            wait_file(&harness, &path, 20),
            b"\x1b[200~partial\x1b[201~\x15",
            "close the envelope, THEN wipe the line — spec order"
        );
        assert!(
            harness.shared().suggestion.is_cancelled(),
            "the pin survives until the terminal event"
        );
        assert!(
            !harness.shared().suggestion.paste_is_open(),
            "the envelope is closed"
        );
        assert!(
            harness
                .shared()
                .tabs
                .active()
                .unwrap()
                .shadow
                .text()
                .is_empty(),
            "the wiped line resets the input shadow"
        );

        fire(&harness, SuggestEvent::Delta("z".into()));
        fire(&harness, SuggestEvent::Error("cancelled".into()));
        assert!(harness.shared().suggestion.is_none());
        assert!(
            harness.shared().toast.is_none(),
            "a user-initiated cancel is not toasted"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// us-9: Enter on the pinned tab never reaches the PTY mid-stream
    /// — not even `Ctrl+Enter`, which encodes `\r` and would execute
    /// the half-written command.
    #[test]
    fn enter_is_swallowed_while_a_suggestion_streams() {
        let path = capture_path("enter");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!("stty raw; head -c 3 > {}", path.display()),
            &path,
        );
        pin_suggestion(&harness, 0);
        fire(&harness, SuggestEvent::Delta("a".into()));

        harness.press(Key::Enter, Modifiers::NONE);
        harness.press(Key::Enter, Modifiers::CONTROL);
        assert!(
            harness.shared().suggestion.is_some(),
            "Enter does not end the request"
        );

        fire(&harness, SuggestEvent::Delta("bc".into()));
        fire(&harness, SuggestEvent::Done);

        assert_eq!(
            wait_file(&harness, &path, 3),
            b"abc",
            "a leaked Enter would put a \\r inside the capture"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// us-10: with nothing in flight Esc and Enter reach the PTY
    /// normally — and a stray `Delta` drops silently.
    #[test]
    fn esc_and_enter_behave_normally_with_nothing_in_flight() {
        let path = capture_path("idle");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!("stty raw; head -c 2 > {}", path.display()),
            &path,
        );

        fire(&harness, SuggestEvent::Delta("nope".into()));
        harness.press(Key::Enter, Modifiers::NONE);
        harness.press(Key::Escape, Modifiers::NONE);

        assert_eq!(wait_file(&harness, &path, 2), b"\r\x1b");
        let _ = std::fs::remove_file(&path);
    }

    /// The find chord is `Ctrl+Shift+F` (or `⌘F` on macOS): bare
    /// `Ctrl+F` is readline's forward-char and must reach the PTY —
    /// the predicate and the byte stream both say so (R13 review F5).
    #[test]
    fn the_find_chord_leaves_ctrl_f_to_the_pty() {
        // The predicate: Shift is required with Ctrl, `⌘` stays the
        // macOS chord (shifted or not), and Alt never opens the bar.
        let mods = |ctrl, shift, alt, mac_cmd| egui::Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd,
            // `command` mirrors the platform accelerator, like egui's
            // `Modifiers::default()` composition does on each host.
            command: ctrl || mac_cmd,
        };
        assert!(find_open_chord(
            egui::Key::F,
            mods(true, true, false, false)
        ));
        assert!(find_open_chord(
            egui::Key::F,
            mods(false, false, false, true)
        ));
        assert!(find_open_chord(
            egui::Key::F,
            mods(false, true, false, true)
        ));
        assert!(
            !find_open_chord(egui::Key::F, mods(true, false, false, false)),
            "bare Ctrl+F is PTY input"
        );
        assert!(!find_open_chord(
            egui::Key::F,
            mods(true, true, true, false)
        ));
        assert!(!find_open_chord(
            egui::Key::G,
            mods(true, true, false, false)
        ));

        // The PTY path: the unshifted chord dispatches PassThrough and
        // lands in the session as the \x06 byte forward-char reads —
        // nothing in the dispatch opens the bar on it.
        let path = capture_path("ctrlf");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!("stty raw; head -c 1 > {}", path.display()),
            &path,
        );
        harness.press(Key::Char('f'), Modifiers::CONTROL);
        assert_eq!(wait_file(&harness, &path, 1), b"\x06");
        assert!(harness.shared().find.is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// Touchpad Point deltas arrive well under a row each — the
    /// remainder carries them into whole lines instead of
    /// truncating every event to zero (the dead-touchpad fix);
    /// a direction reversal eats the carry first, and a Line tick
    /// still lands its `WHEEL_LINES` whole.
    #[test]
    fn sub_row_deltas_accumulate_into_whole_lines() {
        use egui::MouseWheelUnit as Unit;
        let cell = 18.0;
        let mut rem = 0.0;
        // Five 0.3-row flicks: none scrolls alone, together 1.5 rows.
        for _ in 0..5 {
            let lines = wheel_lines(Unit::Point, 0.3 * cell, cell, 0, &mut rem);
            assert!((0..=1).contains(&lines));
        }
        assert_eq!(
            wheel_lines(Unit::Point, 0.3 * cell, cell, 0, &mut rem),
            0,
            "1.8 rows in — the carry keeps the sixth flick short"
        );
        assert_eq!(
            wheel_lines(Unit::Point, 0.3 * cell, cell, 0, &mut rem),
            1,
            "the accumulated 2.1 rows cross the second line"
        );
        // A reverse flick eats the carry before scrolling back.
        assert_eq!(wheel_lines(Unit::Point, -0.2 * cell, cell, 0, &mut rem), 0);
        // A Line tick is its whole `WHEEL_LINES` — no carry needed.
        let mut rem = 0.0;
        assert_eq!(
            wheel_lines(Unit::Line, 1.0, cell, 0, &mut rem),
            WHEEL_LINES as i32
        );
    }

    // ---- The pane host (Spec R14 §5.13, R14-T1) ----

    /// Feed native `egui::Event`s through `dispatch_events` — the
    /// seam `ui` runs each frame, so the chords' interception is
    /// exercised at the same point it happens live.
    fn feed(harness: &mut Harness, events: Vec<egui::Event>) {
        harness.ctx.begin_pass(egui::RawInput {
            events,
            ..Default::default()
        });
        let mut shared = harness.shared.lock().unwrap();
        harness.app.dispatch_events(&harness.ctx, &mut shared);
    }

    /// `Ctrl+Shift+E`/`Ctrl+Shift+B` toggle their slots through the
    /// real event path — intercepted before `translate_key`, so the
    /// keymap never sees them and the PTY stays silent throughout.
    #[test]
    fn pane_chords_toggle_through_the_native_event_path() {
        let path = capture_path("panechords");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!("stty raw; head -c 1 > {}", path.display()),
            &path,
        );
        let modifiers = egui::Modifiers {
            ctrl: true,
            shift: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        // A physical tap — press AND release: egui marks a second
        // press while the key is still down as `repeat`, so distinct
        // toggles need the release like real input delivers.
        let tap = |key| {
            vec![
                egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
                egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers,
                },
            ]
        };

        feed(&mut harness, tap(egui::Key::E));
        assert_eq!(harness.shared().panes.selected(), Some(PaneId::Explorer));
        feed(&mut harness, tap(egui::Key::B));
        assert_eq!(harness.shared().panes.selected(), Some(PaneId::Browser));
        feed(&mut harness, tap(egui::Key::B));
        assert_eq!(harness.shared().panes.selected(), None);

        // `head -c 1` is still blocked on its first byte — nothing
        // leaked through three chord presses.
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// The chords hold while an overlay owns focus — the find bar's
    /// query and a picker's rows would lose the region under them,
    /// so `Ctrl+Shift+E` is swallowed rather than toggled (and the
    /// swallowed chord still never reaches the PTY).
    #[test]
    fn pane_chords_hold_while_an_overlay_owns_focus() {
        let path = capture_path("panemodal");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!("stty raw; head -c 1 > {}", path.display()),
            &path,
        );
        // A physical tap — press AND release (see the strobe test:
        // a held-down key's later presses read as `repeat`).
        let modifiers = egui::Modifiers {
            ctrl: true,
            shift: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        let tap = |key| {
            vec![
                egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
                egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers,
                },
            ]
        };

        // The find bar owns focus — the chord is swallowed.
        {
            let mut shared = harness.shared.lock().unwrap();
            harness.app.open_find(&mut shared);
        }
        feed(&mut harness, tap(egui::Key::E));
        assert_eq!(harness.shared().panes.selected(), None);

        // A modal picker owns it the same way.
        harness.shared().find = None;
        harness.shared().picker = Some(ProfilePicker::new(vec!["a".into()], "a"));
        feed(&mut harness, tap(egui::Key::E));
        assert_eq!(harness.shared().panes.selected(), None);

        // Overlays gone — the same chord toggles again.
        harness.shared().picker = None;
        feed(&mut harness, tap(egui::Key::E));
        assert_eq!(harness.shared().panes.selected(), Some(PaneId::Explorer));

        // Nothing leaked through three chord presses.
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// A held chord's autorepeat must not strobe the toggle — one
    /// flip per physical press; the repeat event is still swallowed
    /// (nothing reaches the PTY either way).
    #[test]
    fn a_held_pane_chord_does_not_strobe_the_toggle() {
        let path = capture_path("panerepeat");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!("stty raw; head -c 1 > {}", path.display()),
            &path,
        );
        let modifiers = egui::Modifiers {
            ctrl: true,
            shift: true,
            command: true,
            ..egui::Modifiers::NONE
        };
        let press = |repeat| egui::Event::Key {
            key: egui::Key::E,
            physical_key: None,
            pressed: true,
            repeat,
            modifiers,
        };
        let release = egui::Event::Key {
            key: egui::Key::E,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        };

        feed(&mut harness, vec![press(false)]);
        assert_eq!(harness.shared().panes.selected(), Some(PaneId::Explorer));
        // Autorepeat while the chord is held — the selection stays.
        // (egui itself marks a press while the key is still down as
        // `repeat`, so real autorepeat arrives exactly like this.)
        for _ in 0..3 {
            feed(&mut harness, vec![press(true)]);
        }
        assert_eq!(
            harness.shared().panes.selected(),
            Some(PaneId::Explorer),
            "repeats never toggled the pane back off"
        );
        // The key comes up and goes down again — a real second press
        // still toggles.
        feed(&mut harness, vec![release, press(false)]);
        assert_eq!(harness.shared().panes.selected(), None);

        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// On twin-emitting platforms an `Alt+<letter>` chord arrives as
    /// `Key{.., alt}` + `Text("x")` — the Text twin must land
    /// nowhere: not the PTY, and not a shown pane's address entry
    /// (R14-T3's browser used to append it). Real text while the
    /// pane shows still appends normally.
    #[test]
    fn an_alt_chord_text_twin_never_reaches_a_shown_pane() {
        let path = capture_path("panetwin");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!("stty raw; head -c 1 > {}", path.display()),
            &path,
        );
        harness.toggle_pane(PaneId::Browser);
        let chord = egui::Event::Key {
            key: egui::Key::X,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                alt: true,
                ..egui::Modifiers::NONE
            },
        };

        // The twin pair — the Key's translated char is eaten, so the
        // Text payload appends nothing to the entry.
        feed(&mut harness, vec![chord, egui::Event::Text("x".into())]);
        assert_eq!(harness.shared().browser.entry(), "");

        // Ordinary text still lands — the filter only eats twins.
        feed(&mut harness, vec![egui::Event::Text("docs".into())]);
        assert_eq!(harness.shared().browser.entry(), "docs");

        // `head -c 1` is still blocked — nothing leaked to the PTY.
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// The twin window is the ADJACENT Text only: a chord whose
    /// press+release is followed by typed text in the SAME event
    /// batch (injected input, a scripted macro — native streams
    /// coalesce like this too) must not lose the typed char that
    /// happens to share the chord's letter (R15-T2: `Alt+R` then
    /// `printf` used to land `pintf`).
    #[test]
    fn a_chords_twin_window_closes_at_the_release() {
        let mut harness = Harness::new(sh_manager());
        wait_until(&harness, |h| !logical(h).is_empty());
        let modifiers = egui::Modifiers {
            alt: true,
            ..egui::Modifiers::NONE
        };
        feed(
            &mut harness,
            vec![
                egui::Event::Key {
                    key: egui::Key::R,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                },
                egui::Event::Key {
                    key: egui::Key::R,
                    physical_key: None,
                    pressed: false,
                    repeat: false,
                    modifiers,
                },
                egui::Event::Text("printf".into()),
            ],
        );
        // `Alt+R` on a disabled store toasts — and the whole typed
        // line survived, 'r' included.
        assert_eq!(
            harness.shared().tabs.active().unwrap().shadow.text(),
            "printf"
        );
    }

    /// The winsize invariant under the host: a pane toggle narrows
    /// EVERY session to the grid-pane columns and the deselect
    /// restores the band — the agent-open rule, unchanged for a new
    /// claimant (R14 us-30).
    #[test]
    fn pane_toggle_resizes_every_session_to_the_grid_pane() {
        let mut harness = Harness::new(sh_manager());
        harness.toggle_pane(PaneId::Explorer);
        {
            let shared = harness.shared();
            // The 40-column band's region share leaves 24 columns.
            assert_eq!(shared.tabs.active().unwrap().session.size().columns, 24);
            assert_eq!(shared.shown_pane(), Some(SidePane::Aux(PaneId::Explorer)));
        }
        harness.toggle_pane(PaneId::Explorer);
        let shared = harness.shared();
        assert_eq!(shared.tabs.active().unwrap().session.size().columns, 40);
        assert_eq!(shared.shown_pane(), None);
    }

    /// The agent panel claims the region while open — `Alt+A` still
    /// runs its open→focus→close cycle — and an aux selection asks
    /// the claim to yield: the panel closes (its conversation
    /// survives) and the pane takes the region at the same winsize.
    /// `Alt+A` brings the claim straight back over the selection.
    #[test]
    fn the_agent_claim_yields_only_to_an_explicit_selection() {
        let mut harness = Harness::new(sh_manager());
        harness.press(Key::Char('a'), Modifiers::ALT);
        {
            let shared = harness.shared();
            assert!(shared.tabs.active().unwrap().agent_panel_open());
            assert_eq!(shared.shown_pane(), Some(SidePane::Agent));
        }
        harness.toggle_pane(PaneId::Browser);
        {
            let shared = harness.shared();
            assert!(
                !shared.tabs.active().unwrap().agent_panel_open(),
                "the claim yielded to the explicit selection"
            );
            assert_eq!(shared.shown_pane(), Some(SidePane::Aux(PaneId::Browser)));
            // The region never closed through the swap: same winsize.
            assert_eq!(shared.tabs.active().unwrap().session.size().columns, 24);
        }
        // `Alt+A` reopens — the claim returns over the still-live
        // selection.
        harness.press(Key::Char('a'), Modifiers::ALT);
        let shared = harness.shared();
        assert!(shared.tabs.active().unwrap().agent_panel_open());
        assert_eq!(shared.shown_pane(), Some(SidePane::Agent));
        assert_eq!(shared.panes.selected(), Some(PaneId::Browser));
    }

    /// While a pane shows it owns unbound input (the find bar's
    /// focus rule): a plain char routes to the pane — the input
    /// shadow stays empty, so nothing reached the PTY — and Esc
    /// returns focus to the grid.
    #[test]
    fn a_shown_pane_owns_unbound_keys_and_esc_releases() {
        let mut harness = Harness::new(sh_manager());
        wait_until(&harness, |h| !logical(h).is_empty());
        harness.toggle_pane(PaneId::Explorer);

        harness.press(Key::Char('x'), Modifiers::NONE);
        assert!(
            harness
                .shared()
                .tabs
                .active()
                .unwrap()
                .shadow
                .text()
                .is_empty(),
            "the pane swallowed the key — the shadow never saw it"
        );

        harness.press(Key::Escape, Modifiers::NONE);
        assert_eq!(harness.shared().shown_pane(), None);

        // Focus is back — the same char types into the line now.
        harness.press(Key::Char('x'), Modifiers::NONE);
        assert_eq!(harness.shared().tabs.active().unwrap().shadow.text(), "x");
    }

    /// The selection is app-global but the claim is per-tab: the
    /// explorer on one tab and the agent panel on another hold the
    /// region in turn as the active tab changes, and every session
    /// already carries its grid-pane winsize (R14 us-4/30).
    #[test]
    fn the_selection_follows_the_active_tab() {
        let mut harness = Harness::new(sh_manager());
        // Tab 1 opens the agent panel; `Alt+T`'s new tab becomes
        // active, then the explorer selection lands on it.
        harness.press(Key::Char('a'), Modifiers::ALT);
        harness.press(Key::Char('t'), Modifiers::ALT);
        harness.toggle_pane(PaneId::Explorer);
        {
            let shared = harness.shared();
            assert_eq!(shared.tabs.active_index(), 1);
            assert_eq!(shared.shown_pane(), Some(SidePane::Aux(PaneId::Explorer)));
            assert_eq!(shared.tabs.active().unwrap().session.size().columns, 24);
        }

        // Tab 1's claim fills the region — the selection waits
        // underneath — and its session was resized ahead of the
        // switch by the global `resize_all`.
        harness.press(Key::Char('1'), Modifiers::ALT);
        {
            let shared = harness.shared();
            assert_eq!(shared.shown_pane(), Some(SidePane::Agent));
            assert_eq!(shared.tabs.active().unwrap().session.size().columns, 24);
        }

        // Back to tab 2 — the same selection shows again.
        harness.press(Key::Char('2'), Modifiers::ALT);
        let shared = harness.shared();
        assert_eq!(shared.shown_pane(), Some(SidePane::Aux(PaneId::Explorer)));
        assert_eq!(shared.tabs.active().unwrap().session.size().columns, 24);
    }

    // ---- The explorer pane (Spec R14 §5.13, R14-T2) ----

    /// Emit an OSC 7 cwd report on the active tab's shell — the same
    /// `printf` line ait-app's `cwd_for` tests use — and wait for the
    /// fold to land on THIS path (a second report on the same tab must
    /// not pass on the first's flag).
    fn report_cwd(harness: &Harness, path: &std::path::Path) {
        run_line(
            harness,
            &format!("printf '\\033]7;file://h{}\\a'", path.display()),
        );
        wait_until(harness, |h| h.shared().tabs.active().unwrap().cwd() == path);
    }

    /// While the explorer shows, its routed keys drive the model on
    /// `Shared` — arrows move the highlight and load the file
    /// preview, `Enter` descends, `⌫` walks back — and nothing
    /// reaches the PTY (the input shadow stays empty).
    #[test]
    fn explorer_keys_drive_the_shared_model() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir(fixture.path().join("subdir")).unwrap();
        std::fs::write(fixture.path().join("file.txt"), "x\n").unwrap();
        let mut harness = Harness::new(sh_manager());
        report_cwd(&harness, fixture.path());
        harness.toggle_pane(PaneId::Explorer);
        {
            let mut shared = harness.shared();
            crate::explorer::sync_explorer(&mut shared);
            assert_eq!(shared.explorer.dir(), fixture.path());
            assert_eq!(shared.explorer.selected_entry().unwrap().name, "subdir");
        }

        harness.press(Key::Down, Modifiers::NONE);
        {
            let shared = harness.shared();
            assert_eq!(shared.explorer.selected_entry().unwrap().name, "file.txt");
            assert!(
                matches!(
                    shared.explorer.preview(),
                    Some(crate::explorer::Preview::Text { .. })
                ),
                "selecting the file loaded its preview"
            );
            assert!(
                shared.tabs.active().unwrap().shadow.text().is_empty(),
                "the pane owns the key — the PTY saw nothing"
            );
        }

        // Up onto the dir row, Enter descends into it.
        harness.press(Key::Up, Modifiers::NONE);
        harness.press(Key::Enter, Modifiers::NONE);
        {
            let shared = harness.shared();
            assert_eq!(shared.explorer.dir(), fixture.path().join("subdir"));
        }

        // ⌫ walks back and re-selects the dir we left.
        harness.press(Key::Backspace, Modifiers::NONE);
        let shared = harness.shared();
        assert_eq!(shared.explorer.dir(), fixture.path());
        assert_eq!(shared.explorer.selected_entry().unwrap().name, "subdir");
    }

    /// us-3/4: the explorer's root tracks the active tab — an OSC-7
    /// report re-roots the listing, and switching tabs re-roots to
    /// that tab's own cwd.
    #[test]
    fn the_explorer_re_roots_on_osc7_and_tab_switch() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::create_dir(first.path().join("alpha")).unwrap();
        std::fs::create_dir(second.path().join("beta")).unwrap();
        let mut harness = Harness::new(sh_manager());
        report_cwd(&harness, first.path());
        harness.toggle_pane(PaneId::Explorer);
        {
            let mut shared = harness.shared();
            crate::explorer::sync_explorer(&mut shared);
            assert_eq!(shared.explorer.dir(), first.path());
        }

        // The new tab reports its own cwd — the pane re-roots to it.
        harness.press(Key::Char('t'), Modifiers::ALT);
        report_cwd(&harness, second.path());
        {
            let mut shared = harness.shared();
            crate::explorer::sync_explorer(&mut shared);
            assert_eq!(shared.explorer.dir(), second.path());
            let names: Vec<_> = shared
                .explorer
                .visible()
                .map(|(_, entry)| entry.name.as_str())
                .collect();
            assert_eq!(names, ["beta"], "the listing re-read the new root");
        }

        // Back on tab 1 its cwd is the root again — the browsed
        // position doesn't follow across a re-root.
        harness.press(Key::Char('1'), Modifiers::ALT);
        let mut shared = harness.shared();
        crate::explorer::sync_explorer(&mut shared);
        assert_eq!(shared.explorer.dir(), first.path());
    }

    /// us-13's toast defers to a live one: a newly-failed listing
    /// wants a toast, but a live toast in the strip is never
    /// clobbered — the error row still reports the failure in-pane.
    /// Once the strip is free, the NEXT new error toasts normally.
    #[test]
    fn a_new_listing_error_yields_to_a_live_toast() {
        let fixture = tempfile::tempdir().unwrap();
        let missing = fixture.path().join("gone");
        let harness = Harness::new(sh_manager());
        report_cwd(&harness, &missing);
        {
            let mut shared = harness.shared();
            shared.toast = Some(Toast::new("keep me", Instant::now()));
            crate::explorer::sync_explorer(&mut shared);
            assert!(
                shared.explorer.error().is_some(),
                "the missing root failed to list"
            );
            assert_eq!(
                shared.toast.as_ref().unwrap().text(),
                "keep me",
                "the live toast was not clobbered"
            );
        }
        // The strip free again — a NEW error toasts like before.
        report_cwd(&harness, &missing.join("deeper"));
        {
            let mut shared = harness.shared();
            shared.toast = None;
            crate::explorer::sync_explorer(&mut shared);
            assert!(
                shared
                    .toast
                    .as_ref()
                    .is_some_and(|toast| toast.text().contains("deeper")),
                "the fresh error toasts: {:?}",
                shared.toast.as_ref().map(Toast::text)
            );
        }
    }

    // ---- The browser pane (Spec R14 §5.13, R14-T3) ----

    /// While the browser shows, `Event::Text` is the address entry —
    /// through the real dispatch path, so nothing reaches the PTY —
    /// and Enter queues the normalized navigation. The harness's
    /// `embed` stays `Unknown` (no frame to probe), so the drain maps
    /// it to the external-open sink: the fallback decision exercised
    /// at the same seam the app drains.
    #[test]
    fn browser_entry_takes_text_and_enter_queues_the_navigation() {
        let path = capture_path("browserentry");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &harness,
            &format!("stty raw; head -c 1 > {}", path.display()),
            &path,
        );
        harness.toggle_pane(PaneId::Browser);

        feed(&mut harness, vec![egui::Event::Text("docs.rs".into())]);
        assert_eq!(harness.shared().browser.entry(), "docs.rs");

        let enter = egui::Event::Key {
            key: egui::Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        feed(&mut harness, vec![enter]);
        {
            let mut shared = harness.shared();
            assert_eq!(shared.browser.entry(), "https://docs.rs");
            assert_eq!(
                shared.browser.drain_nav(),
                vec![crate::browser::NavCommand::OpenExternal(
                    "https://docs.rs".into()
                )],
                "Unknown verdict drains to the system-browser sink"
            );
        }
        // `head -c 1` is still blocked — nothing leaked to the PTY.
        assert_eq!(std::fs::read(&path).unwrap().len(), 0);
        let _ = std::fs::remove_file(&path);
    }

    /// `open_browser_at` — T4's Ctrl+click entry point: the URL
    /// queues normalized, the browser selection lands, an agent
    /// claim yields, and every session keeps the grid-pane winsize.
    #[test]
    fn open_browser_at_navigates_and_shows_the_pane() {
        let mut harness = Harness::new(sh_manager());
        harness.press(Key::Char('a'), Modifiers::ALT);
        assert!(harness.shared().tabs.active().unwrap().agent_panel_open());
        {
            // The `toggle_pane` helper's own pattern: the field lock
            // borrows `shared`, `app` borrows disjointly.
            let mut shared = harness.shared.lock().unwrap();
            harness.app.open_browser_at(&mut shared, "example.com");
        }
        {
            let mut shared = harness.shared();
            assert_eq!(shared.shown_pane(), Some(SidePane::Aux(PaneId::Browser)));
            assert!(
                !shared.tabs.active().unwrap().agent_panel_open(),
                "the claim yielded"
            );
            assert_eq!(shared.browser.current(), Some("https://example.com"));
            assert_eq!(shared.browser.entry(), "https://example.com");
            assert_eq!(shared.tabs.active().unwrap().session.size().columns, 24);
            let _ = shared.browser.drain_nav();
        }

        // Already showing: a second open only navigates — the
        // selection is never toggled off by an explicit open.
        {
            let mut shared = harness.shared.lock().unwrap();
            harness.app.open_browser_at(&mut shared, "https://b.dev");
            assert_eq!(shared.shown_pane(), Some(SidePane::Aux(PaneId::Browser)));
            assert_eq!(shared.browser.current(), Some("https://b.dev"));
            assert!(shared.browser.can_go_back());
        }
    }

    /// A rejected address input stays out of every queue — the
    /// hint flag is the only trace, and nothing unfetchable is
    /// fetched (§5.13 local-first).
    #[test]
    fn a_rejected_address_never_queues() {
        let mut harness = Harness::new(sh_manager());
        harness.toggle_pane(PaneId::Browser);
        feed(&mut harness, vec![egui::Event::Text("not a url".into())]);
        harness.press(Key::Enter, Modifiers::NONE);
        let mut shared = harness.shared();
        assert!(shared.browser.bad_entry());
        assert!(shared.browser.drain_nav().is_empty());
        assert_eq!(shared.browser.current(), None);
    }

    // ---- Ctrl+click detection & routing (Spec R14 §5.13, R14-T4) ----

    /// `GuiApp::open_detection` under the shared lock — the route
    /// `paint_grid_pane`'s Ctrl+click arm applies.
    fn open_detection(harness: &mut Harness, target: detect::Target) {
        let mut shared = harness.shared.lock().unwrap();
        harness.app.open_detection(&mut shared, target);
    }

    /// The URL route: a detected URL navigates the browser pane open
    /// — the claim yields, every session keeps the grid-pane winsize,
    /// and the pending navigation waits for the frame's drain
    /// (external-open under the harness's `Unknown` verdict).
    #[test]
    fn ctrl_click_on_a_url_opens_the_browser_pane_at_it() {
        let mut harness = Harness::new(sh_manager());
        harness.press(Key::Char('a'), Modifiers::ALT);
        assert!(harness.shared().tabs.active().unwrap().agent_panel_open());

        open_detection(
            &mut harness,
            detect::Target::Url("https://docs.rs/eframe".to_string()),
        );

        let mut shared = harness.shared();
        assert_eq!(shared.shown_pane(), Some(SidePane::Aux(PaneId::Browser)));
        assert!(
            !shared.tabs.active().unwrap().agent_panel_open(),
            "the claim yielded to the routed open"
        );
        assert_eq!(shared.browser.current(), Some("https://docs.rs/eframe"));
        assert_eq!(shared.tabs.active().unwrap().session.size().columns, 24);
        let _ = shared.browser.drain_nav();
    }

    /// The file route: a detected file selects it in the explorer —
    /// the pane browses to its parent and the preview loads — a
    /// directory navigates instead, and either way the explorer pane
    /// shows.
    #[test]
    fn ctrl_click_on_a_path_opens_the_explorer_on_it() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir(fixture.path().join("sub")).unwrap();
        std::fs::write(fixture.path().join("file.txt"), "hello\n").unwrap();
        let file = fixture.path().join("file.txt");
        let dir = fixture.path().join("sub");
        let mut harness = Harness::new(sh_manager());

        open_detection(&mut harness, detect::Target::Path(file.clone()));
        {
            let shared = harness.shared();
            assert_eq!(shared.shown_pane(), Some(SidePane::Aux(PaneId::Explorer)));
            assert_eq!(shared.explorer.dir(), fixture.path());
            assert_eq!(shared.explorer.selected_entry().unwrap().path, file);
            assert!(
                matches!(
                    shared.explorer.preview(),
                    Some(crate::explorer::Preview::Text { .. })
                ),
                "the routed file loads its preview"
            );
            assert_eq!(shared.tabs.active().unwrap().session.size().columns, 24);
        }

        open_detection(&mut harness, detect::Target::Path(dir.clone()));
        let shared = harness.shared();
        assert_eq!(shared.explorer.dir(), dir);
        assert_eq!(shared.shown_pane(), Some(SidePane::Aux(PaneId::Explorer)));
    }

    /// `detect_at` over a real grid row: the shell's own echo is the
    /// scanned text, the column under the pointer picks the token —
    /// a relative filename resolves under the tab's reported cwd.
    #[test]
    fn detect_at_resolves_tokens_on_real_grid_rows() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::write(fixture.path().join("file.txt"), "x\n").unwrap();
        let harness = Harness::new(sh_manager());
        report_cwd(&harness, fixture.path());
        // The `out=` marker exists ONLY in the printf output — the
        // typed command never contains it, so the wait gates on
        // execution-produced output, not the input echo
        // (CODING_STANDARDS §15). 34 columns of output — under the
        // harness's 40-wide grid so the whole row stays one line.
        run_line(
            &harness,
            "printf 'out=%s\\n' 'see https://a/b then file.txt'",
        );
        wait_until(&harness, |h| {
            let shared = h.shared();
            let tab = shared.tabs.active().unwrap();
            let grid = tab.grid();
            (-(grid.scrollback() as i32)..grid.lines() as i32).any(|line| {
                grid.line(line)
                    .is_some_and(|row| row.text().contains("out=see https://a/b then file.txt"))
            })
        });

        let shared = harness.shared();
        let tab = shared.tabs.active().unwrap();
        let grid = tab.grid();
        let (line, text) = (-(grid.scrollback() as i32)..grid.lines() as i32)
            .find_map(|line| {
                grid.line(line)
                    .map(|row| (line, row.text()))
                    .filter(|(_, text)| text.contains("out=see https://a/b then file.txt"))
            })
            .unwrap();
        let home = fixture.path().to_path_buf();
        let cwd = PathBuf::from(suggest::cwd_for(tab));
        // The URL's column and the file's column both afford — the
        // space between them never does.
        let url_col = text.find("https").unwrap();
        let hit = detect::detect_at(&grid, line, url_col, false, &cwd, &home)
            .expect("the URL column affords");
        assert_eq!(hit.target, detect::Target::Url("https://a/b".into()));
        assert_eq!(hit.column, url_col);
        assert_eq!(hit.cells, "https://a/b".chars().count());

        let file_col = text.find("file.txt").unwrap();
        let hit = detect::detect_at(&grid, line, file_col, false, &cwd, &home)
            .expect("the real file's column affords");
        assert_eq!(
            hit.target,
            detect::Target::Path(fixture.path().join("file.txt"))
        );
        assert!(
            detect::detect_at(
                &grid,
                line,
                text.find("then").unwrap() + "then".len(),
                false,
                &cwd,
                &home
            )
            .is_none(),
            "the space between tokens affords nothing"
        );
    }
}
