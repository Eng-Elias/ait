//! `ait-term` — AIT's frontend-agnostic terminal core.
//!
//! A [`Session`] is a pure, byte-fed terminal: feed PTY output in with
//! [`Session::feed_bytes`], read the styled [`GridView`] back out, and
//! collect observed [`SessionEvent`]s (OSC 7/9/52/133/…). No PTY and no TTY
//! are involved, so the whole core is unit-testable (REQUIREMENTS.md
//! §5.1, §6). [`LiveSession`] is the same session driven by a real PTY:
//! it spawns `$SHELL` (fallback `/bin/sh`) behind `portable-pty` and pumps
//! output in through `drain_events`, surfacing [`LiveEvent`]s — synchronous,
//! no async runtime.
//!
//! `alacritty_terminal` provides the VT emulation under the hood (ADR-0002)
//! and `portable-pty` the PTY; both are private dependencies: every type on
//! this API surface is ours.

mod events;
mod grid;
mod key;
mod live;
mod session;

pub use events::{ClipboardSelection, PromptMarkKind, SessionEvent};
pub use grid::{
    AnsiColor, Cell, CellKind, Color, Cursor, CursorShape, Damage, DamageBounds, GridView, Rgb,
    Row, Style, Underline,
};
pub use key::{Key, KeyEvent, Keypad, Modifiers};
pub use live::{ExitStatus, LiveEvent, LiveSession, LiveSessionOptions, SessionState, SpawnError};
pub use session::{Session, SessionOptions, Size, DEFAULT_SCROLLBACK};
