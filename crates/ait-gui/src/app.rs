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
use std::time::{Duration, Instant};

use ait_agent::CommandSuggester;
use ait_app::config::{self, LiveSettings};
use ait_app::ctl::CtlServer;
use ait_app::keymap::{self, AppAction, CapabilityTier, Resolution};
use ait_app::profile::{self, ProfilePicker};
use ait_app::scroll::WHEEL_LINES;
use ait_app::selection::{self, GridPoint};
use ait_app::skills::LiveSkills;
use ait_app::status::{status_text, StatusLine, Toast};
use ait_app::store::StoreHandle;
use ait_app::suggest::{self, SuggestionPin};
use ait_app::tabs::TabManager;
use ait_config::Settings;
use ait_term::{Key, KeyEvent, Keypad, Size};
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
        let cell = cell_metrics(ctx, &metrics);
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
            copied_text: None,
            suggester: CommandSuggester::new(),
            suggestion: SuggestionPin::new(),
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
            // Wired in T5 with the conversation picker — bound keys
            // must still be consumed (never leaked to the PTY), so the
            // stub reports the gap instead of no-op'ing.
            Resolution::Action(AppAction::Conversations) => {
                shared.toast = Some(Toast::new(
                    "conversation picker is not wired in the GUI frontend yet",
                    Instant::now(),
                ));
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
                // The active tab's index BEFORE the mutable borrow —
                // in-flight keys are scoped to the pinned tab (Spec
                // R04 user story 14).
                let index = shared.tabs.active_index();
                // In-flight keys (Spec R04 us-8/9/10): intercepted
                // BEFORE the PTY-input path, and scoped to the pinned
                // tab — on the tab whose request is streaming, Esc
                // cancels and Enter is swallowed so a half-written
                // command can't run; on every other tab both keys
                // behave normally.
                let suggestion_here = shared.suggestion.is_live_for(&shared.tabs, index);
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
                if suggestion_here {
                    match key.map(|key| key.key) {
                        // Plain Esc cancels: token + envelope close +
                        // `Ctrl+U` to wipe the partial line. A modified
                        // Esc is left for the PTY like any other chord.
                        Some(Key::Escape) if key.is_some_and(|key| key.modifiers.is_empty()) => {
                            shared.suggestion.cancel(&shared.suggester, tab);
                            return;
                        }
                        // Any Enter variant is swallowed — including
                        // `Ctrl+Enter`, which encodes `\r` and would
                        // execute the partial command (us-9) — and the
                        // keypad Enter that encodes the same.
                        Some(Key::Enter) | Some(Key::Keypad(Keypad::Enter)) => {
                            return;
                        }
                        _ => {}
                    }
                }
                if tab.exit_status().is_none() {
                    if let Some(key_event) = key {
                        // The `# `+Enter trigger (Spec R04): verify
                        // while the shadow still holds the line and
                        // the grid still shows its echo — before the
                        // submission resets the shadow and the
                        // forwarded `\r` moves the cursor. Enter is
                        // sent to the PTY either way: `# …` is a
                        // comment no-op that lands in history
                        // (us-2). A live pin gates the next
                        // trigger — one request in flight at a time.
                        if shared.suggestion.is_none() && suggest::is_submission(&key_event) {
                            if let Some(request) =
                                suggest::request_for(tab, shared.config.settings(), &shared.profile)
                            {
                                shared.suggestion.fire(
                                    &shared.suggester,
                                    tab.id,
                                    request,
                                    &mut shared.toast,
                                );
                            } else if let Some((line, request)) = suggest::classified_request(
                                tab,
                                shared.config.settings(),
                                &shared.profile,
                            ) {
                                // The third trigger (§5.10.4, Spec
                                // R11-T2): a natural-language verdict
                                // swallows the Enter and rewrites the
                                // submission — `Ctrl+U` wipes the
                                // typed line, `# <line>` + `\r` lands
                                // the request in history as a comment
                                // and draws the fresh prompt the
                                // armed suggestion streams onto
                                // (ADR-0007 semantics). The rewrite
                                // goes out ONLY once the request is
                                // armed: a refused `fire` falls
                                // through to the ordinary verbatim
                                // submission below — the
                                // erase-after-fire ordering
                                // `suggest_from_line` established.
                                if shared.suggestion.fire(
                                    &shared.suggester,
                                    tab.id,
                                    request,
                                    &mut shared.toast,
                                ) {
                                    suggest::rewrite_as_comment(tab, &line);
                                    return;
                                }
                            }
                        }
                        // The input shadow tracks what reaches the
                        // PTY — the `# `-trigger verification reads it.
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
    /// panel region or the chrome does not (the layout contract's
    /// "panel wheel events must not scroll the terminal grid" rule).
    fn wheel(
        &mut self,
        shared: &mut Shared,
        ctx: &egui::Context,
        unit: egui::MouseWheelUnit,
        delta: Vec2,
    ) {
        let Some(pane) = self.grid_pane else { return };
        let hovering = ctx
            .input(|input| input.pointer.hover_pos())
            .is_some_and(|pos| pane.contains(pos));
        if !hovering {
            return;
        }
        let Some(tab) = shared.tabs.active_mut() else {
            return;
        };
        match unit {
            egui::MouseWheelUnit::Line => {
                // One notch reports ±1.0 in LineDelta units — scale by
                // the shared WHEEL_LINES so a tick scrolls the same
                // 3 lines the TUI's wheel binding does
                // (CODING_STANDARDS §17 — one constant, both
                // frontends).
                let lines = (delta.y.abs() * WHEEL_LINES as f32).ceil().max(1.0) as usize;
                if delta.y > 0.0 {
                    tab.scroll_up(lines);
                } else {
                    tab.scroll_down(lines);
                }
            }
            egui::MouseWheelUnit::Page => {
                if delta.y > 0.0 {
                    tab.page_up();
                } else {
                    tab.page_down();
                }
            }
            egui::MouseWheelUnit::Point => {
                let cell = self.cell_height(ctx);
                let lines = (delta.y / cell).trunc() as i64;
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

    /// The side-panel region (Spec R13-T5's hook): the layout contract
    /// already carves and resizes for it — the skeleton paints the
    /// region and the panel's headline state, and T5 fills in the
    /// conversation transcript, the input line and the ask cards.
    fn paint_panel(&self, ui: &mut egui::Ui, shared: &Shared, rect: Rect) {
        let painter = ui.painter().with_clip_rect(rect);
        painter.rect_filled(rect, 0.0, Color32::from_rgb(0x1B, 0x1B, 0x1B));
        painter.line_segment(
            [rect.left_top(), rect.left_bottom()],
            Stroke::new(1.0, Color32::from_rgb(0x33, 0x33, 0x33)),
        );
        let text = shared
            .tabs
            .active()
            .and_then(|tab| tab.agent_panel())
            .map(|panel| format!("agent — {} items", panel.entries().len()))
            .unwrap_or_else(|| "agent".to_string());
        painter.text(
            rect.left_top() + Vec2::new(8.0, 8.0),
            Align2::LEFT_TOP,
            text,
            FontId::proportional(12.0),
            Color32::from_rgb(0x88, 0x88, 0x88),
        );
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

    /// The modal layer — painted last so overlays always win. Hosts the
    /// `Alt+P` profile picker now; T4's find bar and T5+'s conversation
    /// picker anchor here (same slot the TUI's `paint_profile_picker`
    /// and `paint_conversations` occupy).
    fn paint_modals(&self, ui: &mut egui::Ui, shared: &Shared, area: Rect) {
        if let Some(picker) = shared.picker.as_ref() {
            let painter = ui.painter();
            let font = FontId::monospace(13.0);
            let row_h = 18.0;
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
        if let Some(rect) = panel_rect {
            self.paint_panel(ui, &shared, rect);
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
