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
//! — the empty tab set is guarded before any indexing (standard 6). The
//! [`TerminalGuard`](crate::terminal::TerminalGuard) restores the host
//! terminal on every way out; dropping the manager drops every
//! `LiveSession`, which kills the children (SIGHUP → SIGKILL).

use std::cell::RefCell;
use std::io::{self, Write};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::{Duration, Instant};

use ait_term::{CursorShape, Damage, ExitStatus, LiveEvent, SessionEvent};
use crossterm::clipboard::CopyToClipboard;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{self, Event, MouseButton, MouseEvent, MouseEventKind};
use crossterm::queue;
use ratatui::layout::{Position, Rect};
use ratatui::Terminal;

use crate::backend::StyledBackend;
use crate::keymap::{self, AppAction, CapabilityTier, Resolution};
use crate::keys;
use crate::render::{
    apply_selection, chrome_areas, content_size, paint_session, paint_status_line, paint_tab_bar,
    to_cursor_style, UnderlineOverlay,
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

/// Whether the loop keeps running after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handled {
    Continue,
    /// `Alt+Q` (or enhanced `Ctrl+Shift+Q`), or the last tab closed —
    /// exit cleanly.
    Quit,
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
    // race `event::poll`.
    let tier = if crate::terminal::enable_keyboard_enhancement() {
        CapabilityTier::Enhanced
    } else {
        CapabilityTier::Legacy
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

    'main: loop {
        // 1. Input first, completely — floods must not starve keystrokes.
        if event::poll(INPUT_POLL)? {
            if let Handled::Quit = handle_event(
                event::read()?,
                &mut tabs,
                &mut state,
                terminal.backend_mut().writer_mut(),
                tier,
            ) {
                break;
            }
            while event::poll(Duration::ZERO)? {
                if let Handled::Quit = handle_event(
                    event::read()?,
                    &mut tabs,
                    &mut state,
                    terminal.backend_mut().writer_mut(),
                    tier,
                ) {
                    break 'main;
                }
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
                            .map(|tab| format!("{}:{}", tab_event.tab + 1, tab.title()))
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
        let damaged = !matches!(tab.session.damage(), Damage::Partial(bounds) if bounds.is_empty());
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
/// also clears the selection highlight. The wheel moves the scroll
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
        Event::Key(key_event) => match keymap::resolve(&key_event, tier) {
            Resolution::Action(AppAction::Quit) => return Handled::Quit,
            Resolution::Action(AppAction::NewTab) => {
                // The outgoing tab's highlight ends with the keypress;
                // the spawned tab becomes active.
                if let Some(tab) = tabs.active_mut() {
                    tab.selection.clear();
                }
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
            Resolution::Action(AppAction::NextTab) => {
                if let Some(tab) = tabs.active_mut() {
                    tab.selection.clear();
                }
                tabs.activate_next();
            }
            Resolution::Action(AppAction::PrevTab) => {
                if let Some(tab) = tabs.active_mut() {
                    tab.selection.clear();
                }
                tabs.activate_prev();
            }
            Resolution::Action(AppAction::JumpToTab(n)) => {
                if let Some(tab) = tabs.active_mut() {
                    tab.selection.clear();
                }
                tabs.jump_to(n);
            }
            // Per-active-tab viewport actions: scrollback + paste.
            Resolution::Action(action) => {
                let Some(tab) = tabs.active_mut() else {
                    return Handled::Continue;
                };
                tab.selection.clear();
                match action {
                    AppAction::ScrollPageUp => {
                        let grid = tab.session.grid();
                        tab.scroll.scroll_up(grid.lines(), grid.scrollback());
                    }
                    AppAction::ScrollPageDown => {
                        let page = tab.session.grid().lines();
                        tab.scroll.scroll_down(page);
                    }
                    AppAction::ScrollTop => {
                        let scrollback = tab.session.grid().scrollback();
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
                    other => unreachable!(
                        "keymap actions handled above: {other:?} is not a viewport action"
                    ),
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
        },
        Event::Mouse(mouse_event) => {
            let content = chrome_areas(state.screen).content;
            let Some(tab) = tabs.active_mut() else {
                return Handled::Continue;
            };
            match mouse_event.kind {
                MouseEventKind::ScrollUp => {
                    tab.selection.clear();
                    let scrollback = tab.session.grid().scrollback();
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
                    if let Some(range) = tab.selection.finish() {
                        let text = extract(&tab.session.grid(), range);
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

/// Screen mouse position → grid coordinates: `line = row - content.y -
/// scroll_offset` (the same rule the painter uses), `column = column -
/// content.x`.
fn grid_point(mouse: &MouseEvent, content: Rect, scroll_offset: usize) -> GridPoint {
    let offset = i32::try_from(scroll_offset).unwrap_or(i32::MAX);
    GridPoint::new(
        i32::from(mouse.row) - i32::from(content.y) - offset,
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
    let cursor = tab.session.cursor();
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
                &tab.session.grid(),
                &mut overlay,
                tab.scroll.offset(),
            );
            if let Some(range) = tab.selection.range() {
                apply_selection(
                    buf,
                    areas.content,
                    &tab.session.grid(),
                    range,
                    tab.scroll.offset(),
                );
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
    use crossterm::event::{KeyCode, KeyEvent as CtKeyEvent, KeyModifiers};

    fn manager() -> TabManager {
        TabManager::spawn(Size::new(40, 10)).unwrap()
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(CtKeyEvent::new(code, modifiers))
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
}
