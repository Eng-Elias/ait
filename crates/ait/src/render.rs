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
use std::ops::Range;

use ait_term::{AnsiColor, CellKind, Color, CursorShape, GridView, Size, Style, Underline};
use crossterm::cursor::SetCursorStyle;

use ait_app::conversations::ConversationPicker;
use ait_app::profile::ProfilePicker;
use ait_app::selection::SelectionRange;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color as RtColor, Modifier};

/// Per-position map of the *exact* underline variant on the current frame.
///
/// Filled by [`paint_session`], consumed by
/// [`StyledBackend::draw`](crate::backend::StyledBackend). Only cells whose
/// underline differs from `None` appear — a missing key means "whatever
/// `Modifier::UNDERLINED` on the buffer cell says".
pub type UnderlineOverlay = HashMap<(u16, u16), Underline>;

/// Blank `xs` columns on row `y` — the reset every painter uses for
/// uncovered tails and stale cells.
fn reset_span(buf: &mut Buffer, y: u16, xs: Range<u16>) {
    for x in xs {
        buf[(x, y)].reset();
    }
}

/// The one screen→grid rule, shared by painting and mouse hit-testing:
/// the content-band screen row `row` displays
/// `grid.line(row - content.y - scroll_offset)` — line `0` is the
/// viewport's first line, negatives reach into scrollback. The reverse
/// direction (grid line → screen row, used to place the live cursor) is
/// `content.y + line + scroll_offset`.
///
/// A `scroll_offset` past `i32::MAX` clamps the same way the painters do.
pub fn grid_line(row: u16, content: Rect, scroll_offset: usize) -> i32 {
    let offset = i32::try_from(scroll_offset).unwrap_or(i32::MAX);
    i32::from(row) - i32::from(content.y) - offset
}

/// Paint the session's grid into `buf`, covering `area`, shifted
/// `scroll_offset` lines into scrollback (`0` = live viewport).
///
/// Screen row `i` displays `grid.line(i - scroll_offset)` — a positive
/// offset pulls history lines in from the top. `area` is the region the
/// grid occupies — the content band [`chrome_areas`] carves between the
/// tab bar and the status line. Cells past the grid's edge — e.g. a
/// just-grown frame before the session's resize is processed — are reset
/// to blank.
pub fn paint_session(
    buf: &mut Buffer,
    area: Rect,
    grid: &GridView<'_>,
    overlay: &mut UnderlineOverlay,
    scroll_offset: usize,
) {
    let viewport_rows = grid.lines().min(usize::from(area.height));
    for row_index in 0..viewport_rows {
        let y = area.y + row_index as u16;
        let line_index = grid_line(y, area, scroll_offset);
        let Some(row) = grid.line(line_index) else {
            // Above the oldest scrollback line (offset deeper than the
            // history, e.g. right after a shrink) — paint blank.
            reset_span(buf, y, area.x..area.right());
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
        reset_span(buf, y, area.x..area.right());
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
    let viewport_rows = grid.lines().min(usize::from(area.height));
    for row_index in 0..viewport_rows {
        let y = area.y + row_index as u16;
        let line = grid_line(y, area, scroll_offset);
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

/// The three chrome bands of a frame (Spec R02 layout decision): the tab
/// bar on top, the status line T5 paints at the bottom, and the session
/// content between them. Chrome is drawn OUTSIDE the session grid — the
/// PTY's winsize is the content area only, so resize math starts from
/// [`content_size`], never the raw window.
pub struct ChromeAreas {
    /// Row 0: the always-visible tab bar.
    pub tab_bar: Rect,
    /// The session grid's region.
    pub content: Rect,
    /// The last row: T5's status line (`cwd · shell · model · state` +
    /// toasts). T4 already reserves it so the status line lands without
    /// reshuffling the layout.
    pub status: Rect,
}

/// Split `area` into the three chrome bands. On a degenerate frame
/// (`height < 3`) the content band can be empty — painting handles a
/// zero-height rect, and the PTY still gets a 1-line grid via
/// [`content_size`]'s clamp.
pub fn chrome_areas(area: Rect) -> ChromeAreas {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(area);
    ChromeAreas {
        tab_bar: rows[0],
        content: rows[1],
        status: rows[2],
    }
}

/// The grid/PTY size for a frame of `area`: the content band's
/// dimensions. A terminal can never have zero lines (`Size` itself
/// clamps), so a starved content band still maps to a 1-line grid.
pub fn content_size(area: Rect) -> Size {
    let content = chrome_areas(area).content;
    Size::new(
        usize::from(content.width).max(1),
        usize::from(content.height).max(1),
    )
}

/// The panel's column count on a `width`-column content band: two
/// fifths of the band (Spec R06 §3's side-by-side split), never more
/// than what leaves the session one column.
fn panel_columns(width: usize) -> usize {
    (width * 2 / 5).min(width.saturating_sub(1))
}

/// The session's column count on a `width`-column content band — the
/// whole band, or the band minus the open agent panel's share.
fn session_columns(width: usize, panel_open: bool) -> usize {
    if panel_open {
        width - panel_columns(width)
    } else {
        width
    }
}

/// Split the content band into the session grid's rect and — when the
/// active tab's agent panel is open — the right-side panel rect (Spec
/// R06 §3's side-by-side diagram). `None` for the panel when it is
/// closed or the band is too narrow to spare it a column. The same
/// [`panel_columns`] math backs this and [`session_size`], so the
/// painted split and the PTY winsize can never disagree.
pub fn split_content(content: Rect, panel_open: bool) -> (Rect, Option<Rect>) {
    let session = session_columns(usize::from(content.width), panel_open) as u16;
    let terminal = Rect {
        width: session,
        ..content
    };
    let panel = (usize::from(content.width - session) > 0).then(|| Rect {
        x: content.x + session,
        width: content.width - session,
        ..content
    });
    (terminal, panel)
}

/// One tab's PTY winsize derived from the content-band `size`: the
/// whole band, or its terminal side when that tab's agent panel is
/// open (Spec R06 / issue #85 — opening the panel shrinks the session
/// through the same resize path a window resize takes).
pub fn session_size(size: Size, panel_open: bool) -> Size {
    Size::new(session_columns(size.columns, panel_open).max(1), size.lines)
}

/// Paint the always-visible tab bar: `" 1:bash "` chunks in order, the
/// active tab reversed. Titles beyond the row's width are clipped —
/// jumping (`Alt+N`) still reaches them; T5's status line owns any
/// richer state display.
pub fn paint_tab_bar(buf: &mut Buffer, area: Rect, titles: &[String], active: usize) {
    if area.height == 0 {
        return;
    }
    let y = area.y;
    let mut x = area.x;
    'titles: for (index, title) in titles.iter().enumerate() {
        for c in format!(" {title} ").chars() {
            if x >= area.right() {
                break 'titles;
            }
            let cell = &mut buf[(x, y)];
            cell.reset();
            cell.set_char(c);
            if index == active {
                cell.modifier |= Modifier::REVERSED;
            }
            x += 1;
        }
    }
    // Clear whatever the bar doesn't cover (a stale longer bar, the
    // unused tail).
    reset_span(buf, y, x..area.right());
}

/// Paint the status-line row: `text` clipped at the row's right edge,
/// the tail cleared so a shrinking line never leaves stale cells (the
/// same contract as [`paint_tab_bar`]). `highlighted` reverses the
/// text's cells — the toast's "get my attention" look vs. the plain
/// session summary.
pub fn paint_status_line(buf: &mut Buffer, area: Rect, text: &str, highlighted: bool) {
    if area.height == 0 {
        return;
    }
    let y = area.y;
    // Reset first: `set_stringn` patches symbols but doesn't clear a
    // stale REVERSED left by an expired toast.
    reset_span(buf, y, area.x..area.right());
    // `set_stringn` sizes graphemes itself, so a wide char in a cwd
    // occupies two columns like it does in the grid.
    let width = usize::from(area.right() - area.x);
    let (end_x, _) = buf.set_stringn(area.x, y, text, width, ratatui::style::Style::default());
    if highlighted {
        for x in area.x..end_x.min(area.right()) {
            buf[(x, y)].modifier |= Modifier::REVERSED;
        }
    }
}

/// Paint the profile switcher (Spec R07 §5.7 / issue #101): the
/// declared names as a small overlay anchored on `area`'s bottom rows
/// — `▸` marks the highlighted row (the ask card's selection idiom,
/// bolded). Each row is reset before its text so the list reads as an
/// overlay rather than a patch over the session grid; more names than
/// `area` has rows clip from the top like every painter's overflow.
pub fn paint_profile_picker(buf: &mut Buffer, area: Rect, picker: &ProfilePicker) {
    let names = picker.names();
    let rows = names.len().min(usize::from(area.height));
    let top = area.bottom().saturating_sub(rows as u16);
    for (index, name) in names.iter().take(rows).enumerate() {
        let y = top + index as u16;
        reset_span(buf, y, area.x..area.right());
        let (text, style) = if index == picker.selected() {
            (
                format!("▸ {name}"),
                ratatui::style::Style::default().add_modifier(Modifier::BOLD),
            )
        } else {
            (format!("  {name}"), ratatui::style::Style::default())
        };
        let width = usize::from(area.right() - area.x);
        buf.set_stringn(area.x, y, text, width, style);
    }
}

/// Paint the `Alt+R` conversation picker (Spec R08 §5.8, ticket
/// R08-T6): the stored rows newest-first over `area`'s bottom rows —
/// `▸ title  age · N items` bolded on the highlight — and a bottom
/// hint row that doubles as `Ctrl+D`'s one-key confirm prompt. The
/// highlight scrolls the window when a long list outgrows the band;
/// each row is reset before its text like every overlay painter.
pub fn paint_conversation_picker(buf: &mut Buffer, area: Rect, picker: &ConversationPicker) {
    if area.height == 0 {
        return;
    }
    let width = usize::from(area.right() - area.x);
    let rows = picker.rows();
    // The hint owns the bottom row; the list anchors above it and
    // windows so the highlight always shows.
    let list_height = usize::from(area.height - 1);
    let shown = rows.len().min(list_height);
    let start = picker
        .selected()
        .saturating_add(1)
        .saturating_sub(shown)
        .min(rows.len().saturating_sub(shown));
    let top = area.bottom().saturating_sub(1 + shown as u16);
    for (offset, row) in rows.iter().skip(start).take(shown).enumerate() {
        let index = start + offset;
        let y = top + offset as u16;
        reset_span(buf, y, area.x..area.right());
        let (mark, style) = if index == picker.selected() {
            (
                "▸",
                ratatui::style::Style::default().add_modifier(Modifier::BOLD),
            )
        } else {
            (" ", ratatui::style::Style::default())
        };
        buf.set_stringn(
            area.x,
            y,
            format!("{mark} {}  {} · {} items", row.title, row.age, row.items),
            width,
            style,
        );
    }
    // The bottom row is the keys hint — and `Ctrl+D`'s armed state
    // replaces it with the confirm prompt (us-11).
    let hint_y = area.bottom() - 1;
    reset_span(buf, hint_y, area.x..area.right());
    let (text, style) = if picker.confirming() {
        let title = picker
            .selected_row()
            .map(|row| row.title.as_str())
            .unwrap_or_default();
        (
            format!(" ctrl+d again deletes \"{title}\""),
            ratatui::style::Style::default()
                .fg(RtColor::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        (
            " enter: resume · ctrl+d: delete · esc: close".to_string(),
            ratatui::style::Style::default().fg(RtColor::DarkGray),
        )
    };
    buf.set_stringn(area.x, hint_y, text, width, style);
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
    use ait_app::selection::{GridPoint, SelectionRange};
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

    // --- chrome: tab bar, content split, status-line row ---

    /// The layout: tab bar on top, status line at the bottom, the
    /// session content between — and `content_size` (the PTY winsize)
    /// is the content band only.
    #[test]
    fn chrome_splits_off_two_rows() {
        let areas = chrome_areas(Rect::new(0, 0, 80, 24));
        assert_eq!(areas.tab_bar, Rect::new(0, 0, 80, 1));
        assert_eq!(areas.content, Rect::new(0, 1, 80, 22));
        assert_eq!(areas.status, Rect::new(0, 23, 80, 1));
        assert_eq!(content_size(Rect::new(0, 0, 80, 24)), Size::new(80, 22));
    }

    /// A degenerate frame still yields a usable PTY size — never zero.
    #[test]
    fn content_size_never_has_zero_lines() {
        assert_eq!(content_size(Rect::new(0, 0, 80, 2)), Size::new(80, 1));
        assert_eq!(content_size(Rect::new(0, 0, 80, 0)), Size::new(80, 1));
    }

    /// Spec R06's right-side split: a closed panel leaves the whole
    /// content band to the session; an open one takes two fifths off
    /// the right edge and the winsize (`session_size`) agrees with the
    /// painted rect (`split_content`) — one formula feeds both.
    #[test]
    fn split_content_carves_a_right_side_panel() {
        let content = Rect::new(0, 1, 100, 22);

        assert_eq!(
            split_content(content, false),
            (content, None),
            "closed panel: the session owns the whole band"
        );

        let (terminal, panel) = split_content(content, true);
        assert_eq!(terminal, Rect::new(0, 1, 60, 22));
        assert_eq!(panel, Some(Rect::new(60, 1, 40, 22)));
        // The PTY winsize is the same answer — the split and the
        // resize can never disagree (us: winsize shrinks on open).
        assert_eq!(session_size(Size::new(100, 22), true), Size::new(60, 22));
        assert_eq!(session_size(Size::new(100, 22), false), Size::new(100, 22));
    }

    /// Degenerate bands never yield a zero-width session: a one-column
    /// band shows no panel rect (nothing to paint) but still reports a
    /// one-column session size.
    #[test]
    fn split_content_clamps_on_narrow_bands() {
        let (terminal, panel) = split_content(Rect::new(0, 1, 1, 22), true);
        assert_eq!(terminal.width, 1);
        assert_eq!(panel, None, "no room for a panel column");
        assert_eq!(session_size(Size::new(1, 22), true).columns, 1);
    }

    /// Every tab renders `" N:title "`, the active one inverted; the
    /// row tail is cleared, not left stale.
    #[test]
    fn tab_bar_paints_titles_and_marks_active() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 3));
        // Pre-fill the tail so the clear is observable.
        buf[(30, 0)].set_symbol("!");
        let titles = vec!["1:bash".to_string(), "2:sh".to_string()];
        paint_tab_bar(&mut buf, Rect::new(0, 0, 40, 1), &titles, 1);

        // " 1:bash " occupies cols 0..8, plain.
        let label1: String = (0..8).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(label1, " 1:bash ");
        assert!(!buf[(1, 0)].modifier.contains(Modifier::REVERSED));

        // " 2:sh " occupies cols 8..14, every cell reversed.
        let label2: String = (8..14).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(label2, " 2:sh ");
        for x in 8..14 {
            assert!(
                buf[(x, 0)].modifier.contains(Modifier::REVERSED),
                "active tab cell {x} reversed"
            );
        }

        // The tail was cleared; other rows untouched.
        assert_eq!(buf[(30, 0)].symbol(), " ");
        assert_eq!(buf[(0, 1)].symbol(), " ");
    }

    /// Titles past the row's width clip instead of overflowing; the
    /// active marker can sit anywhere, including on a hidden tab.
    #[test]
    fn tab_bar_clips_overflow() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let titles = vec![
            "1:averylongshellname".to_string(),
            "2:sh".to_string(),
            "3:bash".to_string(),
        ];
        let area = buf.area;
        paint_tab_bar(&mut buf, area, &titles, 2);
        let row: String = (0..10).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, " 1:averylo", "clipped at the row's edge");
    }

    /// An empty tab set paints a clean blank bar — the guarded state
    /// the app exits on must still be safe to render a frame for.
    #[test]
    fn tab_bar_on_empty_set_is_blank() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 1));
        buf[(0, 0)].set_symbol("x");
        let area = buf.area;
        paint_tab_bar(&mut buf, area, &[], 0);
        assert_eq!(buf[(0, 0)].symbol(), " ");
    }

    /// The status line paints its text left-aligned, plain when it is
    /// the session summary, and clears the tail so a shrinking line
    /// leaves nothing stale behind.
    #[test]
    fn status_line_paints_text_and_clears_the_tail() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 1));
        // Pre-fill the tail so the clear is observable.
        buf[(25, 0)].set_symbol("!");
        buf[(25, 0)].modifier |= Modifier::REVERSED;
        let area = buf.area;
        paint_status_line(&mut buf, area, "/tmp · sh · — · running", false);

        let row: String = (0..23).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "/tmp · sh · — · running");
        assert!(!buf[(0, 0)].modifier.contains(Modifier::REVERSED));
        // The tail cell's symbol AND its stale highlight are gone.
        assert_eq!(buf[(25, 0)].symbol(), " ");
        assert!(!buf[(25, 0)].modifier.contains(Modifier::REVERSED));
    }

    /// A live toast paints its text reversed — the attention marker —
    /// over the whole of its cells.
    #[test]
    fn status_line_toast_is_highlighted() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
        let area = buf.area;
        paint_status_line(&mut buf, area, "build finished", true);

        let row: String = (0..14).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "build finished");
        for x in 0..14 {
            assert!(
                buf[(x, 0)].modifier.contains(Modifier::REVERSED),
                "toast cell {x} reversed"
            );
        }
        // Untouched tail: reset but not part of the highlight.
        assert!(!buf[(15, 0)].modifier.contains(Modifier::REVERSED));
    }

    /// Text past the row's width clips instead of overflowing (a deep
    /// cwd on a narrow window).
    #[test]
    fn status_line_clips_overflow() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let area = buf.area;
        paint_status_line(&mut buf, area, "/very/deep/cwd · bash", false);
        let row: String = (0..10).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row, "/very/deep", "clipped at the row's edge");
    }

    /// A wide char in the cwd occupies two columns, matching how the
    /// grid itself paints it.
    #[test]
    fn status_line_wide_chars_take_two_columns() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 1));
        let area = buf.area;
        paint_status_line(&mut buf, area, "/日本語 · sh", false);

        assert_eq!(buf[(0, 0)].symbol(), "/");
        assert_eq!(buf[(1, 0)].symbol(), "日");
        // '本' lands two columns over — the spacer belongs to '日'.
        assert_eq!(buf[(3, 0)].symbol(), "本");
    }
}
