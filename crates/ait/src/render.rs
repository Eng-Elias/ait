//! `GridView` → ratatui `Buffer` painting (Spec R02 render seam).
//!
//! [`paint_session`] copies a session's visible grid into a ratatui buffer:
//! text via `Row::text` (keeps zero-width/combining characters riding on
//! their base cell, so ZWJ emoji render whole), then per-cell colors and
//! style flags. Wide characters are handled by ratatui's own grapheme
//! sizing; `CellKind::WideSpacer` columns are covered automatically.
//!
//! ratatui's `Modifier::UNDERLINED` is a single bit — it cannot carry *which*
//! underline an application requested. Every non-`None` underline therefore
//! sets that bit (so plain backends still underline) *and* records the real
//! variant in the [`UnderlineOverlay`], which [`StyledBackend`](crate::backend::StyledBackend)
//! reads when emitting SGR `4:x` attributes.

use std::collections::HashMap;

use ait_term::{AnsiColor, CellKind, Color, CursorShape, GridView, Style, Underline};
use crossterm::cursor::SetCursorStyle;

use crate::selection::SelectionRange;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color as RtColor, Modifier};

/// Per-position map of the *exact* underline variant on the current frame.
///
/// Filled by [`paint_session`], consumed by
/// [`StyledBackend::draw`](crate::backend::StyledBackend). Only cells whose
/// underline differs from `None` appear — a missing key means "whatever
/// `Modifier::UNDERLINED` on the buffer cell says".
pub type UnderlineOverlay = HashMap<(u16, u16), Underline>;

/// Paint the session's grid into `buf`, covering `area`, shifted
/// `scroll_offset` lines into scrollback (`0` = live viewport).
///
/// Screen row `i` displays `grid.line(i - scroll_offset)` — a positive
/// offset pulls history lines in from the top. `area` is the region the
/// grid occupies (the whole frame for the single-session skeleton; a
/// smaller rect once chrome lands). Cells past the grid's edge — e.g. a
/// just-grown frame before the session's resize is processed — are reset
/// to blank.
pub fn paint_session(
    buf: &mut Buffer,
    area: Rect,
    grid: &GridView<'_>,
    overlay: &mut UnderlineOverlay,
    scroll_offset: usize,
) {
    // `offset` can't exceed i32 (scrollback depth is small), but guard the
    // cast anyway — a pathological offset just paints blanks.
    let offset = i32::try_from(scroll_offset).unwrap_or(i32::MAX);
    let viewport_rows = grid.lines().min(usize::from(area.height));
    for row_index in 0..viewport_rows {
        let y = area.y + row_index as u16;
        let line_index = row_index as i32 - offset;
        let Some(row) = grid.line(line_index) else {
            // Above the oldest scrollback line (offset deeper than the
            // history, e.g. right after a shrink) — paint blank.
            for x in area.x..area.right() {
                buf[(x, y)].reset();
            }
            continue;
        };
        // Text first: `set_stringn` sizes graphemes itself, so wide
        // characters occupy two columns and zero-width characters stay
        // attached to their base grapheme.
        let width = usize::from(area.right() - area.x);
        buf.set_stringn(
            area.x,
            y,
            row.text(),
            width,
            ratatui::style::Style::default(),
        );
        // Then per-cell styling, column-aligned with the grid.
        for (col, cell) in row.cells().enumerate() {
            if col >= usize::from(area.right() - area.x) {
                break;
            }
            if cell.kind == CellKind::WideSpacer {
                // Covered by the wide grapheme's cell; skip it — the
                // overlay and diff only care about the leading cell.
                continue;
            }
            let x = area.x + col as u16;
            let target = &mut buf[(x, y)];
            target.fg = to_ratatui_color(cell.fg);
            target.bg = to_ratatui_color(cell.bg);
            target.modifier = to_modifier(cell.style);
            if cell.style.underline != Underline::None {
                overlay.insert((x, y), cell.style.underline);
            }
        }
    }
    // Clear any rows the grid doesn't cover (resize slack).
    for y in (area.y + viewport_rows as u16)..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)].reset();
        }
    }
}

/// Highlight a mouse-selection range by reversing the covered buffer
/// cells — the frontend-only drag highlight (Spec R02).
///
/// Same coordinate rule as [`paint_session`]: screen row `i` shows grid
/// line `i - scroll_offset`. The range lives in grid coordinates, so a
/// selection spanning scrollback highlights exactly where its rows
/// appear. A selected [`CellKind::WideSpacer`] reverses its leading cell
/// instead — the buffer cell under a wide grapheme's trailing column has
/// no symbol of its own, and the lead is what the host inverts.
pub fn apply_selection(
    buf: &mut Buffer,
    area: Rect,
    grid: &GridView<'_>,
    range: SelectionRange,
    scroll_offset: usize,
) {
    let offset = i32::try_from(scroll_offset).unwrap_or(i32::MAX);
    let viewport_rows = grid.lines().min(usize::from(area.height));
    for row_index in 0..viewport_rows {
        let line = row_index as i32 - offset;
        let y = area.y + row_index as u16;
        for col in 0..usize::from(area.right() - area.x) {
            if !range.contains(line, col) {
                continue;
            }
            let x = area.x + col as u16;
            buf[(x, y)].modifier |= Modifier::REVERSED;
            // The spacer half of a wide char renders nothing itself;
            // carry the highlight onto the grapheme's leading cell.
            if grid
                .cell(line, col)
                .is_some_and(|cell| cell.kind == CellKind::WideSpacer)
                && x > area.x
            {
                buf[(x - 1, y)].modifier |= Modifier::REVERSED;
            }
        }
    }
}

/// `ait_term` color → ratatui color. Beware the naming trap: ratatui's
/// `Red`/`Green`/… are the *normal* ANSI colors (SGR 3x) while
/// `LightRed`/… are the bright variants (SGR 9x) — the same convention as
/// `ratatui::backend::IntoCrossterm`.
pub fn to_ratatui_color(color: Color) -> RtColor {
    match color {
        Color::Default => RtColor::Reset,
        Color::Indexed(index) => RtColor::Indexed(index),
        Color::Rgb(rgb) => RtColor::Rgb(rgb.r, rgb.g, rgb.b),
        Color::Ansi(ansi) => match ansi {
            AnsiColor::Black => RtColor::Black,
            AnsiColor::Red => RtColor::Red,
            AnsiColor::Green => RtColor::Green,
            AnsiColor::Yellow => RtColor::Yellow,
            AnsiColor::Blue => RtColor::Blue,
            AnsiColor::Magenta => RtColor::Magenta,
            AnsiColor::Cyan => RtColor::Cyan,
            AnsiColor::White => RtColor::Gray,
            AnsiColor::BrightBlack => RtColor::DarkGray,
            AnsiColor::BrightRed => RtColor::LightRed,
            AnsiColor::BrightGreen => RtColor::LightGreen,
            AnsiColor::BrightYellow => RtColor::LightYellow,
            AnsiColor::BrightBlue => RtColor::LightBlue,
            AnsiColor::BrightMagenta => RtColor::LightMagenta,
            AnsiColor::BrightCyan => RtColor::LightCyan,
            AnsiColor::BrightWhite => RtColor::White,
        },
    }
}

/// `ait_term` style flags → ratatui modifier bits. Every underline variant
/// sets `UNDERLINED`; the variant itself rides in the overlay.
fn to_modifier(style: Style) -> Modifier {
    let mut modifier = Modifier::empty();
    if style.bold {
        modifier |= Modifier::BOLD;
    }
    if style.dim {
        modifier |= Modifier::DIM;
    }
    if style.italic {
        modifier |= Modifier::ITALIC;
    }
    if style.underline != Underline::None {
        modifier |= Modifier::UNDERLINED;
    }
    if style.inverse {
        modifier |= Modifier::REVERSED;
    }
    if style.hidden {
        modifier |= Modifier::HIDDEN;
    }
    if style.strikeout {
        modifier |= Modifier::CROSSED_OUT;
    }
    modifier
}

/// Application-requested cursor shape → crossterm `SetCursorStyle`.
///
/// DECSCUSR has no hollow block: [`CursorShape::HollowBlock`] maps to a
/// steady block (a terminal never *requests* hollow — the emulator uses it
/// for its own "unfocused" rendering). [`CursorShape::Hidden`] has no
/// `SetCursorStyle` — the app hides the cursor instead.
pub fn to_cursor_style(shape: CursorShape) -> Option<SetCursorStyle> {
    match shape {
        CursorShape::Block | CursorShape::HollowBlock => Some(SetCursorStyle::SteadyBlock),
        CursorShape::Underline => Some(SetCursorStyle::SteadyUnderScore),
        CursorShape::Beam => Some(SetCursorStyle::SteadyBar),
        CursorShape::Hidden => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::{GridPoint, SelectionRange};
    use ait_term::{Session, SessionOptions, Size};

    /// Byte-fed session fixture (the R01 seam): feed PTY output bytes, get a
    /// grid to paint.
    fn session(columns: usize, lines: usize) -> Session {
        Session::new(SessionOptions {
            size: Size::new(columns, lines),
            ..Default::default()
        })
    }

    fn paint(session: &Session, width: u16, height: u16) -> (Buffer, UnderlineOverlay) {
        paint_scrolled(session, width, height, 0)
    }

    fn paint_scrolled(
        session: &Session,
        width: u16,
        height: u16,
        scroll_offset: usize,
    ) -> (Buffer, UnderlineOverlay) {
        let mut buf = Buffer::empty(Rect::new(0, 0, width, height));
        let mut overlay = UnderlineOverlay::new();
        let area = buf.area;
        paint_session(&mut buf, area, &session.grid(), &mut overlay, scroll_offset);
        (buf, overlay)
    }

    #[test]
    fn text_lands_in_the_buffer() {
        let mut session = session(80, 24);
        session.feed_bytes(b"hi");
        let (buf, _) = paint(&session, 80, 24);

        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(1, 0)].symbol(), "i");
        assert_eq!(buf[(2, 0)].symbol(), " ");
    }

    #[test]
    fn named_ansi_colors_map() {
        let mut session = session(80, 24);
        session.feed_bytes(b"\x1b[31;104mX");
        let (buf, _) = paint(&session, 80, 24);

        assert_eq!(buf[(0, 0)].fg, RtColor::Red);
        assert_eq!(buf[(0, 0)].bg, RtColor::LightBlue);
        // ratatui naming: normal ANSI colors are Red/…, bright are Light*.
        assert_eq!(
            to_ratatui_color(Color::Ansi(AnsiColor::White)),
            RtColor::Gray
        );
        assert_eq!(
            to_ratatui_color(Color::Ansi(AnsiColor::BrightWhite)),
            RtColor::White
        );
        assert_eq!(
            to_ratatui_color(Color::Ansi(AnsiColor::BrightBlack)),
            RtColor::DarkGray
        );
    }

    #[test]
    fn indexed_and_rgb_colors_map() {
        let mut session = session(80, 24);
        session.feed_bytes(b"\x1b[38;5;196mA\x1b[48;2;10;20;30mB");
        let (buf, _) = paint(&session, 80, 24);

        assert_eq!(buf[(0, 0)].fg, RtColor::Indexed(196));
        assert_eq!(buf[(1, 0)].bg, RtColor::Rgb(10, 20, 30));
    }

    #[test]
    fn default_colors_are_reset() {
        let mut session = session(80, 24);
        session.feed_bytes(b"x");
        let (buf, _) = paint(&session, 80, 24);

        assert_eq!(buf[(0, 0)].fg, RtColor::Reset);
        assert_eq!(buf[(0, 0)].bg, RtColor::Reset);
    }

    #[test]
    fn style_flags_become_modifiers() {
        let mut session = session(80, 24);
        // bold, dim, italic, single underline, inverse, hidden, strikeout.
        session.feed_bytes(b"\x1b[1mB\x1b[2mD\x1b[3mI\x1b[4mU\x1b[7mR\x1b[8mH\x1b[9mS\x1b[0mN");
        let (buf, _) = paint(&session, 80, 24);

        let at = |x: u16| buf[(x, 0)].modifier;
        assert!(at(0).contains(Modifier::BOLD));
        // DIM persists on following cells until reset — read each flag.
        assert!(at(1).contains(Modifier::DIM));
        assert!(at(2).contains(Modifier::ITALIC));
        assert!(at(3).contains(Modifier::UNDERLINED));
        assert!(at(4).contains(Modifier::REVERSED));
        assert!(at(5).contains(Modifier::HIDDEN));
        assert!(at(6).contains(Modifier::CROSSED_OUT));
        assert_eq!(at(7), Modifier::empty());
    }

    /// SGR 58 sets an underline *color*; alacritty reports it via the cell's
    /// fg/bg… actually it surfaces nowhere in ait-term's `Style` — the
    /// overlay only carries the variant. Single underline lands as
    /// `Modifier::UNDERLINED` + overlay `Single`.
    #[test]
    fn underline_variants_land_in_the_overlay() {
        let mut session = session(80, 24);
        // 4:0 none, 4:1 single, 4:2 double, 4:3 curly, 4:4 dotted, 4:5 dashed.
        session.feed_bytes(b"\x1b[4:1mA\x1b[4:2mB\x1b[4:3mC\x1b[4:4mD\x1b[4:5mE\x1b[0mF");
        let (buf, overlay) = paint(&session, 80, 24);

        assert_eq!(overlay.get(&(0, 0)), Some(&Underline::Single), "single");
        assert_eq!(overlay.get(&(1, 0)), Some(&Underline::Double));
        assert_eq!(overlay.get(&(2, 0)), Some(&Underline::Curly));
        assert_eq!(overlay.get(&(3, 0)), Some(&Underline::Dotted));
        assert_eq!(overlay.get(&(4, 0)), Some(&Underline::Dashed));
        assert_eq!(overlay.get(&(5, 0)), None);
        // Every variant still sets the buffer's UNDERLINED bit so the cell
        // style is truthful even without the overlay.
        for x in 0..5 {
            assert!(
                buf[(x, 0)].modifier.contains(Modifier::UNDERLINED),
                "cell {x} should carry UNDERLINED"
            );
        }
    }

    #[test]
    fn wide_characters_occupy_two_columns() {
        let mut session = session(80, 24);
        session.feed_bytes("コx".as_bytes());
        let (buf, _) = paint(&session, 80, 24);

        assert_eq!(buf[(0, 0)].symbol(), "コ");
        // The spacer column belongs to the wide glyph; 'x' lands at col 2,
        // matching the grid.
        assert_eq!(buf[(2, 0)].symbol(), "x");
    }

    /// Emoji with zero-width riders (VS16 / ZWJ parts) render as one
    /// grapheme, not as their base char alone.
    #[test]
    fn zero_width_riders_stay_attached() {
        let mut session = session(80, 24);
        session.feed_bytes("☑️x".as_bytes()); // ☑ + VS16 + x
        let (buf, _) = paint(&session, 80, 24);

        let symbol = buf[(0, 0)].symbol();
        assert!(
            symbol.starts_with('☑'),
            "expected the grapheme on the buffer cell, got {symbol:?}"
        );
        assert!(
            symbol.contains('\u{fe0f}'),
            "variation selector must ride along, got {symbol:?}"
        );
    }

    /// Rows the grid doesn't cover get blanked (fresh-frame slack).
    #[test]
    fn uncovered_rows_are_cleared() {
        let mut session = session(10, 2);
        session.feed_bytes(b"top");
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 5));
        // Pre-fill a row the grid doesn't reach.
        buf[(0, 4)].set_symbol("!");
        let mut overlay = UnderlineOverlay::new();
        let area = buf.area;
        paint_session(&mut buf, area, &session.grid(), &mut overlay, 0);

        assert_eq!(buf[(0, 4)].symbol(), " ");
    }

    /// A scrolled-up view pulls scrollback rows in from the top: screen row
    /// `i` shows `grid.line(i - offset)` — styled cells included.
    #[test]
    fn scroll_offset_paints_scrollback_rows() {
        let mut session = session(80, 4);
        // Seven numbered lines on a 4-line viewport: each trailing CRLF
        // scrolls once at the bottom, so line0..line3 end in scrollback
        // and line4..line6 fill the live viewport.
        for n in 0..7 {
            session.feed_bytes(format!("line{n}\r\n").as_bytes());
        }
        assert_eq!(session.grid().scrollback(), 4);

        let (buf, _) = paint_scrolled(&session, 80, 4, 2);
        // Screen rows 0-1 show the last two scrollback lines; the rest of
        // the viewport shifted down.
        let row0: String = (0..5).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        let row1: String = (0..5).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        let row2: String = (0..5).map(|x| buf[(x, 2)].symbol().to_string()).collect();
        assert_eq!(row0, "line2");
        assert_eq!(row1, "line3");
        assert_eq!(row2, "line4");
    }

    /// Scrollback cells keep their colors and styles, not just their text.
    #[test]
    fn scroll_offset_keeps_scrollback_styles() {
        let mut session = session(80, 2);
        session.feed_bytes(b"\x1b[31mRED\r\nplain\r\n");
        assert!(session.grid().scrollback() >= 1);

        let (buf, _) = paint_scrolled(&session, 80, 2, 1);
        assert_eq!(buf[(0, 0)].symbol(), "R");
        assert_eq!(buf[(0, 0)].fg, RtColor::Red);
        assert_eq!(buf[(0, 1)].symbol(), "p");
    }

    /// Scrolled to the top of history, the oldest scrollback line leads;
    /// an offset beyond the depth paints blanks rather than panicking.
    #[test]
    fn scroll_offset_at_top_and_beyond() {
        let mut session = session(80, 2);
        for n in 0..5 {
            session.feed_bytes(format!("row{n}\r\n").as_bytes());
        }
        let depth = session.grid().scrollback();
        assert!(depth >= 3);

        let (buf, _) = paint_scrolled(&session, 80, 2, depth);
        let row0: String = (0..4).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row0, "row0", "deepest offset shows the oldest line");

        let (buf, _) = paint_scrolled(&session, 80, 2, depth + 100);
        assert_eq!(buf[(0, 0)].symbol(), " ", "beyond depth paints blank");
    }

    /// Selected cells render inverted, edge columns bounded by the
    /// range — the cell-accurate highlight.
    #[test]
    fn selection_reverses_covered_cells() {
        let mut session = session(80, 24);
        session.feed_bytes(b"ab\r\ncd\r\nef");
        let (mut buf, _) = paint(&session, 80, 24);

        let range = SelectionRange {
            start: GridPoint::new(0, 1),
            end: GridPoint::new(1, 0),
        };
        let area = buf.area;
        apply_selection(&mut buf, area, &session.grid(), range, 0);

        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(1, 0)].modifier.contains(Modifier::REVERSED));
        assert!(buf[(2, 0)].modifier.contains(Modifier::REVERSED));
        // Line 1 is inside only through its first column; line 2 is out.
        assert!(buf[(0, 1)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(1, 1)].modifier.contains(Modifier::REVERSED));
        assert!(!buf[(0, 2)].modifier.contains(Modifier::REVERSED));
    }

    /// The highlight respects the scroll offset: a selection reaching
    /// into scrollback paints the scrollback rows where they appear.
    #[test]
    fn selection_paints_scrollback_rows() {
        let mut session = session(80, 2);
        session.feed_bytes(b"one\r\ntwo\r\nthree");
        let (mut buf, _) = paint_scrolled(&session, 80, 2, 1);
        // Screen row i shows grid line (i - 1): row 0 is scrollback "one".
        let range = SelectionRange {
            start: GridPoint::new(-1, 0),
            end: GridPoint::new(-1, 2),
        };
        let area = buf.area;
        apply_selection(&mut buf, area, &session.grid(), range, 1);

        for x in 0..3 {
            assert!(
                buf[(x, 0)].modifier.contains(Modifier::REVERSED),
                "scrollback row cell {x} highlighted"
            );
            assert!(!buf[(x, 1)].modifier.contains(Modifier::REVERSED));
        }
    }

    /// A wide char's spacer cell highlights through its leading cell —
    /// the grapheme occupies both columns visually.
    #[test]
    fn selection_on_wide_char_covers_both_columns() {
        let mut session = session(80, 24);
        session.feed_bytes("コx".as_bytes());
        let (mut buf, _) = paint(&session, 80, 24);

        // Select only the spacer cell (col 1): its lead cell renders the
        // grapheme, so the lead carries the highlight.
        let range = SelectionRange {
            start: GridPoint::new(0, 1),
            end: GridPoint::new(0, 1),
        };
        let area = buf.area;
        apply_selection(&mut buf, area, &session.grid(), range, 0);

        assert!(
            buf[(0, 0)].modifier.contains(Modifier::REVERSED),
            "spacer selection highlights the wide glyph's lead cell"
        );
        assert!(!buf[(2, 0)].modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn cursor_shape_mapping() {
        assert_eq!(
            to_cursor_style(CursorShape::Block),
            Some(SetCursorStyle::SteadyBlock)
        );
        assert_eq!(
            to_cursor_style(CursorShape::HollowBlock),
            Some(SetCursorStyle::SteadyBlock)
        );
        assert_eq!(
            to_cursor_style(CursorShape::Underline),
            Some(SetCursorStyle::SteadyUnderScore)
        );
        assert_eq!(
            to_cursor_style(CursorShape::Beam),
            Some(SetCursorStyle::SteadyBar)
        );
        assert_eq!(to_cursor_style(CursorShape::Hidden), None);
    }
}
