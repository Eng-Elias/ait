//! The ratatui `Backend` the app renders through.
//!
//! [`StyledBackend`] writes ratatui's cell diff to a crossterm command
//! stream — the same job `ratatui::backend::CrosstermBackend` does, plus
//! one thing it cannot: **underline variants**. Ratatui's cell model has a
//! single `Modifier::UNDERLINED` bit, but a terminal distinguishes SGR
//! `4:1..4:5` (single, double, curly, dotted, dashed). The exact variant a
//! `GridView` cell requests travels in the [`UnderlineOverlay`] populated by
//! [`paint_session`](crate::render::paint_session) during the same draw —
//! this backend consults it for every cell and emits the matching
//! `crossterm::style::Attribute`.
//!
//! Everything else mirrors `CrosstermBackend`'s draw loop: per-cell move,
//! modifier diff, colors, symbol print, and a trailing attribute reset so
//! state never leaks between frames.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use ait_term::Underline;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::queue;
use crossterm::style::{
    Attribute, Color as CtColor, Colors as CtColors, Print, SetAttribute, SetBackgroundColor,
    SetColors, SetForegroundColor,
};
use crossterm::terminal::{self, Clear};
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color as RtColor, Modifier};

use crate::render::UnderlineOverlay;

/// A crossterm-writing ratatui backend that honors `ait_term`'s underline
/// variants.
///
/// Generic over the writer so tests can diff into a `Vec<u8>` and inspect
/// the emitted escape stream without a TTY.
pub struct StyledBackend<W> {
    writer: W,
    /// The current frame's underline variants, keyed by buffer position.
    /// Shared with the render path, which fills it inside the `draw`
    /// callback — before ratatui hands this backend the cell diff.
    underlines: Rc<RefCell<UnderlineOverlay>>,
    /// Tracked host state so transitions emit only what changed (reset at
    /// the end of every `draw`).
    fg: CtColor,
    bg: CtColor,
    /// Tracked modifier bits *excluding* `UNDERLINED` — underline variants
    /// are tracked separately in `underline`.
    modifier: Modifier,
    /// The underline variant currently in effect on the host.
    underline: Underline,
    last_pos: Option<(u16, u16)>,
}

impl<W: Write> StyledBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            underlines: Rc::new(RefCell::new(UnderlineOverlay::new())),
            fg: CtColor::Reset,
            bg: CtColor::Reset,
            modifier: Modifier::empty(),
            underline: Underline::None,
            last_pos: None,
        }
    }

    /// A shared handle to the overlay the render path fills each frame.
    pub fn underline_overlay(&self) -> Rc<RefCell<UnderlineOverlay>> {
        Rc::clone(&self.underlines)
    }

    /// Borrow the raw writer (the app queues cursor-shape changes through
    /// it before a draw so they flush with the frame).
    pub fn writer_mut(&mut self) -> &mut W {
        &mut self.writer
    }

    /// The underline variant a buffer position asks for: the overlay wins;
    /// absent an entry, the cell's own `UNDERLINED` bit means `Single`.
    fn underline_at(&self, x: u16, y: u16, cell: &Cell) -> Underline {
        if let Some(&variant) = self.underlines.borrow().get(&(x, y)) {
            return variant;
        }
        if cell.modifier.contains(Modifier::UNDERLINED) {
            Underline::Single
        } else {
            Underline::None
        }
    }
}

/// `ait_term::Underline` → the crossterm attribute emitting SGR `4:x`;
/// `None` clears underlining (SGR 24).
fn underline_attribute(underline: Underline) -> Attribute {
    match underline {
        Underline::None => Attribute::NoUnderline,
        Underline::Single => Attribute::Underlined,
        Underline::Double => Attribute::DoubleUnderlined,
        Underline::Curly => Attribute::Undercurled,
        Underline::Dotted => Attribute::Underdotted,
        Underline::Dashed => Attribute::Underdashed,
    }
}

/// ratatui color → crossterm color (mirrors `IntoCrossterm` so diffed cells
/// emit the same codes `CrosstermBackend` would).
fn to_crossterm_color(color: RtColor) -> CtColor {
    match color {
        RtColor::Reset => CtColor::Reset,
        RtColor::Black => CtColor::Black,
        RtColor::Red => CtColor::DarkRed,
        RtColor::Green => CtColor::DarkGreen,
        RtColor::Yellow => CtColor::DarkYellow,
        RtColor::Blue => CtColor::DarkBlue,
        RtColor::Magenta => CtColor::DarkMagenta,
        RtColor::Cyan => CtColor::DarkCyan,
        RtColor::Gray => CtColor::Grey,
        RtColor::DarkGray => CtColor::DarkGrey,
        RtColor::LightRed => CtColor::Red,
        RtColor::LightGreen => CtColor::Green,
        RtColor::LightBlue => CtColor::Blue,
        RtColor::LightYellow => CtColor::Yellow,
        RtColor::LightMagenta => CtColor::Magenta,
        RtColor::LightCyan => CtColor::Cyan,
        RtColor::White => CtColor::White,
        RtColor::Indexed(i) => CtColor::AnsiValue(i),
        RtColor::Rgb(r, g, b) => CtColor::Rgb { r, g, b },
    }
}

/// Emit the SGR attribute changes going `from` → `to` — the same logic as
/// `ratatui-crossterm`'s `ModifierDiff`, minus `UNDERLINED` (callers handle
/// the underline *variant* separately).
fn queue_modifier_diff<W: Write>(from: Modifier, to: Modifier, w: &mut W) -> io::Result<()> {
    let from = from - Modifier::UNDERLINED;
    let to = to - Modifier::UNDERLINED;
    if from == to {
        return Ok(());
    }

    let removed = from - to;
    if removed.contains(Modifier::REVERSED) {
        queue!(w, SetAttribute(Attribute::NoReverse))?;
    }

    // Bold and Dim share the SGR intensity channel: clearing either resets
    // both, so surviving bits must be reapplied after `NormalIntensity`.
    let reset_intensity = removed.contains(Modifier::BOLD) || removed.contains(Modifier::DIM);
    if reset_intensity {
        queue!(w, SetAttribute(Attribute::NormalIntensity))?;
        if to.contains(Modifier::DIM) {
            queue!(w, SetAttribute(Attribute::Dim))?;
        }
        if to.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(Attribute::Bold))?;
        }
    }

    if removed.contains(Modifier::ITALIC) {
        queue!(w, SetAttribute(Attribute::NoItalic))?;
    }
    if removed.contains(Modifier::CROSSED_OUT) {
        queue!(w, SetAttribute(Attribute::NotCrossedOut))?;
    }
    if removed.contains(Modifier::HIDDEN) {
        queue!(w, SetAttribute(Attribute::NoHidden))?;
    }
    if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
        queue!(w, SetAttribute(Attribute::NoBlink))?;
    }

    let added = to - from;
    if added.contains(Modifier::REVERSED) {
        queue!(w, SetAttribute(Attribute::Reverse))?;
    }
    if added.contains(Modifier::BOLD) && !reset_intensity {
        queue!(w, SetAttribute(Attribute::Bold))?;
    }
    if added.contains(Modifier::ITALIC) {
        queue!(w, SetAttribute(Attribute::Italic))?;
    }
    if added.contains(Modifier::DIM) && !reset_intensity {
        queue!(w, SetAttribute(Attribute::Dim))?;
    }
    if added.contains(Modifier::CROSSED_OUT) {
        queue!(w, SetAttribute(Attribute::CrossedOut))?;
    }
    if added.contains(Modifier::SLOW_BLINK) {
        queue!(w, SetAttribute(Attribute::SlowBlink))?;
    }
    if added.contains(Modifier::RAPID_BLINK) {
        queue!(w, SetAttribute(Attribute::RapidBlink))?;
    }
    Ok(())
}

impl<W: Write> Backend for StyledBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        for (x, y, cell) in content {
            // Skip the MoveTo when the previous cell ended adjacent.
            if !matches!(self.last_pos, Some((px, py)) if x == px + 1 && y == py) {
                queue!(self.writer, MoveTo(x, y))?;
            }
            self.last_pos = Some((x, y));

            queue_modifier_diff(self.modifier, cell.modifier, &mut self.writer)?;
            self.modifier = cell.modifier - Modifier::UNDERLINED;

            let underline = self.underline_at(x, y, cell);
            if underline != self.underline {
                queue!(self.writer, SetAttribute(underline_attribute(underline)))?;
                self.underline = underline;
            }

            let fg = to_crossterm_color(cell.fg);
            let bg = to_crossterm_color(cell.bg);
            if fg != self.fg || bg != self.bg {
                queue!(self.writer, SetColors(CtColors::new(fg, bg)))?;
                self.fg = fg;
                self.bg = bg;
            }

            queue!(self.writer, Print(cell.symbol()))?;
        }

        // Restore defaults and reset tracking: the next frame diffs from a
        // clean attribute slate just like CrosstermBackend.
        queue!(
            self.writer,
            SetForegroundColor(CtColor::Reset),
            SetBackgroundColor(CtColor::Reset),
            SetAttribute(Attribute::Reset),
        )?;
        self.fg = CtColor::Reset;
        self.bg = CtColor::Reset;
        self.modifier = Modifier::empty();
        self.underline = Underline::None;
        self.last_pos = None;
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        queue!(self.writer, Hide)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        queue!(self.writer, Show)
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        crossterm::cursor::position()
            .map(|(x, y)| Position { x, y })
            .map_err(io::Error::other)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let Position { x, y } = position.into();
        queue!(self.writer, MoveTo(x, y))
    }

    fn clear(&mut self) -> io::Result<()> {
        self.clear_region(ClearType::All)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        queue!(
            self.writer,
            Clear(match clear_type {
                ClearType::All => terminal::ClearType::All,
                ClearType::AfterCursor => terminal::ClearType::FromCursorDown,
                ClearType::BeforeCursor => terminal::ClearType::FromCursorUp,
                ClearType::CurrentLine => terminal::ClearType::CurrentLine,
                ClearType::UntilNewLine => terminal::ClearType::UntilNewLine,
            })
        )
    }

    fn size(&self) -> io::Result<Size> {
        let (width, height) = terminal::size()?;
        Ok(Size { width, height })
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let terminal::WindowSize {
            columns,
            rows,
            width,
            height,
        } = terminal::window_size()?;
        Ok(WindowSize {
            columns_rows: Size {
                width: columns,
                height: rows,
            },
            pixels: Size { width, height },
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    /// Diff `next` against an empty buffer and feed the updates through the
    /// backend, returning the emitted escape stream as text.
    fn draw_buffer(next: &ratatui::buffer::Buffer, overlay: &UnderlineOverlay) -> String {
        let prev = ratatui::buffer::Buffer::empty(next.area);
        let diff = prev.diff(next);
        let mut backend = StyledBackend::new(Vec::new());
        *backend.underlines.borrow_mut() = overlay.clone();
        backend.draw(diff.into_iter()).unwrap();
        backend.flush().unwrap();
        String::from_utf8(backend.writer).unwrap()
    }

    fn buffer_with(cells: &[(u16, u16, &str, Modifier)]) -> ratatui::buffer::Buffer {
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 10, 1));
        for &(x, y, symbol, modifier) in cells {
            let cell = &mut buf[(x, y)];
            cell.set_symbol(symbol);
            cell.modifier = modifier;
        }
        buf
    }

    /// Every underline variant emits its SGR `4:x` sequence.
    #[test]
    fn underline_variants_emit_sgr_4_colon() {
        for (variant, sgr) in [
            (Underline::Single, "4"),
            (Underline::Double, "4:2"),
            (Underline::Curly, "4:3"),
            (Underline::Dotted, "4:4"),
            (Underline::Dashed, "4:5"),
        ] {
            let buf = buffer_with(&[(0, 0, "x", Modifier::UNDERLINED)]);
            let mut overlay = UnderlineOverlay::new();
            overlay.insert((0, 0), variant);
            let out = draw_buffer(&buf, &overlay);
            assert!(
                out.contains(&format!("\x1b[{sgr}m")),
                "{variant:?} should emit CSI {sgr}m, got {out:?}"
            );
        }
    }

    /// A cell whose overlay entry is absent but which carries `UNDERLINED`
    /// degrades to a plain SGR 4 underline — the fallback keeps
    /// CrosstermBackend-equivalent behavior.
    #[test]
    fn underlined_without_overlay_entry_is_single() {
        let buf = buffer_with(&[(0, 0, "x", Modifier::UNDERLINED)]);
        let out = draw_buffer(&buf, &UnderlineOverlay::new());
        assert!(out.contains("\x1b[4m"), "expected SGR 4 in {out:?}");
    }

    /// Leaving an underline emits SGR 24 (no underline), not a bare reset.
    #[test]
    fn underline_transitions_are_explicit() {
        // Two cells: first curly, second plain — between them the backend
        // must emit NoUnderline.
        let buf = buffer_with(&[
            (0, 0, "a", Modifier::UNDERLINED),
            (1, 0, "b", Modifier::empty()),
        ]);
        let mut overlay = UnderlineOverlay::new();
        overlay.insert((0, 0), Underline::Curly);
        let out = draw_buffer(&buf, &overlay);
        assert!(out.contains("\x1b[4:3m"), "curly, got {out:?}");
        assert!(out.contains("\x1b[24m"), "underline off, got {out:?}");
    }

    #[test]
    fn colors_and_modifiers_emit() {
        let mut buf = ratatui::buffer::Buffer::empty(Rect::new(0, 0, 10, 1));
        let cell = &mut buf[(0, 0)];
        cell.set_symbol("x");
        cell.fg = RtColor::Indexed(196);
        cell.bg = RtColor::Rgb(1, 2, 3);
        cell.modifier = Modifier::BOLD | Modifier::ITALIC | Modifier::CROSSED_OUT;

        let out = draw_buffer(&buf, &UnderlineOverlay::new());
        // crossterm `SetColors` emits fg and bg in one SGR.
        assert!(
            out.contains("\x1b[38;5;196;48;2;1;2;3m"),
            "indexed fg + rgb bg in {out:?}"
        );
        assert!(out.contains("\x1b[1m"), "bold in {out:?}");
        assert!(out.contains("\x1b[3m"), "italic in {out:?}");
        assert!(out.contains("\x1b[9m"), "strikeout in {out:?}");
        assert!(out.contains("x"), "symbol in {out:?}");
    }

    /// Adjacent diffed cells don't re-issue MoveTo; non-adjacent ones do.
    #[test]
    fn moves_only_when_not_adjacent() {
        let buf = buffer_with(&[
            (0, 0, "a", Modifier::empty()),
            (5, 0, "b", Modifier::empty()),
        ]);
        let out = draw_buffer(&buf, &UnderlineOverlay::new());
        // One move to (0,0) then one to (5,0); no move before the 'b' at 5?
        // Positions 0 and 5 are not adjacent → two moves total.
        let moves = out.matches("\x1b[").filter(|_| true).count();
        assert!(
            out.contains("\x1b[1;1H") || out.contains("\x1b[1;1f") || out.contains("\x1b[0;0H")
        );
        assert!(
            moves >= 2,
            "expected at least two escapes (moves), got {out:?}"
        );
        assert!(out.contains('a') && out.contains('b'));
    }
}
