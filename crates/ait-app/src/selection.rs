//! Mouse selection state and selection→text extraction (Spec R02
//! scroll-and-selection seam).
//!
//! Frontend-only state (CODING_STANDARDS.md §9): a drag produces a
//! [`SelectionRange`] in **grid coordinates** — `line` follows
//! [`GridView::line`](ait_term::GridView::line) (`0..lines` viewport,
//! negative into scrollback) — so a selection can span scrollback and
//! viewport alike. The screen↔grid conversion reuses the scroll rule:
//! `line = row - scroll_offset`.
//!
//! [`extract`] turns a range into clipboard text the way terminals do:
//! linear flow (each line's slice joined in order), rows that soft-wrap
//! join **without** a newline ([`Row::wrapped`]), hard line breaks join
//! with `'\n'`, wide-char spacer cells contribute nothing, and trailing
//! blanks are trimmed per line — mirroring `alacritty`'s
//! `bounds_to_string`/`line_to_string`.
//!
//! The state tracks output arriving mid-selection: [`Selection::on_output`]
//! shifts the endpoints the way [`ScrollState`](crate::scroll::ScrollState)
//! pins the view, so a dragged range stays on its text.

use ait_term::{CellKind, GridView, Row, Style};

/// One cell in grid coordinates.
///
/// Ordered by `(line, column)` so a normalized range is just `min`/`max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GridPoint {
    /// Grid line: `0..lines` viewport, negative into scrollback.
    pub line: i32,
    /// Grid column.
    pub column: usize,
}

impl GridPoint {
    pub fn new(line: i32, column: usize) -> Self {
        Self { line, column }
    }
}

/// A normalized (`start <= end`) cell range in grid coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRange {
    pub start: GridPoint,
    pub end: GridPoint,
}

impl SelectionRange {
    /// Whether the cell at (`line`, `column`) falls inside the range —
    /// what the highlight paints.
    pub fn contains(&self, line: i32, column: usize) -> bool {
        (line > self.start.line || (line == self.start.line && column >= self.start.column))
            && (line < self.end.line || (line == self.end.line && column <= self.end.column))
    }
}

/// The drag-selection state machine.
///
/// `Down` → [`begin`](Self::begin), `Drag` → [`extend`](Self::extend),
/// `Up` → [`finish`](Self::finish). The last range stays visible after
/// release — the highlight clears on a new drag, a scroll, a keypress or a
/// resize ([`clear`](Self::clear)).
#[derive(Debug, Default)]
pub struct Selection {
    anchor: Option<GridPoint>,
    head: Option<GridPoint>,
    dragging: bool,
}

impl Selection {
    /// Start a new drag at `point`; clears any previous range.
    pub fn begin(&mut self, point: GridPoint) {
        self.anchor = Some(point);
        self.head = Some(point);
        self.dragging = true;
    }

    /// Move the drag's head to `point`. No-op outside a drag.
    pub fn extend(&mut self, point: GridPoint) {
        if self.dragging {
            self.head = Some(point);
        }
    }

    /// End the drag; the range stays for highlighting (and is what was
    /// just copied). Returns the normalized range, `None` outside a drag.
    pub fn finish(&mut self) -> Option<SelectionRange> {
        if !self.dragging {
            return None;
        }
        self.dragging = false;
        self.range()
    }

    /// Forget the range entirely (scroll, keypress, resize, new drag).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The current normalized range — during and after a drag.
    pub fn range(&self) -> Option<SelectionRange> {
        let anchor = self.anchor?;
        let head = self.head?;
        Some(SelectionRange {
            start: anchor.min(head),
            end: anchor.max(head),
        })
    }

    /// New output arrived: `grown` lines scrolled from the viewport into
    /// scrollback, so every existing cell's line index drops by `grown` —
    /// shift the endpoints to keep the range on the same text (the same
    /// adjustment the pinned scroll offset makes).
    ///
    /// Once scrollback is full its depth stops growing while lines still
    /// rotate out, so a selection held through a flood drifts — the same
    /// accepted bound as the pinned view.
    pub fn on_output(&mut self, grown: usize) {
        let grown = i32::try_from(grown).unwrap_or(i32::MAX);
        for point in [&mut self.anchor, &mut self.head].into_iter().flatten() {
            point.line = point.line.saturating_sub(grown);
        }
    }
}

/// Extract the selected text from the grid.
///
/// The range is linear: from `start` to `end`, each line contributing the
/// cells between the range's column bounds (full width on interior
/// lines). Rows joined by a hard line break get `'\n'`; a soft-wrapped
/// row flows into the next without one. Trailing blanks on each line are
/// dropped; the result never ends with `'\n'`. A line that scrolled out
/// of history contributes no cells but still counts as a hard break —
/// its neighbors must not silently concatenate into one line.
pub fn extract(grid: &GridView<'_>, range: SelectionRange) -> String {
    let mut text = String::new();
    for line in range.start.line..=range.end.line {
        let Some(row) = grid.line(line) else {
            if line != range.end.line {
                text.push('\n');
            }
            continue;
        };
        let first = if line == range.start.line {
            range.start.column
        } else {
            0
        };
        let last = if line == range.end.line {
            range.end.column
        } else {
            row.len().saturating_sub(1)
        };
        text.push_str(&line_text(&row, first, last));
        // A hard break joins lines with '\n'; a wrapped row flows on.
        if line != range.end.line && !row.wrapped() {
            text.push('\n');
        }
    }
    text
}

/// The selected text of one row between columns `first..=last`
/// (inclusive), wide-char spacers contributing nothing. Cells beyond the
/// row's content (the last non-blank cell) are trimmed — blanks *inside*
/// the content stay, so `last` is capped at the content end rather than
/// trimming the extracted string (mirrors alacritty's `line_length`).
/// A `first` landing on a wide char's spacer expands to the leading
/// cell — selecting half a wide char selects the whole one.
fn line_text(row: &Row<'_>, first: usize, last: usize) -> String {
    let content = content_length(row);
    if content == 0 {
        return String::new();
    }
    let last = last.min(content - 1);
    // When the start cell is a spacer, its wide char's lead is the cell
    // to the left (mirrors alacritty's `line_to_string`).
    let first = if row
        .cell(first)
        .is_some_and(|cell| cell.kind == CellKind::WideSpacer)
    {
        first.saturating_sub(1)
    } else {
        first
    };
    if first > last {
        return String::new();
    }
    let cells = cell_texts(row);
    cells[first..=last].concat()
}

/// One past the last non-blank cell — the row's content end. Blank means
/// a literal space with default styling (styled blanks count as
/// content), the same notion `alacritty` uses for `line_length`.
fn content_length(row: &Row<'_>) -> usize {
    // `cells()` isn't double-ended; walk the row back to front by index.
    (0..row.len())
        .rev()
        .find(|&column| {
            row.cell(column)
                .is_some_and(|cell| cell.character != ' ' || cell.style != Style::default())
        })
        .map_or(0, |index| index + 1)
}

/// Per-cell text chunks of `row`, indexed by column: each non-spacer
/// cell's base character plus its zero-width riders (combining marks,
/// ZWJ-emoji parts); `""` for spacer cells.
///
/// Rebuilt by walking [`Row::cells`] alongside [`Row::text`]: a chunk
/// starts at the cell's base char and ends where the next non-spacer
/// cell's base char begins. Should a rider ever equal the next base char
/// the boundary lands early — only the attribution between adjacent
/// chunks shifts, never the concatenated text.
fn cell_texts(row: &Row<'_>) -> Vec<String> {
    let cells: Vec<_> = row.cells().collect();
    let row_text = row.text();
    let mut text = row_text.chars().peekable();
    let mut chunks = Vec::with_capacity(cells.len());
    for (index, cell) in cells.iter().enumerate() {
        if cell.kind == CellKind::WideSpacer {
            chunks.push(String::new());
            continue;
        }
        let mut chunk = String::new();
        if let Some(c) = text.next() {
            debug_assert_eq!(c, cell.character, "text/cell drift");
            chunk.push(c);
        }
        let next_base = cells[index + 1..]
            .iter()
            .find(|cell| cell.kind != CellKind::WideSpacer)
            .map(|cell| cell.character);
        while let Some(&c) = text.peek() {
            if Some(c) == next_base {
                break;
            }
            chunk.push(c);
            text.next();
        }
        chunks.push(chunk);
    }
    chunks
}

/// The bracketed-paste open marker (DECSET 2004) — one half of the
/// envelope [`encode_paste`] wraps one-shot paste text in. Spec R04's
/// streaming suggestion insertion opens and closes the same envelope
/// by hand around a `Delta` stream — the stream isn't one string, so
/// `encode_paste` can't apply there — hence `pub`, not private.
pub const PASTE_BEGIN: &[u8] = b"\x1b[200~";
/// See [`PASTE_BEGIN`].
pub const PASTE_END: &[u8] = b"\x1b[201~";

/// Encode paste text for the PTY, wrapped in bracketed-paste markers
/// (`\x1b[200~` … `\x1b[201~`) iff the session reports mode 2004 —
/// see [`Session::bracketed_paste`](ait_term::Session::bracketed_paste).
pub fn encode_paste(text: &str, bracketed_paste: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() + 12);
    if bracketed_paste {
        bytes.extend_from_slice(PASTE_BEGIN);
    }
    bytes.extend_from_slice(text.as_bytes());
    if bracketed_paste {
        bytes.extend_from_slice(PASTE_END);
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use ait_term::{Session, SessionOptions, Size};

    /// Byte-fed session fixture (the R01 seam).
    fn session(columns: usize, lines: usize) -> Session {
        Session::new(SessionOptions {
            size: Size::new(columns, lines),
            ..Default::default()
        })
    }

    fn point(line: i32, column: usize) -> GridPoint {
        GridPoint::new(line, column)
    }

    fn range(start_line: i32, start_col: usize, end_line: i32, end_col: usize) -> SelectionRange {
        SelectionRange {
            start: point(start_line, start_col),
            end: point(end_line, end_col),
        }
    }

    /// A mid-line slice copies just the covered cells.
    #[test]
    fn extract_partial_line() {
        let mut session = session(80, 24);
        session.feed_bytes(b"hello world");
        let grid = session.grid();

        assert_eq!(
            extract(&grid, range(0, 6, 0, 10)),
            "world",
            "cols 6..=10 of 'hello world'"
        );
        assert_eq!(extract(&grid, range(0, 0, 0, 0)), "h");
    }

    /// Separate hard lines join with '\n'.
    #[test]
    fn extract_joins_hard_lines_with_newline() {
        let mut session = session(80, 24);
        session.feed_bytes(b"ab\r\ncd");
        let grid = session.grid();

        assert_eq!(extract(&grid, range(0, 0, 1, 1)), "ab\ncd");
    }

    /// A line scrolled out of history contributes nothing but still
    /// breaks the output — its neighbors never concatenate into one
    /// line that was never on screen.
    #[test]
    fn extract_breaks_on_lines_outside_the_grid() {
        let mut session = session(80, 24);
        session.feed_bytes(b"ab\r\ncd");
        let grid = session.grid();

        // Lines -5..-1 are above all scrollback — each is a break, so
        // the selection spanning them still reads as separate lines
        // rather than "ab" gluing straight onto nothing.
        assert_eq!(extract(&grid, range(-5, 0, 1, 1)), "\n\n\n\n\nab\ncd");
    }

    /// A soft-wrapped row joins the next WITHOUT a newline — the spec's
    /// wrapped-row rule.
    #[test]
    fn extract_joins_wrapped_rows_without_newline() {
        let mut session = session(5, 24);
        // Eight chars on a 5-column grid wrap: line 0 'abcde' (wrapped),
        // line 1 'fgh'.
        session.feed_bytes(b"abcdefgh");
        let grid = session.grid();
        assert!(grid.line(0).unwrap().wrapped());
        assert!(!grid.line(1).unwrap().wrapped());

        assert_eq!(extract(&grid, range(0, 2, 1, 2)), "cdefgh");
        // Selecting to the wrap's end still yields no newline.
        assert_eq!(extract(&grid, range(0, 0, 1, 2)), "abcdefgh");
    }

    /// A chain of wrapped rows flows through; the final hard break ends
    /// the text without a trailing newline.
    #[test]
    fn extract_wrapped_run_then_hard_break() {
        let mut session = session(5, 24);
        // 'abcde' + 'fgh' + wrap again: 'ij' then hard break, 'kl'.
        session.feed_bytes(b"abcdefghij\r\nkl");
        let grid = session.grid();

        assert_eq!(extract(&grid, range(0, 0, 2, 0)), "abcdefghij\nk");
    }

    /// Wide characters copy whole; their spacer cells add nothing, and a
    /// range starting on the spacer still captures the wide char.
    #[test]
    fn extract_skips_wide_spacers() {
        let mut session = session(80, 24);
        session.feed_bytes("コx".as_bytes()); // コ = cols 0-1, x = col 2
        let grid = session.grid();
        assert_eq!(
            grid.cell(0, 1).unwrap().kind,
            CellKind::WideSpacer,
            "fixture check"
        );

        assert_eq!(extract(&grid, range(0, 0, 0, 2)), "コx");
        // Start on the spacer → expands to the leading cell.
        assert_eq!(extract(&grid, range(0, 1, 0, 2)), "コx");
        // Selecting only the spacer still yields the wide char.
        assert_eq!(extract(&grid, range(0, 1, 0, 1)), "コ");
    }

    /// Blanks past a line's content never make it into the copied text,
    /// even when the range covers them.
    #[test]
    fn extract_trims_trailing_blanks() {
        let mut trailing = session(80, 24);
        trailing.feed_bytes(b"ab");
        let grid = trailing.grid();
        assert_eq!(extract(&grid, range(0, 0, 0, 79)), "ab");

        // Interior blanks inside a line's content are kept.
        let mut interior = session(80, 24);
        interior.feed_bytes(b"ab   xy");
        let grid = interior.grid();
        assert_eq!(extract(&grid, range(0, 0, 0, 4)), "ab   ");
    }

    /// Blank lines inside the range survive as empty segments.
    #[test]
    fn extract_keeps_blank_interior_lines() {
        let mut session = session(80, 24);
        session.feed_bytes(b"a\r\n\r\nb");
        let grid = session.grid();

        assert_eq!(extract(&grid, range(0, 0, 2, 0)), "a\n\nb");
    }

    /// Negative line indices reach scrollback; a range can span the
    /// scrollback/viewport boundary.
    #[test]
    fn extract_reaches_into_scrollback() {
        let mut session = session(80, 2);
        session.feed_bytes(b"one\r\ntwo\r\nthree");
        let grid = session.grid();
        assert_eq!(grid.scrollback(), 1, "fixture check");

        assert_eq!(extract(&grid, range(-1, 0, 0, 2)), "one\ntwo");
        assert_eq!(extract(&grid, range(-1, 1, 1, 4)), "ne\ntwo\nthree");
    }

    /// Zero-width riders (combining marks) copy with their base cell.
    #[test]
    fn extract_keeps_zero_width_riders() {
        let mut session = session(80, 24);
        session.feed_bytes("e\u{301}x".as_bytes()); // e + combining acute
        let grid = session.grid();

        assert_eq!(extract(&grid, range(0, 0, 0, 1)), "e\u{301}x");
        // Selecting just the base cell still carries the rider.
        assert_eq!(extract(&grid, range(0, 0, 0, 0)), "e\u{301}");
    }

    /// The result never ends with a newline, even when the last line's
    /// range runs to the column edge.
    #[test]
    fn extract_never_trails_a_newline() {
        let mut session = session(80, 24);
        session.feed_bytes(b"ab\r\ncd");
        let grid = session.grid();

        assert_eq!(extract(&grid, range(0, 0, 1, 79)), "ab\ncd");
    }

    // --- drag state machine ---

    /// begin→extend→finish yields the normalized range; a drag upwards
    /// and left normalizes to start <= end.
    #[test]
    fn drag_normalizes_direction() {
        let mut selection = Selection::default();
        assert_eq!(selection.range(), None);

        selection.begin(point(2, 5));
        selection.extend(point(0, 1));

        let finished = selection.finish().unwrap();
        assert_eq!(finished.start, point(0, 1));
        assert_eq!(finished.end, point(2, 5));
        // The drag ended: extend is a no-op now, and the range stays for
        // the post-release highlight.
        selection.extend(point(9, 9));
        assert_eq!(selection.range(), Some(finished));
    }

    /// A click without movement selects (and copies) the single cell.
    #[test]
    fn click_selects_one_cell() {
        let mut selection = Selection::default();
        selection.begin(point(0, 3));
        let finished = selection.finish().unwrap();
        assert_eq!(finished.start, finished.end);
        assert_eq!(finished.end, point(0, 3));
    }

    /// A new begin or an explicit clear drops the previous range; extend
    /// and finish are no-ops outside a drag.
    #[test]
    fn begin_clears_and_extend_needs_a_drag() {
        let mut selection = Selection::default();
        selection.begin(point(0, 0));
        selection.extend(point(0, 5));
        selection.finish();
        assert!(selection.range().is_some());

        selection.begin(point(1, 1));
        assert_eq!(selection.range().unwrap().end, point(1, 1), "reset head");

        selection.clear();
        assert_eq!(selection.range(), None);
        selection.extend(point(2, 2));
        assert_eq!(selection.range(), None, "extend without a drag");
        assert_eq!(selection.finish(), None, "finish without a drag");
    }

    /// Output arriving mid-selection shifts endpoints like the pinned
    /// scroll offset, keeping the range on the same text.
    #[test]
    fn output_shifts_the_range_with_the_cells() {
        let mut selection = Selection::default();
        selection.begin(point(0, 0));
        selection.extend(point(1, 4));

        selection.on_output(3);
        let shifted = selection.range().unwrap();
        assert_eq!(shifted.start, point(-3, 0));
        assert_eq!(shifted.end, point(-2, 4));
    }

    /// `contains` follows the linear (line, column) order: interior
    /// lines are fully inside, edge lines bound by their columns.
    #[test]
    fn range_contains_bounds_edges() {
        let range = range(0, 2, 2, 4);
        assert!(!range.contains(0, 1));
        assert!(range.contains(0, 2));
        assert!(range.contains(1, 0));
        assert!(range.contains(1, 79));
        assert!(range.contains(2, 4));
        assert!(!range.contains(2, 5));
        assert!(!range.contains(3, 0));
        assert!(!range.contains(-1, 0));
    }

    // --- paste encoding ---

    /// Mode 2004 on → the payload is wrapped in the bracketed-paste
    /// markers; off → the raw text.
    #[test]
    fn paste_wraps_only_when_bracketed() {
        assert_eq!(
            encode_paste("hello", true),
            b"\x1b[200~hello\x1b[201~".to_vec()
        );
        assert_eq!(encode_paste("hello", false), b"hello".to_vec());
        // Markers bracket even multi-line text — that's their point.
        assert_eq!(
            encode_paste("a\nb", true),
            b"\x1b[200~a\nb\x1b[201~".to_vec()
        );
    }
}
