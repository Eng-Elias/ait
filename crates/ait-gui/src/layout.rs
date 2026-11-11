//! The GUI's region contract (Spec R13 §5.12): which pixels belong to
//! which region, and how those pixels map back to grid cells.
//!
//! Five regions, the same ones the TUI's `chrome_areas`/`split_content`
//! carve: a **tab-bar** strip on top, the **content band** split into
//! the **grid pane** and — while a tab's agent panel is open — the
//! **side-panel** region on the right, a **status-line** strip on the
//! bottom, and the **modal layer** painted last over everything (the
//! layout math doesn't model it; `ui` draws it on top).
//!
//! Everything is pure `f32`/`usize` math so the mapping is testable
//! without a GPU context — the PTY winsize invariant the TUI calls
//! "winsize = content band, never the window" lives in
//! [`grid_size`]/[`session_columns`]: the session gets the GRID PANE's
//! cells, never the whole content band's, while the panel is open.

use ait_term::Size;
use eframe::egui::{Pos2, Rect};

/// One terminal cell's pixel metrics — from egui's glyph table at the
/// current font/zoom ([`GuiMetrics`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    /// One grid column in points (the monospace advance).
    pub width: f32,
    /// One grid row in points (the font's row height).
    pub height: f32,
}

/// The cell metrics for a zoom level: `base` is the font size in
/// points at zoom 1.0. Glyph *width* comes from egui's font tables at
/// paint time; the constants here derive the rest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GuiMetrics {
    /// Terminal font size in points at zoom = 1.
    pub base_px: f32,
    /// Ephemeral Ctrl± zoom (1.0 = normal) — the spec's
    /// terminal-text-only zoom, never `Context::set_zoom_factor`.
    pub zoom: f32,
}

impl GuiMetrics {
    /// The egui `FontId` size this metric set implies.
    pub fn font_px(&self) -> f32 {
        self.base_px * self.zoom
    }

    /// Zoom in one step (Ctrl+=): ~10% bigger, capped.
    pub fn zoom_in(&mut self) {
        self.zoom = (self.zoom * ZOOM_STEP).min(ZOOM_MAX);
    }

    /// Zoom out one step (Ctrl+-).
    pub fn zoom_out(&mut self) {
        self.zoom = (self.zoom / ZOOM_STEP).max(ZOOM_MIN);
    }

    /// Ctrl+0 — back to 1.
    pub fn zoom_reset(&mut self) {
        self.zoom = 1.0;
    }
}

/// Ctrl± zoom step and bounds.
pub const ZOOM_STEP: f32 = 1.1;
pub const ZOOM_MIN: f32 = 0.5;
pub const ZOOM_MAX: f32 = 4.0;

/// The three chrome strips' heights in points — multiples of the cell
/// height would tie chrome to zoom; fixed heights keep the tab bar and
/// status line readable at any zoom (the grid pane absorbs the rest).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Chrome {
    /// Tab-bar strip height (points).
    pub tab_bar: f32,
    /// Status-line strip height (points).
    pub status: f32,
}

impl Default for Chrome {
    fn default() -> Self {
        Self {
            tab_bar: 28.0,
            status: 22.0,
        }
    }
}

/// The window's region split — pixel rects in egui's root coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Regions {
    /// Top strip: the clickable tab labels.
    pub tab_bar: Rect,
    /// Middle band: grid pane + optional side panel.
    pub content: Rect,
    /// Bottom strip: `status_text` / toasts.
    pub status: Rect,
}

/// Split `area` into the three bands — the TUI's `chrome_areas` in
/// pixels. Degenerate windows (height below both strips) clamp rather
/// than produce negative content.
pub fn chrome_regions(area: Rect, chrome: Chrome) -> Regions {
    let top = area.top();
    let status_top = (area.bottom() - chrome.status).max(top);
    let content_top = (top + chrome.tab_bar).min(status_top);
    Regions {
        tab_bar: Rect::from_min_max(
            Pos2::new(area.left(), top),
            Pos2::new(area.right(), content_top),
        ),
        content: Rect::from_min_max(
            Pos2::new(area.left(), content_top),
            Pos2::new(area.right(), status_top),
        ),
        status: Rect::from_min_max(
            Pos2::new(area.left(), status_top),
            Pos2::new(area.right(), area.bottom()),
        ),
    }
}

/// The side panel's column share on a `width`-column band: two fifths,
/// never more than leaves the session one column — the same §3-of-R06
/// split rule the TUI's `panel_columns` applies (kept in step by the
/// pair of tests below; the formula is identical by contract).
fn panel_columns(width: usize) -> usize {
    (width * 2 / 5).min(width.saturating_sub(1))
}

/// The grid pane's column count on a `width`-column band — the whole
/// band, or minus the open agent panel's share. The closure handed to
/// `TabManager::resize_all`/`resize_active` applies this same rule so
/// the painted split and the PTY winsize can never disagree.
pub fn session_columns(width: usize, panel_open: bool) -> usize {
    if panel_open {
        width - panel_columns(width)
    } else {
        width
    }
}

/// The frontend-computed winsize derivation `TabManager::resize_all` /
/// `resize_active` take (`Size`-in-band → `Size`-winsize): the grid
/// pane's cells — the band minus the panel share when `panel_open`.
pub fn session_size(band: Size, panel_open: bool) -> Size {
    Size::new(
        session_columns(band.columns, panel_open).max(1),
        band.lines.max(1),
    )
}

/// Split the content band into the grid pane and — when `panel_open` —
/// the right-side side-panel region. Pane widths snap to whole cells:
/// the panel's pixel width is its column count × cell width, so the
/// painted split matches the winsize [`session_size`] hands the PTY
/// exactly (CODING_STANDARDS §17 — one formula, two consumers).
pub fn split_content(content: Rect, panel_open: bool, cell: CellMetrics) -> (Rect, Option<Rect>) {
    let band_columns = (content.width() / cell.width).floor() as usize;
    let pane_columns = session_columns(band_columns, panel_open);
    let pane_right = content.left() + pane_columns as f32 * cell.width;
    let grid = Rect::from_min_max(
        content.min,
        Pos2::new(pane_right.min(content.right()), content.bottom()),
    );
    let panel = (grid.right() < content.right())
        .then(|| Rect::from_min_max(Pos2::new(grid.right(), content.top()), content.max));
    (grid, panel)
}

/// A rect's cell capacity — how many whole columns/lines fit. Never
/// zero: a terminal always has at least a 1×1 grid (the TUI's
/// `content_size` clamp).
pub fn grid_size(rect: Rect, cell: CellMetrics) -> Size {
    Size::new(
        (rect.width() / cell.width).floor().max(1.0) as usize,
        (rect.height() / cell.height).floor().max(1.0) as usize,
    )
}

/// Pixel position → grid coordinates: `GridPoint::new(line, column)`
/// where `line` follows the shared `grid_line` rule (row_index −
/// scroll_offset; 0 = live viewport top, negative = scrollback).
/// Positions outside the pane clamp to its cell edges — a drag past
/// the top selects into scrollback like the TUI's
/// `grid_point`/`extend_selection` pair.
pub fn grid_point(pos: Pos2, pane: Rect, cell: CellMetrics, scroll_offset: usize) -> (i32, usize) {
    let row = ((pos.y - pane.top()) / cell.height).floor() as i64;
    let col = ((pos.x - pane.left()) / cell.width).floor() as i64;
    let offset = i64::try_from(scroll_offset).unwrap_or(i64::MAX);
    let line = (row - offset).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    (
        line,
        col.clamp(0, (pane.width() / cell.width) as i64 - 1).max(0) as usize,
    )
}

/// A grid line index + scroll offset → the screen row it paints on
/// (the reverse direction of the `grid_line` rule; used to place the
/// cursor — it lives in the live viewport only, so rows past the pane
/// simply aren't drawn).
pub fn cursor_row(cursor_line: usize, scroll_offset: usize) -> i64 {
    cursor_line as i64 + scroll_offset as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::Vec2;

    const CELL: CellMetrics = CellMetrics {
        width: 8.0,
        height: 16.0,
    };
    const CHROME: Chrome = Chrome {
        tab_bar: 28.0,
        status: 22.0,
    };

    fn window() -> Rect {
        Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 500.0))
    }

    /// The chrome split: tab bar on top, status on bottom, content in
    /// between — pixel mirrors of the TUI's 1/Min/1 layout.
    #[test]
    fn chrome_regions_split_bands() {
        let regions = chrome_regions(window(), CHROME);
        assert_eq!(regions.tab_bar.height(), 28.0);
        assert_eq!(regions.status.height(), 22.0);
        assert_eq!(regions.content.height(), 450.0);
        assert_eq!(regions.tab_bar.top(), 0.0);
        assert_eq!(regions.status.bottom(), 500.0);
    }

    /// The panel takes 2/5 of the band, never the whole thing — and
    /// the winsize derivation hands the PTY the REMAINING columns.
    #[test]
    fn panel_split_matches_winsize() {
        // 100-column band: panel 40, session 60.
        assert_eq!(session_columns(100, true), 60);
        assert_eq!(session_columns(100, false), 100);
        // A 2-column band can't spare even one to the panel.
        assert_eq!(session_columns(2, true), 2);
        let size = session_size(Size::new(100, 40), true);
        assert_eq!(size.columns, 60);
        assert_eq!(size.lines, 40);

        let (grid, panel) = split_content(
            Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 450.0)),
            true,
            CELL,
        );
        // 800px/8px = 100 columns → pane 60 → 480px, panel the rest.
        assert_eq!(grid.width(), 480.0);
        assert_eq!(panel.unwrap().width(), 320.0);
    }

    /// `grid_size` clamps to 1×1 — a starved pane never implies a
    /// zero-cell PTY.
    #[test]
    fn grid_size_never_zero() {
        let size = grid_size(Rect::from_min_size(Pos2::ZERO, Vec2::new(1.0, 1.0)), CELL);
        assert_eq!((size.columns, size.lines), (1, 1));
    }

    /// Pixel→grid: scrolled views map screen rows into scrollback
    /// (negative lines), and out-of-pane positions clamp to the edges.
    #[test]
    fn grid_point_follows_scroll_offset() {
        let pane = Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(80.0, 48.0));
        // Row 0 live: scroll 0 → line 0; scroll 3 → line -3.
        assert_eq!(grid_point(Pos2::new(0.0, 0.0), pane, CELL, 0), (0, 0));
        assert_eq!(grid_point(Pos2::new(0.0, 0.0), pane, CELL, 3), (-3, 0));
        // Cell (2,1) un-scrolled.
        assert_eq!(grid_point(Pos2::new(20.0, 20.0), pane, CELL, 0), (1, 2));
        // Below the pane clamps to its last column/line edge.
        let (line, col) = grid_point(Pos2::new(400.0, 500.0), pane, CELL, 0);
        assert_eq!(col, 9);
        assert!(line > 0);
    }
}
