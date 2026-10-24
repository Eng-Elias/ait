//! The Round-2 application loop: one full-screen [`LiveSession`] in the
//! host terminal (Spec R02, tickets T1+T2).
//!
//! The loop is synchronous per the spec — no async runtime in Round 2:
//!
//! 1. `crossterm::event::poll` for input (a short timeout caps the frame
//!    period); every queued input event is serviced first so output floods
//!    can never starve the keyboard (REQUIREMENTS.md §6). Keys resolve
//!    against the app [`keymap`] **before** PTY input (standard 7): bound
//!    keys run app actions, everything else snaps the view to the bottom
//!    and encodes to the shell. The mouse wheel scrolls scrollback; a
//!    left-button drag highlights cells and its release copies the text
//!    to the host clipboard via OSC 52 (`Shift+Insert` pastes ait's own
//!    copy back, bracketed-paste-wrapped when the child enabled mode
//!    2004).
//! 2. [`LiveSession::drain_events`] feeds pending PTY output into the
//!    grid; a scrolled-up view stays pinned on its lines.
//! 3. A frame renders when the session reports damage or the scroll
//!    offset moved since the last frame.
//!
//! `Alt+Q` quits, and when the session's child exits `ait` exits with the
//! child's status — the [`TerminalGuard`](crate::terminal::TerminalGuard)
//! restores the host terminal on every way out.

use std::cell::RefCell;
use std::io::{self, Write};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use ait_term::{
    CursorShape, Damage, ExitStatus, LiveEvent, LiveSession, LiveSessionOptions, SessionEvent,
    SessionOptions, Size,
};
use crossterm::clipboard::CopyToClipboard;
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{self, Event, MouseButton, MouseEvent, MouseEventKind};
use crossterm::queue;
use ratatui::Terminal;

use crate::backend::StyledBackend;
use crate::keymap::{self, AppAction, CapabilityTier, Resolution};
use crate::keys;
use crate::render::{apply_selection, paint_session, to_cursor_style, UnderlineOverlay};
use crate::scroll::{ScrollState, WHEEL_LINES};
use crate::selection::{encode_paste, extract, GridPoint, Selection, SelectionRange};
use crate::terminal::TerminalGuard;

/// Upper bound on one loop iteration's wait for input: at 8 ms the loop
/// idles at ~125 iterations/sec — well above the §6 floor of 30 fps — and
/// input latency stays under a frame.
const INPUT_POLL: Duration = Duration::from_millis(8);

/// Whether the loop keeps running after an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handled {
    Continue,
    /// `Alt+Q` (or enhanced `Ctrl+Shift+Q`) — exit cleanly.
    Quit,
}

/// Run the TUI: spawn `$SHELL` in a full-screen session and drive it until
/// the child exits or the user quits. Returns the child's exit status
/// mapped to an [`ExitCode`], or `SUCCESS` for a user quit.
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
    let mut session = LiveSession::spawn(LiveSessionOptions {
        session: SessionOptions {
            size: Size::new(usize::from(columns), usize::from(rows)),
            ..Default::default()
        },
        // `program: None` resolves $SHELL, falling back to /bin/sh.
        ..Default::default()
    })
    .map_err(|e| io::Error::other(e.to_string()))?;

    let backend = StyledBackend::new(io::stdout());
    let underlines = backend.underline_overlay();
    let mut terminal = Terminal::new(backend)?;
    // No `terminal.clear()` here: it issues a cursor-position report that
    // blocks on stdin, and the just-entered alternate screen is already
    // clean — the first damage render repaints everything.
    let mut last_cursor_shape = None;
    // Scroll state lives next to the session: `rendered_offset` is what
    // the last painted frame showed, so a change alone triggers repaint.
    // The same goes for the mouse selection and ait's own copied text —
    // per-tab viewport state once T4's tab manager lands.
    let mut scroll = ScrollState::new();
    let mut rendered_offset = 0;
    let mut selection = Selection::default();
    let mut rendered_selection: Option<SelectionRange> = None;
    let mut copied_text: Option<String> = None;

    let mut exit_status = None;
    'main: while exit_status.is_none() {
        // 1. Input first, completely — floods must not starve keystrokes.
        if event::poll(INPUT_POLL)? {
            if let Handled::Quit = handle_event(
                event::read()?,
                &mut session,
                &mut scroll,
                &mut selection,
                &mut copied_text,
                terminal.backend_mut().writer_mut(),
                tier,
            ) {
                break;
            }
            while event::poll(Duration::ZERO)? {
                if let Handled::Quit = handle_event(
                    event::read()?,
                    &mut session,
                    &mut scroll,
                    &mut selection,
                    &mut copied_text,
                    terminal.backend_mut().writer_mut(),
                    tier,
                ) {
                    break 'main;
                }
            }
        }

        // 2. Drain the session: feeds queued PTY bytes into the grid and
        //    surfaces lifecycle events.
        for live_event in session.drain_events() {
            match live_event {
                LiveEvent::Exited(status) => exit_status = Some(status),
                // Child-app OSC 52 stays off (`SessionOptions::osc52`
                // default), so this event can't fire — when Round 3's
                // config flips the option, honor the request by copying
                // `contents` to the host clipboard exactly like our own
                // selection copies.
                LiveEvent::Osc(SessionEvent::ClipboardWrite { .. }) => {}
                // OSC 7/9/133/… are consumed by later tickets (status
                // line, toasts).
                _ => {}
            }
        }
        if exit_status.is_some() {
            break;
        }

        // A scrolled-up view stays pinned on its lines while output grows
        // the scrollback underneath it; an in-flight selection follows
        // the same shift to stay on its text.
        let grown = scroll.on_output(session.grid().scrollback());
        selection.on_output(grown);

        // 3. Render when the grid reports damage, the view moved, or the
        //    selection changed.
        let damaged = !matches!(session.damage(), Damage::Partial(bounds) if bounds.is_empty());
        let selection_range = selection.range();
        if damaged || scroll.offset() != rendered_offset || selection_range != rendered_selection {
            render_frame(
                &mut terminal,
                &session,
                &underlines,
                &mut last_cursor_shape,
                scroll.offset(),
                selection_range,
            )?;
            rendered_offset = scroll.offset();
            rendered_selection = selection_range;
        }
    }

    Ok(exit_code(exit_status.as_ref()))
}

/// Route one host event. Keys hit the app keymap **before** PTY input
/// (standard 7): a bound key runs its action and never reaches the PTY; an
/// unbound key snaps the view to the bottom and encodes to the shell as
/// before. Any keypress also clears the selection highlight. The wheel
/// moves the scroll offset; a left-button drag selects cells and its
/// release copies the text to the host clipboard via OSC 52 — a silent
/// no-op where the host denies it (Spec R02). Resizes forward to the PTY.
///
/// Write/resize failures (e.g. a just-dead child) are tolerated — the
/// session's `Exited` event ends the loop cleanly.
fn handle_event<W: Write>(
    event: Event,
    session: &mut LiveSession,
    scroll: &mut ScrollState,
    selection: &mut Selection,
    copied_text: &mut Option<String>,
    host: &mut W,
    tier: CapabilityTier,
) -> Handled {
    match event {
        Event::Key(key_event) => {
            // Any keypress drops the highlight — never stale state.
            selection.clear();
            match keymap::resolve(&key_event, tier) {
                Resolution::Action(action) => match action {
                    AppAction::Quit => return Handled::Quit,
                    AppAction::ScrollPageUp => {
                        let grid = session.grid();
                        scroll.scroll_up(grid.lines(), grid.scrollback());
                    }
                    AppAction::ScrollPageDown => {
                        let page = session.grid().lines();
                        scroll.scroll_down(page);
                    }
                    AppAction::ScrollTop => {
                        let scrollback = session.grid().scrollback();
                        scroll.jump_to_top(scrollback);
                    }
                    AppAction::ScrollBottom => scroll.snap_to_bottom(),
                    AppAction::Paste => {
                        // Paste ait's own copied selection (there is no
                        // host-clipboard read), bracketed iff the child
                        // enabled mode 2004. The user pastes to type, so
                        // the view snaps to the live viewport first.
                        scroll.snap_to_bottom();
                        if let Some(text) = copied_text.as_deref() {
                            let bytes = encode_paste(text, session.bracketed_paste());
                            let _ = session.send_bytes(&bytes);
                        }
                    }
                    // The tab actions are bound so the keymap already
                    // owns the full Spec R02 table; they consume the key
                    // and do nothing until T4's dispatch lands.
                    AppAction::NewTab
                    | AppAction::CloseTab
                    | AppAction::NextTab
                    | AppAction::PrevTab
                    | AppAction::JumpToTab(_) => {}
                },
                Resolution::PassThrough => {
                    // Typing snaps the view back to the live viewport,
                    // then reaches the PTY encoded exactly as before.
                    scroll.snap_to_bottom();
                    if let Some(key_event) = keys::translate(&key_event) {
                        let _ = session.send_key(key_event);
                    }
                }
            }
        }
        Event::Mouse(mouse_event) => match mouse_event.kind {
            MouseEventKind::ScrollUp => {
                selection.clear();
                let scrollback = session.grid().scrollback();
                scroll.scroll_up(WHEEL_LINES, scrollback);
            }
            MouseEventKind::ScrollDown => {
                selection.clear();
                scroll.scroll_down(WHEEL_LINES);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                selection.begin(grid_point(&mouse_event, scroll.offset()));
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                selection.extend(grid_point(&mouse_event, scroll.offset()));
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(range) = selection.finish() {
                    let text = extract(&session.grid(), range);
                    if !text.is_empty() {
                        *copied_text = Some(text.clone());
                        // OSC 52 silently no-ops where the host denies
                        // it — never an error (Spec R02).
                        let _ = queue!(host, CopyToClipboard::to_clipboard_from(&text));
                        let _ = host.flush();
                    }
                }
            }
            // Other buttons and motion aren't captured/used.
            _ => {}
        },
        Event::Resize(columns, rows) => {
            // Reflow invalidates grid coordinates the range points at.
            selection.clear();
            let _ = session.resize(Size::new(usize::from(columns), usize::from(rows)));
        }
        // Focus and paste events have no Round-2 handler yet.
        _ => {}
    }
    Handled::Continue
}

/// Screen mouse position → grid coordinates: `line = row - scroll_offset`
/// (the same rule the painter uses).
fn grid_point(mouse: &MouseEvent, scroll_offset: usize) -> GridPoint {
    let offset = i32::try_from(scroll_offset).unwrap_or(i32::MAX);
    GridPoint::new(i32::from(mouse.row) - offset, usize::from(mouse.column))
}

/// Draw one frame: repaint the grid at `scroll_offset`, invert the
/// selected cells if a range is active, apply the application-requested
/// cursor shape (only when it changed), and place the cursor.
fn render_frame(
    terminal: &mut Terminal<StyledBackend<io::Stdout>>,
    session: &LiveSession,
    underlines: &Rc<RefCell<UnderlineOverlay>>,
    last_cursor_shape: &mut Option<SetCursorStyle>,
    scroll_offset: usize,
    selection: Option<SelectionRange>,
) -> io::Result<()> {
    let cursor = session.cursor();
    if let Some(style) = to_cursor_style(cursor.shape) {
        if *last_cursor_shape != Some(style) {
            // Queued on the backend's writer; flushes with this draw.
            queue!(terminal.backend_mut().writer_mut(), style)?;
            *last_cursor_shape = Some(style);
        }
    }

    terminal.draw(|frame| {
        let area = frame.area();
        let grid = session.grid();
        {
            let mut overlay = underlines.borrow_mut();
            overlay.clear();
            paint_session(frame.buffer_mut(), area, &grid, &mut overlay, scroll_offset);
            if let Some(range) = selection {
                apply_selection(frame.buffer_mut(), area, &grid, range, scroll_offset);
            }
        }
        // The cursor's live line renders at screen row `line + offset`;
        // scrolled far enough up it leaves the frame and stays hidden.
        let y = cursor.line.saturating_add(scroll_offset);
        if cursor.shape != CursorShape::Hidden && y < usize::from(area.height) {
            let x = (cursor.column as u16).min(area.width.saturating_sub(1));
            frame.set_cursor_position((x, y as u16));
        }
        // No position set → ratatui hides the cursor this frame.
    })?;
    terminal.backend_mut().writer_mut().flush()?;
    Ok(())
}

/// The child's exit status as the process exit code.
fn exit_code(status: Option<&ExitStatus>) -> ExitCode {
    match status {
        Some(status) => ExitCode::from(u8::try_from(status.code).unwrap_or(u8::MAX)),
        None => ExitCode::SUCCESS,
    }
}
