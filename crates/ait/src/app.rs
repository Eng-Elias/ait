//! The Round-2 application loop: one full-screen [`LiveSession`] in the
//! host terminal (Spec R02-T1).
//!
//! The loop is synchronous per the spec — no async runtime in Round 2:
//!
//! 1. `crossterm::event::poll` for input (a short timeout caps the frame
//!    period); every queued input event is serviced first so output floods
//!    can never starve the keyboard (REQUIREMENTS.md §6).
//! 2. [`LiveSession::drain_events`] feeds pending PTY output into the grid.
//! 3. A frame renders only when the session reports damage.
//!
//! When the session's child exits, `ait` exits with the child's status —
//! the [`TerminalGuard`](crate::terminal::TerminalGuard) restores the host
//! terminal on the way out.

use std::cell::RefCell;
use std::io::{self, Write};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

use ait_term::{
    CursorShape, Damage, ExitStatus, LiveEvent, LiveSession, LiveSessionOptions, SessionOptions,
    Size,
};
use crossterm::cursor::SetCursorStyle;
use crossterm::event::{self, Event};
use crossterm::queue;
use ratatui::Terminal;

use crate::backend::StyledBackend;
use crate::keys;
use crate::render::{paint_session, to_cursor_style, UnderlineOverlay};
use crate::terminal::TerminalGuard;

/// Upper bound on one loop iteration's wait for input: at 8 ms the loop
/// idles at ~125 iterations/sec — well above the §6 floor of 30 fps — and
/// input latency stays under a frame.
const INPUT_POLL: Duration = Duration::from_millis(8);

/// Run the TUI: spawn `$SHELL` in a full-screen session and drive it until
/// the child exits. Returns the child's exit status mapped to an
/// [`ExitCode`].
pub fn run() -> io::Result<ExitCode> {
    let _guard = TerminalGuard::enter()?;
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

    let mut exit_status = None;
    while exit_status.is_none() {
        // 1. Input first, completely — floods must not starve keystrokes.
        if event::poll(INPUT_POLL)? {
            handle_event(event::read()?, &mut session);
            while event::poll(Duration::ZERO)? {
                handle_event(event::read()?, &mut session);
            }
        }

        // 2. Drain the session: feeds queued PTY bytes into the grid and
        //    surfaces lifecycle events.
        for live_event in session.drain_events() {
            // OSC events (OSC 7/9/52/133/…) are consumed by later tickets
            // (status line, toasts); T1 drops them.
            if let LiveEvent::Exited(status) = live_event {
                exit_status = Some(status);
            }
        }
        if exit_status.is_some() {
            break;
        }

        // 3. Render only when the grid reports damage.
        let damaged = !matches!(session.damage(), Damage::Partial(bounds) if bounds.is_empty());
        if damaged {
            render_frame(&mut terminal, &session, &underlines, &mut last_cursor_shape)?;
        }
    }

    Ok(exit_code(exit_status.as_ref()))
}

/// Route one host event: keys translate and go to the PTY (the keymap seam
/// slots in ahead of this in a later ticket — every key goes to the
/// session in T1), resizes forward to the PTY, the rest are ignored.
///
/// Write/resize failures (e.g. a just-dead child) are tolerated — the
/// session's `Exited` event ends the loop cleanly.
fn handle_event(event: Event, session: &mut LiveSession) {
    match event {
        Event::Key(key_event) => {
            if let Some(key_event) = keys::translate(&key_event) {
                let _ = session.send_key(key_event);
            }
        }
        Event::Resize(columns, rows) => {
            let _ = session.resize(Size::new(usize::from(columns), usize::from(rows)));
        }
        // Focus, paste and mouse events have no T1 handler.
        _ => {}
    }
}

/// Draw one frame: repaint the grid, apply the application-requested
/// cursor shape (only when it changed), and place the cursor.
fn render_frame(
    terminal: &mut Terminal<StyledBackend<io::Stdout>>,
    session: &LiveSession,
    underlines: &Rc<RefCell<UnderlineOverlay>>,
    last_cursor_shape: &mut Option<SetCursorStyle>,
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
            paint_session(frame.buffer_mut(), area, &grid, &mut overlay);
        }
        if cursor.shape != CursorShape::Hidden {
            let x = (cursor.column as u16).min(area.width.saturating_sub(1));
            let y = (cursor.line as u16).min(area.height.saturating_sub(1));
            frame.set_cursor_position((x, y));
        }
        // A hidden cursor gets no position → ratatui hides it this frame.
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
