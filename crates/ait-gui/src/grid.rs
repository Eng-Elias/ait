//! `GridView` → styled-run planning (Spec R13 §5.12): the pure half of
//! the GUI's grid painter.
//!
//! [`plan_viewport`] folds the visible grid into [`RowPlan`]s of
//! [`CellSpan`]s — runs of cells sharing one resolved style — with wide
//! characters covering their spacer column and zero-width/combining
//! characters riding on their lead cell (the same `Row::text` contract
//! the TUI's `set_stringn` relies on). Everything here is pure data
//! plus `egui` color types: no `Painter`, no GPU context, so the
//! cell→glyph-run mapping is unit-testable headless (spec testing
//! decisions).

use ait_term::{AnsiColor, Cell, CellKind, Color, Cursor, Damage, GridView, Rgb, Underline};
use eframe::egui::Color32;

/// The GUI's resolved cell palette. `Color::Default` resolves through
/// `fg`/`bg`; every other `ait_term::Color` maps through
/// [`to_color32`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GridTheme {
    /// Default cell foreground.
    pub fg: Color32,
    /// Default cell background — also the pane's clear color.
    pub bg: Color32,
    /// The block cursor's fill.
    pub cursor: Color32,
    /// The selection overlay's translucent wash.
    pub selection: Color32,
    /// The find bar's match wash — every hit except the current one.
    pub find_match: Color32,
    /// The current match's wash — brighter so it stands out in a row
    /// of hits.
    pub find_current: Color32,
    /// The command-block divider line (integrated sessions only).
    pub block_edge: Color32,
    /// The exit-status chip's success color (`✓`).
    pub block_ok: Color32,
    /// The exit-status chip's failure color (`✗`).
    pub block_err: Color32,
}

impl Default for GridTheme {
    fn default() -> Self {
        Self {
            fg: Color32::from_rgb(0xD8, 0xD8, 0xD8),
            bg: Color32::from_rgb(0x14, 0x14, 0x14),
            cursor: Color32::from_rgba_unmultiplied(0xD8, 0xD8, 0xD8, 0x70),
            selection: Color32::from_rgba_unmultiplied(0x33, 0x66, 0xFF, 0x60),
            find_match: Color32::from_rgba_unmultiplied(0xCC, 0xAA, 0x00, 0x50),
            find_current: Color32::from_rgba_unmultiplied(0xFF, 0x88, 0x00, 0x90),
            block_edge: Color32::from_rgba_unmultiplied(0x88, 0x88, 0x88, 0x40),
            block_ok: Color32::from_rgb(0x4C, 0xAF, 0x50),
            block_err: Color32::from_rgb(0xE0, 0x50, 0x50),
        }
    }
}

/// `ait_term` color → `egui::Color32`: `Default` resolves through the
/// theme slot, the 16 named ANSI entries through the standard xterm
/// palette, `Indexed` through the 256-color cube + grayscale tail, and
/// `Rgb` verbatim.
pub fn to_color32(color: Color, theme: GridTheme, is_fg: bool) -> Color32 {
    match color {
        Color::Default => {
            if is_fg {
                theme.fg
            } else {
                theme.bg
            }
        }
        Color::Ansi(ansi) => ansi_color(ansi),
        Color::Indexed(index) => indexed_color(index),
        Color::Rgb(Rgb { r, g, b }) => Color32::from_rgb(r, g, b),
    }
}

/// The 16-entry ANSI palette (xterm values — the same table terminals
/// ship, so SGR colors match a user's muscle memory of them).
fn ansi_color(ansi: AnsiColor) -> Color32 {
    let rgb = match ansi {
        AnsiColor::Black => (0x00, 0x00, 0x00),
        AnsiColor::Red => (0xCD, 0x00, 0x00),
        AnsiColor::Green => (0x00, 0xCD, 0x00),
        AnsiColor::Yellow => (0xCD, 0xCD, 0x00),
        AnsiColor::Blue => (0x00, 0x00, 0xEE),
        AnsiColor::Magenta => (0xCD, 0x00, 0xCD),
        AnsiColor::Cyan => (0x00, 0xCD, 0xCD),
        AnsiColor::White => (0xE5, 0xE5, 0xE5),
        AnsiColor::BrightBlack => (0x7F, 0x7F, 0x7F),
        AnsiColor::BrightRed => (0xFF, 0x00, 0x00),
        AnsiColor::BrightGreen => (0x00, 0xFF, 0x00),
        AnsiColor::BrightYellow => (0xFF, 0xFF, 0x00),
        AnsiColor::BrightBlue => (0x5C, 0x5C, 0xFF),
        AnsiColor::BrightMagenta => (0xFF, 0x00, 0xFF),
        AnsiColor::BrightCyan => (0x00, 0xFF, 0xFF),
        AnsiColor::BrightWhite => (0xFF, 0xFF, 0xFF),
    };
    Color32::from_rgb(rgb.0, rgb.1, rgb.2)
}

/// The 256-color palette beyond the named 16: `16..=231` is the
/// 6×6×6 cube (`0,95,135,175,215,255` steps), `232..=255` the
/// 24-step grayscale ramp.
fn indexed_color(index: u8) -> Color32 {
    match index {
        16..=231 => {
            const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
            let i = index - 16;
            let (r, g, b) = (i / 36, (i % 36) / 6, i % 6);
            Color32::from_rgb(STEPS[r as usize], STEPS[g as usize], STEPS[b as usize])
        }
        232..=255 => {
            let v = 8 + 10 * (index - 232);
            Color32::from_rgb(v, v, v)
        }
        // 0..=15 aliases the named ANSI entries.
        _ => ansi_color(NAMED[index as usize]),
    }
}

/// `Indexed(0..=15)` → the `AnsiColor` they alias.
const NAMED: [AnsiColor; 16] = [
    AnsiColor::Black,
    AnsiColor::Red,
    AnsiColor::Green,
    AnsiColor::Yellow,
    AnsiColor::Blue,
    AnsiColor::Magenta,
    AnsiColor::Cyan,
    AnsiColor::White,
    AnsiColor::BrightBlack,
    AnsiColor::BrightRed,
    AnsiColor::BrightGreen,
    AnsiColor::BrightYellow,
    AnsiColor::BrightBlue,
    AnsiColor::BrightMagenta,
    AnsiColor::BrightCyan,
    AnsiColor::BrightWhite,
];

/// One run of same-styled grid text. `cells` is the column count the
/// run covers — a wide char's lead span covers 2. `text` already holds
/// the run's zero-width/combining characters.
#[derive(Debug, Clone, PartialEq)]
pub struct CellSpan {
    /// First grid column of the run.
    pub col: usize,
    /// How many grid columns the run covers.
    pub cells: usize,
    /// The run's text (combining characters included).
    pub text: String,
    /// Resolved foreground.
    pub fg: Color32,
    /// Resolved background — `None` paints nothing (the pane's theme
    /// `bg` shows through, cheaper than a rect per cell).
    pub bg: Option<Color32>,
    /// The `hidden` style — the run's text is not painted at all.
    pub hidden: bool,
    /// Bold — egui's bundled monospace has no bold face, so the
    /// painter brightens the foreground (documented degrade).
    pub bold: bool,
    /// Italic — honored through `TextFormat::italics`.
    pub italic: bool,
    /// Strikeout — drawn as a mid-cell line.
    pub strikeout: bool,
    /// The underline variant, if any (single/double/curly/dotted/
    /// dashed — all drawn as explicit line shapes).
    pub underline: Underline,
}

/// One viewport row's planned paint work: `line` is the grid line index
/// (`0` = live viewport top, negative = scrollback — the same
/// `grid_line` rule the TUI shares).
#[derive(Debug, Clone, PartialEq)]
pub struct RowPlan {
    /// The grid line this row paints.
    pub line: i32,
    /// Same-style runs in column order.
    pub spans: Vec<CellSpan>,
}

/// Resolve a cell's colors and flags into span values, shared by the
/// plan loop. `inverse` swaps the resolved fg/bg.
fn resolve(cell: &Cell, theme: GridTheme) -> (Color32, Option<Color32>, bool) {
    let mut fg = to_color32(cell.fg, theme, true);
    let mut bg = to_color32(cell.bg, theme, false);
    if cell.style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.style.dim {
        fg = dim(fg);
    }
    // `bg` collapses to `None` at the theme default — no rect needed.
    let bg = (bg != theme.bg).then_some(bg);
    (fg, bg, cell.style.hidden)
}

/// `style.dim` on a color: two-thirds brightness — the host-independent
/// degrade (a TUI host terminal picks its own dim).
fn dim(color: Color32) -> Color32 {
    let [r, g, b, a] = color.to_array();
    Color32::from_rgba_unmultiplied(r * 2 / 3, g * 2 / 3, b * 2 / 3, a)
}

/// Whether `cell` carries the same paint style as `span`'s (style-only:
/// the text is concatenated on merge).
fn same_style(span: &CellSpan, cell: &Cell, theme: GridTheme) -> bool {
    let (fg, bg, hidden) = resolve(cell, theme);
    span.fg == fg
        && span.bg == bg
        && span.hidden == hidden
        && span.bold == cell.style.bold
        && span.italic == cell.style.italic
        && span.strikeout == cell.style.strikeout
        && span.underline == cell.style.underline
}

/// Split a row's `text()` into per-cell slices and merge same-styled
/// cells into [`CellSpan`]s.
///
/// `Row::text` emits each non-spacer cell's character plus its
/// zero-width tail, so a cell's text runs up to the *next* cell's
/// character — the walk that keeps ZWJ emoji and combining marks on
/// their lead cell. `WideSpacer` cells contribute nothing: their
/// column is already covered by the lead's `cells` count.
fn plan_row(line: i32, row: &ait_term::Row<'_>, width: usize, theme: GridTheme) -> RowPlan {
    let text = row.text();
    let cells: Vec<Cell> = row.cells().take(width).collect();
    let mut rest = text.as_str();
    let mut spans: Vec<CellSpan> = Vec::new();
    // Non-spacer cells in order — the text walk's alignment source.
    let filled: Vec<(usize, &Cell)> = cells
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.kind != CellKind::WideSpacer)
        .collect();
    for (index, (col, cell)) in filled.iter().enumerate() {
        // This cell's slice: its character plus the zero-width tail —
        // everything up to the NEXT cell's character (end for the last).
        // `Row::text` emits every non-spacer cell's `c` first, so the
        // current char is consumed unconditionally before looking for
        // the boundary (a next cell whose `c` equals this one would
        // otherwise slice empty).
        let own = cell.character.len_utf8();
        let next_char = filled.get(index + 1).map(|(_, cell)| cell.character);
        let end = match next_char {
            Some(next) => {
                own + rest[own.min(rest.len())..]
                    .find(next)
                    .unwrap_or(rest.len().saturating_sub(own))
            }
            None => rest.len(),
        };
        let (slice, tail) = rest.split_at(end.min(rest.len()));
        rest = tail;
        // Trailing all-blank cells carry no paintable content — their
        // bg rect (if styled) still matters, so only unstyled blanks
        // are dropped entirely.
        let (fg, bg, hidden) = resolve(cell, theme);
        let span = CellSpan {
            col: *col,
            cells: if cell.kind == CellKind::WideLead {
                2
            } else {
                1
            },
            text: slice.to_string(),
            fg,
            bg,
            hidden,
            bold: cell.style.bold,
            italic: cell.style.italic,
            strikeout: cell.style.strikeout,
            underline: cell.style.underline,
        };
        match spans.last_mut() {
            // Merge into the previous run: same style and adjacent.
            Some(last) if last.col + last.cells == span.col && same_style(last, cell, theme) => {
                last.cells += span.cells;
                last.text.push_str(&span.text);
            }
            _ => spans.push(span),
        }
    }
    RowPlan { line, spans }
}

/// Plan the visible viewport: `rows` screen rows starting
/// `scroll_offset` lines into scrollback (`0` = live viewport — the
/// same `grid_line` rule `paint_session` applies: screen row `i` shows
/// `grid.line(i - scroll_offset)`).
///
/// `width`/`rows` are the grid pane's cell dimensions — the winsize the
/// PTY already knows, so out-of-grid rows/columns simply yield shorter
/// plans.
pub fn plan_viewport(
    grid: &GridView<'_>,
    scroll_offset: usize,
    width: usize,
    rows: usize,
    theme: GridTheme,
) -> Vec<RowPlan> {
    let offset = i32::try_from(scroll_offset).unwrap_or(i32::MAX);
    let mut plans = Vec::with_capacity(rows);
    for row_index in 0..rows {
        let line = row_index as i32 - offset;
        let Some(row) = grid.line(line) else {
            // Past the oldest scrollback line — the pane's theme bg
            // shows through (no plan).
            continue;
        };
        plans.push(plan_row(line, &row, width, theme));
    }
    plans
}

/// Whether a [`Damage`] report marks anything besides the cursor's own
/// cell — the GUI's repaint-decision filter.
///
/// alacritty unconditionally damages the cursor cell, so trusting the
/// report alone would repaint on every drain (the same idle-spin trap
/// `app.rs`'s `damaged_beyond_cursor` documents). A real cursor move
/// damages the old AND new cells, and the pump separately compares the
/// cursor snapshot — both still repaint.
pub fn damage_needs_repaint(damage: &Damage, cursor: Cursor) -> bool {
    match damage {
        Damage::Full => true,
        Damage::Partial(bounds) => bounds.iter().any(|b| {
            !(b.line == cursor.line && b.left == cursor.column && b.right == cursor.column)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ait_term::{Session, SessionOptions, Size};

    /// Byte-fed session fixture: feed PTY output, get a grid to plan
    /// (the R01 seam — no PTY needed).
    fn session(columns: usize, lines: usize) -> Session {
        Session::new(SessionOptions {
            size: Size::new(columns, lines),
            ..Default::default()
        })
    }

    fn plans(session: &Session) -> Vec<RowPlan> {
        let size = session.size();
        plan_viewport(
            &session.grid(),
            0,
            size.columns,
            size.lines,
            GridTheme::default(),
        )
    }

    /// Plain output lands in one span carrying the row text.
    #[test]
    fn plain_text_plans_one_span() {
        let mut session = session(80, 24);
        session.feed_bytes(b"hello");
        let plan = &plans(&session)[0];
        assert_eq!(plan.line, 0);
        // The whole row is one span (blanks share the default style).
        assert_eq!(plan.spans.len(), 1);
        let span = &plan.spans[0];
        assert_eq!((span.col, span.cells), (0, 80));
        assert!(span.text.starts_with("hello"));
        assert_eq!(span.fg, GridTheme::default().fg);
        assert_eq!(span.bg, None);
    }

    /// SGR-styled runs split at style boundaries, colors resolved.
    #[test]
    fn styled_runs_split_on_style_change() {
        let mut session = session(80, 24);
        // "ab" plain, "CD" bright-red bold, "ef" back to plain.
        session.feed_bytes(b"ab\x1b[91;1mCD\x1b[0mef");
        let plan = &plans(&session)[0];
        assert!(plan.spans.len() >= 2, "{:?}", plan.spans);
        let (plain, styled) = (&plan.spans[0], &plan.spans[1]);
        assert!(plain.text.starts_with("ab"));
        assert_eq!(styled.col, 2);
        assert!(styled.text.starts_with("CD"));
        assert_eq!(styled.fg, ansi_color(AnsiColor::BrightRed));
        assert!(styled.bold);
        assert!(!styled.italic);
    }

    /// A colored background resolves to a rect color; the theme
    /// default stays `None` (no rect).
    #[test]
    fn backgrounds_resolve_against_theme() {
        let mut session = session(80, 24);
        session.feed_bytes(b"\x1b[41mX\x1b[0m");
        let plan = &plans(&session)[0];
        let span = &plan.spans[0];
        assert_eq!(span.bg, Some(Color32::from_rgb(0xCD, 0x00, 0x00)));
    }

    /// Wide characters' lead cell covers both columns; the spacer
    /// contributes no span of its own.
    #[test]
    fn wide_char_covers_two_columns() {
        let mut session = session(80, 24);
        // '\u{4e16}' is a double-width char: it eats one grid column
        // (WideLead + WideSpacer) while adding one char of text.
        session.feed_bytes("a\u{4e16}b".as_bytes());
        let plan = &plans(&session)[0];
        let text: String = plan.spans.iter().map(|span| span.text.clone()).collect();
        assert!(text.starts_with("a\u{4e16}b"), "{text:?}");
        // Column math stays in GRID columns: 80 covered even though the
        // text holds one fewer char than columns.
        let covered: usize = plan.spans.iter().map(|span| span.cells).sum();
        assert_eq!(covered, 80);
    }

    /// `inverse` swaps the resolved fg/bg; `hidden` marks the run
    /// unpaintable.
    #[test]
    fn inverse_and_hidden_resolve() {
        let mut session = session(80, 24);
        session.feed_bytes(b"\x1b[7mAB\x1b[8mCD\x1b[0m");
        let plan = &plans(&session)[0];
        let inverse = &plan.spans[0];
        assert_eq!(inverse.fg, GridTheme::default().bg);
        // Inverse of the default bg yields a visible bg rect: the
        // theme fg becomes the bg.
        assert_eq!(inverse.bg, Some(GridTheme::default().fg));
        let hidden = &plan.spans[1];
        assert!(hidden.hidden);
        assert!(hidden.text.starts_with("CD"));
    }

    /// Scrolling pulls scrollback rows into the plan with negative
    /// line indexes — the shared `grid_line` rule.
    #[test]
    fn scroll_offset_plans_scrollback_lines() {
        let mut session = session(10, 3);
        session.feed_bytes(b"one\r\ntwo\r\nthree\r\nfour");
        // "one" scrolled into history (3-line viewport shows
        // two/three/four); scroll_offset 1 pulls it back as line -1.
        let grid = session.grid();
        assert_eq!(grid.scrollback(), 1);
        let plans = plan_viewport(&grid, 1, 10, 3, GridTheme::default());
        assert_eq!(plans[0].line, -1);
        let text: String = plans[0].spans.iter().map(|s| s.text.clone()).collect();
        assert!(text.starts_with("one"), "{text:?}");
        // …and the live viewport's first row follows at line 0.
        assert_eq!(plans[1].line, 0);
        let text: String = plans[1].spans.iter().map(|s| s.text.clone()).collect();
        assert!(text.starts_with("two"), "{text:?}");
    }

    /// The 256-palette cube and grayscale resolve exactly.
    #[test]
    fn indexed_palette_math() {
        // 196 = cube 5,0,0 → pure red 255,0,0.
        assert_eq!(indexed_color(196), Color32::from_rgb(255, 0, 0));
        // 21 = cube 0,0,5 → 0,0,255.
        assert_eq!(indexed_color(21), Color32::from_rgb(0, 0, 255));
        // 244 = grayscale index 12 → 8+120=128.
        assert_eq!(indexed_color(244), Color32::from_rgb(128, 128, 128));
        // Indexed 0..=15 alias the named entries.
        assert_eq!(indexed_color(1), ansi_color(AnsiColor::Red));
        assert_eq!(indexed_color(15), ansi_color(AnsiColor::BrightWhite));
    }

    /// Cursor-only damage does NOT repaint; anything else does — the
    /// filter the pump applies before `request_repaint`.
    #[test]
    fn cursor_only_damage_is_filtered() {
        let cursor = Cursor {
            line: 4,
            column: 7,
            shape: ait_term::CursorShape::Block,
        };
        assert!(!damage_needs_repaint(
            &Damage::Partial(vec![ait_term::DamageBounds {
                line: 4,
                left: 7,
                right: 7,
            }]),
            cursor
        ));
        assert!(damage_needs_repaint(
            &Damage::Partial(vec![
                ait_term::DamageBounds {
                    line: 4,
                    left: 7,
                    right: 7,
                },
                ait_term::DamageBounds {
                    line: 4,
                    left: 8,
                    right: 8,
                },
            ]),
            cursor
        ));
        assert!(damage_needs_repaint(&Damage::Full, cursor));
    }
}
