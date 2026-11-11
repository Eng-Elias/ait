//! Scrollback search (Spec R13 §5.12 — "scrollback with
//! search/selection"; the spec's scrollback-search seam): the pure
//! scan every frontend shares plus the find bar's state.
//!
//! [`search_grid`] walks a [`GridView`] — scrollback first, then the
//! viewport — matching against [`Row::text`] and returning hits in
//! **grid-line coordinates** (`0` = live viewport top, negative into
//! scrollback — the same `grid_line` rule [`crate::selection`] and the
//! scroll offset share). [`FindBar`] is the bar's frontend-agnostic
//! state: the query, the last scan's hits, the current index and the
//! anchor fresh matches are measured from. Neither type knows a
//! frontend exists — the whole seam is unit-testable without a GPU or
//! a PTY (CODING_STANDARDS.md §11).

use ait_term::{CellKind, GridView, Row};

/// One match in [`GridView`] line space: the row it sits on and the
/// grid columns it covers — a hit that ends on a wide character covers
/// that character's spacer column too, so a highlight never slices a
/// glyph in half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchHit {
    /// The grid line: `0` = live viewport top, negative = scrollback.
    pub line: i32,
    /// The first covered grid column.
    pub column: usize,
    /// How many grid columns the match covers.
    pub cells: usize,
}

/// Scan `grid` for `needle`, oldest scrollback line through the live
/// viewport's last row — hits come out sorted by `(line, column)`.
///
/// Matching is case-insensitive, folded per character (the find bar's
/// convention); an empty needle matches nothing. A match never spans
/// rows — even soft-wrapped ones: `Row::text` is the unit, so a query
/// split across a wrap boundary is not found (the accepted bound,
/// same as most terminal find bars).
pub fn search_grid(grid: &GridView<'_>, needle: &str) -> Vec<SearchHit> {
    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let top = -(grid.scrollback() as i32);
    let mut hits = Vec::new();
    for line in top..grid.lines() as i32 {
        let Some(row) = grid.line(line) else {
            continue;
        };
        search_row(&row, &needle, line, &mut hits);
    }
    hits
}

/// One row's contribution: fold its text to lowercase chars tagged
/// with their cell's `(column, cover)` and slide the folded needle
/// across. `Row::text` emits each non-spacer cell's character plus its
/// zero-width tail — the cells walk the text in the same order, so a
/// cell's slice runs up to the next cell's character (the alignment
/// every text consumer shares). A `to_lowercase` expansion keeps its
/// source char's cell, so a folded multi-char letter still highlights
/// the cell it came from.
fn search_row(row: &Row<'_>, needle: &[char], line: i32, hits: &mut Vec<SearchHit>) {
    let text = row.text();
    if text.is_empty() {
        return;
    }
    let cells: Vec<(usize, ait_term::Cell)> = row
        .cells()
        .enumerate()
        .filter(|(_, cell)| cell.kind != CellKind::WideSpacer)
        .collect();
    let mut rest = text.as_str();
    let mut folded: Vec<char> = Vec::with_capacity(text.len());
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(text.len());
    for (index, (column, cell)) in cells.iter().enumerate() {
        let own = cell.character.len_utf8();
        let end = match cells.get(index + 1) {
            Some((_, next)) => {
                own + rest[own.min(rest.len())..]
                    .find(next.character)
                    .unwrap_or_else(|| rest.len().saturating_sub(own))
            }
            None => rest.len(),
        };
        let (slice, tail) = rest.split_at(end.min(rest.len()));
        rest = tail;
        let cover = if cell.kind == CellKind::WideLead {
            2
        } else {
            1
        };
        for c in slice.chars().flat_map(char::to_lowercase) {
            folded.push(c);
            spans.push((*column, cover));
        }
    }
    if needle.len() > folded.len() {
        return;
    }
    for start in 0..=folded.len() - needle.len() {
        if folded[start..start + needle.len()] == needle[..] {
            let (column, _) = spans[start];
            let (last, last_cover) = spans[start + needle.len() - 1];
            hits.push(SearchHit {
                line,
                column,
                cells: last + last_cover - column,
            });
        }
    }
}

/// The find bar's state: `query`, the last scan's `hits`, the current
/// index, and `anchor` — the grid line fresh matches are measured
/// against. `tab` is the stable `Tab::id` the hits were scanned from
/// (CODING_STANDARDS.md §14): a frontend that hosts several sessions
/// retargets rather than highlighting the wrong one.
///
/// Hits are snapshots: output arriving after a scan shifts grid line
/// indices, so the consumer rescans when the session drains (the same
/// documented bound [`crate::scroll::ScrollState::on_output`] sets —
/// the grid exposes no per-line identity).
#[derive(Debug)]
pub struct FindBar {
    /// The `Tab::id` `hits` were scanned from.
    tab: u64,
    /// The query text as typed.
    query: String,
    /// [`search_grid`]'s last result, sorted by `(line, column)`.
    hits: Vec<SearchHit>,
    /// The current hit's index into `hits` — `None` only when `hits`
    /// is empty.
    current: Option<usize>,
    /// The grid line a fresh scan's "first match" is measured from:
    /// the view's top row when the bar opened, then the current hit's
    /// line — so editing the query keeps its position instead of
    /// jumping back to the oldest hit.
    anchor: i32,
}

impl FindBar {
    /// A bar just opened on `tab`, measuring first matches from grid
    /// line `anchor` (the view's top row: `-scroll_offset`).
    pub fn new(tab: u64, anchor: i32) -> Self {
        Self {
            tab,
            query: String::new(),
            hits: Vec::new(),
            current: None,
            anchor,
        }
    }

    /// The `Tab::id` the hits were scanned from.
    pub fn tab(&self) -> u64 {
        self.tab
    }

    /// Point the bar at another session (the user switched tabs with
    /// the bar open): the query carries over — the browser convention —
    /// and `anchor` resets to the new view's top row before the caller
    /// rescans.
    pub fn retarget(&mut self, tab: u64, anchor: i32, grid: &GridView<'_>) {
        self.tab = tab;
        self.anchor = anchor;
        self.rescan(grid);
    }

    /// The query text as typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The last scan's hits, sorted by `(line, column)`.
    pub fn hits(&self) -> &[SearchHit] {
        &self.hits
    }

    /// The current hit, if any match exists.
    pub fn current(&self) -> Option<SearchHit> {
        self.current.map(|index| self.hits[index])
    }

    /// The current hit's index into [`hits`](Self::hits) — what a
    /// highlight pass needs to paint it differently.
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    /// The bar's `n/m` readout — `Some((current + 1, hits))` while a
    /// match is current, `None` when nothing matched.
    pub fn position(&self) -> Option<(usize, usize)> {
        self.current.map(|index| (index + 1, self.hits.len()))
    }

    /// Append typed text to the query and rescan `grid`.
    pub fn push_str(&mut self, grid: &GridView<'_>, text: &str) {
        self.query.push_str(text);
        self.rescan(grid);
    }

    /// Remove the query's last character and rescan `grid` — a no-op
    /// on an empty query.
    pub fn pop(&mut self, grid: &GridView<'_>) {
        if self.query.pop().is_some() {
            self.rescan(grid);
        }
    }

    /// Replace the query wholesale and rescan `grid`.
    pub fn set_query(&mut self, grid: &GridView<'_>, query: &str) {
        self.query.clear();
        self.query.push_str(query);
        self.rescan(grid);
    }

    /// Rescan `grid` with the same query — the refresh a drain pass
    /// runs when output arrived while the bar was open. The current
    /// hit's line becomes the anchor first, so a still-visible match
    /// stays current across the rescan.
    pub fn rescan(&mut self, grid: &GridView<'_>) {
        if let Some(hit) = self.current() {
            self.anchor = hit.line;
        }
        self.hits = search_grid(grid, &self.query);
        self.current = self.pick();
    }

    /// `Enter`: advance to the next match, wrapping at the end —
    /// returns the new current hit for the caller to reveal.
    pub fn next_match(&mut self) -> Option<SearchHit> {
        self.step(1)
    }

    /// `Shift+Enter`: step back to the previous match, wrapping at the
    /// start — returns the new current hit.
    pub fn prev_match(&mut self) -> Option<SearchHit> {
        self.step(-1)
    }

    /// Shared next/prev: `direction` is +1/-1. A bar with no current
    /// hit but some matches starts from the anchor's pick.
    fn step(&mut self, direction: i64) -> Option<SearchHit> {
        if self.hits.is_empty() {
            self.current = None;
            return None;
        }
        let len = self.hits.len() as i64;
        let index = match self.current {
            Some(current) => (current as i64 + direction).rem_euclid(len) as usize,
            None => self.pick()?,
        };
        self.current = Some(index);
        self.anchor = self.hits[index].line;
        Some(self.hits[index])
    }

    /// The first hit at or below `anchor` — the "find next from the
    /// view" convention; when every hit sits above it, the nearest one
    /// (the last) wins.
    fn pick(&self) -> Option<usize> {
        if self.hits.is_empty() {
            return None;
        }
        let index = self.hits.partition_point(|hit| hit.line < self.anchor);
        Some(if index < self.hits.len() {
            index
        } else {
            self.hits.len() - 1
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ait_term::{Session, SessionOptions, Size};

    /// Byte-fed session fixture — the R01 seam (no PTY needed).
    fn session(columns: usize, lines: usize) -> Session {
        Session::new(SessionOptions {
            size: Size::new(columns, lines),
            ..Default::default()
        })
    }

    fn feed(session: &mut Session, text: &str) {
        session.feed_bytes(text.as_bytes());
    }

    /// A plain scan: matches land on the right line and cover the
    /// needle's columns; an empty needle scans nothing.
    #[test]
    fn matches_report_grid_cells() {
        let mut session = session(40, 10);
        feed(&mut session, "hello hello\r\nworld");
        let hits = search_grid(&session.grid(), "hello");
        assert_eq!(
            hits,
            vec![
                SearchHit {
                    line: 0,
                    column: 0,
                    cells: 5
                },
                SearchHit {
                    line: 0,
                    column: 6,
                    cells: 5
                },
            ]
        );
        assert!(search_grid(&session.grid(), "").is_empty());
        assert!(search_grid(&session.grid(), "zzz").is_empty());
    }

    /// The scan covers scrollback, not just the viewport — lines that
    /// scrolled off carry negative line indices (the shared grid-line
    /// rule).
    #[test]
    fn scrollback_lines_search_negative() {
        let mut session = session(10, 3);
        feed(&mut session, "one\r\ntwo\r\nthree\r\nfour");
        let grid = session.grid();
        assert_eq!(grid.scrollback(), 1);
        let hits = search_grid(&grid, "one");
        assert_eq!(
            hits,
            vec![SearchHit {
                line: -1,
                column: 0,
                cells: 3
            }]
        );
        // …and the viewport's hits keep their own lines.
        let hits = search_grid(&grid, "o");
        let lines: Vec<i32> = hits.iter().map(|hit| hit.line).collect();
        assert_eq!(lines, vec![-1, 0, 2], "sorted scrollback→viewport");
    }

    /// Case-insensitive matching, folded per character — `ERROR` finds
    /// `error`, and a fold-expanding needle still maps onto cells.
    #[test]
    fn matching_is_case_insensitive() {
        let mut session = session(40, 5);
        feed(&mut session, "Error error ERROR");
        let hits = search_grid(&session.grid(), "error");
        assert_eq!(hits.len(), 3);
        assert_eq!(
            hits.iter().map(|hit| hit.column).collect::<Vec<_>>(),
            vec![0, 6, 12]
        );
        let hits = search_grid(&session.grid(), "ERROR");
        assert_eq!(hits.len(), 3);
    }

    /// A match that ends on a wide character covers its spacer column;
    /// a match starting after one accounts for the two columns it
    /// occupies.
    #[test]
    fn wide_chars_cover_their_columns() {
        let mut session = session(40, 5);
        // '' occupies columns 1–2 (lead + spacer); 'end' starts at 3.
        feed(&mut session, "a\u{4e16}end");
        let hits = search_grid(&session.grid(), "\u{4e16}");
        assert_eq!(
            hits,
            vec![SearchHit {
                line: 0,
                column: 1,
                cells: 2
            }]
        );
        let hits = search_grid(&session.grid(), "a\u{4e16}");
        assert_eq!(
            hits,
            vec![SearchHit {
                line: 0,
                column: 0,
                cells: 3
            }]
        );
        let hits = search_grid(&session.grid(), "end");
        assert_eq!(hits[0].column, 3);
    }

    /// `Tab::id` pinning, the `n/m` readout and the anchor pick: a bar
    /// opened at the live viewport (anchor 0) starts on the first
    /// viewport match, not the scrollback's.
    #[test]
    fn find_bar_anchors_on_the_view() {
        let mut session = session(10, 3);
        feed(&mut session, "x\r\nx\r\nx\r\nx");
        let grid = session.grid();
        // "x" on every line: one scrollback line (-1), three viewport.
        let mut bar = FindBar::new(7, 0);
        bar.set_query(&grid, "x");
        assert_eq!(bar.tab(), 7);
        assert_eq!(bar.hits().len(), 4);
        // hits[0] is the scrollback line; the anchor pick lands on the
        // first viewport hit (line 0 → hits[1]).
        assert_eq!(bar.current(), Some(bar.hits()[1]));
        assert_eq!(bar.current().unwrap().line, 0);
        assert_eq!(bar.position(), Some((2, 4)));
        // Every hit above the anchor → the nearest one (the last) wins.
        let mut bar = FindBar::new(7, 99);
        bar.set_query(&grid, "x");
        assert_eq!(bar.current().unwrap().line, 2);
    }

    /// Enter/Shift+Enter walk the hits in both directions and wrap at
    /// both ends; each move re-anchors the bar on the hit it landed on.
    #[test]
    fn next_prev_wrap_both_ends() {
        let mut one_hit = session(10, 5);
        feed(&mut one_hit, "a\r\nb\r\nc");
        let grid = one_hit.grid();
        let mut bar = FindBar::new(0, 0);
        // An empty query matches nothing — stepping is a no-op, not a
        // panic or a ghost hit.
        bar.set_query(&grid, "");
        assert_eq!(bar.next_match(), None);
        bar.set_query(&grid, "a");
        // Only "a" matches — next/prev both wrap back to it.
        let hit = bar.current().unwrap();
        assert_eq!(bar.next_match(), Some(hit));
        assert_eq!(bar.prev_match(), Some(hit));

        // Three hits on three rows — 'o' lands on "one" (line 0),
        // "two" (line 1) and "four" (line 3): the walk goes down the
        // rows and wraps at the end, backwards at the start.
        let mut three_hits = session(10, 5);
        feed(&mut three_hits, "one\r\ntwo\r\nthree\r\nfour");
        let grid = three_hits.grid();
        let mut bar = FindBar::new(0, 0);
        bar.set_query(&grid, "o");
        assert_eq!(bar.hits().len(), 3);
        assert_eq!(bar.current().unwrap().line, 0);
        assert_eq!(bar.next_match().unwrap().line, 1);
        assert_eq!(bar.next_match().unwrap().line, 3);
        assert_eq!(bar.next_match().unwrap().line, 0, "wraps to the first");
        assert_eq!(bar.prev_match().unwrap().line, 3, "prev wraps to the last");
    }

    /// Typing edits the query live: `push_str`/`pop` rescan and keep
    /// `current` near the anchor instead of snapping to the oldest hit.
    #[test]
    fn typing_rescans_around_the_anchor() {
        let mut session = session(20, 6);
        feed(&mut session, "cat\r\ndog\r\nCAR\r\nbird");
        let grid = session.grid();
        let mut bar = FindBar::new(0, 2);
        bar.push_str(&grid, "c");
        // Anchor 2 → first hit at/below line 2 is "CAR" on line 2 —
        // hits[1] ("cat" on line 0 sorts first).
        assert_eq!(bar.current().unwrap().line, 2);
        assert_eq!(bar.current(), bar.hits().get(1).copied());
        bar.push_str(&grid, "a");
        // "ca" still hits "cat" (line 0) and "CAR" (line 2) — the
        // anchor stays on line 2, so current stays there.
        assert_eq!(bar.current().unwrap().line, 2);
        bar.pop(&grid);
        assert_eq!(bar.query(), "c");
        bar.pop(&grid);
        assert_eq!(bar.query(), "");
        assert!(bar.hits().is_empty());
        assert_eq!(bar.current(), None);
        bar.pop(&grid); // empty-query pop is a no-op, not a panic.
    }

    /// A rescan after output shifted the grid re-picks `current` by
    /// the anchor — the bar tracks fresh line numbers instead of
    /// highlighting stale ones.
    #[test]
    fn rescan_follows_output() {
        let mut session = session(10, 3);
        feed(&mut session, "x\r\ny");
        {
            let grid = session.grid();
            let mut bar = FindBar::new(0, 0);
            bar.set_query(&grid, "x");
            assert_eq!(bar.current().unwrap().line, 0);
            // One more line of output pushes "x" into scrollback (-1).
            feed(&mut session, "\r\nz\r\nw");
            let grid = session.grid();
            bar.rescan(&grid);
            assert_eq!(bar.current().unwrap().line, -1);
            assert_eq!(bar.hits().len(), 1);
        }
    }

    /// Retargeting carries the query to the new session and re-anchors
    /// on its view — the tab-switch case.
    #[test]
    fn retarget_rescans_the_new_tab() {
        let mut one = session(10, 3);
        feed(&mut one, "x");
        let mut two = session(10, 3);
        feed(&mut two, "y\r\nx\r\nx");
        let mut bar = FindBar::new(1, 0);
        bar.set_query(&one.grid(), "x");
        assert_eq!(bar.tab(), 1);
        bar.retarget(2, 0, &two.grid());
        assert_eq!(bar.tab(), 2);
        assert_eq!(bar.hits().len(), 2);
        assert_eq!(bar.current().unwrap().line, 1);
    }
}
