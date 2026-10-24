//! Read-side grid types: what a frontend (or a test) sees when it looks at a
//! [`Session`](crate::Session). These are `ait-term`'s own types — the VT
//! emulator's cell/grid types never cross this boundary (ADR-0002).

use alacritty_terminal::grid::{Dimensions, Grid};
use alacritty_terminal::index::Line;
use alacritty_terminal::term::cell::{Cell as TermCell, Flags};
use alacritty_terminal::vte::ansi::{Color as TermColor, NamedColor};

/// A read-only view over a session's grid: the visible rows plus scrollback.
///
/// Row indices are `0..lines()` for the viewport and negative for scrollback
/// (`-1` is the line that scrolled off most recently).
#[derive(Debug)]
pub struct GridView<'a> {
    grid: &'a Grid<TermCell>,
}

impl<'a> GridView<'a> {
    pub(crate) fn new(grid: &'a Grid<TermCell>) -> Self {
        Self { grid }
    }

    /// Grid width in columns.
    pub fn columns(&self) -> usize {
        self.grid.columns()
    }

    /// Grid height in visible lines.
    pub fn lines(&self) -> usize {
        self.grid.screen_lines()
    }

    /// Number of lines held in scrollback above the viewport.
    pub fn scrollback(&self) -> usize {
        self.grid.history_size()
    }

    /// One row by index: `0..lines()` visible, negative into scrollback.
    /// Returns `None` when out of range.
    pub fn line(&self, index: i32) -> Option<Row<'_>> {
        if index < -(self.scrollback() as i32) || index >= self.lines() as i32 {
            return None;
        }
        Some(Row {
            row: &self.grid[Line(index)],
        })
    }

    /// One cell by `(line, column)`; `line` follows [`line`](Self::line).
    pub fn cell(&self, line: i32, column: usize) -> Option<Cell> {
        self.line(line)?.cell(column)
    }

    /// Scrollback rows, oldest first.
    pub fn scrollback_rows(&self) -> impl Iterator<Item = Row<'_>> {
        let top = -(self.scrollback() as i32);
        (top..0).filter_map(move |i| self.line(i))
    }

    /// Visible rows, top to bottom.
    pub fn visible_rows(&self) -> impl Iterator<Item = Row<'_>> {
        (0..self.lines() as i32).filter_map(move |i| self.line(i))
    }
}

/// One row of cells.
#[derive(Debug)]
pub struct Row<'a> {
    row: &'a alacritty_terminal::grid::Row<TermCell>,
}

impl Row<'_> {
    /// Row width in columns.
    pub fn len(&self) -> usize {
        self.row.len()
    }

    /// Always false for a live grid row; provided because `len()` exists.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The cell at `column`, or `None` when out of range.
    pub fn cell(&self, column: usize) -> Option<Cell> {
        self.row[..].get(column).map(Cell::from_term)
    }

    /// All cells in the row, left to right.
    pub fn cells(&self) -> impl Iterator<Item = Cell> + '_ {
        self.row[..].iter().map(Cell::from_term)
    }

    /// The row's text: each cell's character plus any zero-width characters
    /// (combining marks, ZWJ emoji parts). Wide-char spacer cells are skipped.
    /// Trailing blank cells are included — callers may `trim_end()`.
    pub fn text(&self) -> String {
        let mut text = String::with_capacity(self.len());
        for cell in self.row[..].iter() {
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            text.push(cell.c);
            if let Some(zerowidth) = cell.zerowidth() {
                text.extend(zerowidth.iter());
            }
        }
        text
    }

    /// True when this row soft-wraps into the next one.
    pub fn wrapped(&self) -> bool {
        self.row[..]
            .last()
            .is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE))
    }
}

/// A styled grid cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    /// The character in this cell (zero-width/combining characters ride along
    /// in [`Row::text`], not here).
    pub character: char,
    pub fg: Color,
    pub bg: Color,
    pub style: Style,
    /// How this cell relates to wide (2-column) characters.
    pub kind: CellKind,
}

impl Cell {
    fn from_term(cell: &TermCell) -> Self {
        let kind = if cell.flags.contains(Flags::WIDE_CHAR) {
            CellKind::WideLead
        } else if cell
            .flags
            .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
        {
            CellKind::WideSpacer
        } else {
            CellKind::Normal
        };
        Self {
            character: cell.c,
            fg: Color::from_term(cell.fg),
            bg: Color::from_term(cell.bg),
            style: Style::from_term(cell.flags),
            kind,
        }
    }
}

/// How a cell relates to wide (2-column) characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    /// A single-width cell.
    Normal,
    /// The leading cell of a wide character.
    WideLead,
    /// The trailing half of a wide character (no content of its own).
    WideSpacer,
}

/// A cell color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// The terminal default for this slot (foreground or background).
    Default,
    /// One of the 16 named ANSI palette entries.
    Ansi(AnsiColor),
    /// A 256-color palette index.
    Indexed(u8),
    /// 24-bit truecolor.
    Rgb(Rgb),
}

impl Color {
    fn from_term(color: TermColor) -> Self {
        match color {
            TermColor::Named(named) => Self::from_named(named),
            TermColor::Indexed(index) => Self::Indexed(index),
            TermColor::Spec(rgb) => Self::Rgb(Rgb {
                r: rgb.r,
                g: rgb.g,
                b: rgb.b,
            }),
        }
    }

    fn from_named(named: NamedColor) -> Self {
        let ansi = match named {
            NamedColor::Black | NamedColor::DimBlack => AnsiColor::Black,
            NamedColor::Red | NamedColor::DimRed => AnsiColor::Red,
            NamedColor::Green | NamedColor::DimGreen => AnsiColor::Green,
            NamedColor::Yellow | NamedColor::DimYellow => AnsiColor::Yellow,
            NamedColor::Blue | NamedColor::DimBlue => AnsiColor::Blue,
            NamedColor::Magenta | NamedColor::DimMagenta => AnsiColor::Magenta,
            NamedColor::Cyan | NamedColor::DimCyan => AnsiColor::Cyan,
            NamedColor::White | NamedColor::DimWhite => AnsiColor::White,
            NamedColor::BrightBlack => AnsiColor::BrightBlack,
            NamedColor::BrightRed => AnsiColor::BrightRed,
            NamedColor::BrightGreen => AnsiColor::BrightGreen,
            NamedColor::BrightYellow => AnsiColor::BrightYellow,
            NamedColor::BrightBlue => AnsiColor::BrightBlue,
            NamedColor::BrightMagenta => AnsiColor::BrightMagenta,
            NamedColor::BrightCyan => AnsiColor::BrightCyan,
            NamedColor::BrightWhite => AnsiColor::BrightWhite,
            // Palette slots without a fixed ANSI name collapse to the slot
            // default; the dim variants still carry the `dim` style flag.
            NamedColor::Foreground
            | NamedColor::BrightForeground
            | NamedColor::DimForeground
            | NamedColor::Background
            | NamedColor::Cursor => return Self::Default,
        };
        Self::Ansi(ansi)
    }
}

/// One of the 16 named ANSI palette entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnsiColor {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

/// A 24-bit color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// Text attributes on a cell.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: Underline,
    pub inverse: bool,
    pub hidden: bool,
    pub strikeout: bool,
}

impl Style {
    fn from_term(flags: Flags) -> Self {
        let underline = if flags.contains(Flags::DOUBLE_UNDERLINE) {
            Underline::Double
        } else if flags.contains(Flags::UNDERCURL) {
            Underline::Curly
        } else if flags.contains(Flags::DOTTED_UNDERLINE) {
            Underline::Dotted
        } else if flags.contains(Flags::DASHED_UNDERLINE) {
            Underline::Dashed
        } else if flags.contains(Flags::UNDERLINE) {
            Underline::Single
        } else {
            Underline::None
        };
        Self {
            bold: flags.contains(Flags::BOLD),
            dim: flags.contains(Flags::DIM),
            italic: flags.contains(Flags::ITALIC),
            underline,
            inverse: flags.contains(Flags::INVERSE),
            hidden: flags.contains(Flags::HIDDEN),
            strikeout: flags.contains(Flags::STRIKEOUT),
        }
    }
}

/// Underline variant on a [`Style`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// The renderable cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// Row within the viewport (`0..lines`).
    pub line: usize,
    /// Column within the viewport (`0..columns`).
    pub column: usize,
    pub shape: CursorShape,
}

/// Cursor shape as requested by the application (or the default block).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    HollowBlock,
    Underline,
    Beam,
    /// The application hid the cursor.
    Hidden,
}

/// Grid damage since the last call to `Session::damage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    /// Everything changed; repaint the whole viewport.
    Full,
    /// Only these line ranges changed.
    Partial(Vec<DamageBounds>),
}

/// A damaged line's column range (inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageBounds {
    pub line: usize,
    pub left: usize,
    pub right: usize,
}
