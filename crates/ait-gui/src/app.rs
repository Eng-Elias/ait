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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use ait_agent::ApprovalVerdict;
use ait_app::agent_panel::AgentPanel;
use ait_app::config::{self, LiveSettings};
use ait_app::conversations::ConversationPicker;
use ait_app::ctl::CtlServer;
use ait_app::keymap::{self, AppAction, CapabilityTier, Resolution};
use ait_app::profile::{self, ProfilePicker};
use ait_app::scroll::WHEEL_LINES;
use ait_app::selection::{self, GridPoint};
use ait_app::skills::LiveSkills;
use ait_app::status::{status_text, StatusLine, Toast};
use ait_app::store::StoreHandle;
use ait_app::tabs::TabManager;
use ait_config::Settings;
use ait_term::{Key, KeyEvent, Keypad, Modifiers, Size};
use eframe::egui::{
    self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2, ViewportCommand,
};

use crate::grid::GridTheme;
use crate::keys::{translate_key, translate_text};
use crate::layout::{
    chrome_regions, grid_point, grid_size, session_size, split_content, tab_chips, CellMetrics,
    Chrome, GuiMetrics, CHIP_PAD,
};
use crate::paint::paint_grid;
use crate::panel::{self, Gate, Tone};
use crate::pump::Pump;

/// How long before a live toast's expiry the UI asks for one more
/// frame — the only *timed* repaint the GUI schedules (everything
/// else is activity-driven; an idle window with a toast showing still
/// needs exactly one frame when the toast ages out).
const TOAST_REFRESH: Duration = Duration::from_millis(500);

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
    /// Set when the input dispatch decided the app should close —
    /// applied after `ui` returns so the frame can't see a
    /// half-drained state.
    quit: bool,
    /// The find bar's open state (Spec R13-T4 hook): the skeleton keeps
    /// the field and the modal-region paint site; the binding, the
    /// grid scan and the highlight pass are T4's.
    #[allow(dead_code)]
    find_open: bool,
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
            Err(error) => (None, Some(format!("ctl socket unavailable: {error}"))),
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
        let mut toast = {
            let failures: Vec<String> = [
                load_error.map(|text| format!("config error: {text}")),
                store_error.map(|text| format!("store unavailable: {text}")),
                ctl_error,
            ]
            .into_iter()
            .flatten()
            .collect();
            (!failures.is_empty()).then(|| Toast::new(failures.join(" · "), Instant::now()))
        };
        let mut store = store;
        if let Some(tab) = tabs.active_mut() {
            if let Some(id) = store.session_started(&mut toast, tab.title(), tab.cwd()) {
                tab.set_store_session(id);
            }
        }

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
        }));
        let watcher = config::watcher();
        let pump = Pump::start(ctx.clone(), Arc::clone(&shared), ctl, watcher);
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
            quit: false,
            find_open: false,
        })
    }

    // -----------------------------------------------------------------
    // Input dispatch — the TUI `handle_event`'s GUI counterpart: native
    // events translate to `ait_term::KeyEvent`, resolve through the
    // shared keymap, and only `PassThrough` may reach the PTY.

    /// One frame's native events, in order — keys, composed text,
    /// paste, wheel. Selection drags ride `interact` in the paint pass
    /// (they need the pane rect, which is layout state).
    fn dispatch_events(&mut self, ctx: &egui::Context, shared: &mut Shared) {
        let events = ctx.input(|input| input.events.clone());
        for event in events {
            match event {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    // GUI-local bindings the keymap can't own — zoom is
                    // a frontend concern and these chords are unbound in
                    // the table (Ctrl+0/=/- type nothing a shell wants).
                    if self.zoom_key(key, modifiers) {
                        continue;
                    }
                    self.dispatch_key(ctx, shared, translate_key(key, modifiers));
                }
                // Releases carry no input — same as the TUI's
                // `KeyEventKind::Release` guard.
                egui::Event::Key { pressed: false, .. } => {}
                // Composed text: one Char event per character through
                // the same resolver (every Char resolves PassThrough —
                // the trip costs nothing and keeps ONE dispatch path).
                egui::Event::Text(text) => {
                    for key in translate_text(&text) {
                        self.dispatch_key(ctx, shared, Some(key));
                    }
                }
                // A native paste (Ctrl+V / ⌘V, menu paste): encode the
                // payload through the shared `encode_paste` so
                // bracketed-paste children see their mode honored.
                egui::Event::Paste(text) => {
                    self.paste(shared, &text);
                }
                egui::Event::MouseWheel { unit, delta, .. } => {
                    self.wheel(shared, ctx, unit, delta);
                }
                _ => {}
            }
        }
    }

    /// Ctrl+=/+-/0 — the terminal-text zoom (never `set_zoom_factor`:
    /// only the grid font grows, the tabs/status/panel stay put — the
    /// spec's terminal-text-only zoom rule). Returns whether the key
    /// was a zoom chord.
    fn zoom_key(&mut self, key: egui::Key, modifiers: egui::Modifiers) -> bool {
        if !(modifiers.command && !modifiers.alt) {
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
            Resolution::Action(AppAction::AgentPanel) => {
                let Some(tab) = shared.tabs.active_mut() else {
                    return;
                };
                tab.clear_selection();
                tab.toggle_agent_panel();
                // Only THIS tab's winsize changed — `resize_active`
                // re-derives it through the same GUI session_size.
                shared.tabs.resize_active(session_size);
            }
            Resolution::Action(AppAction::SwitchProfile) => {
                if shared.picker.take().is_none() {
                    let names: Vec<String> = shared
                        .config
                        .settings()
                        .agents
                        .execution_profiles
                        .keys()
                        .cloned()
                        .collect();
                    match profile::switch(&names, &shared.profile) {
                        profile::Switch::Picker => {
                            shared.picker = Some(ProfilePicker::new(names, &shared.profile));
                        }
                        profile::Switch::Cycle(name) => activate_profile(shared, name),
                        profile::Switch::Unavailable => {
                            shared.toast = Some(Toast::new(
                                "no other execution profiles declared",
                                Instant::now(),
                            ));
                        }
                    }
                }
            }
            // Wired in T3/T6 with the suggester — bound keys must
            // still be consumed (never leaked to the PTY), so the
            // stub reports the gap instead of no-op'ing.
            Resolution::Action(AppAction::SuggestCommand) => {
                shared.toast = Some(Toast::new(
                    "suggest is not wired in the GUI frontend yet",
                    Instant::now(),
                ));
            }
            // `Alt+R` (Spec R08 §5.8): the conversation picker over
            // the store's rows — newest-first, Enter resumes into the
            // active tab's panel, `Ctrl+D` deletes behind a one-key
            // confirm, Esc closes. A second `Alt+R` toggles it closed,
            // and opening it drops the profile picker — one overlay at
            // a time. The list is a snapshot taken here; an empty
            // store toasts instead of opening nothing.
            Resolution::Action(AppAction::Conversations) => {
                if shared.conversations.take().is_some() {
                    // Toggled closed — the highlight is dropped.
                } else {
                    shared.picker = None;
                    match shared.store.conversations() {
                        Ok(listed) if listed.is_empty() => {
                            shared.toast =
                                Some(Toast::new("no stored conversations", Instant::now()));
                        }
                        Ok(listed) => {
                            shared.conversations =
                                Some(ConversationPicker::new(&listed, SystemTime::now()));
                        }
                        Err(error) => {
                            shared.toast = Some(Toast::new(
                                format!("conversations list failed: {error}"),
                                Instant::now(),
                            ));
                        }
                    }
                }
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
                                tab.shadow.mark_stale();
                            }
                        }
                    }
                    AppAction::Quit
                    | AppAction::NewTab
                    | AppAction::CloseTab
                    | AppAction::NextTab
                    | AppAction::PrevTab
                    | AppAction::JumpToTab(_)
                    | AppAction::AgentPanel
                    | AppAction::SwitchProfile
                    | AppAction::SuggestCommand
                    | AppAction::Conversations => {} // dispatched above
                }
            }
            Resolution::PassThrough => {
                // An open profile picker owns unbound keys (same as the
                // TUI): nothing leaks to the PTY mid-pick.
                if shared.picker.is_some() {
                    if let Some(key) = key {
                        picker_key(&key, shared);
                    }
                    return;
                }
                // The open conversation picker owns unbound keys the
                // same way (Spec R08-T6): ↑/↓ move, Enter resumes,
                // `Ctrl+D` deletes behind its confirm, plain Esc
                // dismisses — nothing leaks to the PTY mid-pick.
                if shared.conversations.is_some() {
                    if let Some(key) = key {
                        conversation_key(&key, shared);
                    }
                    return;
                }
                let Some(tab) = shared.tabs.active_mut() else {
                    return;
                };
                tab.clear_selection();
                // A focused agent panel owns the keystroke (Spec R06)
                // — never the PTY, never the input shadow.
                if tab.agent_focused() {
                    if let Some(key) = key {
                        tab.agent_key(
                            &key,
                            shared.config.settings(),
                            &shared.profile,
                            &mut shared.skills,
                            &mut shared.toast,
                        );
                    }
                    return;
                }
                tab.snap_to_bottom();
                if tab.exit_status().is_none() {
                    if let Some(key_event) = key {
                        // The input shadow tracks what reaches the PTY —
                        // T3's `# `-trigger verification reads it.
                        tab.shadow.update(&key_event);
                        let _ = tab.session.send_key(key_event);
                    }
                }
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
            tab.shadow.mark_stale();
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
    ) {
        let Some(hover) = ctx.input(|input| input.pointer.hover_pos()) else {
            return;
        };
        if self.panel_pane.is_some_and(|pane| pane.contains(hover)) {
            let Some(tab) = shared.tabs.active_mut() else {
                return;
            };
            let lines = self.wheel_lines(ctx, unit, delta.y);
            if delta.y > 0.0 {
                tab.agent_scroll_up(lines);
            } else {
                tab.agent_scroll_down(lines);
            }
            return;
        }
        if !self.grid_pane.is_some_and(|pane| pane.contains(hover)) {
            return;
        }
        let Some(tab) = shared.tabs.active_mut() else {
            return;
        };
        let lines = self.wheel_lines(ctx, unit, delta.y);
        match unit {
            egui::MouseWheelUnit::Page => {
                if delta.y > 0.0 {
                    tab.page_up();
                } else {
                    tab.page_down();
                }
            }
            _ => {
                if delta.y > 0.0 {
                    tab.scroll_up(lines);
                } else {
                    tab.scroll_down(lines);
                }
            }
        }
    }

    /// One wheel delta → scroll lines: Line units scale by the shared
    /// `WHEEL_LINES` so a tick scrolls the same 3 lines the TUI's
    /// wheel binding does (CODING_STANDARDS §17 — one constant, both
    /// frontends); Point units divide by the cell height; Page units
    /// are handled by the grid arm's page_up/page_down (the panel's
    /// conversation scrolls in lines regardless).
    fn wheel_lines(&self, ctx: &egui::Context, unit: egui::MouseWheelUnit, delta_y: f32) -> usize {
        match unit {
            egui::MouseWheelUnit::Line => {
                (delta_y.abs() * WHEEL_LINES as f32).ceil().max(1.0) as usize
            }
            egui::MouseWheelUnit::Point => {
                let cell = self.cell_height(ctx);
                (delta_y / cell).trunc().abs() as usize
            }
            // The panel arm uses this as its page distance (the grid
            // arm handles Page through page_up/page_down itself).
            egui::MouseWheelUnit::Page => self.panel_rows.max(1),
        }
    }

    /// The current cell height — the wheel's pixel→lines divisor.
    fn cell_height(&self, ctx: &egui::Context) -> f32 {
        let font = FontId::monospace(self.metrics.font_px());
        ctx.fonts_mut(|fonts| fonts.row_height(&font)).max(1.0)
    }

    // -----------------------------------------------------------------
    // Tab lifecycle — shared by the keymap actions above and the tab
    // bar's click targets below, so binding and button can never drift.

    /// `Alt+T` / the bar's "+": spawn a tab and register its
    /// `terminal_sessions` row. A spawn failure toasts instead of
    /// taking the window down — the session model stays up (graceful
    /// degrade, like the TUI).
    fn open_tab(&mut self, shared: &mut Shared) {
        if let Err(error) = shared.tabs.new_tab() {
            shared.toast = Some(Toast::new(
                format!("new tab failed: {error}"),
                Instant::now(),
            ));
        } else if let Some(tab) = shared.tabs.active_mut() {
            if let Some(id) =
                shared
                    .store
                    .session_started(&mut shared.toast, tab.title(), tab.cwd())
            {
                tab.set_store_session(id);
            }
        }
    }

    /// `Alt+W` / a chip's "×": store bookkeeping lands BEFORE the tab
    /// goes (same order the TUI captures it), then the manager drops
    /// it. Closing the last tab leaves an empty manager — guard before
    /// indexing and exit (CODING_STANDARDS §6).
    fn close_active_tab(&mut self, shared: &mut Shared) {
        if let Some(id) = shared.tabs.active().and_then(|tab| tab.store_session()) {
            shared.store.session_ended(&mut shared.toast, id);
        }
        if let Some(id) = shared.tabs.active().and_then(|tab| tab.conversation()) {
            shared.store.conversation_closed(&mut shared.toast, id);
        }
        if !shared.tabs.close_active() {
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
        painter.rect_filled(rect, 0.0, Color32::from_rgb(0x1E, 0x1E, 0x1E));
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
            let chip_hit = ui.interact(chip.rect, egui::Id::unique(("tab", index)), Sense::click());
            let close_hit = ui.interact(
                chip.close,
                egui::Id::unique(("tab-close", index)),
                Sense::click(),
            );
            let bg = if index == active {
                Color32::from_rgb(0x3A, 0x3A, 0x3A)
            } else if chip_hit.hovered() || close_hit.hovered() {
                Color32::from_rgb(0x2A, 0x2A, 0x2A)
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
                    Color32::from_rgb(0x88, 0x88, 0x88)
                },
            );
            if close_hit.clicked() {
                // Closing by click activates first (the user's target
                // is unambiguous) — `jump_to` is 1-based — then the
                // shared close path runs, quitting on the last tab.
                shared.tabs.jump_to(index + 1);
                self.close_active_tab(shared);
            } else if chip_hit.clicked() {
                shared.tabs.jump_to(index + 1);
            }
        }
        if let Some(new_rect) = bar.new_tab {
            let new_hit = ui.interact(new_rect, egui::Id::unique("tab-new"), Sense::click());
            if new_hit.hovered() {
                painter.rect_filled(new_rect, 4.0, Color32::from_rgb(0x2A, 0x2A, 0x2A));
            }
            painter.text(
                new_rect.center(),
                Align2::CENTER_CENTER,
                "+",
                font,
                if new_hit.hovered() {
                    Color32::WHITE
                } else {
                    Color32::from_rgb(0xAA, 0xAA, 0xAA)
                },
            );
            if new_hit.clicked() {
                self.open_tab(shared);
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
        let Some(tab) = shared.tabs.active_mut() else {
            return;
        };
        let response = ui.interact(pane, egui::Id::unique("grid"), Sense::click_and_drag());
        let scroll = tab.scroll_offset();
        if let Some(pos) = response.interact_pointer_pos() {
            let (line, column) = grid_point(pos, pane, cell, scroll);
            if response.drag_started() {
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
        let painter = ui.painter().with_clip_rect(pane);
        paint_grid(
            &crate::paint::GridSurface {
                painter: &painter,
                pane,
                cell,
                theme: self.theme,
                font,
            },
            &tab.grid(),
            scroll,
            tab.cursor(),
            tab.selection_range(),
        );
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
    fn paint_panel(&mut self, ui: &mut egui::Ui, shared: &mut Shared, rect: Rect) {
        let ctx = ui.ctx().clone();
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, PANEL_BG);
        painter.line_segment(
            [rect.left_top(), rect.left_bottom()],
            Stroke::new(1.0, Color32::from_rgb(0x33, 0x33, 0x33)),
        );
        let font = FontId::monospace(13.0);
        let (line_h, cell_w) = ctx.fonts_mut(|fonts| {
            (
                fonts.row_height(&font).max(1.0),
                fonts.glyph_width(&font, 'M').max(1.0),
            )
        });
        let mut action: Option<PanelAction> = None;
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
                &font,
                line_h,
                left,
                right,
                input_rect.top(),
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
        // The whole region is the focus affordance: a click anywhere
        // that isn't a button re-focuses an unfocused panel (the
        // `Alt+A` refocus arm's mouse counterpart — never a close).
        let body = ui.interact(rect, egui::Id::unique("agent-focus"), Sense::click());
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
    #[allow(clippy::too_many_arguments)]
    fn paint_gate_card(
        &self,
        ui: &mut egui::Ui,
        painter: &egui::Painter,
        panel: &AgentPanel,
        font: &FontId,
        line_h: f32,
        left: f32,
        right: f32,
        input_top: f32,
        action: &mut Option<PanelAction>,
    ) -> f32 {
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
        painter.rect_filled(card, 4.0, Color32::from_rgb(0x22, 0x22, 0x22));
        painter.rect_stroke(
            card,
            4.0,
            Stroke::new(1.0, Color32::from_rgb(0x55, 0x55, 0x55)),
            egui::StrokeKind::Inside,
        );
        let mut y = card.top() + 4.0;
        match gate {
            Some(Gate::Approve { name, denying }) => {
                painter.text(
                    Pos2::new(left, y),
                    Align2::LEFT_TOP,
                    if denying {
                        format!(" deny {name} — type a reason · enter: deny ")
                    } else {
                        format!(" approve {name} ")
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
                            PanelAction::Verdict(ApprovalVerdict::AllowOnce),
                        ),
                        (
                            "t: task",
                            egui::Id::unique("agent-allow-task"),
                            PanelAction::Verdict(ApprovalVerdict::AllowForTask),
                        ),
                        (
                            "d: deny",
                            egui::Id::unique("agent-deny"),
                            PanelAction::BeginDenyReason,
                        ),
                    ];
                    let button_w = (card.width() - 16.0 - 8.0 * 2.0) / 3.0;
                    for (index, (label, id, verdict)) in buttons.into_iter().enumerate() {
                        let button = Rect::from_min_size(
                            Pos2::new(card.left() + 8.0 + index as f32 * (button_w + 8.0), y),
                            Vec2::new(button_w, line_h),
                        );
                        let hit = ui.interact(button, id, Sense::click());
                        painter.rect_filled(
                            button,
                            3.0,
                            if hit.hovered() {
                                Color32::from_rgb(0x3A, 0x3A, 0x3A)
                            } else {
                                Color32::from_rgb(0x2A, 0x2A, 0x2A)
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
                        " ask — type your answer · enter: send ",
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
                        let selected = index == ask.selected;
                        if selected || hit.hovered() {
                            painter.rect_filled(row, 2.0, Color32::from_rgb(0x33, 0x44, 0x66));
                        }
                        painter.text(
                            Pos2::new(left, y),
                            Align2::LEFT_TOP,
                            if selected {
                                format!("    ▸ {choice}")
                            } else {
                                format!("      {choice}")
                            },
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
    /// TUI composes (toast wins while live).
    fn paint_status(&self, ui: &mut egui::Ui, shared: &Shared, rect: Rect) {
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(0x1E, 0x1E, 0x1E));
        let now = Instant::now();
        let text = match shared.tabs.active() {
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
        let highlighted = shared.toast.as_ref().is_some_and(|t| t.is_live(now));
        let color = if highlighted {
            Color32::BLACK
        } else {
            Color32::from_rgb(0xAA, 0xAA, 0xAA)
        };
        if highlighted {
            painter.rect_filled(rect, 0.0, Color32::from_rgb(0xD8, 0xD8, 0xD8));
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
            painter.rect_filled(rect, 4.0, Color32::from_rgb(0x22, 0x22, 0x22));
            painter.rect_stroke(
                rect,
                4.0,
                Stroke::new(1.0, Color32::from_rgb(0x55, 0x55, 0x55)),
                egui::StrokeKind::Inside,
            );
            for (index, name) in picker.names().iter().enumerate() {
                let y = rect.top() + 8.0 + index as f32 * row_h;
                let selected = index == picker.selected();
                if selected {
                    painter.rect_filled(
                        Rect::from_min_size(
                            Pos2::new(rect.left() + 4.0, y - 1.0),
                            Vec2::new(rect.width() - 8.0, row_h),
                        ),
                        2.0,
                        Color32::from_rgb(0x33, 0x44, 0x66),
                    );
                }
                painter.text(
                    Pos2::new(rect.left() + 12.0, y),
                    Align2::LEFT_TOP,
                    if selected {
                        format!("▸ {name}")
                    } else {
                        format!("  {name}")
                    },
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
            painter.rect_filled(rect, 4.0, Color32::from_rgb(0x22, 0x22, 0x22));
            painter.rect_stroke(
                rect,
                4.0,
                Stroke::new(1.0, Color32::from_rgb(0x55, 0x55, 0x55)),
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
                        Color32::from_rgb(0x33, 0x44, 0x66),
                    );
                }
                painter.text(
                    Pos2::new(rect.left() + 12.0, y),
                    Align2::LEFT_TOP,
                    format!(
                        "{} {}  {} · {} items",
                        if selected { "▸" } else { " " },
                        row.title,
                        row.age,
                        row.items,
                    ),
                    font.clone(),
                    Color32::WHITE,
                );
            }
            // The bottom row is the keys hint — `Ctrl+D`'s armed
            // state replaces it with the confirm prompt (us-11).
            let (hint, color) = if picker.confirming() {
                let title = picker
                    .selected_row()
                    .map(|row| row.title.as_str())
                    .unwrap_or_default();
                (
                    format!(" ctrl+d again deletes \"{title}\""),
                    tone_color(Tone::Alert, self.theme),
                )
            } else {
                (
                    " enter: resume · ctrl+d: delete · esc: close".to_string(),
                    tone_color(Tone::Dim, self.theme),
                )
            };
            painter.text(
                Pos2::new(rect.left() + 12.0, rect.top() + 8.0 + shown as f32 * row_h),
                Align2::LEFT_TOP,
                hint,
                font.clone(),
                color,
            );
        }
        // T4 hook: the find bar anchors above the status strip here.
        let _ = self.find_open;
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

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
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

        // 2. Regions + cell metrics (zoom included), then the band→
        //    winsize resize when they moved.
        let area = ctx.content_rect();
        let regions = chrome_regions(area, self.chrome);
        let font = FontId::monospace(self.metrics.font_px());
        let cell = cell_metrics(&ctx, &self.metrics);
        let band = grid_size(regions.content, cell);
        if band != self.last_band {
            shared.tabs.resize_all(band, session_size);
            self.last_band = band;
        }

        // 3. The content split — the panel's share comes off the band
        //    exactly like `session_size` told the PTY.
        let panel_open = shared
            .tabs
            .active()
            .is_some_and(|tab| tab.agent_panel_open());
        let (grid_pane, panel_rect) = split_content(regions.content, panel_open, cell);

        self.paint_tab_bar(ui, &mut shared, regions.tab_bar);
        self.paint_grid_pane(&ctx, ui, &mut shared, grid_pane, cell, &font);
        // The wheel hit-test mirrors THIS frame's split — a stale
        // rect would deaden the region the grid just reclaimed.
        self.panel_pane = panel_rect;
        if let Some(rect) = panel_rect {
            self.paint_panel(ui, &mut shared, rect);
        }
        self.paint_status(ui, &shared, regions.status);
        self.paint_modals(ui, &shared, area);
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

/// A panel row's [`Tone`] → the paint color — `Strong` lifts the
/// theme foreground to full white (the same bright-foreground
/// degrade `bold_color` documents for the grid, since egui's
/// bundled monospace has no bold face).
fn tone_color(tone: Tone, theme: GridTheme) -> Color32 {
    match tone {
        Tone::Plain => theme.fg,
        Tone::Strong => Color32::WHITE,
        Tone::Dim => Color32::from_rgb(0x77, 0x77, 0x77),
        Tone::Alert => Color32::from_rgb(0xE0, 0xC0, 0x40),
        Tone::Good => Color32::from_rgb(0x50, 0xC8, 0x50),
        Tone::Bad => Color32::from_rgb(0xE0, 0x50, 0x50),
        Tone::Info => Color32::from_rgb(0x50, 0xC0, 0xC0),
    }
}

/// One key while the conversation picker is open (Spec R08-T6,
/// us-5/11) — the TUI's `conversation_key`, verbatim: ↑/↓ move the
/// highlight, Enter resumes the highlighted row into the active
/// tab's panel, `Ctrl+D` arms then confirms the delete (one
/// gesture, never an accident — any other key disarms), plain Esc
/// dismisses. `shared.conversations` is `Some` on entry — the
/// caller already routed.
fn conversation_key(key: &KeyEvent, shared: &mut Shared) {
    enum Outcome {
        /// Nothing happened — key consumed.
        None,
        /// Close the picker.
        Close,
        /// Resume `id` into the active tab's panel.
        Resume(i64),
        /// The armed confirm fired: delete `id`.
        Delete(i64),
    }
    let outcome = {
        let Some(picker) = shared.conversations.as_mut() else {
            return;
        };
        // `Ctrl+D` is the one-key confirm's whole vocabulary: the
        // first press arms, the second deletes. Anything else first
        // disarms, then does its usual thing — a stray key can never
        // sneak a delete past the user (us-11).
        if key.key == Key::Char('d') && key.modifiers == Modifiers::CONTROL {
            if picker.confirming() {
                match picker.selected_row() {
                    Some(row) => Outcome::Delete(row.id),
                    None => Outcome::Close,
                }
            } else {
                picker.arm_delete();
                Outcome::None
            }
        } else {
            picker.disarm_delete();
            match key.key {
                Key::Up => {
                    picker.prev();
                    Outcome::None
                }
                Key::Down => {
                    picker.next();
                    Outcome::None
                }
                Key::Enter => match picker.selected_row() {
                    Some(row) => Outcome::Resume(row.id),
                    None => Outcome::Close,
                },
                Key::Escape if key.modifiers.is_empty() => Outcome::Close,
                _ => Outcome::None,
            }
        }
    };
    match outcome {
        Outcome::None => {}
        Outcome::Close => shared.conversations = None,
        Outcome::Resume(id) => {
            shared.conversations = None;
            resume_conversation(id, shared);
        }
        Outcome::Delete(id) => {
            if shared.store.delete_conversation(&mut shared.toast, id) {
                // The row is gone — every tab bound to it must open
                // a new row rather than append to a deleted one.
                shared.tabs.detach_conversation(id);
                if let Some(picker) = shared.conversations.as_mut() {
                    picker.remove_selected();
                    if picker.rows().is_empty() {
                        shared.conversations = None;
                    }
                }
            }
        }
    }
}

/// `Enter` on a picker row: load the conversation and swap the
/// active tab's panel to it (Spec R08-T6, us-6/10) — the TUI's
/// `resume_conversation`, verbatim. Refused with a toast while the
/// tab's task runs — a live run is never clobbered. The reads land
/// BEFORE the swap so a failed load changes nothing; the row binds
/// so every later flush appends to it.
fn resume_conversation(id: i64, shared: &mut Shared) {
    let running = shared
        .tabs
        .active()
        .and_then(|tab| tab.agent_panel())
        .is_some_and(AgentPanel::is_running);
    if running {
        shared.toast = Some(Toast::new(
            "a task is still running — resume refused",
            Instant::now(),
        ));
        return;
    }
    let items = match shared.store.load_transcript(id) {
        Ok(items) => items,
        Err(error) => {
            shared.toast = Some(Toast::new(
                format!("transcript load failed: {error}"),
                Instant::now(),
            ));
            return;
        }
    };
    let plan = match shared.store.load_plan(id) {
        Ok(Some(ait_agent::TranscriptItem::Plan { todos })) => Some(todos),
        Ok(_) => None,
        Err(error) => {
            shared.toast = Some(Toast::new(
                format!("plan load failed: {error}"),
                Instant::now(),
            ));
            return;
        }
    };
    if let Some(tab) = shared.tabs.active_mut() {
        let previous = tab.conversation();
        tab.resume_conversation(id, items, plan);
        shared.toast = Some(Toast::new("conversation resumed", Instant::now()));
        // `status` mirrors the binding: the resumed row re-opens,
        // and the row this tab just swapped away from is unowned now.
        shared.store.conversation_reopened(&mut shared.toast, id);
        if let Some(old) = previous.filter(|old| *old != id) {
            shared.store.conversation_closed(&mut shared.toast, old);
        }
    }
    // The panel may have opened on a tab that never had one —
    // re-derive its winsize through the same path `Alt+A` takes.
    shared.tabs.resize_active(session_size);
}

/// The creation-time cell estimate — fonts don't exist until the
/// first `Context::run`, so `GuiApp::new` can't read the glyph table.
/// Typical monospace ratios (advance ~0.6×em, line ~1.2×em) seed the
/// first tab's winsize; the first frame's real `cell_metrics` +
/// `resize_all` correct it before anything paints.
fn provisional_cell(metrics: &GuiMetrics) -> CellMetrics {
    CellMetrics {
        width: metrics.font_px() * 0.6,
        height: metrics.font_px() * 1.2,
    }
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

/// `Alt+P` picker keys — the TUI's `picker_key` unchanged: ↑/↓ move,
/// Enter arms, plain Esc dismisses, everything else is swallowed.
fn picker_key(key: &KeyEvent, shared: &mut Shared) {
    let Some(picker) = shared.picker.as_mut() else {
        return;
    };
    match key.key {
        Key::Up => picker.prev(),
        Key::Down => picker.next(),
        Key::Enter | Key::Keypad(Keypad::Enter) => {
            let name = picker.selected_name().to_string();
            shared.picker = None;
            activate_profile(shared, name);
        }
        Key::Escape if key.modifiers.is_empty() => {
            shared.picker = None;
        }
        _ => {}
    }
}

/// Arm the chosen profile — the TUI's `activate_profile`: a real
/// switch confirms with the spec's toast, re-arming stays silent.
fn activate_profile(shared: &mut Shared, name: String) {
    if name == shared.profile {
        return;
    }
    shared.toast = Some(Toast::new(
        format!("execution profile switched to \"{name}\""),
        Instant::now(),
    ));
    shared.profile = name;
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
