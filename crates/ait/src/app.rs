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
//!    key path, and errors land on the status line.
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

use ait_agent::{CommandSuggester, SuggestEvent};
use ait_config::{ConfigError, Settings};
use ait_term::{Cell, Cursor, CursorShape, Damage, ExitStatus, LiveEvent, SessionEvent};
use crossterm::clipboard::CopyToClipboard;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{self, Event, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use crossterm::queue;
use ratatui::layout::{Position, Rect};
use ratatui::Terminal;

use crate::backend::StyledBackend;
use crate::config::{self, LiveSettings, NoSecrets};
use crate::keymap::{self, AppAction, CapabilityTier, Resolution};
use crate::keys;
use crate::render::{
    apply_selection, chrome_areas, content_size, grid_line, paint_session, paint_status_line,
    paint_tab_bar, to_cursor_style, UnderlineOverlay,
};
use crate::scroll::WHEEL_LINES;
use crate::selection::{encode_paste, extract, GridPoint, SelectionRange};
use crate::status::{notification_text, status_text, Toast};
use crate::suggest;
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
    /// (Spec R04 user story 14) — `None` while idle. One request at a
    /// time is the suggester's frontend contract, so a live pin also
    /// gates the next trigger. T5 routes `Delta` text into the pinned
    /// tab's session.
    suggestion: Option<InFlightSuggestion>,
}

/// An in-flight command suggestion, pinned to the tab index that
/// fired it (Spec R04 user story 14). Indices shift when tabs close;
/// T5 revalidates against the tab list before writing deltas.
struct InFlightSuggestion {
    /// The tab position in the tab list at trigger time.
    tab: usize,
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
        Ok(settings) => (LiveSettings::adopt(settings, &NoSecrets), None),
        Err(error) => (
            LiveSettings::adopt(Settings::default(), &NoSecrets),
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
        suggestion: None,
    };
    // The PTY winsize is the CONTENT area: the tab bar and the (reserved)
    // status-line row are chrome outside the session grid (Spec R02).
    let mut tabs = TabManager::spawn_with(
        content_size(state.screen),
        config::session_options(&state.config.settings().terminal),
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
                // Child-app OSC 52 stays off (`SessionOptions::osc52`
                // default), so this event can't fire — when Round 3's
                // config flips the option, honor the request by copying
                // `contents` to the host clipboard exactly like our own
                // selection copies.
                LiveEvent::Osc(SessionEvent::ClipboardWrite { .. }) => {}
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
        drain_suggest_events(&mut state, &tabs);

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
        let view = Rendered {
            active,
            count,
            offset: tab.scroll_offset(),
            selection: tab.selection_range(),
            status: status_text(
                tab.cwd(),
                tab.title(),
                state.config.model(),
                tab.exit_status(),
                state.toast.as_ref(),
                now,
            ),
            toast: state.toast.as_ref().is_some_and(|t| t.is_live(now)),
            cursor,
            cursor_cell: tab.grid().cell(
                i32::try_from(cursor.line).unwrap_or(i32::MAX),
                cursor.column,
            ),
        };
        if damaged || rendered.as_ref() != Some(&view) {
            render_frame(
                &mut terminal,
                &tabs,
                &view.status,
                view.toast,
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
                        | AppAction::JumpToTab(_) => {}
                    }
                }
                Resolution::PassThrough => {
                    // The originating tab's index BEFORE the mutable
                    // borrow — a fired request is pinned to it (Spec R04
                    // user story 14).
                    let index = tabs.active_index();
                    let Some(tab) = tabs.active_mut() else {
                        return Handled::Continue;
                    };
                    // Any keypress drops the highlight — never stale state.
                    tab.clear_selection();
                    // Typing snaps the view back to the live viewport, then
                    // reaches the PTY encoded exactly as before.
                    tab.snap_to_bottom();
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
                                if let Some(request) =
                                    suggest::request_for(tab, state.config.settings())
                                {
                                    match state.suggester.suggest(request) {
                                        Ok(()) => {
                                            state.suggestion =
                                                Some(InFlightSuggestion { tab: index });
                                        }
                                        Err(error) => {
                                            state.toast = Some(Toast::new(
                                                format!("{error}"),
                                                Instant::now(),
                                            ));
                                        }
                                    }
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
            let Some(tab) = tabs.active_mut() else {
                return Handled::Continue;
            };
            match mouse_event.kind {
                MouseEventKind::ScrollUp => {
                    tab.clear_selection();
                    tab.scroll_up(WHEEL_LINES);
                }
                MouseEventKind::ScrollDown => {
                    tab.clear_selection();
                    tab.scroll_down(WHEEL_LINES);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // Only a drag starting inside the session area
                    // selects; the chrome rows (tab bar, status line)
                    // are inert.
                    if content.contains(Position::new(mouse_event.column, mouse_event.row)) {
                        tab.begin_selection(grid_point(&mouse_event, content, tab.scroll_offset()));
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    // An in-flight drag follows the pointer anywhere —
                    // past the top it selects into scrollback, the way
                    // the view would scroll.
                    tab.extend_selection(grid_point(&mouse_event, content, tab.scroll_offset()));
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    // Fold the release point into the range before
                    // finishing: hosts coalesce motion reports, so the
                    // last `Drag` can lag the cell the button released
                    // on — a fast flick would otherwise copy short.
                    // Outside a drag `extend` is a no-op.
                    tab.extend_selection(grid_point(&mouse_event, content, tab.scroll_offset()));
                    if let Some(range) = tab.finish_selection() {
                        let text = extract(&tab.grid(), range);
                        if !text.is_empty() {
                            state.copied_text = Some(text.clone());
                            // OSC 52 silently no-ops where the host
                            // denies it — never an error (Spec R02).
                            let _ = queue!(host, CopyToClipboard::to_clipboard_from(&text));
                            let _ = host.flush();
                        }
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
fn drain_config_events(
    receiver: &Receiver<Result<Settings, ConfigError>>,
    state: &mut AppState,
    tabs: &mut TabManager,
) {
    while let Ok(event) = receiver.try_recv() {
        match event {
            Ok(settings) => {
                tabs.set_session_options(config::session_options(&settings.terminal));
                state.config = LiveSettings::adopt(settings, &NoSecrets);
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

/// Drain one tick of suggester events (Spec R04). T4's detection half
/// only keeps the bookkeeping honest: `Error` surfaces as the §6
/// status-line toast — prefixed with the pinned tab's `N:title` label
/// when it points at a background tab, the same attribution rule the
/// notification toasts use — `Done`/`Error` release the in-flight pin
/// so the next trigger can fire, and `Delta` text is ignored until
/// T5's streaming insertion writes it into the pinned tab's session.
/// `try_recv` keeps the drain zero-cost inside the 8 ms loop.
fn drain_suggest_events(state: &mut AppState, tabs: &TabManager) {
    while let Ok(event) = state.suggester.events().try_recv() {
        match event {
            SuggestEvent::Delta(_) => {}
            SuggestEvent::Done => state.suggestion = None,
            SuggestEvent::Error(message) => {
                // Tab indices shift on close — look the pin up
                // defensively (a gone tab just toasts unlabeled).
                let source = state.suggestion.as_ref().and_then(|pin| {
                    if pin.tab == tabs.active_index() {
                        None
                    } else {
                        tabs.tabs().get(pin.tab).map(|tab| tab.label(pin.tab))
                    }
                });
                state.suggestion = None;
                state.toast = Some(match source {
                    Some(label) => Toast::new(format!("{label}: {message}"), Instant::now()),
                    None => Toast::new(message, Instant::now()),
                });
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
/// cells inverted), the status line at the bottom (`status`, reversed
/// while it's a live toast), then the application-requested cursor
/// shape and position.
fn render_frame(
    terminal: &mut Terminal<StyledBackend<io::Stdout>>,
    tabs: &TabManager,
    status: &str,
    toast: bool,
    underlines: &Rc<RefCell<UnderlineOverlay>>,
    last_cursor_shape: &mut Option<SetCursorStyle>,
) -> io::Result<()> {
    let Some(tab) = tabs.active() else {
        return Ok(());
    };
    let cursor = tab.cursor();
    if let Some(style) = to_cursor_style(cursor.shape) {
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
        let buf = frame.buffer_mut();
        {
            let mut overlay = underlines.borrow_mut();
            overlay.clear();
            paint_session(
                buf,
                areas.content,
                &tab.grid(),
                &mut overlay,
                tab.scroll_offset(),
            );
            if let Some(range) = tab.selection_range() {
                apply_selection(buf, areas.content, &tab.grid(), range, tab.scroll_offset());
            }
        }
        paint_tab_bar(buf, areas.tab_bar, &titles, active);
        paint_status_line(buf, areas.status, status, toast);
        // The cursor's live line renders at `content.y + line + offset`;
        // scrolled far enough up it leaves the band and stays hidden.
        let y = usize::from(areas.content.y) + cursor.line + tab.scroll_offset();
        if cursor.shape != CursorShape::Hidden && y < usize::from(areas.content.bottom()) {
            let x = (areas.content.x + cursor.column as u16)
                .min(areas.content.right().saturating_sub(1));
            frame.set_cursor_position((x, y as u16));
        }
        // No position set → ratatui hides the cursor this frame.
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
    use ait_term::{LiveSessionOptions, Size};
    use crossterm::event::{
        KeyCode, KeyEvent as CtKeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
    };
    use std::collections::VecDeque;

    fn manager() -> TabManager {
        TabManager::spawn_with(Size::new(40, 10), LiveSessionOptions::default()).unwrap()
    }

    /// A manager on `/bin/sh` — deterministic tty echo for the trigger
    /// tests (`$SHELL` resolves interactive bash on some hosts, whose
    /// readline prompt timing makes echo waits flaky; the tabs tests
    /// spawn `/bin/sh` for the same reason).
    fn sh_manager() -> TabManager {
        TabManager::spawn_with(
            Size::new(40, 10),
            LiveSessionOptions {
                program: Some(std::ffi::OsString::from("/bin/sh")),
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
                    config: LiveSettings::adopt(Settings::default(), &NoSecrets),
                    // `NoSecrets` keeps keyring lookups out of tests —
                    // resolution never blocks on the Secret Service.
                    suggester: CommandSuggester::with_secret_store(NoSecrets),
                    suggestion: None,
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

    /// Drain sessions and suggester events until `pred` holds, or fail
    /// after a generous bound — mirrors `tabs::tests`' `drain_until`.
    fn wait_until(harness: &mut Harness, pred: impl Fn(&Harness) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !pred(harness) {
            harness.tabs.drain_events();
            drain_suggest_events(&mut harness.state, &harness.tabs);
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

        // The verified request fired, pinned to tab 0 (us-14).
        assert_eq!(
            harness.state.suggestion.as_ref().map(|s| s.tab),
            Some(0),
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
            harness.state.config.model(),
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
            harness.state.config.model(),
            "Alias Name",
            "last-good settings are kept (story 11)"
        );
    }
}
