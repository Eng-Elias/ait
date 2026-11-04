//! The Round-2 application loop: shell sessions in tabs inside the host
//! terminal (Spec R02, tickets T1–T6).
//!
//! The loop is synchronous per the spec — no async runtime in Round 2:
//!
//! 1. `crossterm::event::poll` for input (a short timeout caps the frame
//!    period); every queued input event is serviced first so output floods
//!    can never starve the keyboard (REQUIREMENTS.md §6). Keys resolve
//!    against the app [`keymap`] **before** PTY input (standard 7): bound
//!    keys run app actions — tab spawn/close/switch land on the
//!    [`TabManager`], scroll/paste act on the active tab — everything else
//!    snaps the view to the bottom and encodes to the shell. The mouse
//!    wheel scrolls scrollback; a left-button drag highlights cells and
//!    its release copies the text to the host clipboard via OSC 52
//!    (`Shift+Insert` pastes ait's own copy back, bracketed-paste-wrapped
//!    when the child enabled mode 2004). Input to a tab whose session
//!    already exited is dropped.
//! 2. [`TabManager::drain_events`] feeds pending PTY output into EVERY
//!    tab's grid and surfaces lifecycle events — background sessions keep
//!    up, `Exited` marks the tab rather than ending the loop, OSC 7
//!    reports track each tab's cwd for `Alt+T` to inherit, and OSC 9/777
//!    `Notification`s become a ~3 s toast on the status line. On the
//!    same tick the settings watcher's channel is drained (Spec R03):
//!    a good reload swaps the live [`Settings`] — status line and
//!    NEW-session `[terminal]` options included — while a broken save
//!    toasts the error and keeps last-good. The suggester's channel
//!    drains on that tick too (Spec R04): the `# `+Enter trigger
//!    verified in [`suggest`] fires a request from the pass-through
//!    key path, and `Alt+Enter` fires the same request from the whole
//!    verified input line (erasing it via DELs once the request was
//!    accepted — never before), `Delta`s type
//!    into the pinned tab's session — one bracketed-paste envelope
//!    when the child enabled mode 2004 — and errors land on the
//!    status line. While a suggestion is in flight on the focused
//!    tab, Esc cancels it (token + `Ctrl+U` wipe) and Enter is
//!    swallowed so a half-written command can't run.
//! 3. A frame renders when the active session reports damage or what the
//!    last frame showed changed (view offset, selection, active tab, tab
//!    count, status-line text). The status text is part of that snapshot,
//!    so a session exiting or a toast appearing/expiring repaints even
//!    with no grid damage. Chrome — the tab bar on top and the status
//!    line at the bottom — lives outside the session grid; the PTY
//!    winsize is the content area only.
//!
//! `Alt+Q` quits everything, and closing the LAST tab exits `ait` cleanly
//! — the empty tab set is guarded before any indexing (standard 6). A
//! host input path that keeps failing past a bounded retry — stdin EOF,
//! or a source that never initialized — is the spec's third exit path:
//! the loop breaks and exits cleanly instead of spinning on errors. The
//! [`TerminalGuard`](crate::terminal::TerminalGuard) restores the host
//! terminal on every way out; dropping the manager drops every
//! `LiveSession`, which kills the children (SIGHUP → SIGKILL).

use std::cell::RefCell;
use std::io::{self, Write};
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use ait_agent::CommandSuggester;
use ait_config::{ConfigError, Settings};
use ait_term::{
    Cell, ClipboardSelection, Cursor, CursorShape, Damage, ExitStatus, LiveEvent, SessionEvent,
};
use crossterm::clipboard::CopyToClipboard;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::queue;
use ratatui::layout::{Position, Rect};
use ratatui::Terminal;

use crate::agent_panel::paint_agent_panel;
use crate::backend::StyledBackend;
use crate::config::{self, LiveSettings};
use crate::keymap::{self, AppAction, CapabilityTier, Resolution};
use crate::keys;
use crate::profile::{self, ProfilePicker};
use crate::render::{
    apply_selection, chrome_areas, content_size, grid_line, paint_profile_picker, paint_session,
    paint_status_line, paint_tab_bar, split_content, to_cursor_style, UnderlineOverlay,
};
use crate::scroll::WHEEL_LINES;
use crate::selection::{encode_paste, extract, GridPoint, SelectionRange};
use crate::status::{notification_text, status_text, StatusLine, Toast};
use crate::suggest::{self, SuggestionPin};
use crate::tabs::TabManager;
use crate::terminal::TerminalGuard;

/// Upper bound on one loop iteration's wait for input: at 8 ms the loop
/// idles at ~125 iterations/sec — well above the §6 floor of 30 fps — and
/// input latency stays under a frame.
const INPUT_POLL: Duration = Duration::from_millis(8);

/// Consecutive input failures tolerated before the loop treats the host
/// input path as dead — ~1 s of retries at the poll tick. crossterm
/// reports a missing source (stdin not a tty and `/dev/tty` cannot be
/// opened) as an error on EVERY `poll`, forever; a bound distinguishes
/// that permanent failure — the spec's stdin-EOF exit path — from a
/// transient one.
const INPUT_ERROR_LIMIT: u32 = 125;
/// Events serviced per [`service_input`] call before control returns
/// to the loop — bounds the zero-time drain so a sustained input
/// flood can't starve session drains and repaints (§6's starvation
/// rule cuts both ways). What's left is serviced next tick: 8 ms
/// later, invisible to the user.
const INPUT_DRAIN_LIMIT: u32 = 256;

/// Watchdog bound on the startup capability probe: comfortably past
/// crossterm's own ~2 s query timeout, so only a genuinely wedged probe
/// (kitty flags answered, primary-DA never) trips it — never a merely
/// slow-to-answer legacy host.
const DETECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether the loop keeps running after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handled {
    Continue,
    /// `Alt+Q` (or enhanced `Ctrl+Shift+Q`), or the last tab closed —
    /// exit cleanly.
    Quit,
}

/// What servicing the host input queue once produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputOutcome {
    /// Every queued event was handled (or none were queued); continue.
    Continue,
    /// A bound quit ran, or the last tab closed.
    Quit,
    /// `poll`/`read` failed — the caller counts consecutive failures
    /// against [`INPUT_ERROR_LIMIT`].
    Failed,
}

/// App-global mutable state the loop owns and `handle_event` mutates:
/// the bits that don't belong to any one tab.
struct AppState {
    /// The current window rect (updated on resize).
    screen: Rect,
    /// The last copied selection — whichever tab it came from;
    /// `Shift+Insert` pastes it into the active tab.
    copied_text: Option<String>,
    /// The exit status of the last closed tab, captured when closing
    /// empties the set — it becomes ait's exit code then.
    last_exit: Option<ExitStatus>,
    /// The live status-line toast, if any — a notification or error
    /// message that replaces the summary until its deadline (~3 s),
    /// expiring on the loop's 8 ms tick, not on a thread.
    toast: Option<Toast>,
    /// The live configuration: last-good [`Settings`] plus the derived
    /// status-line model slot, swapped wholesale by a successful
    /// reload (Spec R03 stories 10–11).
    config: LiveSettings,
    /// The channel-based command suggester (Spec R04): the `# `+Enter
    /// trigger queues a [`SuggestRequest`](ait_agent::SuggestRequest)
    /// carrying the live `Settings` snapshot; its events drain once
    /// per tick. Its tokio runtime stays on `ait-agent`'s worker
    /// thread (ADR-0004).
    suggester: CommandSuggester,
    /// The in-flight suggestion, pinned to the tab that fired it
    /// (Spec R04 user story 14) — idle while no request streams. One
    /// request at a time is the suggester's frontend contract, so a
    /// live pin also gates the next trigger. `Delta` text streams into
    /// the pinned tab's session, under one bracketed-paste envelope
    /// when the child enabled mode 2004; the pin re-resolves through
    /// the tab's stable [`id`](crate::tabs::Tab) — see
    /// [`suggest::SuggestionPin`] for the whole state machine.
    suggestion: SuggestionPin,
    /// The active execution profile's name (Spec R07 §5.7 / issue
    /// #101): the stance the NEXT `AgentRequest`/`SuggestRequest`
    /// snapshots beside its `Settings` — an in-flight task keeps the
    /// profile it started with, so consent is never rewritten mid-run.
    /// `default` at launch, `Alt+P` switches it, and nothing ever
    /// writes it back to `settings.toml` (us-27: ephemeral by design).
    /// A name a reload deleted stays armed — the worker's
    /// missing-profile fail-safe asks for every call until the user
    /// picks a live one.
    profile: String,
    /// The open profile switcher, if any (`Alt+P` with ≥3 declared
    /// profiles): the names and the highlighted row. App-global like
    /// the selection itself — a tab switch or panel toggle never
    /// disturbs it; a settings reload drops the stale list.
    picker: Option<ProfilePicker>,
}

/// What the last painted frame showed: a change in any field alone
/// triggers a repaint even when the grid reports no damage (a tab switch
/// repaints an unchanged-but-background grid; a close repaints the tab
/// bar; a session exit, cwd change, or a toast appearing/expiring
/// repaints the status line — including an `Exited` on a grid that
/// reports no further damage).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Rendered {
    /// Which tab was painted.
    active: usize,
    /// How many tabs the bar showed.
    count: usize,
    /// The painted tab's scroll offset.
    offset: usize,
    /// The painted selection range.
    selection: Option<SelectionRange>,
    /// The status line's text this frame (toast or session summary).
    status: String,
    /// Whether `status` was a live toast (painted reversed).
    toast: bool,
    /// The cursor's position and shape — a pure cursor move repaints.
    cursor: Cursor,
    /// The cursor cell's contents last frame. alacritty's `damage()`
    /// always marks the cursor cell, so when that is the only mark the
    /// snapshot must carry enough to notice the cell changing under a
    /// stationary cursor (a spinner redrawing in place).
    cursor_cell: Option<Cell>,
    /// What the active tab's agent panel showed — `None` while the
    /// tab has none (Spec R06): a streamed `TextDelta` on an idle
    /// shell produces no grid damage, so the panel's mutation counter
    /// rides the snapshot to force its repaints.
    panel: Option<PanelView>,
    /// The profile picker overlay's painted state (Spec R07): it
    /// draws over the session grid, which reports no damage of its
    /// own — opening, moving the highlight and closing must all show
    /// up in the snapshot to repaint.
    picker: Option<ProfilePicker>,
}

/// The painted agent panel's state for the [`Rendered`] snapshot:
/// every change the paint can see bumps the panel's revision, so
/// comparing it is comparing the whole panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PanelView {
    /// Whether the split was open.
    open: bool,
    /// Whether the panel owned the keyboard.
    focused: bool,
    /// The panel's mutation counter.
    revision: u64,
}

/// Run the TUI: spawn `$SHELL` in a tab and drive the tab set until the
/// last tab closes or the user quits. The last closed tab's exit status
/// (if it had exited) becomes the process exit code, like the single
/// session's status did in T1; a user quit maps to `SUCCESS`.
pub fn run() -> io::Result<ExitCode> {
    let _guard = TerminalGuard::with_options(crate::terminal::TerminalOptions {
        mouse_capture: true,
    })?;
    // Capability detection runs once, before the event loop: the query
    // blocks up to ~2 s on hosts that don't answer (tmux) and must never
    // race `event::poll`. It runs under a watchdog — a host that answers
    // the kitty flags query but never answers the trailing primary-DA
    // query wedges crossterm's event reader *forever*, and the wedged
    // lock would starve every later `event::poll`. There is no recovery:
    // exit cleanly (the guard restores the host) rather than start an
    // input-dead TUI that can't even quit.
    let tier = match crate::terminal::detect_tier(DETECT_TIMEOUT) {
        crate::terminal::TierDetection::Enhanced => CapabilityTier::Enhanced,
        crate::terminal::TierDetection::Legacy => CapabilityTier::Legacy,
        crate::terminal::TierDetection::Hung => {
            return Err(io::Error::other(
                "host terminal never finished the keyboard-capability probe \
                 (answered the kitty flags query but not primary-DA) — \
                 refusing to start with a dead input path",
            ));
        }
    };
    let (columns, rows) = crossterm::terminal::size()?;
    // Settings load BEFORE the first session spawns: `[terminal]` shapes
    // every tab including the first (Spec R03 stories 12–13). A missing
    // file silently yields built-in defaults (story 6); an invalid one
    // runs on defaults with the `ConfigError` toasted (stories 7–8) —
    // a typo never costs the terminal.
    let (config, load_error) = match ait_config::load() {
        Ok(settings) => (LiveSettings::adopt(settings), None),
        Err(error) => (
            LiveSettings::adopt(Settings::default()),
            Some(error.message().to_string()),
        ),
    };
    // Hot reload watches the same settings path; `config::watcher`
    // creates the config dir when a first run without `--init` left it
    // absent so a mid-session first write still reloads — a genuine
    // watch failure still degrades to running without reload.
    let watcher = config::watcher();
    let mut state = AppState {
        screen: Rect::new(0, 0, columns, rows),
        copied_text: None,
        last_exit: None,
        toast: load_error.map(|text| Toast::new(format!("config error: {text}"), Instant::now())),
        config,
        // `OsKeyring` is the default secret store — `api_key_env`
        // first, OS keyring second (§4.1).
        suggester: CommandSuggester::new(),
        suggestion: SuggestionPin::new(),
        // Every launch starts on `default` (Spec R07 us-27) — the
        // selection is never read back from disk.
        profile: profile::DEFAULT_PROFILE.to_string(),
        picker: None,
    };
    // The PTY winsize is the CONTENT area: the tab bar and the (reserved)
    // status-line row are chrome outside the session grid (Spec R02).
    let mut tabs = TabManager::spawn_with(
        content_size(state.screen),
        config::session_template(&state.config.settings().terminal),
    )
    .map_err(|e| io::Error::other(e.to_string()))?;

    let backend = StyledBackend::new(io::stdout());
    let underlines = backend.underline_overlay();
    let mut terminal = Terminal::new(backend)?;
    // No `terminal.clear()` here: it issues a cursor-position report that
    // blocks on stdin, and the just-entered alternate screen is already
    // clean — the first damage render repaints everything.
    let mut last_cursor_shape = None;
    let mut rendered: Option<Rendered> = None;

    let mut input_failures = 0u32;
    loop {
        // 1. Input first, completely — floods must not starve keystrokes.
        match service_input(
            &mut tabs,
            &mut state,
            terminal.backend_mut().writer_mut(),
            tier,
            &mut |timeout| event::poll(timeout),
            &mut || event::read(),
        ) {
            InputOutcome::Continue => input_failures = 0,
            InputOutcome::Quit => break,
            // A dead input path — stdin EOF, or a source that never
            // initialized (stdin not a tty, no `/dev/tty`) — errors on
            // every call. Retries cover transient failures; past the
            // bound this is the spec's stdin-EOF exit path: leave the
            // loop and exit cleanly, like closing the last tab.
            InputOutcome::Failed => {
                input_failures += 1;
                if input_failures >= INPUT_ERROR_LIMIT {
                    break;
                }
                // A failing call returns immediately — pace the retry
                // so a dead source cannot spin the render loop.
                std::thread::sleep(INPUT_POLL);
            }
        }

        // 2. Drain EVERY tab: feeds queued PTY bytes into the grids and
        //    surfaces lifecycle events. `CwdChanged`/`Exited` are applied
        //    inside the manager; `Notification`s become the status-line
        //    toast here.
        for tab_event in tabs.drain_events() {
            match tab_event.event {
                // OSC 9/777: the child asked for a notification. A
                // background tab's toast is prefixed with its tab-bar
                // label (`N:title`) so it surfaces with attribution
                // (Spec R02 user story 16).
                LiveEvent::Osc(SessionEvent::Notification { title, message }) => {
                    let source = if tab_event.tab == tabs.active_index() {
                        None
                    } else {
                        tabs.tabs()
                            .get(tab_event.tab)
                            .map(|tab| tab.label(tab_event.tab))
                    };
                    state.toast = Some(Toast::new(
                        notification_text(source.as_deref(), title.as_deref(), &message),
                        Instant::now(),
                    ));
                }
                // Child-app OSC 52 — the event only fires when
                // `[terminal] osc52` opted in (`SessionOptions::osc52`,
                // Spec R02). Honor the request through the same OSC 52
                // write our own selection copy uses: a silent no-op
                // where the host denies it.
                LiveEvent::Osc(SessionEvent::ClipboardWrite {
                    selection,
                    contents,
                }) => {
                    let writer = terminal.backend_mut().writer_mut();
                    let _ = match selection {
                        ClipboardSelection::Clipboard => {
                            queue!(writer, CopyToClipboard::to_clipboard_from(&contents))
                        }
                        ClipboardSelection::Selection => {
                            queue!(writer, CopyToClipboard::to_primary_from(&contents))
                        }
                    };
                    let _ = writer.flush();
                }
                _ => {}
            }
        }

        // Config reloads drain on the same tick — one 8 ms frame is the
        // worst-case latency, so no extra waking machinery is needed.
        if let Some(watcher) = &watcher {
            drain_config_events(watcher.receiver(), &mut state, &mut tabs);
        }
        // Suggester events on the same tick too (Spec R04): T4 drains
        // errors to the status line and clears the in-flight pin; T5
        // streams `Delta`s into the pinned tab.
        state
            .suggestion
            .drain(state.suggester.events(), &tabs, &mut state.toast);
        // Agent sessions on the same tick (Spec R06): `TextDelta`s
        // stream into each tab's conversation and terminal events
        // release its running state — a background tab keeps up.
        tabs.drain_agent_events();

        // Standard 6: the last close already exits via `Handled::Quit`;
        // this guard is the belt for any path that could empty the set —
        // never index an empty tab list.
        let active = tabs.active_index();
        let count = tabs.len();
        let Some(tab) = tabs.active_mut() else {
            break;
        };

        // 3. Render when the grid reports damage or the painted view
        //    changed. The status text rides the snapshot, so `Exited`
        //    and toast expiry repaint without grid damage.
        let cursor = tab.cursor();
        // alacritty's `damage()` ALWAYS marks the cursor's own cell —
        // trusting it alone repaints every 8 ms tick (~125 fps idle,
        // emitting empty ratatui diffs). A cursor-only report carries no
        // new pixels: a cursor move marks the old AND new cells, and a
        // cell rewritten under a stationary cursor shows up in
        // `view.cursor_cell` — both still repaint via the snapshot.
        let damaged = damaged_beyond_cursor(&tab.session.damage(), cursor);
        let now = Instant::now();
        // The model slot resolves the ACTIVE profile's `base_model`
        // (Spec R07 §4.2/us-25) — per tick, so a switch or a reload is
        // the very next frame's text.
        let model = state.config.model(&state.profile);
        let view = Rendered {
            active,
            count,
            offset: tab.scroll_offset(),
            selection: tab.selection_range(),
            status: status_text(
                &StatusLine {
                    cwd: tab.cwd(),
                    shell: tab.title(),
                    model: &model,
                    profile: profile::status_slot(&state.profile),
                    block: tab.last_block_status(),
                    exit: tab.exit_status(),
                },
                state.toast.as_ref(),
                now,
            ),
            toast: state.toast.as_ref().is_some_and(|t| t.is_live(now)),
            cursor,
            cursor_cell: tab.grid().cell(
                i32::try_from(cursor.line).unwrap_or(i32::MAX),
                cursor.column,
            ),
            panel: tab.agent_panel().map(|panel| PanelView {
                open: panel.is_open(),
                focused: panel.is_focused(),
                revision: panel.revision(),
            }),
            picker: state.picker.clone(),
        };
        if damaged || rendered.as_ref() != Some(&view) {
            render_frame(
                &mut terminal,
                &tabs,
                &view.status,
                view.toast,
                state.picker.as_ref(),
                &underlines,
                &mut last_cursor_shape,
            )?;
            rendered = Some(view);
        }
    }

    Ok(exit_code(state.last_exit.as_ref()))
}

/// Route one host event. Keys hit the app keymap **before** PTY input
/// (standard 7): a bound key runs its action and never reaches the PTY —
/// tab actions mutate the [`TabManager`], scroll/paste actions act on the
/// active tab. An unbound key snaps the view to the bottom and encodes to
/// the shell, unless the tab's session already exited — input to exited
/// tabs is dropped (the tab stays readable until `Alt+W`). Any keypress
/// also clears the selection highlight.
///
/// The spec's "any keypress snaps to bottom" (story 13) is honored per
/// action rather than blindly: the scroll actions exist to move the
/// view, `Paste` snaps (the user pastes to type), the tab actions
/// change which tab is on screen — each tab keeps its own offset by
/// design — and `Quit` exits. A key release is not a keypress: it does
/// none of this.
///
/// The wheel moves the scroll
/// offset; a left-button drag selects cells and its release copies the
/// text to the host clipboard via OSC 52 — a silent no-op where the host
/// denies it (Spec R02). Resizes forward to EVERY tab's PTY. A failed
/// `Alt+T` spawn surfaces as a status-line toast instead of being
/// swallowed.
///
/// Write/resize failures (e.g. a just-dead child) are tolerated — the
/// session's `Exited` event marks the tab cleanly.
fn handle_event<W: Write>(
    event: Event,
    tabs: &mut TabManager,
    state: &mut AppState,
    host: &mut W,
    tier: CapabilityTier,
) -> Handled {
    match event {
        Event::Key(key_event) => {
            // A key release carries no input: it never binds, sends no
            // bytes — and it is not a keypress, so it clears no
            // selection and snaps no scroll. (Releases shouldn't even
            // arrive: ait pushes only DISAMBIGUATE_ESCAPE_CODES, never
            // REPORT_EVENT_TYPES — the guard is for hosts that send
            // them anyway.)
            if key_event.kind == KeyEventKind::Release {
                return Handled::Continue;
            }
            match keymap::resolve(&key_event, tier) {
                Resolution::Action(AppAction::Quit) => return Handled::Quit,
                Resolution::Action(AppAction::NewTab) => {
                    // `new_tab` clears the outgoing tab's highlight before
                    // switching — even when the spawn fails, the keypress
                    // already left the tab.
                    // A spawn failure (e.g. the tracked cwd was deleted)
                    // leaves the set untouched — toast it instead of
                    // swallowing it.
                    if let Err(error) = tabs.new_tab() {
                        state.toast = Some(Toast::new(
                            format!("new tab failed: {error}"),
                            Instant::now(),
                        ));
                    }
                }
                Resolution::Action(AppAction::CloseTab) => {
                    // Closing the last tab exits ait; capture its status for
                    // the process exit code before it is removed.
                    if tabs.len() == 1 {
                        state.last_exit = tabs.active().and_then(|tab| tab.exit_status().cloned());
                    }
                    // `false` = the set is now empty: quit rather than ever
                    // indexing it (standard 6).
                    if !tabs.close_active() {
                        return Handled::Quit;
                    }
                }
                // Cycling and jumping clear the outgoing tab's selection
                // inside the manager's transition methods.
                Resolution::Action(AppAction::NextTab) => tabs.activate_next(),
                Resolution::Action(AppAction::PrevTab) => tabs.activate_prev(),
                Resolution::Action(AppAction::JumpToTab(n)) => tabs.jump_to(n),
                // `Alt+Enter` (Spec R04 us-11/12): the input line itself
                // is the request — same shadow+grid verification as the
                // `# ` path, then, once the request was accepted, the
                // typed line is erased via DELs so request text and the
                // streamed command can never interleave. A live pin
                // gates it just like the `# ` trigger — one request in
                // flight at a time. The request pins to the tab's
                // stable id, so a mid-stream tab switch or close can
                // never redirect the stream (us-14).
                Resolution::Action(AppAction::SuggestCommand) => {
                    let Some(tab) = tabs.active_mut() else {
                        return Handled::Continue;
                    };
                    // The keypress is input-adjacent: drop the highlight
                    // and snap to the live viewport like typing does.
                    tab.clear_selection();
                    tab.snap_to_bottom();
                    state.suggestion.suggest_from_line(
                        &state.suggester,
                        state.config.settings(),
                        &state.profile,
                        tab,
                        &mut state.toast,
                    );
                }
                // `Alt+A` (Spec R06 / issue #85): toggle the active
                // tab's agent conversation panel — open focuses its
                // input line, a second press while focused closes the
                // split. Only THIS tab's winsize changes (panels are
                // per-tab): `resize_active` re-derives it through the
                // same session-resize path a window resize takes.
                Resolution::Action(AppAction::AgentPanel) => {
                    let Some(tab) = tabs.active_mut() else {
                        return Handled::Continue;
                    };
                    tab.clear_selection();
                    tab.toggle_agent_panel();
                    tabs.resize_active();
                }
                // `Alt+P` (Spec R07 §5.7 / issue #101): switch the
                // active execution profile — a picker over three or
                // more declared names (its keys route through the
                // pass-through arm while open), a direct cycle
                // between two, a toast when there is nothing to
                // switch to. A second press while the picker is open
                // dismisses it. The selection is app-global and
                // ephemeral: it rides the NEXT `AgentRequest`/
                // `SuggestRequest` snapshot, never settings.toml.
                Resolution::Action(AppAction::SwitchProfile) => {
                    if state.picker.take().is_some() {
                        // Toggled closed — the highlight is dropped.
                    } else {
                        let names: Vec<String> = state
                            .config
                            .settings()
                            .agents
                            .execution_profiles
                            .keys()
                            .cloned()
                            .collect();
                        match profile::switch(&names, &state.profile) {
                            profile::Switch::Picker => {
                                state.picker = Some(ProfilePicker::new(names, &state.profile));
                            }
                            profile::Switch::Cycle(name) => {
                                activate_profile(state, name);
                            }
                            profile::Switch::Unavailable => {
                                state.toast = Some(Toast::new(
                                    "no other execution profiles declared",
                                    Instant::now(),
                                ));
                            }
                        }
                    }
                }
                // Per-active-tab viewport actions: scrollback + paste.
                Resolution::Action(action) => {
                    let Some(tab) = tabs.active_mut() else {
                        return Handled::Continue;
                    };
                    tab.clear_selection();
                    match action {
                        AppAction::ScrollPageUp => tab.page_up(),
                        AppAction::ScrollPageDown => tab.page_down(),
                        AppAction::ScrollTop => tab.scroll_to_top(),
                        AppAction::ScrollBottom => tab.snap_to_bottom(),
                        // Spec R05 §5.5: block navigation scrolls like
                        // the page bindings; `Alt+B` programs the
                        // existing selection over the current block's
                        // clamped range and copies it through the same
                        // OSC-52 path a mouse release uses. On a tab
                        // with NO blocks the chords are ordinary input
                        // again — encoded to the PTY like PassThrough
                        // (`ESC b`, `ESC [1;3A`, …) rather than eaten
                        // inertly (§5.5 Out-of-Scope: no integration ⇒
                        // no behavior change). The spec's readline
                        // trade-off — `M-b` is backward-word — applies
                        // only where blocks exist to act on.
                        AppAction::PrevBlockStart
                        | AppAction::NextBlockStart
                        | AppAction::SelectBlock
                            if !tab.has_blocks() =>
                        {
                            tab.snap_to_bottom();
                            if tab.exit_status().is_none() {
                                if let Some(key_event) = keys::translate(&key_event) {
                                    tab.shadow.update(&key_event);
                                    let _ = tab.session.send_key(key_event);
                                }
                            }
                        }
                        AppAction::PrevBlockStart => tab.prev_block_start(),
                        AppAction::NextBlockStart => tab.next_block_start(),
                        AppAction::SelectBlock => {
                            if let Some(range) = tab.select_block() {
                                copy_range(tab, range, state, host);
                            }
                        }
                        AppAction::Paste => {
                            // Paste ait's own copied selection (there is no
                            // host-clipboard read), bracketed iff the child
                            // enabled mode 2004. The user pastes to type, so
                            // the view snaps to the live viewport first.
                            // Exited sessions drop input like keys do.
                            tab.snap_to_bottom();
                            if tab.exit_status().is_none() {
                                if let Some(text) = state.copied_text.as_deref() {
                                    let bytes = encode_paste(text, tab.session.bracketed_paste());
                                    let _ = tab.session.send_bytes(&bytes);
                                    // Pasted bytes are arbitrary input —
                                    // newlines included — so the input
                                    // shadow cannot replay them.
                                    tab.shadow.mark_stale();
                                }
                            }
                        }
                        // The manager actions are matched on `Resolution`
                        // above — the manager-vs-viewport split is runtime,
                        // not type-level, so this arm is defensive: an
                        // action reaching it was added to the keymap
                        // without dispatch. No-op rather than panic the UI
                        // loop; the enum listing means a new `AppAction`
                        // fails exhaustiveness here until the author picks
                        // a side for it.
                        AppAction::Quit
                        | AppAction::NewTab
                        | AppAction::CloseTab
                        | AppAction::NextTab
                        | AppAction::PrevTab
                        | AppAction::JumpToTab(_)
                        | AppAction::SuggestCommand
                        | AppAction::AgentPanel
                        | AppAction::SwitchProfile => {} // dispatched above
                    }
                }
                Resolution::PassThrough => {
                    // An open profile picker owns unbound keys (Spec
                    // R07 §5.7): ↑/↓ move the highlight, Enter arms
                    // the highlighted name, plain Esc dismisses, and
                    // every other key is swallowed so nothing leaks
                    // to the PTY mid-pick. Bound chords already ran
                    // above — `Alt+P` itself dismisses there, like
                    // `Alt+A` closes the focused panel.
                    if state.picker.is_some() {
                        picker_key(&key_event, state);
                        return Handled::Continue;
                    }
                    // The active tab's index BEFORE the mutable borrow —
                    // in-flight keys are scoped to the pinned tab
                    // (Spec R04 user story 14).
                    let index = tabs.active_index();
                    // In-flight keys (Spec R04 us-8/9/10): intercepted
                    // BEFORE the PTY-input path, and scoped to the pinned
                    // tab — on the tab whose request is streaming, Esc
                    // cancels and Enter is swallowed so a half-written
                    // command can't run; on every other tab both keys
                    // behave normally (the request types into ITS origin
                    // session whatever is focused, but the keys that
                    // steer it belong to the user watching that line).
                    let suggestion_here = state.suggestion.is_live_for(tabs, index);
                    let Some(tab) = tabs.active_mut() else {
                        return Handled::Continue;
                    };
                    // Any keypress drops the highlight — never stale state.
                    tab.clear_selection();
                    // A focused agent panel owns the keystroke (Spec
                    // R06): it edits the panel's input line, never
                    // reaches the PTY, never updates the input shadow
                    // and never snaps the terminal's view. The app
                    // keymap already resolved above — its bindings
                    // still win over panel focus (standard 7).
                    if tab.agent_focused() {
                        tab.agent_key(
                            &key_event,
                            state.config.settings(),
                            &state.profile,
                            &mut state.toast,
                        );
                        return Handled::Continue;
                    }
                    // Typing snaps the view back to the live viewport, then
                    // reaches the PTY encoded exactly as before.
                    tab.snap_to_bottom();
                    if suggestion_here {
                        match key_event.code {
                            // Plain Esc cancels: token + envelope close +
                            // `Ctrl+U` to wipe the partial line. A modified
                            // Esc is left for the PTY like any other chord.
                            KeyCode::Esc if key_event.modifiers.is_empty() => {
                                state.suggestion.cancel(&state.suggester, tab);
                                return Handled::Continue;
                            }
                            // Any Enter variant is swallowed — including
                            // `Ctrl+Enter`, which encodes `\r` and would
                            // execute the partial command (us-9).
                            KeyCode::Enter => return Handled::Continue,
                            _ => {}
                        }
                    }
                    if tab.exit_status().is_none() {
                        if let Some(key_event) = keys::translate(&key_event) {
                            // The `# `+Enter trigger (Spec R04): verify
                            // while the shadow still holds the line and
                            // the grid still shows its echo — before the
                            // submission resets the shadow and the
                            // forwarded `\r` moves the cursor. Enter is
                            // sent to the PTY either way: `# …` is a
                            // comment no-op that lands in history
                            // (us-2). A live pin gates the next
                            // trigger — one request in flight at a time.
                            if state.suggestion.is_none() && suggest::is_submission(&key_event) {
                                if let Some(request) = suggest::request_for(
                                    tab,
                                    state.config.settings(),
                                    &state.profile,
                                ) {
                                    state.suggestion.fire(
                                        &state.suggester,
                                        tab.id,
                                        request,
                                        &mut state.toast,
                                    );
                                }
                            }
                            tab.shadow.update(&key_event);
                            let _ = tab.session.send_key(key_event);
                        }
                    }
                }
            }
        }
        Event::Mouse(mouse_event) => {
            let content = chrome_areas(state.screen).content;
            // The panel owns its rect: the wheel there scrolls the
            // conversation, and a click there never starts a terminal
            // selection (Spec R06 — clicks in the panel are the
            // panel's, not the grid's).
            let panel_open = tabs.active().is_some_and(|tab| tab.agent_panel_open());
            let (terminal, panel) = split_content(content, panel_open);
            let position = Position::new(mouse_event.column, mouse_event.row);
            let in_panel = panel.is_some_and(|area| area.contains(position));
            let Some(tab) = tabs.active_mut() else {
                return Handled::Continue;
            };
            match mouse_event.kind {
                MouseEventKind::ScrollUp => {
                    if in_panel {
                        tab.agent_scroll_up(WHEEL_LINES);
                    } else {
                        tab.clear_selection();
                        tab.scroll_up(WHEEL_LINES);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if in_panel {
                        tab.agent_scroll_down(WHEEL_LINES);
                    } else {
                        tab.clear_selection();
                        tab.scroll_down(WHEEL_LINES);
                    }
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // Only a drag starting inside the session area
                    // selects; the chrome rows (tab bar, status line)
                    // and the agent panel are inert.
                    if terminal.contains(position) {
                        tab.begin_selection(grid_point(
                            &mouse_event,
                            terminal,
                            tab.scroll_offset(),
                        ));
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    // An in-flight drag follows the pointer anywhere —
                    // past the top it selects into scrollback, the way
                    // the view would scroll.
                    tab.extend_selection(grid_point(&mouse_event, terminal, tab.scroll_offset()));
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    // Fold the release point into the range before
                    // finishing: hosts coalesce motion reports, so the
                    // last `Drag` can lag the cell the button released
                    // on — a fast flick would otherwise copy short.
                    // Outside a drag `extend` is a no-op.
                    tab.extend_selection(grid_point(&mouse_event, terminal, tab.scroll_offset()));
                    if let Some(range) = tab.finish_selection() {
                        copy_range(tab, range, state, host);
                    }
                }
                // Other buttons and motion aren't captured/used.
                _ => {}
            }
        }
        Event::Resize(columns, rows) => {
            state.screen = Rect::new(0, 0, columns, rows);
            // EVERY tab's PTY gets the new content-area winsize, not just
            // the visible one (Spec R02 user story 17); selections are
            // cleared inside — reflow invalidates their coordinates.
            tabs.resize_all(content_size(state.screen));
        }
        // Focus and paste events have no Round-2 handler yet.
        _ => {}
    }
    Handled::Continue
}

/// The OSC-52 copy both selection paths share (Spec R02's mouse
/// release, Spec R05's `Alt+B` block select): extract the range's
/// text, keep it as ait's own clipboard for `Shift+Insert`, and offer
/// it to the host — a silent no-op where the host denies OSC 52, never
/// an error. An empty range copies nothing, same rule the drag path
/// always applied.
fn copy_range<W: Write>(
    tab: &crate::tabs::Tab,
    range: SelectionRange,
    state: &mut AppState,
    host: &mut W,
) {
    let text = extract(&tab.grid(), range);
    if text.is_empty() {
        return;
    }
    state.copied_text = Some(text.clone());
    let _ = queue!(host, CopyToClipboard::to_clipboard_from(&text));
    let _ = host.flush();
}

/// Service the host input queue once: wait up to [`INPUT_POLL`] for the
/// first event, then drain what follows before returning — bounded by
/// [`INPUT_DRAIN_LIMIT`] so an *input* flood can't starve the output
/// drains and repaints the input-first order exists to protect.
///
/// `poll`/`read` are crossterm's calls in production; tests inject
/// fakes to exercise the failure path without a TTY (the same seam
/// `terminal::detect_tier_with` uses for the probe watchdog). Any
/// failure — an EOF'd or never-initialized input source — surfaces as
/// [`InputOutcome::Failed`] for the caller to bound-retry.
fn service_input<W: Write>(
    tabs: &mut TabManager,
    state: &mut AppState,
    host: &mut W,
    tier: CapabilityTier,
    poll: &mut dyn FnMut(Duration) -> io::Result<bool>,
    read: &mut dyn FnMut() -> io::Result<Event>,
) -> InputOutcome {
    match poll(INPUT_POLL) {
        Ok(true) => {}
        Ok(false) => return InputOutcome::Continue,
        Err(_) => return InputOutcome::Failed,
    }
    for _ in 0..INPUT_DRAIN_LIMIT {
        match read() {
            Ok(event) => {
                if let Handled::Quit = handle_event(event, tabs, state, host, tier) {
                    return InputOutcome::Quit;
                }
            }
            Err(_) => return InputOutcome::Failed,
        }
        match poll(Duration::ZERO) {
            Ok(true) => {}
            Ok(false) => return InputOutcome::Continue,
            Err(_) => return InputOutcome::Failed,
        }
    }
    // The cap hit with (presumably) more input queued — yield to the
    // loop so PTY output drains and the frame renders; the rest of
    // the queue is serviced next tick.
    InputOutcome::Continue
}

/// Drain one tick of settings-watcher events (Spec R03 stories 10–11):
/// a good reload swaps the live [`Settings`] — model slot included —
/// and replaces the NEW-session `[terminal]` template (existing tabs
/// keep spawn-time values, story 14); a broken save toasts the
/// `ConfigError` and keeps last-good. `try_recv` keeps the drain
/// zero-cost inside the 8 ms poll loop — no async, no extra waking.
///
/// Spec R07 §5.7/us-24: a reload that renamed or deleted the ACTIVE
/// execution profile does NOT rewrite the selection — the name stays
/// armed so the worker's missing-profile fail-safe keeps asking for
/// every call until the user picks a live one, and a toast names the
/// vanished profile instead of a silent fallback. An open picker's
/// snapshot is dropped with the old settings; the next `Alt+P` lists
/// the new file's names.
fn drain_config_events(
    receiver: &Receiver<Result<Settings, ConfigError>>,
    state: &mut AppState,
    tabs: &mut TabManager,
) {
    while let Ok(event) = receiver.try_recv() {
        match event {
            Ok(settings) => {
                tabs.set_session_template(config::session_template(&settings.terminal));
                // The OLD declared names decide "deleted" vs
                // "never declared" — an armed `default` deleted
                // with the whole table toasts; one that never was
                // stays silent (us-28).
                let vanished =
                    profile::active_missing(state.config.settings(), &settings, &state.profile)
                        .then(|| state.profile.clone());
                state.config = LiveSettings::adopt(settings);
                state.picker = None;
                if let Some(name) = vanished {
                    state.toast = Some(Toast::new(
                        format!("execution profile \"{name}\" no longer declared"),
                        Instant::now(),
                    ));
                }
            }
            Err(error) => {
                state.toast = Some(Toast::new(
                    format!("config error: {}", error.message()),
                    Instant::now(),
                ));
            }
        }
    }
}

/// Screen mouse position → grid coordinates: the line by
/// [`grid_line`]'s screen→grid rule (the same rule the painters use),
/// `column = column - content.x` clamped at the band's left edge.
fn grid_point(mouse: &MouseEvent, content: Rect, scroll_offset: usize) -> GridPoint {
    GridPoint::new(
        grid_line(mouse.row, content, scroll_offset),
        usize::from(mouse.column.saturating_sub(content.x)),
    )
}

/// Draw one frame: the tab bar on top, the active tab's grid in the
/// content band (shifted `scroll_offset` lines into scrollback, selected
/// cells inverted) — or its terminal side when the tab's agent panel is
/// open, the split's right side going to [`paint_agent_panel`] — the
/// status line at the bottom (`status`, reversed while it's a live
/// toast), then the application-requested cursor shape and position:
/// the panel's input-line cursor while it owns the keyboard, the
/// session's otherwise.
fn render_frame(
    terminal: &mut Terminal<StyledBackend<io::Stdout>>,
    tabs: &TabManager,
    status: &str,
    toast: bool,
    picker: Option<&ProfilePicker>,
    underlines: &Rc<RefCell<UnderlineOverlay>>,
    last_cursor_shape: &mut Option<SetCursorStyle>,
) -> io::Result<()> {
    let Some(tab) = tabs.active() else {
        return Ok(());
    };
    let cursor = tab.cursor();
    // A focused panel's input line wants a bar cursor; the session's
    // own shape governs everywhere else.
    let shape = if tab.agent_focused() {
        CursorShape::Beam
    } else {
        cursor.shape
    };
    if let Some(style) = to_cursor_style(shape) {
        if *last_cursor_shape != Some(style) {
            // Queued on the backend's writer; flushes with this draw.
            queue!(terminal.backend_mut().writer_mut(), style)?;
            *last_cursor_shape = Some(style);
        }
    }

    let titles = tabs.titles();
    let active = tabs.active_index();
    terminal.draw(|frame| {
        let areas = chrome_areas(frame.area());
        // Spec R06's right-side split: the session paints in the
        // terminal half, the agent conversation in the panel half —
        // same `split_content` formula the winsize derivation uses.
        let (terminal_area, panel_area) = split_content(areas.content, tab.agent_panel_open());
        let buf = frame.buffer_mut();
        {
            let mut overlay = underlines.borrow_mut();
            overlay.clear();
            paint_session(
                buf,
                terminal_area,
                &tab.grid(),
                &mut overlay,
                tab.scroll_offset(),
            );
            if let Some(range) = tab.selection_range() {
                apply_selection(buf, terminal_area, &tab.grid(), range, tab.scroll_offset());
            }
        }
        if let (Some(area), Some(panel)) = (panel_area, tab.agent_panel()) {
            paint_agent_panel(buf, area, panel);
        }
        paint_tab_bar(buf, areas.tab_bar, &titles, active);
        paint_status_line(buf, areas.status, status, toast);
        // The profile switcher is an overlay (Spec R07 §5.7): it
        // paints last over the content band's bottom rows so nothing
        // underneath decides its hit-testing.
        if let Some(picker) = picker {
            paint_profile_picker(buf, areas.content, picker);
        }
        if let (Some(area), Some(panel)) = (panel_area, tab.agent_panel()) {
            if panel.is_focused() {
                // The input line owns the host cursor while the panel
                // owns the keyboard.
                let (x, y) = panel.cursor_position(area);
                frame.set_cursor_position((x, y));
            }
        }
        if !tab.agent_focused() {
            // The cursor's live line renders at `area.y + line + offset`;
            // scrolled far enough up it leaves the band and stays hidden.
            let y = usize::from(terminal_area.y) + cursor.line + tab.scroll_offset();
            if cursor.shape != CursorShape::Hidden && y < usize::from(terminal_area.bottom()) {
                let x = (terminal_area.x + cursor.column as u16)
                    .min(terminal_area.right().saturating_sub(1));
                frame.set_cursor_position((x, y as u16));
            }
            // No position set → ratatui hides the cursor this frame.
        }
    })?;
    terminal.backend_mut().writer_mut().flush()?;
    Ok(())
}

/// Whether a damage report marks any cell besides the cursor's own.
///
/// alacritty's `Term::damage` unconditionally damages the cursor cell —
/// under an 8 ms input tick that alone would repaint ~125 frames/sec on
/// an idle session (each an empty ratatui diff). The cases that DO need
/// a repaint stay covered: a cursor move damages the old and new cells,
/// and a cell rewritten in place shows up as a `Rendered::cursor_cell`
/// change — so filtering cursor-only bounds never stalls a real update.
fn damaged_beyond_cursor(damage: &Damage, cursor: Cursor) -> bool {
    match damage {
        Damage::Full => true,
        Damage::Partial(bounds) => bounds.iter().any(|b| {
            !(b.line == cursor.line && b.left == cursor.column && b.right == cursor.column)
        }),
    }
}

/// One key while the profile picker is open (Spec R07 §5.7): the ask
/// card's vocabulary — ↑/↓ move the highlight, Enter arms the
/// highlighted name and closes, plain Esc closes without switching.
/// Anything else is swallowed: a picker that let `q` through would
/// have typed it into the shell. `state.picker` is `Some` on entry —
/// the caller already routed.
fn picker_key(key: &KeyEvent, state: &mut AppState) {
    let Some(picker) = state.picker.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Up => picker.prev(),
        KeyCode::Down => picker.next(),
        KeyCode::Enter => {
            let name = picker.selected_name().to_string();
            state.picker = None;
            activate_profile(state, name);
        }
        // Plain Esc dismisses; a modified Esc is swallowed like any
        // other key — a chord with modifiers never types into the PTY
        // mid-pick.
        KeyCode::Esc if key.modifiers.is_empty() => {
            state.picker = None;
        }
        _ => {}
    }
}

/// Arm a newly chosen execution profile (Spec R07 §5.7): the name is
/// the active stance from the NEXT `AgentRequest`/`SuggestRequest`
/// onward — nothing persists it, nothing rewrites an in-flight
/// request — and a real change confirms itself with the spec's
/// *profile switched* toast (us-23). Re-arming the already-active
/// name — the picker's Enter on the highlighted row — switches
/// nothing, so it stays silent.
fn activate_profile(state: &mut AppState, name: String) {
    if name == state.profile {
        return;
    }
    state.toast = Some(Toast::new(
        format!("execution profile switched to \"{name}\""),
        Instant::now(),
    ));
    state.profile = name;
}

/// The last closed tab's exit status as the process exit code.
fn exit_code(status: Option<&ExitStatus>) -> ExitCode {
    match status {
        Some(status) => ExitCode::from(u8::try_from(status.code).unwrap_or(u8::MAX)),
        None => ExitCode::SUCCESS,
    }
}

#[cfg(test)]
mod tests {
    //! Dispatch tests: `handle_event` is where the keymap's tab actions
    //! meet the [`TabManager`]. Real `/bin/sh`-class sessions keep the
    //! wiring honest — spawning tiny PTYs needs no host TTY.

    use super::*;
    use crate::tabs::SessionTemplate;
    use ait_agent::SuggestEvent;
    use ait_term::{LiveSessionOptions, Size};
    use crossterm::event::{
        KeyCode, KeyEvent as CtKeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
    };
    use std::collections::VecDeque;

    fn manager() -> TabManager {
        TabManager::spawn_with(Size::new(40, 10), SessionTemplate::default()).unwrap()
    }

    /// A manager on `/bin/sh` — deterministic tty echo for the trigger
    /// tests (`$SHELL` resolves interactive bash on some hosts, whose
    /// readline prompt timing makes echo waits flaky; the tabs tests
    /// spawn `/bin/sh` for the same reason).
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

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(CtKeyEvent::new(code, modifiers))
    }

    /// The idle-repaint guard: alacritty always damages the cursor's own
    /// cell, so a naive `is_empty` check redraws every loop tick. Only
    /// damage beyond that exact cell counts — a cursor move (old + new
    /// cells) or any real content change still renders.
    #[test]
    fn cursor_only_damage_is_not_a_repaint() {
        use ait_term::DamageBounds;
        let cursor = Cursor {
            line: 5,
            column: 10,
            shape: CursorShape::Block,
        };
        let bound = |line, left, right| DamageBounds { line, left, right };

        assert!(!damaged_beyond_cursor(&Damage::Partial(vec![]), cursor));
        assert!(!damaged_beyond_cursor(
            &Damage::Partial(vec![bound(5, 10, 10)]),
            cursor
        ));
        // A second damaged cell (the cursor's old spot, or real content).
        assert!(damaged_beyond_cursor(
            &Damage::Partial(vec![bound(5, 10, 10), bound(3, 0, 0)]),
            cursor
        ));
        // A range covering the cursor plus neighbors is real damage.
        assert!(damaged_beyond_cursor(
            &Damage::Partial(vec![bound(5, 8, 12)]),
            cursor
        ));
        assert!(damaged_beyond_cursor(&Damage::Full, cursor));
    }

    struct Harness {
        tabs: TabManager,
        state: AppState,
        sink: Vec<u8>,
    }

    impl Harness {
        fn new(tabs: TabManager) -> Self {
            Self {
                tabs,
                state: AppState {
                    screen: Rect::new(0, 0, 80, 24),
                    copied_text: None,
                    last_exit: None,
                    toast: None,
                    config: LiveSettings::adopt(Settings::default()),
                    // `NoSecrets` keeps keyring lookups out of tests —
                    // resolution never blocks on the Secret Service.
                    suggester: CommandSuggester::with_secret_store(config::NoSecrets),
                    suggestion: SuggestionPin::new(),
                    profile: profile::DEFAULT_PROFILE.to_string(),
                    picker: None,
                },
                sink: Vec::new(),
            }
        }

        fn handle(&mut self, event: Event) -> Handled {
            handle_event(
                event,
                &mut self.tabs,
                &mut self.state,
                &mut self.sink,
                CapabilityTier::Legacy,
            )
        }
    }

    /// `Alt+T` spawns a second tab and activates it.
    #[test]
    fn alt_t_spawns_and_activates() {
        let mut harness = Harness::new(manager());

        let handled = harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));

        assert_eq!(handled, Handled::Continue);
        assert_eq!(harness.tabs.len(), 2);
        assert_eq!(harness.tabs.active_index(), 1);
    }

    /// `Alt+W` closes the active tab; on the last one it reports `Quit`
    /// — and leaves the empty set for the loop's guard, never indexed.
    #[test]
    fn alt_w_closes_and_quits_on_the_last_tab() {
        let mut harness = Harness::new(manager());
        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));

        assert_eq!(
            harness.handle(key(KeyCode::Char('w'), KeyModifiers::ALT)),
            Handled::Continue,
            "one tab remains"
        );
        assert_eq!(harness.tabs.len(), 1);
        assert_eq!(
            harness.handle(key(KeyCode::Char('w'), KeyModifiers::ALT)),
            Handled::Quit,
            "closing the last tab exits ait"
        );
        assert!(harness.tabs.is_empty());
    }

    /// `Ctrl+PageDown`/`Ctrl+PageUp` cycle with wraparound; `Alt+N`
    /// jumps; `Alt+Q` quits regardless of the tab count.
    #[test]
    fn switch_jump_and_quit_dispatch() {
        let mut harness = Harness::new(manager());
        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));
        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));
        assert_eq!(harness.tabs.len(), 3);

        harness.handle(key(KeyCode::PageDown, KeyModifiers::CONTROL));
        assert_eq!(harness.tabs.active_index(), 0, "wrapped");
        harness.handle(key(KeyCode::PageUp, KeyModifiers::CONTROL));
        assert_eq!(harness.tabs.active_index(), 2);
        harness.handle(key(KeyCode::Char('2'), KeyModifiers::ALT));
        assert_eq!(harness.tabs.active_index(), 1);
        harness.handle(key(KeyCode::Char('9'), KeyModifiers::ALT));
        assert_eq!(harness.tabs.active_index(), 1, "inert out-of-range jump");

        assert_eq!(
            harness.handle(key(KeyCode::Char('q'), KeyModifiers::ALT)),
            Handled::Quit
        );
        // Quit leaves the set alone — the manager's Drop kills the
        // children.
        assert_eq!(harness.tabs.len(), 3);
    }

    /// A resize event reaches every tab's PTY — and it is the CONTENT
    /// size (window minus the two chrome rows), never the full window.
    #[test]
    fn resize_event_resizes_every_tab_to_the_content_area() {
        let mut harness = Harness::new(manager());
        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));

        harness.handle(Event::Resize(100, 30));

        for tab in harness.tabs.tabs() {
            assert_eq!(
                tab.session.size(),
                Size::new(100, 28),
                "window minus tab bar + status row"
            );
        }
        // New tabs spawned after the resize get the content size too.
        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));
        assert_eq!(
            harness.tabs.active().unwrap().session.size(),
            Size::new(100, 28)
        );
    }

    /// A press that resolves `PassThrough` snaps the active tab's view
    /// to the bottom — per-tab scroll state survives the switch.
    #[test]
    fn typing_snaps_only_the_active_tab() {
        let mut harness = Harness::new(manager());
        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));
        // Scroll tab 2 up, switch back to tab 1, type there.
        {
            let tab = harness.tabs.active_mut().unwrap();
            // `pub(crate)` seam: stage an offset the fresh session's
            // empty scrollback couldn't produce via `scroll_up`.
            tab.scroll.scroll_up(3, 10);
        }
        harness.handle(key(KeyCode::PageUp, KeyModifiers::CONTROL));
        assert_eq!(harness.tabs.active_index(), 0);
        harness.handle(key(KeyCode::Char('x'), KeyModifiers::NONE));

        assert_eq!(
            harness.tabs.tabs()[0].scroll_offset(),
            0,
            "typed tab snapped"
        );
        assert_eq!(
            harness.tabs.tabs()[1].scroll_offset(),
            3,
            "the background tab kept its offset"
        );
    }

    /// A successful `Alt+T` shows no toast — the status line keeps its
    /// `cwd · shell · model · state` summary.
    #[test]
    fn successful_new_tab_shows_no_toast() {
        let mut harness = Harness::new(manager());

        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));

        assert_eq!(harness.tabs.len(), 2);
        assert!(
            harness.state.toast.is_none(),
            "a successful spawn is silent"
        );
    }

    /// The release point belongs to the selection: hosts coalesce
    /// motion reports, so the last `Drag` can lag the cell the button
    /// was released on — a quick flick released past the last reported
    /// drag must still copy through the release cell.
    #[test]
    fn release_extends_the_selection_to_the_release_cell() {
        use crossterm::event::MouseEvent;
        use std::time::Instant;

        let mut harness = Harness::new(manager());
        // A marker the whole grid knows: the sh output `HELLOWORLD` on
        // its own line. Wait for it, then find its grid line.
        // Same gate `run_line` uses: the first `send_bytes` must wait
        // for the prompt or the kernel echo races it onto the marker's
        // row (deterministic failure on fast machines). `logical` is
        // NOT the gate: a prompt that ends exactly at the right margin
        // leaves the cursor on a fresh blank row below it, so the
        // cursor's logical line stays empty even though the prompt
        // already rendered — any non-empty row means the prompt landed.
        wait_until(&mut harness, |h| {
            h.tabs
                .active()
                .unwrap()
                .grid()
                .visible_rows()
                .any(|row| !row.text().trim_end().is_empty())
        });
        harness
            .tabs
            .active()
            .unwrap()
            .session
            .send_bytes(b"printf 'HELLOWORLD\\n'\n")
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let line = loop {
            let tab = harness.tabs.active().unwrap();
            let grid = tab.session.grid();
            let found = (0..grid.lines()).find(|&line| {
                grid.line(line as i32)
                    .is_some_and(|row| row.text().starts_with("HELLOWORLD"))
            });
            if let Some(line) = found {
                break line;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for the marker"
            );
            harness.tabs.drain_events();
            std::thread::sleep(Duration::from_millis(5));
        };
        // Grid line `l` paints at screen row `l + content.y` (the tab
        // bar sits at row 0).
        let row = (line + 1) as u16;
        let mouse = |kind, column| {
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };

        // Down on 'H', a drag report at 'O' (col 4), released on the
        // final 'D' (col 9): the copy must reach the release cell.
        harness.handle(mouse(MouseEventKind::Down(MouseButton::Left), 0));
        harness.handle(mouse(MouseEventKind::Drag(MouseButton::Left), 4));
        harness.handle(mouse(MouseEventKind::Up(MouseButton::Left), 9));

        assert_eq!(
            harness.state.copied_text.as_deref(),
            Some("HELLOWORLD"),
            "the release point extends the copied range"
        );
        // …and the copy went out to the host clipboard via OSC 52.
        assert!(
            harness.sink.windows(5).any(|w| w == b"\x1b]52;"),
            "OSC 52 should reach the host stream"
        );
    }

    /// A key `Release` is not a keypress: it sends no bytes — and it
    /// also clears no selection and snaps no scroll. Hosts should never
    /// report releases (ait never pushes REPORT_EVENT_TYPES), but a
    /// release that does arrive must not eat the user's highlight.
    #[test]
    fn key_release_changes_nothing() {
        let mut harness = Harness::new(manager());
        {
            let tab = harness.tabs.active_mut().unwrap();
            tab.scroll.scroll_up(3, 10);
            tab.begin_selection(GridPoint::new(0, 0));
        }
        let release = Event::Key(CtKeyEvent::new_with_kind_and_state(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
            KeyEventState::NONE,
        ));

        assert_eq!(harness.handle(release), Handled::Continue);

        let tab = harness.tabs.active().unwrap();
        assert_eq!(tab.scroll_offset(), 3, "a release never snaps the view");
        assert!(
            tab.selection_range().is_some(),
            "a release never drops the highlight"
        );
    }

    /// A manager whose single `/bin/sh -c` tab emits two complete
    /// `A B C D` quadruples with scrollback padding between them —
    /// the block-UX dispatch tests' fixture (the same scripted-marks
    /// seam `tabs::tests` drives).
    fn block_manager() -> TabManager {
        TabManager::spawn_with(
            Size::new(40, 10),
            SessionTemplate {
                options: LiveSessionOptions {
                    program: Some(std::ffi::OsString::from("/bin/sh")),
                    args: vec![
                        std::ffi::OsString::from("-c"),
                        std::ffi::OsString::from(
                            "printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
                             printf 'first'; sleep 0.2; \
                             printf '\\n\\033]133;C\\007'; sleep 0.2; \
                             printf 'out1\\n'; sleep 0.2; \
                             printf '\\033]133;D;0\\007\\n'; sleep 0.2; \
                             i=0; while [ $i -lt 14 ]; do echo pad$i; i=$((i+1)); done; \
                             sleep 0.2; \
                             printf '\\033]133;A\\007$ \\033]133;B\\007'; sleep 0.2; \
                             printf 'second'; sleep 0.2; \
                             printf '\\n\\033]133;C\\007'; sleep 0.2; \
                             printf 'out2\\n'; sleep 0.2; \
                             printf '\\033]133;D;7\\007'; sleep 0.5",
                        ),
                    ],
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// Wait until the fixture's two blocks completed and scrollback
    /// exists to navigate.
    fn wait_for_blocks(harness: &mut Harness) {
        wait_until(harness, |h| {
            let Some(tab) = h.tabs.active() else {
                return false;
            };
            tab.blocks().iter().filter(|b| b.completed()).count() == 2 && tab.scrollback() > 0
        });
    }

    /// Spec R05 / issue #74: `Alt+↑`/`Alt+↓` dispatch as per-tab
    /// actions and scroll the viewport between block starts — up
    /// lands on the newest start above the view's top row, down walks
    /// back toward the live viewport.
    #[test]
    fn block_navigation_chords_scroll_to_block_starts() {
        let mut harness = Harness::new(block_manager());
        wait_for_blocks(&mut harness);

        let tab = harness.tabs.active().unwrap();
        let scrollback = tab.scrollback();
        let starts: Vec<usize> = tab
            .blocks()
            .iter()
            .map(|block| *block.range().start())
            .collect();
        let target = starts
            .iter()
            .copied()
            .filter(|start| *start < scrollback)
            .max()
            .expect("the first block sits in scrollback");

        assert_eq!(
            harness.handle(key(KeyCode::Up, KeyModifiers::ALT)),
            Handled::Continue
        );
        assert_eq!(
            harness.tabs.active().unwrap().scroll_offset(),
            scrollback - target,
            "Alt+Up tops the view on the previous block's start"
        );

        let next = starts
            .iter()
            .copied()
            .filter(|start| *start > target)
            .min()
            .expect("the second block is below");
        harness.handle(key(KeyCode::Down, KeyModifiers::ALT));
        assert_eq!(
            harness.tabs.active().unwrap().scroll_offset(),
            scrollback.saturating_sub(next),
            "Alt+Down walks to the next block start"
        );
    }

    /// `Alt+B` (Spec R05 us-17/18): the current block — the one the
    /// view's bottom row sits in — becomes a real [`Selection`], and
    /// its text copies through the same OSC-52 path a drag release
    /// uses: `copied_text` set for `Shift+Insert`, the OSC 52 write on
    /// the host stream. Scrolled up on the first block, that's the
    /// block that gets selected.
    #[test]
    fn alt_b_selects_and_copies_the_current_block() {
        let mut harness = Harness::new(block_manager());
        wait_for_blocks(&mut harness);

        // Park the view on the first block — "current" is then the
        // block the view's bottom row sits in.
        harness.handle(key(KeyCode::Up, KeyModifiers::ALT));
        harness.handle(key(KeyCode::Char('b'), KeyModifiers::ALT));

        let tab = harness.tabs.active().unwrap();
        let range = tab
            .selection_range()
            .expect("Alt+B programs a real selection");
        let first = &tab.blocks()[0];
        let scrollback = i32::try_from(tab.scrollback()).unwrap();
        assert_eq!(range.start.line, *first.range().start() as i32 - scrollback);
        assert_eq!(range.end.line, *first.range().end() as i32 - scrollback);

        let copied = harness.state.copied_text.as_deref().unwrap_or_default();
        assert!(copied.contains("first"), "the echo row: {copied:?}");
        assert!(copied.contains("out1"), "the output row: {copied:?}");
        assert!(
            harness.sink.windows(5).any(|w| w == b"\x1b]52;"),
            "the copy went out via OSC 52"
        );
    }

    /// No blocks → the chords are inert: nothing selected, nothing
    /// copied, no panic (§5.5 graceful degrade).
    #[test]
    fn block_chords_are_inert_without_blocks() {
        let mut harness = Harness::new(sh_manager());

        harness.handle(key(KeyCode::Char('b'), KeyModifiers::ALT));
        harness.handle(key(KeyCode::Up, KeyModifiers::ALT));
        harness.handle(key(KeyCode::Down, KeyModifiers::ALT));

        let tab = harness.tabs.active().unwrap();
        assert!(tab.selection_range().is_none());
        assert_eq!(tab.scroll_offset(), 0);
        assert!(harness.state.copied_text.is_none());
    }

    /// The input seam: a `poll`/`read` pair that always fails reports
    /// `Failed`, so the loop's bounded retry turns a permanently dead
    /// input path (stdin EOF, a source that never initialized) into the
    /// spec's clean exit instead of an unhandled `Err`.
    #[test]
    fn dead_input_path_reports_failed() {
        let mut harness = Harness::new(manager());

        // A source that never initialized errors on every poll.
        let outcome = service_input(
            &mut harness.tabs,
            &mut harness.state,
            &mut harness.sink,
            CapabilityTier::Legacy,
            &mut |_| Err(io::Error::other("no input source")),
            &mut || unreachable!("poll said nothing to read"),
        );
        assert_eq!(outcome, InputOutcome::Failed);

        // A read-side failure mid-drain reports Failed the same way.
        let outcome = service_input(
            &mut harness.tabs,
            &mut harness.state,
            &mut harness.sink,
            CapabilityTier::Legacy,
            &mut |_| Ok(true),
            &mut || Err(io::Error::other("input stream died")),
        );
        assert_eq!(outcome, InputOutcome::Failed);
    }

    /// Queued input is drained completely: every event behind the first
    /// is serviced before the call returns (a resize proves the second
    /// event was handled), and a bound quit propagates `Quit`.
    #[test]
    fn queued_input_drains_and_quit_propagates() {
        let mut harness = Harness::new(manager());
        // `poll` reports readable while the queue has events — the
        // first (timed) wait and the zero-time drain use it alike.
        let events = RefCell::new(VecDeque::from([
            Event::Resize(100, 30),
            Event::Resize(120, 40),
        ]));
        let outcome = service_input(
            &mut harness.tabs,
            &mut harness.state,
            &mut harness.sink,
            CapabilityTier::Legacy,
            &mut |_| Ok(!events.borrow().is_empty()),
            &mut || Ok(events.borrow_mut().pop_front().unwrap()),
        );
        assert_eq!(outcome, InputOutcome::Continue);
        assert_eq!(
            harness.state.screen,
            Rect::new(0, 0, 120, 40),
            "both queued resizes were handled"
        );

        let events = RefCell::new(VecDeque::from([key(KeyCode::Char('q'), KeyModifiers::ALT)]));
        let outcome = service_input(
            &mut harness.tabs,
            &mut harness.state,
            &mut harness.sink,
            CapabilityTier::Legacy,
            &mut |_| Ok(!events.borrow().is_empty()),
            &mut || Ok(events.borrow_mut().pop_front().unwrap()),
        );
        assert_eq!(outcome, InputOutcome::Quit);
    }

    /// A flood past [`INPUT_DRAIN_LIMIT`] yields the call instead of
    /// draining forever — the loop gets its turn to drain sessions and
    /// repaint, and the leftover events are serviced next tick.
    #[test]
    fn input_flood_yields_at_the_drain_limit() {
        let mut harness = Harness::new(manager());
        let events = RefCell::new(VecDeque::from_iter(
            std::iter::repeat_with(|| key(KeyCode::Char('x'), KeyModifiers::NONE))
                .take(INPUT_DRAIN_LIMIT as usize + 10),
        ));
        let outcome = service_input(
            &mut harness.tabs,
            &mut harness.state,
            &mut harness.sink,
            CapabilityTier::Legacy,
            &mut |_| Ok(!events.borrow().is_empty()),
            &mut || Ok(events.borrow_mut().pop_front().unwrap()),
        );
        assert_eq!(outcome, InputOutcome::Continue);
        assert_eq!(
            events.borrow().len(),
            10,
            "the cap released control with the tail still queued"
        );
    }

    /// Drain sessions, suggester and agent events until `pred` holds,
    /// or fail after a generous bound — mirrors `tabs::tests`'
    /// `drain_until`.
    fn wait_until(harness: &mut Harness, pred: impl Fn(&Harness) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !pred(harness) {
            harness.tabs.drain_events();
            harness.tabs.drain_agent_events();
            harness.state.suggestion.drain(
                harness.state.suggester.events(),
                &harness.tabs,
                &mut harness.state.toast,
            );
            assert!(Instant::now() < deadline, "timed out waiting");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The logical input line at the active tab's cursor.
    fn logical(harness: &Harness) -> String {
        let tab = harness.tabs.active().unwrap();
        suggest::logical_line(&tab.grid(), tab.cursor())
    }

    /// `/bin/sh` on a PTY is interactive — it prints a prompt that can
    /// race the kernel echo of typed keys (echo first would bury the
    /// typed text mid-line and `ends_with` could never hold). Wait for
    /// the prompt to exist BEFORE typing: the echo then lands after it
    /// and the shadow's text is the line's tail, deterministically.
    fn type_keys(harness: &mut Harness, text: &str) {
        wait_until(harness, |h| !logical(h).is_empty());
        for c in text.chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    /// The `# `+Enter trigger end to end through `handle_event` (Spec
    /// R04, us-1/2/14): the shadow built by typed keys verifies against
    /// the PTY's own echo, the request fires into the suggester pinned
    /// to its tab — and Enter is forwarded normally, so the comment
    /// lands in the shell's line discipline like any input. The
    /// unconfigured default settings answer `Error`, which the tick
    /// drain surfaces as a toast and clears the pin.
    #[test]
    fn hash_enter_fires_request_and_forwards_enter() {
        let mut harness = Harness::new(sh_manager());
        type_keys(&mut harness, "# ls");
        // The verification reads the grid — wait for the PTY echo.
        wait_until(&mut harness, |h| logical(h).ends_with("# ls"));
        assert!(!harness.tabs.active().unwrap().shadow.is_stale());

        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));

        // The verified request fired, pinned to tab 0's id (us-14).
        assert_eq!(
            harness.state.suggestion.pinned_id(),
            Some(harness.tabs.tabs()[0].id),
            "the '# ' request fired into the suggester"
        );
        // The shadow reset for the next line.
        assert_eq!(harness.tabs.active().unwrap().shadow.text(), "");

        // Enter reached the PTY normally: the kernel echoes the
        // submitted line and the cursor advances past it (us-2).
        wait_until(&mut harness, |h| h.tabs.active().unwrap().cursor().line > 0);

        // Default settings hold no model — the worker answers `Error`,
        // the drain toasts it and releases the pin (us-22).
        wait_until(&mut harness, |h| {
            h.state.toast.is_some() && h.state.suggestion.is_none()
        });
        assert!(
            harness
                .state
                .toast
                .as_ref()
                .unwrap()
                .text()
                .contains("profile"),
            "the toast names the model-resolution failure: {}",
            harness.state.toast.as_ref().unwrap().text()
        );
    }

    /// A non-`# ` line never fires: Enter forwards to the PTY, no pin
    /// is taken and no toast appears — the feature stays invisible
    /// (us-10).
    #[test]
    fn plain_enter_does_not_fire() {
        let mut harness = Harness::new(sh_manager());
        type_keys(&mut harness, "ls");
        wait_until(&mut harness, |h| logical(h).ends_with("ls"));

        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(harness.state.suggestion.is_none());
        wait_until(&mut harness, |h| h.tabs.active().unwrap().cursor().line > 0);
        assert!(harness.state.toast.is_none());
    }

    /// A desynced shadow cannot fire (us-13): `Up` (history recall)
    /// marks it stale, so the following Enter — still forwarded — puts
    /// nothing on the suggester.
    #[test]
    fn desynced_shadow_does_not_fire() {
        let mut harness = Harness::new(sh_manager());
        type_keys(&mut harness, "# ls");
        wait_until(&mut harness, |h| logical(h).ends_with("# ls"));

        harness.handle(key(KeyCode::Up, KeyModifiers::NONE));
        assert!(harness.tabs.active().unwrap().shadow.is_stale());

        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(
            harness.state.suggestion.is_none(),
            "stale shadow → no trigger"
        );
    }

    /// `Shift+Insert` pastes arbitrary bytes — the shadow cannot
    /// replay them, so it goes stale until the next submission.
    #[test]
    fn paste_marks_the_shadow_stale() {
        let mut harness = Harness::new(sh_manager());
        for c in "# l".chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(!harness.tabs.active().unwrap().shadow.is_stale());

        harness.state.copied_text = Some("s".to_string());
        harness.handle(key(KeyCode::Insert, KeyModifiers::SHIFT));

        assert!(harness.tabs.active().unwrap().shadow.is_stale());
    }

    /// The shadow tracks editing honestly through `handle_event`:
    /// typed chars append, Backspace pops, a submission resets.
    #[test]
    fn shadow_tracks_editing_through_the_key_path() {
        let mut harness = Harness::new(sh_manager());
        for c in "# abc".chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(harness.tabs.active().unwrap().shadow.text(), "# abc");

        harness.handle(key(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(harness.tabs.active().unwrap().shadow.text(), "# ab");

        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(harness.tabs.active().unwrap().shadow.text(), "");
    }

    /// Spec R03 stories 10–11 and 14: a good watcher event swaps the
    /// live settings — the model slot and the NEW-session `[terminal]`
    /// template both follow it — while a broken save toasts the
    /// `ConfigError` and keeps last-good.
    #[test]
    fn config_events_swap_last_good_or_toast() {
        let mut harness = Harness::new(manager());
        let (tx, rx) = std::sync::mpsc::channel();

        let good = Settings::from_toml(
            r#"
[terminal]
shell = "/bin/cat"

[agents.custom_endpoints.local]
name     = "Local"
base_url = "http://localhost:11434/v1"
schema   = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name       = "wire-name"
alias      = "Alias Name"
config_key = "key-1"

[agents.execution_profiles.default]
base_model = "key-1"
"#,
        )
        .unwrap();

        tx.send(Ok(good)).unwrap();
        drain_config_events(&rx, &mut harness.state, &mut harness.tabs);

        assert_eq!(
            harness.state.config.model("default"),
            "Alias Name",
            "the status slot follows the swap (story 10)"
        );
        // The `[terminal]` half lands on the template: the NEXT tab
        // spawns the configured shell; the open one is untouched.
        harness.tabs.new_tab().unwrap();
        assert_eq!(
            harness.tabs.active().unwrap().title(),
            "cat",
            "the shell override applies to new sessions (story 12)"
        );

        let broken = Settings::from_toml("[terminal]\nbogus = 1\n").unwrap_err();
        tx.send(Err(broken)).unwrap();
        drain_config_events(&rx, &mut harness.state, &mut harness.tabs);

        let toast = harness.state.toast.as_ref().expect("a bad save toasts");
        assert!(
            toast.text().contains("terminal.bogus"),
            "the toast names the offending key: {}",
            toast.text()
        );
        assert_eq!(
            harness.state.config.model("default"),
            "Alias Name",
            "last-good settings are kept (story 11)"
        );
    }

    // ---- Spec R07 §5.7 / issue #101: the active execution profile ----

    /// `Alt+P` with fewer than two declared profiles leaves the armed
    /// `default` alone and says so — the toast is the whole action,
    /// no picker opens (us-23).
    #[test]
    fn alt_p_toasts_when_there_is_nothing_to_switch_to() {
        // `Settings::default()` declares no `execution_profiles`.
        let mut harness = Harness::new(manager());

        harness.handle(key(KeyCode::Char('p'), KeyModifiers::ALT));

        assert_eq!(harness.state.profile, "default");
        assert!(harness.state.picker.is_none());
        let toast = harness
            .state
            .toast
            .as_ref()
            .expect("the small count toasts");
        assert!(
            toast.text().contains("no other execution profiles"),
            "the toast says why: {}",
            toast.text()
        );
    }

    /// Exactly two declared profiles: `Alt+P` arms the other one
    /// directly — no picker ever appears — and pressing again cycles
    /// straight back (us-23). Each real switch confirms itself with
    /// the spec's *profile switched* toast.
    #[test]
    fn alt_p_cycles_between_two_profiles() {
        let mut harness = Harness::new(manager());
        harness.state.config = LiveSettings::adopt(
            Settings::from_toml(
                "[agents.execution_profiles.default]\n\
                 [agents.execution_profiles.work]\n",
            )
            .unwrap(),
        );

        harness.handle(key(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(harness.state.profile, "work");
        assert!(harness.state.picker.is_none(), "two profiles never pick");
        let toast = harness
            .state
            .toast
            .as_ref()
            .expect("a real switch toasts");
        assert!(
            toast.text().contains("\"work\""),
            "the toast names the armed profile: {}",
            toast.text()
        );

        harness.handle(key(KeyCode::Char('p'), KeyModifiers::ALT));
        assert_eq!(harness.state.profile, "default");
        assert!(harness.state.picker.is_none());
        let toast = harness
            .state
            .toast
            .as_ref()
            .expect("cycling back toasts too");
        assert!(
            toast.text().contains("\"default\""),
            "the toast names the armed profile: {}",
            toast.text()
        );
    }

    /// Three or more open the picker (us-23): the armed name starts
    /// highlighted, ↑/↓ move over the declared list, Enter arms the
    /// highlighted name and closes, plain Esc dismisses without
    /// switching, a stray character is swallowed rather than typed,
    /// and `Alt+P` itself toggles the picker away.
    #[test]
    fn alt_p_picks_from_three_or_more() {
        let mut harness = Harness::new(manager());
        harness.state.config = LiveSettings::adopt(
            Settings::from_toml(
                "[agents.execution_profiles.default]\n\
                 [agents.execution_profiles.scratch]\n\
                 [agents.execution_profiles.work]\n",
            )
            .unwrap(),
        );

        harness.handle(key(KeyCode::Char('p'), KeyModifiers::ALT));
        let picker = harness
            .state
            .picker
            .as_ref()
            .expect("three or more open the picker");
        assert_eq!(picker.names(), &["default", "scratch", "work"]);
        assert_eq!(
            picker.selected_name(),
            "default",
            "the armed name starts highlighted"
        );

        // ↓ moves the highlight; a stray character reaches neither
        // the armed name nor the PTY — the pass-through arm routed
        // it to the picker.
        harness.handle(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(
            harness.state.picker.as_ref().unwrap().selected_name(),
            "scratch"
        );
        harness.handle(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(harness.state.picker.is_some());
        assert_eq!(harness.state.profile, "default");

        // Plain Esc dismisses without switching.
        harness.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(harness.state.picker.is_none());
        assert_eq!(harness.state.profile, "default");

        // Enter arms the highlighted name and closes — the switch
        // toasts the armed name.
        harness.handle(key(KeyCode::Char('p'), KeyModifiers::ALT));
        harness.handle(key(KeyCode::Down, KeyModifiers::NONE));
        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(harness.state.picker.is_none());
        assert_eq!(harness.state.profile, "scratch");
        let toast = harness
            .state
            .toast
            .as_ref()
            .expect("the picker's switch toasts");
        assert!(
            toast.text().contains("\"scratch\""),
            "the toast names the armed profile: {}",
            toast.text()
        );

        // `Alt+P` while open toggles the picker away — like `Alt+A`
        // closes the focused panel.
        harness.handle(key(KeyCode::Char('p'), KeyModifiers::ALT));
        assert!(harness.state.picker.is_some());
        harness.handle(key(KeyCode::Char('p'), KeyModifiers::ALT));
        assert!(harness.state.picker.is_none());
        assert_eq!(
            harness.state.profile, "scratch",
            "the toggle changes nothing"
        );

        // Enter on the already-armed row switches nothing, so no
        // fresh toast fires.
        harness.state.toast = None;
        harness.handle(key(KeyCode::Char('p'), KeyModifiers::ALT));
        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(harness.state.profile, "scratch");
        assert!(
            harness.state.toast.is_none(),
            "re-arming the armed name stays silent"
        );
    }

    /// A reload that drops or renames the armed profile toasts its
    /// name and keeps the selection armed (us-24/us-28): the worker's
    /// missing-profile fail-safe — ask everything — is what the name
    /// resolves to, so the toast is the frontend's whole part of the
    /// story. An open picker's stale list drops with the old file.
    #[test]
    fn a_reload_that_drops_the_active_profile_toasts_its_name() {
        let mut harness = Harness::new(manager());
        let (tx, rx) = std::sync::mpsc::channel();
        harness.state.config = LiveSettings::adopt(
            Settings::from_toml(
                "[agents.execution_profiles.default]\n\
                 [agents.execution_profiles.scratch]\n\
                 [agents.execution_profiles.work]\n",
            )
            .unwrap(),
        );
        harness.state.profile = "work".to_string();
        harness.handle(key(KeyCode::Char('p'), KeyModifiers::ALT));
        assert!(harness.state.picker.is_some());

        let renamed = Settings::from_toml(
            "[agents.execution_profiles.default]\n\
             [agents.execution_profiles.scratch]\n",
        )
        .unwrap();
        tx.send(Ok(renamed)).unwrap();
        drain_config_events(&rx, &mut harness.state, &mut harness.tabs);

        assert_eq!(
            harness.state.profile, "work",
            "the selection is never rewritten — the name stays armed"
        );
        assert!(
            harness.state.picker.is_none(),
            "the stale list dropped with the old settings"
        );
        let toast = harness
            .state
            .toast
            .as_ref()
            .expect("the vanished profile toasts");
        assert!(
            toast.text().contains("\"work\""),
            "the toast names the missing profile: {}",
            toast.text()
        );
    }

    /// The whole-table deletion under an armed `default` (us-28): a
    /// `default` the OLD file declared is gone from the new one — a
    /// deletion the new file alone cannot distinguish from
    /// never-declared, so the drain reads the old settings before
    /// adopting. A `default` that was never declared stays silent:
    /// armed-but-never-existed is not a deletion.
    #[test]
    fn a_reload_deleting_a_declared_default_toasts() {
        let mut harness = Harness::new(manager());
        let (tx, rx) = std::sync::mpsc::channel();
        harness.state.config = LiveSettings::adopt(
            Settings::from_toml("[agents.execution_profiles.default]\n").unwrap(),
        );

        tx.send(Ok(Settings::default())).unwrap();
        drain_config_events(&rx, &mut harness.state, &mut harness.tabs);

        assert_eq!(
            harness.state.profile, "default",
            "the selection is never rewritten"
        );
        let toast = harness
            .state
            .toast
            .as_ref()
            .expect("a declared default deleted with the table toasts");
        assert!(
            toast.text().contains("\"default\""),
            "the toast names the missing profile: {}",
            toast.text()
        );

        // The same empty-table reload against a file that never
        // declared `default`: no profile was ever lost, so no toast.
        let mut harness = Harness::new(manager());
        tx.send(Ok(Settings::default())).unwrap();
        drain_config_events(&rx, &mut harness.state, &mut harness.tabs);
        assert!(
            harness.state.toast.is_none(),
            "a never-declared default is the profile-less steady state"
        );
    }

    // ---- Spec R04 T5: streaming insertion + in-flight keys ----
    //
    // The insertion seam under test is byte-exact: the pinned tab runs
    // `stty raw; head -c N > <file>`, so the file ends up holding the
    // exact byte stream the PTY received — bracketed-paste markers,
    // `Ctrl+U` and all. `SuggestEvent`s are injected through
    // `SuggestionPin::apply` (the seam `drain` uses) instead of a real
    // model — no network, standard 11.

    /// A unique temp path for one test's capture file.
    fn capture_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ait-t5-{name}-{}", std::process::id()))
    }

    /// Run `line` on the active tab's shell once its prompt exists —
    /// the shell must be up before `stty`/`head` can be started.
    fn run_line(harness: &mut Harness, line: &str) {
        wait_until(harness, |h| !logical(h).is_empty());
        harness
            .tabs
            .active()
            .unwrap()
            .session
            .send_bytes(format!("{line}\n").as_bytes())
            .unwrap();
    }

    /// Run `line` and wait until `path` exists — the `> path`
    /// redirect creates it the moment `head` starts, which proves the
    /// preceding `stty raw` already applied: without that sync, bytes
    /// sent early hit the still-canonical tty and ICRNL rewrites
    /// `\r` to `\n` before `head` ever sees them.
    fn run_capture(harness: &mut Harness, line: &str, path: &std::path::Path) {
        run_line(harness, line);
        wait_until(harness, |_h| path.exists());
    }

    /// The state a fired `SuggestRequest` leaves behind — a pin on
    /// `tab`, re-resolved through the tab's stable id.
    fn pin_suggestion(harness: &mut Harness, tab: usize) {
        harness.state.suggestion = SuggestionPin::staged(harness.tabs.tabs()[tab].id);
    }

    /// Inject one suggester event through the drain's seam.
    fn fire(harness: &mut Harness, event: SuggestEvent) {
        harness
            .state
            .suggestion
            .apply(&harness.tabs, event, &mut harness.state.toast);
    }

    /// Wait for the capture file to hold `len` bytes, then read it.
    fn wait_file(harness: &mut Harness, path: &std::path::Path, len: usize) -> Vec<u8> {
        wait_until(harness, |_h| {
            std::fs::read(path).is_ok_and(|bytes| bytes.len() >= len)
        });
        std::fs::read(path).unwrap()
    }

    /// us-3/4: a `Delta` sequence lands byte-for-byte as typed text on
    /// the pinned session — exactly the bytes a user would have typed
    /// (no newline can ever arrive, so nothing executes).
    #[test]
    fn deltas_type_byte_for_byte() {
        let path = capture_path("deltas");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &mut harness,
            &format!("stty raw; head -c 10 > {}", path.display()),
            &path,
        );
        pin_suggestion(&mut harness, 0);

        for delta in ["git ", "stat", "us"] {
            fire(&mut harness, SuggestEvent::Delta(delta.to_string()));
        }
        fire(&mut harness, SuggestEvent::Done);

        assert_eq!(wait_file(&mut harness, &path, 10), b"git status");
        assert!(harness.state.suggestion.is_none(), "Done released the pin");
        let _ = std::fs::remove_file(&path);
    }

    /// us-7: under mode 2004 the whole stream rides inside ONE
    /// `\x1b[200~`…`\x1b[201~` envelope — opened on the first `Delta`,
    /// closed on `Done` (the streaming counterpart of `encode_paste`).
    #[test]
    fn mode_2004_wraps_the_stream_in_one_envelope() {
        let path = capture_path("envelope");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &mut harness,
            &format!(
                "stty raw; printf '\\033[?2004h'; head -c 18 > {}",
                path.display()
            ),
            &path,
        );
        wait_until(&mut harness, |h| {
            h.tabs.active().unwrap().session.bracketed_paste()
        });
        pin_suggestion(&mut harness, 0);

        fire(&mut harness, SuggestEvent::Delta("abc".into()));
        fire(&mut harness, SuggestEvent::Delta("def".into()));
        fire(&mut harness, SuggestEvent::Done);

        assert_eq!(
            wait_file(&mut harness, &path, 18),
            b"\x1b[200~abcdef\x1b[201~",
            "one envelope around the whole stream, closed on Done"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// us-8: Esc on the pinned tab cancels — the session sees the
    /// envelope close and `Ctrl+U` (in that order) wiping the partial
    /// line. The pin survives until the worker's terminal event so a
    /// post-cancel `Delta` can't retype onto the wiped line and the
    /// cancellation `Error` arrives silently (no toast for the user's
    /// own abort).
    #[test]
    fn esc_cancels_closes_the_envelope_and_wipes_the_line() {
        let path = capture_path("esc");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &mut harness,
            &format!(
                "stty raw; printf '\\033[?2004h'; head -c 20 > {}",
                path.display()
            ),
            &path,
        );
        wait_until(&mut harness, |h| {
            h.tabs.active().unwrap().session.bracketed_paste()
        });
        pin_suggestion(&mut harness, 0);
        fire(&mut harness, SuggestEvent::Delta("partial".into()));
        // Shadow text a mid-flight keystroke could have left — the
        // Ctrl+U wipe makes it untrue, so the cancel resets it.
        harness
            .tabs
            .active_mut()
            .unwrap()
            .shadow
            .update(&ait_term::KeyEvent::new(
                ait_term::Key::Char('x'),
                ait_term::Modifiers::NONE,
            ));

        harness.handle(key(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(
            wait_file(&mut harness, &path, 20),
            b"\x1b[200~partial\x1b[201~\x15",
            "close the envelope, THEN wipe the line — spec order"
        );
        assert!(
            harness.state.suggestion.is_cancelled(),
            "the pin survives until the terminal event"
        );
        assert!(
            !harness.state.suggestion.paste_is_open(),
            "the envelope is closed"
        );
        assert!(
            harness.tabs.active().unwrap().shadow.text().is_empty(),
            "the wiped line resets the input shadow"
        );

        fire(&mut harness, SuggestEvent::Delta("z".into()));
        fire(&mut harness, SuggestEvent::Error("cancelled".into()));
        assert!(harness.state.suggestion.is_none());
        assert!(
            harness.state.toast.is_none(),
            "a user-initiated cancel is not toasted"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// us-9: Enter on the pinned tab never reaches the PTY mid-stream —
    /// not even `Ctrl+Enter`, which encodes `\r` and would execute the
    /// half-written command.
    #[test]
    fn enter_is_swallowed_while_a_suggestion_streams() {
        let path = capture_path("enter");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &mut harness,
            &format!("stty raw; head -c 3 > {}", path.display()),
            &path,
        );
        pin_suggestion(&mut harness, 0);
        fire(&mut harness, SuggestEvent::Delta("a".into()));

        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        harness.handle(key(KeyCode::Enter, KeyModifiers::CONTROL));
        assert!(
            harness.state.suggestion.is_some(),
            "Enter does not end the request"
        );

        fire(&mut harness, SuggestEvent::Delta("bc".into()));
        fire(&mut harness, SuggestEvent::Done);

        assert_eq!(
            wait_file(&mut harness, &path, 3),
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
            &mut harness,
            &format!("stty raw; head -c 2 > {}", path.display()),
            &path,
        );

        fire(&mut harness, SuggestEvent::Delta("nope".into()));
        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        harness.handle(key(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(wait_file(&mut harness, &path, 2), b"\r\x1b");
        let _ = std::fs::remove_file(&path);
    }

    /// us-14 + the in-flight key scope: a `Delta` types into the
    /// PINNED tab's session across a tab switch, while Esc and Enter
    /// on a different tab behave normally — they steer their own
    /// session, never the request.
    #[test]
    fn the_pin_survives_a_tab_switch_and_foreign_keys_stay_normal() {
        let f0 = capture_path("pin0");
        let f1 = capture_path("pin1");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &mut harness,
            &format!("stty raw; head -c 3 > {}", f0.display()),
            &f0,
        );
        pin_suggestion(&mut harness, 0);

        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));
        assert_eq!(harness.tabs.active_index(), 1);
        run_capture(
            &mut harness,
            &format!("stty raw; head -c 2 > {}", f1.display()),
            &f1,
        );
        fire(&mut harness, SuggestEvent::Delta("xyz".into()));

        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        harness.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(wait_file(&mut harness, &f1, 2), b"\r\x1b");
        assert!(
            harness.state.suggestion.is_some() && !harness.state.suggestion.is_cancelled(),
            "Esc on another tab never touches the request"
        );

        fire(&mut harness, SuggestEvent::Done);
        assert_eq!(
            wait_file(&mut harness, &f0, 3),
            b"xyz",
            "the stream stayed with its origin tab (us-14)"
        );
        let _ = std::fs::remove_file(&f0);
        let _ = std::fs::remove_file(&f1);
    }

    /// Deltas aimed at an exited session are dropped — never typed
    /// into another tab — while the pin stays until `Done`/`Error`.
    #[test]
    fn deltas_for_an_exited_session_are_dropped() {
        let f1 = capture_path("dead");
        let mut harness = Harness::new(sh_manager());
        pin_suggestion(&mut harness, 0);
        harness
            .tabs
            .active()
            .unwrap()
            .session
            .send_bytes(b"exit\n")
            .unwrap();
        wait_until(&mut harness, |h| h.tabs.tabs()[0].exit_status().is_some());

        // A second, live tab proves the drop isn't a redirect.
        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));
        run_capture(
            &mut harness,
            &format!("stty raw; head -c 1 > {}", f1.display()),
            &f1,
        );

        fire(&mut harness, SuggestEvent::Delta("z".into()));
        std::thread::sleep(Duration::from_millis(200));
        harness.tabs.drain_events();
        assert!(
            harness.state.suggestion.is_some(),
            "a dropped delta keeps the pin until the terminal event"
        );
        assert_eq!(
            std::fs::read(&f1).unwrap_or_default(),
            b"",
            "a dropped delta never lands on another tab"
        );

        fire(&mut harness, SuggestEvent::Done);
        assert!(harness.state.suggestion.is_none());
        let _ = std::fs::remove_file(&f1);
    }

    /// `Error` on a live request closes the envelope, toasts the §6
    /// degrade-gracefully message and releases the pin.
    #[test]
    fn error_closes_the_envelope_and_toasts() {
        let path = capture_path("error");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &mut harness,
            &format!(
                "stty raw; printf '\\033[?2004h'; head -c 15 > {}",
                path.display()
            ),
            &path,
        );
        wait_until(&mut harness, |h| {
            h.tabs.active().unwrap().session.bracketed_paste()
        });
        pin_suggestion(&mut harness, 0);

        fire(&mut harness, SuggestEvent::Delta("xyz".into()));
        fire(&mut harness, SuggestEvent::Error("boom".into()));

        assert_eq!(
            wait_file(&mut harness, &path, 15),
            b"\x1b[200~xyz\x1b[201~",
            "the envelope closes on Error too"
        );
        assert!(harness.state.suggestion.is_none());
        assert!(
            harness
                .state
                .toast
                .as_ref()
                .is_some_and(|t| t.text().contains("boom")),
            "the error surfaces as a status-line toast"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ---- Spec R04 T6: the Alt+Enter trigger ----
    //
    // Same trigger seam as T4 — a real `/bin/sh` echoes the typed line,
    // the shadow verifies against it — plus the us-12 erase: the grid
    // must return to the bare prompt before the stream types.

    /// us-11/12/14: `Alt+Enter` on a verified bare line fires the
    /// request pinned to its tab, erases the typed line via DELs (the
    /// grid returns to the bare prompt), resets the shadow, and the
    /// suggestion streams into the cleared input line.
    #[test]
    fn alt_enter_fires_erases_and_streams_into_the_cleared_line() {
        let mut harness = Harness::new(sh_manager());
        wait_until(&mut harness, |h| !logical(h).is_empty());
        let prompt = logical(&harness);
        type_keys(&mut harness, "git undo");
        wait_until(&mut harness, |h| logical(h).ends_with("git undo"));

        harness.handle(key(KeyCode::Enter, KeyModifiers::ALT));

        assert_eq!(
            harness.state.suggestion.pinned_id(),
            Some(harness.tabs.tabs()[0].id),
            "the bare line fired as a request, pinned to its tab"
        );
        let shadow = &harness.tabs.active().unwrap().shadow;
        assert_eq!(shadow.text(), "", "the erased line resets the shadow");
        assert!(!shadow.is_stale());

        // us-12: the typed line is gone from the grid — the request
        // text and the incoming command can never interleave.
        wait_until(&mut harness, |h| logical(h) == prompt);

        // The real request answers `Error` (default settings hold no
        // model) — drain it out of the channel so it can't race the
        // manual pin the stream seam is driven through below.
        wait_until(&mut harness, |h| h.state.suggestion.is_none());

        // The shared stream path types into the cleared line (tty
        // echo makes it visible on the grid).
        pin_suggestion(&mut harness, 0);
        fire(&mut harness, SuggestEvent::Delta("git reset HEAD~1".into()));
        wait_until(&mut harness, |h| logical(h).ends_with("git reset HEAD~1"));
        fire(&mut harness, SuggestEvent::Done);
        assert!(harness.state.suggestion.is_none());
    }

    /// us-13: a desynced shadow (history recall via `Up`) cannot fire
    /// — `Alt+Enter` leaves the line, the shadow and the suggester
    /// alone, exactly like the `# ` path's desync-is-inert rule.
    #[test]
    fn alt_enter_on_a_desynced_shadow_does_not_fire() {
        let mut harness = Harness::new(sh_manager());
        type_keys(&mut harness, "git undo");
        wait_until(&mut harness, |h| logical(h).ends_with("git undo"));
        harness.handle(key(KeyCode::Up, KeyModifiers::NONE));
        assert!(harness.tabs.active().unwrap().shadow.is_stale());

        harness.handle(key(KeyCode::Enter, KeyModifiers::ALT));

        assert!(
            harness.state.suggestion.is_none(),
            "stale shadow → no trigger"
        );
        assert!(harness.state.toast.is_none());
    }

    /// An empty input line fires nothing — no request, no erase, no
    /// toast. `Alt+Enter` on a bare prompt is inert.
    #[test]
    fn alt_enter_on_an_empty_line_does_not_fire() {
        let mut harness = Harness::new(sh_manager());
        wait_until(&mut harness, |h| !logical(h).is_empty());

        harness.handle(key(KeyCode::Enter, KeyModifiers::ALT));

        assert!(harness.state.suggestion.is_none());
        assert!(harness.state.toast.is_none());
    }

    /// One request at a time: a live pin gates `Alt+Enter` exactly
    /// like the `# ` trigger — the keypress does not stack a second
    /// request, does not erase the line, and does not toast.
    #[test]
    fn alt_enter_does_not_stack_while_a_request_streams() {
        let mut harness = Harness::new(sh_manager());
        type_keys(&mut harness, "git undo");
        wait_until(&mut harness, |h| logical(h).ends_with("git undo"));
        pin_suggestion(&mut harness, 0);

        harness.handle(key(KeyCode::Enter, KeyModifiers::ALT));

        assert!(
            harness.state.suggestion.is_some() && !harness.state.suggestion.is_cancelled(),
            "the in-flight pin is untouched"
        );
        assert!(
            logical(&harness).ends_with("git undo"),
            "nothing was erased"
        );
        // Let any wrongly-sent request surface its error before the
        // negative assertions.
        std::thread::sleep(Duration::from_millis(200));
        harness.state.suggestion.drain(
            harness.state.suggester.events(),
            &harness.tabs,
            &mut harness.state.toast,
        );
        assert!(harness.state.toast.is_none());
        assert!(
            harness
                .tabs
                .active()
                .unwrap()
                .shadow
                .text()
                .ends_with("git undo"),
            "the shadow kept its line"
        );
    }

    // ---- Spec R06 T2: the agent conversation panel (issue #85) ----
    //
    // The dispatch seam under test is `handle_event`: `Alt+A` toggles
    // the split (and the tab's winsize through the resize path), a
    // focused panel owns keystrokes before the PTY does, Enter submits
    // or steers, and Esc cancels or unfocuses. A real `AgentSession`
    // backs the panel — default `Settings` resolve no model, so the
    // worker's terminal `Error` lands as a conversation status line.

    /// The active tab's panel, staged or toggled live.
    fn panel(harness: &Harness) -> &crate::agent_panel::AgentPanel {
        harness
            .tabs
            .active()
            .unwrap()
            .agent_panel()
            .expect("a panel exists")
    }

    /// `Alt+A` opens the split focused and shrinks the session through
    /// the band→session derivation — the same math the resize path
    /// uses (`Size::new(40, 10)` band → `24` session columns at the
    /// two-fifths share).
    #[test]
    fn alt_a_opens_focused_and_shrinks_the_session() {
        let mut harness = Harness::new(manager());
        assert_eq!(
            harness.tabs.active().unwrap().session.size(),
            Size::new(40, 10)
        );

        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));

        let tab = harness.tabs.active().unwrap();
        assert!(tab.agent_panel_open() && tab.agent_focused());
        assert_eq!(tab.session.size(), Size::new(24, 10));

        // The focus cycle: Esc unfocuses while keeping the split,
        // `Alt+A` refocuses, and a press while focused closes —
        // restoring the whole band.
        harness.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(
            harness.tabs.active().unwrap().agent_panel_open()
                && !harness.tabs.active().unwrap().agent_focused(),
            "idle Esc returns focus, split stays open"
        );
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));
        assert!(
            harness.tabs.active().unwrap().agent_focused(),
            "Alt+A refocuses an unfocused panel"
        );
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));
        let tab = harness.tabs.active().unwrap();
        assert!(!tab.agent_panel_open() && !tab.agent_focused());
        assert_eq!(
            tab.session.size(),
            Size::new(40, 10),
            "closing restores the band"
        );
    }

    /// A host resize while the panel is open re-derives the split —
    /// the session keeps its terminal side, it never restores the
    /// band under the panel.
    #[test]
    fn resize_re_derives_the_open_split() {
        let mut harness = Harness::new(manager());
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));

        harness.handle(Event::Resize(100, 30));

        // Content band 100×28 → session 60 (the panel's two fifths).
        assert_eq!(
            harness.tabs.active().unwrap().session.size(),
            Size::new(60, 28)
        );
    }

    /// Focused keystrokes reach the input line and never the PTY:
    /// type into the panel while the shell sits inside a byte-exact
    /// `head -c` capture — the capture must hold ONLY the character
    /// typed after focus returned. The input shadow stays empty too:
    /// panel text is not shell input.
    #[test]
    fn focused_typing_reaches_the_panel_and_never_the_pty() {
        let path = capture_path("panel-keys");
        let mut harness = Harness::new(sh_manager());
        run_capture(
            &mut harness,
            &format!("stty raw; head -c 1 > {}", path.display()),
            &path,
        );
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));

        for c in "ab".chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(panel(&harness).input(), "ab");
        assert_eq!(
            harness.tabs.active().unwrap().shadow.text(),
            "",
            "panel text is not shell input — the shadow never saw it"
        );

        // Idle Esc returns focus; the next char reaches the PTY —
        // and the capture proves "ab" never did.
        harness.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        harness.handle(key(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(
            wait_file(&mut harness, &path, 1),
            b"c",
            "only the post-Esc key reached the session"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// Enter submits the input as a task: the conversation gains the
    /// `Task` entry, the panel runs until the worker's terminal event
    /// (the unconfigured model's `Error`) lands it as a status line.
    #[test]
    fn enter_submits_and_the_terminal_event_lands() {
        let mut harness = Harness::new(sh_manager());
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));
        for c in "fix it".chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }

        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(panel(&harness).is_running());
        assert_eq!(panel(&harness).input(), "", "submitted input clears");
        assert_eq!(
            panel(&harness).entries(),
            &[crate::agent_panel::Entry::Task("fix it".into())]
        );

        wait_until(&mut harness, |h| !panel(h).is_running());
        assert!(
            matches!(
                panel(&harness).entries().last(),
                Some(crate::agent_panel::Entry::Status(s)) if s.contains("coding_model")
            ),
            "the worker's error became a conversation status line: {:?}",
            panel(&harness).entries().last()
        );
    }

    /// Text typed while a task runs queues as a steering message —
    /// on the channel AND in the conversation.
    #[test]
    fn enter_during_a_run_steers_instead_of_submitting() {
        let mut harness = Harness::new(sh_manager());
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));
        for c in "fix it".chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(panel(&harness).is_running());

        for c in "use sudo".chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            panel(&harness).entries().last(),
            Some(&crate::agent_panel::Entry::Steering("use sudo".into())),
            "mid-run input recorded as steering"
        );
        // Let the task die of its unconfigured model — the steering
        // entry stays part of the conversation.
        wait_until(&mut harness, |h| !panel(h).is_running());
    }

    /// Esc while a task runs cancels it — the in-flight request
    /// stops — and focus stays for the next task; the terminal event
    /// releases `running` and leaves the status line.
    #[test]
    fn esc_cancels_the_running_task() {
        let mut harness = Harness::new(sh_manager());
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));
        for c in "fix it".chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(panel(&harness).is_running());

        harness.handle(key(KeyCode::Esc, KeyModifiers::NONE));

        assert!(
            harness.tabs.active().unwrap().agent_focused(),
            "cancelling keeps panel focus"
        );
        wait_until(&mut harness, |h| !panel(h).is_running());
        assert!(
            matches!(
                panel(&harness).entries().last(),
                Some(crate::agent_panel::Entry::Status(_))
            ),
            "a terminal status line closed the task: {:?}",
            panel(&harness).entries().last()
        );
    }

    /// Focused keys never reach the keymap's blind spot the other way
    /// either: app bindings resolve BEFORE panel focus, so `Alt+T`
    /// still spawns a tab while the panel owns typing.
    #[test]
    fn app_keymap_wins_over_panel_focus() {
        let mut harness = Harness::new(manager());
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));
        assert!(harness.tabs.active().unwrap().agent_focused());

        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));
        assert_eq!(harness.tabs.len(), 2, "Alt+T spawned past panel focus");
        assert!(
            harness.tabs.active().unwrap().agent_panel().is_none(),
            "a fresh tab owns no panel"
        );
        // And its session took the whole band, not a phantom split.
        assert_eq!(
            harness.tabs.active().unwrap().session.size(),
            Size::new(40, 10)
        );

        // The first tab kept its conversation and its split.
        harness.handle(key(KeyCode::PageUp, KeyModifiers::CONTROL));
        let tab = harness.tabs.active().unwrap();
        assert!(tab.agent_panel_open());
        assert_eq!(tab.session.size(), Size::new(24, 10));
    }

    /// Closing the owning tab drops its agent — the `AgentSession`'s
    /// Drop cancels and joins the worker even mid-task: no panic, no
    /// orphan.
    #[test]
    fn closing_a_tab_drops_its_agent() {
        let mut harness = Harness::new(sh_manager());
        harness.handle(key(KeyCode::Char('t'), KeyModifiers::ALT));
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));
        for c in "fix it".chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(panel(&harness).is_running());

        assert_eq!(
            harness.handle(key(KeyCode::Char('w'), KeyModifiers::ALT)),
            Handled::Continue
        );
        assert_eq!(harness.tabs.len(), 1);
        assert!(
            harness.tabs.active().unwrap().agent_panel().is_none(),
            "the surviving tab owns no panel"
        );
    }

    /// The wheel inside the panel's rect scrolls the conversation —
    /// the terminal's scroll offset is untouched — while a click there
    /// never starts a selection.
    #[test]
    fn the_wheel_and_clicks_stay_inside_the_split() {
        let mut harness = Harness::new(manager());
        harness.handle(Event::Resize(100, 30));
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::ALT));
        // Stage a conversation tall enough to scroll.
        let tab = harness.tabs.active_mut().unwrap();
        for n in 0..10 {
            tab.agent
                .as_mut()
                .unwrap()
                .apply(ait_agent::AgentEvent::TextDelta(format!("line{n}\n")));
        }

        // Band is 100×28 → terminal 60 cols, panel x ≥ 60.
        let wheel_in_panel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 80,
                row: 10,
                modifiers: KeyModifiers::NONE,
            })
        };
        harness.handle(wheel_in_panel(MouseEventKind::ScrollUp));

        let tab = harness.tabs.active().unwrap();
        assert_eq!(tab.scroll_offset(), 0, "the terminal never scrolled");
        assert!(
            !panel(&harness).is_pinned(),
            "the conversation took the scroll"
        );

        harness.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 80,
            row: 10,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(
            harness.tabs.active().unwrap().selection_range().is_none(),
            "a click in the panel selects nothing"
        );
    }

    // ---- Spec R06 T4: `run_command` through the ShellBridge (issue #87) ----

    /// A `chat.completion.chunk` line carrying a tool-call fragment —
    /// `id`/`name` on announce, bare `arguments` after (ait's tests
    /// have no `serde_json` edge; the literals are hand-escaped).
    fn tool_fragment(index: u32, announce: Option<(&str, &str)>, args: &str) -> String {
        let call = match announce {
            Some((id, name)) => format!(
                "{{\"index\":{index},\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":\"{args}\"}}}}"
            ),
            None => format!("{{\"index\":{index},\"function\":{{\"arguments\":\"{args}\"}}}}"),
        };
        format!("data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{call}]}},\"finish_reason\":null}}]}}\n\n")
    }

    /// All text in a session's grid — the `tabs::tests` helper's
    /// shape, for the trace assertions below.
    fn grid_text(session: &ait_term::LiveSession) -> String {
        let grid = session.grid();
        grid.scrollback_rows()
            .chain(grid.visible_rows())
            .map(|row| row.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole e2e path (issue #87): a staged panel + bridge on a
    /// REAL injected-bash tab, a fixture model calling `run_command`,
    /// the approve card gating the request — no `__ait_run` bytes
    /// before `a` — then the in-band run types its visible trace and
    /// the merged result re-enters the conversation.
    #[test]
    fn run_command_e2e_gates_types_visibly_and_finishes() {
        if !std::path::Path::new("/bin/bash").exists() {
            return; // bash isn't installed everywhere tests run
        }
        let fixture = ait_llm::testing::Fixture::serve(vec![
            ait_llm::testing::sse(&format!(
                "{}{}{}",
                tool_fragment(0, Some(("call_1", "run_command")), ""),
                tool_fragment(
                    0,
                    None,
                    "{\\\"command\\\":\\\"printf 'agent-marker'\\\"}"
                ),
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
            )),
            ait_llm::testing::sse(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n",
            ),
        ]);
        let mut harness = Harness::new(
            TabManager::spawn_with(
                Size::new(40, 10),
                SessionTemplate {
                    options: LiveSessionOptions {
                        program: Some(std::ffi::OsString::from("/bin/bash")),
                        cwd: Some(std::path::PathBuf::from("/tmp")),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        wait_until(&mut harness, |h| {
            h.tabs.active().unwrap().integration_state()
                == crate::integration::IntegrationState::Integrated
        });
        // Point `coding_model` at the fixture.
        harness.state.config = LiveSettings::adopt(
            Settings::from_toml(&format!(
                r#"
[agents.custom_endpoints.local]
name = "Fixture"
base_url = "{}"
schema = "openai_chat_completions"

[[agents.custom_endpoints.local.models]]
name = "test-model"
alias = "Test Model"
config_key = "coding-key"

[agents.execution_profiles.default]
coding_model = "coding-key"
"#,
                fixture.base_url
            ))
            .expect("settings"),
        );
        // Stage the panel + bridge service — `with_session` opens
        // focused; the service is what `toggle_agent_panel` creates.
        {
            let tab = harness.tabs.active_mut().unwrap();
            let (_handle, service) = crate::shell_bridge::pair();
            tab.bridge = Some(service);
            tab.agent = Some(crate::agent_panel::AgentPanel::with_session(
                ait_agent::AgentSession::with_secret_store(config::NoSecrets),
            ));
        }

        // Type the task and submit.
        for c in "do it".chars() {
            harness.handle(key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        harness.handle(key(KeyCode::Enter, KeyModifiers::NONE));

        wait_until(&mut harness, |h| panel(h).awaiting_verdict());
        // The gate is parked: no request line has been typed.
        let grid_before = grid_text(&harness.tabs.active().unwrap().session);
        assert!(
            !grid_before.contains("__ait_run"),
            "no request bytes before approval: {grid_before}"
        );

        // `a` — allow once — releases the in-band run.
        harness.handle(key(KeyCode::Char('a'), KeyModifiers::NONE));
        wait_until(&mut harness, |h| !panel(h).is_running());

        // The task finished; the typed `__ait_run` trace is in the
        // grid (us-23) while the 7701 output never rendered (us-12).
        let text = grid_text(&harness.tabs.active().unwrap().session);
        assert!(text.contains("__ait_run"), "the request's trace: {text}");
        assert!(
            !text.contains("agent-marker"),
            "7701 output is invisible: {text}"
        );
        let tool_card_done = panel(&harness).entries().iter().any(|entry| matches!(
            entry,
            crate::agent_panel::Entry::Tool(card)
                if card.name == "run_command" && card.status == crate::agent_panel::CardStatus::Done
        ));
        assert!(tool_card_done, "{:?}", panel(&harness).entries());

        // The result re-entered the conversation: the second request
        // carries the merged output and the exit code.
        fixture.captured();
        let second = fixture.captured();
        assert!(second.body.contains("agent-marker"), "{}", second.body);
        assert!(second.body.contains("exit code"), "{}", second.body);
    }
}
