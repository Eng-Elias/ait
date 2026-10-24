//! Host-terminal lifecycle: raw mode + alternate screen in, clean restore
//! out — on every exit path including panic (REQUIREMENTS.md §5.2, §6).
//!
//! [`TerminalGuard::enter`] prepares the host terminal and installs the
//! panic hook; [`Drop`] restores it. [`TerminalGuard::restore`] is
//! idempotent and unconditional — it emits every restore sequence even when
//! the matching feature was never enabled (disabling unenabled capture is
//! harmless), so panic paths cannot leave the host half-configured.
//!
//! Mouse capture is an explicit option: Round 2's mouse ticket flips it on.
//! Keyboard-enhancement flags are likewise a later-ticket concern; restore
//! already pops them so the future `Push` never strands state on the host.

use std::io::{self, Write};
use std::panic;
use std::sync::Once;

use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, PopKeyboardEnhancementFlags};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};

/// What the guard switches on while it lives.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalOptions {
    /// Capture the mouse (wheel, drags, clicks). Off for R02-T1 — the
    /// scrollback/selection ticket enables it and `Drop` releases it.
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
    /// Prepare the host terminal for the TUI.
    pub fn enter() -> io::Result<Self> {
        Self::with_options(TerminalOptions::default())
    }

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
        execute!(stdout, EnableMouseCapture)?;
    }
    Ok(())
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
