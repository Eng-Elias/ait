//! Host-terminal lifecycle: raw mode + alternate screen in, clean restore
//! out — on every exit path including panic (REQUIREMENTS.md §5.2, §6).
//!
//! [`TerminalGuard::with_options`] prepares the host terminal and installs
//! the panic hook; [`Drop`] restores it. [`TerminalGuard::restore`] is
//! idempotent and unconditional — it emits every restore sequence even when
//! the matching feature was never enabled (disabling unenabled capture is
//! harmless), so panic paths cannot leave the host half-configured.
//!
//! Mouse capture is an explicit option ([`TerminalOptions::mouse_capture`]);
//! the keyboard-enhancement flags are detected and pushed at startup by
//! [`enable_keyboard_enhancement`]. Restore pops the flags and releases the
//! mouse unconditionally, so neither can strand state on the host.

use std::io::{self, Write};
use std::panic;
use std::sync::Once;

use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    DisableMouseCapture, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

/// What the guard switches on while it lives.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalOptions {
    /// Capture the mouse (wheel, drags, clicks). Raises `?1002h` +
    /// `?1006h` — button-event tracking with SGR coordinates — and `Drop`
    /// releases it.
    pub mouse_capture: bool,
}

/// RAII owner of the host terminal's UI state.
///
/// While it lives: raw mode is on and the alternate screen is active.
/// `drop` (or the panic hook) hands the terminal back: alternate screen
/// left, mouse released, cursor shape reset to the user's default and made
/// visible, raw mode off.
pub struct TerminalGuard {
    // Private so construction only happens through `enter`/`with_options`.
    _private: (),
}

impl TerminalGuard {
    /// Prepare the host terminal with explicit options.
    ///
    /// On failure the half-entered state is undone before the error
    /// propagates — a partially-entered TUI must never strand the terminal.
    pub fn with_options(options: TerminalOptions) -> io::Result<Self> {
        install_panic_hook();
        let mut stdout = io::stdout();
        let result = (|| -> io::Result<()> {
            enable_raw_mode()?;
            queue_commands(&mut stdout, options)?;
            Ok(())
        })();
        if result.is_err() {
            Self::restore();
            result?;
        }
        Ok(Self { _private: () })
    }

    /// Restore the host terminal to its pre-TUI state. Idempotent: emits
    /// every restore sequence unconditionally, so it is also what the panic
    /// hook runs (REQUIREMENTS.md §6 — "panics restore the host terminal").
    pub fn restore() {
        let mut stdout = io::stdout();
        // Order matters: release captures/screen first (still in raw mode,
        // so the sequences are cheap), then leave raw mode, then flush.
        let _ = execute!(
            stdout,
            DisableMouseCapture,
            PopKeyboardEnhancementFlags,
            SetCursorStyle::DefaultUserShape,
            Show,
            LeaveAlternateScreen,
        );
        let _ = disable_raw_mode();
        let _ = stdout.flush();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        Self::restore();
    }
}

/// Emit the enter-side commands for `options`.
fn queue_commands(stdout: &mut impl Write, options: TerminalOptions) -> io::Result<()> {
    execute!(stdout, EnterAlternateScreen)?;
    if options.mouse_capture {
        // Deliberately NOT crossterm's `EnableMouseCapture`: that also
        // raises `?1003h` any-motion tracking, which floods the poll loop
        // with `Moved` events we never use. `?1002h` reports clicks,
        // drags and the wheel (all T2/T3 need); `?1006h` is the SGR
        // extended-coordinate mode every modern host prefers.
        // `DisableMouseCapture` on restore releases both (it clears the
        // whole 1000-family set).
        stdout.write_all(b"\x1b[?1002h\x1b[?1006h")?;
        stdout.flush()?;
    }
    Ok(())
}

/// Detect kitty keyboard-enhancement support and, when confirmed, push
/// the disambiguation flags. Returns whether the enhanced keymap tier is
/// active.
///
/// `supports_keyboard_enhancement` is the only public detection crossterm
/// offers: it queries `CSI ? u` (+ a primary-DA fallback probe) on
/// `/dev/tty` and waits ≤ 2 s for the reply. Where the query is refused —
/// legacy terminals, and tmux ≤ 3.5a which never answers it inside a
/// pane — it returns `Err` on timeout, so `Err` and `Ok(false)` both
/// degrade to the legacy tier. Input is never disturbed: the response
/// events are consumed internally, and the query only runs here, once,
/// before the event loop.
pub fn enable_keyboard_enhancement() -> bool {
    match crossterm::terminal::supports_keyboard_enhancement() {
        Ok(true) => {
            // DISAMBIGUATE alone is what the enhanced keymap tier needs:
            // Ctrl+Shift+letter arrives as CSI-u, distinct from
            // Ctrl+letter. REPORT_EVENT_TYPES (Release/Repeat) would need
            // extra filtering for no Round-2 benefit.
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )
            .is_ok()
        }
        _ => false,
    }
}

/// Chain our terminal-restore onto the existing panic hook — once.
///
/// The hook restores the host terminal *before* delegating to the previous
/// hook, so the panic message prints on the user's normal screen rather
/// than inside the alternate screen.
fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            TerminalGuard::restore();
            previous(info);
        }));
    });
}
