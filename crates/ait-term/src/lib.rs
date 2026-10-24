//! `ait-term` — AIT's frontend-agnostic terminal core.
//!
//! A [`Session`] is a pure, byte-fed terminal: feed PTY output in with
//! [`Session::feed_bytes`], read the styled [`GridView`] back out. No PTY and
//! no TTY are involved, so the whole core is unit-testable (REQUIREMENTS.md
//! §5.1, §6). A live PTY driver wraps it in a later ticket.
//!
//! `alacritty_terminal` provides the VT emulation under the hood (ADR-0002)
//! and is a private dependency: every type on this API surface is ours.

mod grid;
mod session;

pub use grid::{
    AnsiColor, Cell, CellKind, Color, Cursor, CursorShape, Damage, DamageBounds, GridView, Rgb,
    Row, Style, Underline,
};
pub use session::{Session, SessionOptions, Size, DEFAULT_SCROLLBACK};
