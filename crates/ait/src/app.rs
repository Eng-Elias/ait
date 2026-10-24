//! The Round-2 application loop: shell sessions in tabs inside the host
//! terminal (Spec R02, tickets T1–T4).
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
//!    `Notification`s become a ~3 s toast on the status line.
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
use std::time::{Duration, Instant};

use ait_term::{Cell, Cursor, CursorShape, Damage, ExitStatus, LiveEvent, SessionEvent};
use crossterm::clipboard::CopyToClipboard;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{self, Event, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use crossterm::queue;
use ratatui::layout::{Position, Rect};
use ratatui::Terminal;

use crate::backend::StyledBackend;
use crate::keymap::{self, AppAction, CapabilityTier, Resolution};
use crate::keys;
use crate::render::{
    apply_selection, chrome_areas, content_size, grid_line, paint_session, paint_status_line,
    paint_tab_bar, to_cursor_style, UnderlineOverlay,
};
use crate::scroll::WHEEL_LINES;
use crate::selection::{encode_paste, extract, GridPoint, SelectionRange};
use crate::status::{notification_text, status_text, Toast};
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
    let mut state = AppState {
        screen: Rect::new(0, 0, columns, rows),
        copied_text: None,
        last_exit: None,
        toast: None,
    };
    // The PTY winsize is the CONTENT area: the tab bar and the (reserved)
    // status-line row are chrome outside the session grid (Spec R02).
    let mut tabs = TabManager::spawn(content_size(state.screen))
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
            offset: tab.scroll.offset(),
            selection: tab.selection.range(),
            status: status_text(
                tab.cwd(),
                tab.title(),
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
/// none of this. The wheel moves the scroll
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
                    tab.selection.clear();
                    match action {
                        AppAction::ScrollPageUp => {
                            let (lines, scrollback) = (tab.grid().lines(), tab.scrollback());
                            tab.scroll.scroll_up(lines, scrollback);
                        }
                        AppAction::ScrollPageDown => {
                            let page = tab.grid().lines();
                            tab.scroll.scroll_down(page);
                        }
                        AppAction::ScrollTop => {
                            let scrollback = tab.scrollback();
                            tab.scroll.jump_to_top(scrollback);
                        }
                        AppAction::ScrollBottom => tab.scroll.snap_to_bottom(),
                        AppAction::Paste => {
                            // Paste ait's own copied selection (there is no
                            // host-clipboard read), bracketed iff the child
                            // enabled mode 2004. The user pastes to type, so
                            // the view snaps to the live viewport first.
                            // Exited sessions drop input like keys do.
                            tab.scroll.snap_to_bottom();
                            if tab.exit_status().is_none() {
                                if let Some(text) = state.copied_text.as_deref() {
                                    let bytes = encode_paste(text, tab.session.bracketed_paste());
                                    let _ = tab.session.send_bytes(&bytes);
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
                    let Some(tab) = tabs.active_mut() else {
                        return Handled::Continue;
                    };
                    // Any keypress drops the highlight — never stale state.
                    tab.selection.clear();
                    // Typing snaps the view back to the live viewport, then
                    // reaches the PTY encoded exactly as before.
                    tab.scroll.snap_to_bottom();
                    if tab.exit_status().is_none() {
                        if let Some(key_event) = keys::translate(&key_event) {
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
                    tab.selection.clear();
                    let scrollback = tab.scrollback();
                    tab.scroll.scroll_up(WHEEL_LINES, scrollback);
                }
                MouseEventKind::ScrollDown => {
                    tab.selection.clear();
                    tab.scroll.scroll_down(WHEEL_LINES);
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    // Only a drag starting inside the session area
                    // selects; the chrome rows (tab bar, status line)
                    // are inert.
                    if content.contains(Position::new(mouse_event.column, mouse_event.row)) {
                        let offset = tab.scroll.offset();
                        tab.selection
                            .begin(grid_point(&mouse_event, content, offset));
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    // An in-flight drag follows the pointer anywhere —
                    // past the top it selects into scrollback, the way
                    // the view would scroll.
                    let offset = tab.scroll.offset();
                    tab.selection
                        .extend(grid_point(&mouse_event, content, offset));
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    // Fold the release point into the range before
                    // finishing: hosts coalesce motion reports, so the
                    // last `Drag` can lag the cell the button released
                    // on — a fast flick would otherwise copy short.
                    // Outside a drag `extend` is a no-op.
                    let offset = tab.scroll.offset();
                    tab.selection
                        .extend(grid_point(&mouse_event, content, offset));
                    if let Some(range) = tab.selection.finish() {
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
/// first event, then drain everything queued behind it before
/// returning, so an output flood can never starve the keyboard.
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
    loop {
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
                tab.scroll.offset(),
            );
            if let Some(range) = tab.selection.range() {
                apply_selection(buf, areas.content, &tab.grid(), range, tab.scroll.offset());
            }
        }
        paint_tab_bar(buf, areas.tab_bar, &titles, active);
        paint_status_line(buf, areas.status, status, toast);
        // The cursor's live line renders at `content.y + line + offset`;
        // scrolled far enough up it leaves the band and stays hidden.
        let y = usize::from(areas.content.y) + cursor.line + tab.scroll.offset();
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
    use ait_term::Size;
    use crossterm::event::{
        KeyCode, KeyEvent as CtKeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
    };
    use std::collections::VecDeque;

    fn manager() -> TabManager {
        TabManager::spawn(Size::new(40, 10)).unwrap()
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
            tab.scroll.scroll_up(3, 10);
        }
        harness.handle(key(KeyCode::PageUp, KeyModifiers::CONTROL));
        assert_eq!(harness.tabs.active_index(), 0);
        harness.handle(key(KeyCode::Char('x'), KeyModifiers::NONE));

        assert_eq!(
            harness.tabs.tabs()[0].scroll.offset(),
            0,
            "typed tab snapped"
        );
        assert_eq!(
            harness.tabs.tabs()[1].scroll.offset(),
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
            tab.selection.begin(GridPoint::new(0, 0));
        }
        let release = Event::Key(CtKeyEvent::new_with_kind_and_state(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
            KeyEventState::NONE,
        ));

        assert_eq!(harness.handle(release), Handled::Continue);

        let tab = harness.tabs.active().unwrap();
        assert_eq!(tab.scroll.offset(), 3, "a release never snaps the view");
        assert!(
            tab.selection.range().is_some(),
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
}
