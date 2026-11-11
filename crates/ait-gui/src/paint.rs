//! The egui painting pass: [`RowPlan`]s + the cursor + the selection
//! → `Painter` calls. Every draw is clipped to the grid pane; nothing
//! here decides WHAT to paint (that is [`grid::plan_viewport`]'s pure
//! planning half — the split the spec's testing decisions ask for).
//!
//! Per-span `painter.text` is the T2-simple path: every span starts at
//! its exact column so ASCII/monospace content lands cell-aligned by
//! the same `CellMetrics` the winsize math used. The seam for T4's
//! cached-galley runs is [`paint_row`] — swap the per-span `text` call
//! for a per-row `LayoutJob` without touching the planner.
//!
//! [Spec R13 §5.12, REQUIREMENTS.md §5.12]

use ait_app::search::SearchHit;
use ait_app::selection::SelectionRange;
use ait_term::{Cursor, CursorShape, GridView, Underline};
use eframe::egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Stroke, Vec2};

use crate::grid::{plan_viewport, CellSpan, GridTheme, RowPlan};
use crate::layout::{cursor_row, BlockChip, CellMetrics};

/// Everything one paint pass needs that isn't per-row state — the
/// painter (already clipped to `pane`), the pane rect, the cell
/// metrics the winsize math used, the theme, and the grid font.
/// Grouping keeps the draw helpers' signatures honest (clippy's
/// `too_many_arguments` is a smell list, not a style rule).
pub struct GridSurface<'a> {
    /// The `Painter`, clipped to `pane` by the caller.
    pub painter: &'a Painter,
    /// The grid pane's rect — the winsize's pixel counterpart.
    pub pane: Rect,
    /// One cell's pixel size — every x/y derives from it.
    pub cell: CellMetrics,
    /// The resolved palette.
    pub theme: GridTheme,
    /// The monospace `FontId` at the current zoom.
    pub font: &'a FontId,
}

/// The overlays painted between the planned rows and the cursor —
/// all optional so a bare `paint_grid` call is just `Overlays::default`.
/// Every input arrives already mapped to the spaces the fields name —
/// the planner fns in `crate::layout`/`ait_app::search` own the
/// conversions.
#[derive(Debug, Clone, Copy, Default)]
pub struct Overlays<'a> {
    /// The find bar's matches (`ait_app::search::search_grid` output —
    /// grid-line coordinates) and the current hit's index, painted as
    /// a translucent wash over the covered cells.
    pub find: Option<FindOverlay<'a>>,
    /// The planned command-block chrome — divider rows plus
    /// exit-status chips — in screen rows already
    /// (`layout::plan_block_chrome` output). Empty/`None` outside
    /// `Integrated` sessions, which is what keeps MarksOnly/Absent
    /// tabs identical to plain output.
    pub block_chips: Option<&'a [BlockChip]>,
}

/// The find bar's paint inputs — the full hit list plus which index
/// is current.
#[derive(Debug, Clone, Copy)]
pub struct FindOverlay<'a> {
    /// All hits in `GridView` line space (sorted by `(line, column)`).
    pub hits: &'a [SearchHit],
    /// The bar's current hit's index into `hits`.
    pub current: Option<usize>,
}

impl GridSurface<'_> {
    /// Cells wide/tall the pane fits (floor — the leftover sliver
    /// stays theme bg).
    fn dimensions(&self) -> (usize, usize) {
        (
            (self.pane.width() / self.cell.width).floor() as usize,
            (self.pane.height() / self.cell.height).floor() as usize,
        )
    }

    /// Grid column → pane x.
    fn x(&self, column: usize) -> f32 {
        self.pane.left() + column as f32 * self.cell.width
    }

    /// Screen row → pane y.
    fn y(&self, row: usize) -> f32 {
        self.pane.top() + row as f32 * self.cell.height
    }
}

/// The underline's vertical offset below the baseline — most of a cell
/// from its top, with a minimum of one pixel off the cell edge.
fn underline_y(row_top: f32, cell: CellMetrics) -> f32 {
    row_top + (cell.height - 1.5).max(1.0)
}

/// Bold's degrade: egui's bundled monospace has no bold face, so a
/// bold run gets a brighter foreground (the same trick light terminals
/// use for "bold" before they had two faces).
fn bold_color(fg: Color32) -> Color32 {
    let [r, g, b, a] = fg.to_array();
    let lift = |v: u8| v.saturating_add((255 - v) / 3);
    Color32::from_rgba_unmultiplied(lift(r), lift(g), lift(b), a)
}

/// Paint one planned row: background rects first (cell-exact, only
/// where the resolved bg differs from the theme's), then each span's
/// text and decoration lines.
fn paint_row(surface: &GridSurface<'_>, row_top: f32, plan: &RowPlan) {
    for span in &plan.spans {
        let x = surface.x(span.col);
        let w = span.cells as f32 * surface.cell.width;
        if let Some(bg) = span.bg {
            surface.painter.rect_filled(
                Rect::from_min_size(Pos2::new(x, row_top), Vec2::new(w, surface.cell.height)),
                0.0,
                bg,
            );
        }
        // `hidden` cells keep their bg but paint no text — SGR 8's
        // "invisible" is a blank with its colors.
        if !span.hidden && !span.text.is_empty() {
            let fg = if span.bold {
                bold_color(span.fg)
            } else {
                span.fg
            };
            surface.painter.text(
                Pos2::new(x, row_top),
                Align2::LEFT_TOP,
                &span.text,
                surface.font.clone(),
                fg,
            );
            paint_decoration(surface, span, x, row_top, w, fg);
        }
    }
}

/// The underline/strikethrough line shapes for one span — egui's
/// `TextFormat` covers single-underline only, so all variants draw
/// explicit segments at the cell-exact span width.
fn paint_decoration(
    surface: &GridSurface<'_>,
    span: &CellSpan,
    x: f32,
    row_top: f32,
    w: f32,
    fg: Color32,
) {
    let painter = surface.painter;
    let stroke = Stroke::new(1.0, fg);
    let underline_y = underline_y(row_top, surface.cell);
    let from = Pos2::new(x, underline_y);
    let to = Pos2::new(x + w, underline_y);
    match span.underline {
        Underline::None => {}
        Underline::Single => {
            painter.line_segment([from, to], stroke);
        }
        Underline::Double => {
            painter.line_segment([from, to], stroke);
            let dy = Vec2::new(0.0, 2.0);
            painter.line_segment([from + dy, to + dy], stroke);
        }
        Underline::Curly => {
            // A sine wave across the span — one segment per ~3px so
            // short spans still show the wave's shape.
            let segments = ((w / 3.0) as usize).max(4);
            let points: Vec<Pos2> = (0..=segments)
                .map(|i| {
                    let t = i as f32 / segments as f32;
                    Pos2::new(
                        x + t * w,
                        underline_y + (t * std::f32::consts::TAU).sin() * 1.2,
                    )
                })
                .collect();
            painter.add(eframe::egui::Shape::line(points, stroke));
        }
        Underline::Dotted | Underline::Dashed => {
            // Dash pattern: on 2px / off 2px (dotted) or on 4px / off
            // 2px (dashed), drawn as separate segments — egui has no
            // dashed stroke primitive.
            let on = if span.underline == Underline::Dotted {
                2.0
            } else {
                4.0
            };
            let off = 2.0;
            let mut start = 0.0f32;
            while start < w {
                let end = (start + on).min(w);
                painter.line_segment(
                    [
                        Pos2::new(x + start, underline_y),
                        Pos2::new(x + end, underline_y),
                    ],
                    stroke,
                );
                start += on + off;
            }
        }
    }
    if span.strikeout {
        let y = row_top + surface.cell.height * 0.55;
        painter.line_segment([Pos2::new(x, y), Pos2::new(x + w, y)], stroke);
    }
}

/// The block/beam/underline/hollow cursor at its viewport cell — only
/// painted when the cursor's screen row is inside the pane (a scrolled
/// view shows it at its live position near the bottom, like the TUI;
/// past the pane's rows it simply isn't drawn).
fn paint_cursor(surface: &GridSurface<'_>, cursor: Cursor, scroll_offset: usize, rows: usize) {
    if cursor.shape == CursorShape::Hidden {
        return;
    }
    let row = cursor_row(cursor.line, scroll_offset);
    if row < 0 || row >= rows as i64 {
        return;
    }
    let rect = Rect::from_min_size(
        Pos2::new(surface.x(cursor.column), surface.y(row as usize)),
        Vec2::new(surface.cell.width, surface.cell.height),
    );
    let painter = surface.painter;
    let stroke = Stroke::new(1.0, surface.theme.cursor);
    match cursor.shape {
        CursorShape::Block => {
            painter.rect_filled(rect, 0.0, surface.theme.cursor);
        }
        CursorShape::HollowBlock => {
            painter.rect_stroke(rect, 0.0, stroke, eframe::egui::StrokeKind::Inside);
        }
        CursorShape::Underline => {
            painter.line_segment(
                [
                    Pos2::new(rect.left(), rect.bottom() - 1.0),
                    Pos2::new(rect.right(), rect.bottom() - 1.0),
                ],
                Stroke::new(2.0, surface.theme.cursor),
            );
        }
        CursorShape::Beam => {
            painter.line_segment(
                [
                    Pos2::new(rect.left(), rect.top()),
                    Pos2::new(rect.left(), rect.bottom()),
                ],
                Stroke::new(2.0, surface.theme.cursor),
            );
        }
        CursorShape::Hidden => {}
    }
}

/// The selection wash: for each painted row inside `range`, the
/// covered column span gets the theme's translucent selection rect —
/// the GUI's counterpart to the TUI's reverse-video highlight.
fn paint_selection(
    surface: &GridSurface<'_>,
    range: SelectionRange,
    scroll_offset: usize,
    width: usize,
    rows: usize,
) {
    let offset = i64::try_from(scroll_offset).unwrap_or(i64::MAX);
    for row_index in 0..rows {
        let line = row_index as i64 - offset;
        let Ok(line) = i32::try_from(line) else {
            continue;
        };
        // First and last covered column on this line — a selection's
        // coverage within one line is always contiguous.
        let mut start = None;
        let mut end = 0usize;
        for column in 0..width {
            if range.contains(line, column) {
                if start.is_none() {
                    start = Some(column);
                }
                end = column;
            }
        }
        if let Some(start) = start {
            surface.painter.rect_filled(
                Rect::from_min_size(
                    Pos2::new(surface.x(start), surface.y(row_index)),
                    Vec2::new(
                        (end - start + 1) as f32 * surface.cell.width,
                        surface.cell.height,
                    ),
                ),
                0.0,
                surface.theme.selection,
            );
        }
    }
}

/// The find bar's wash: each hit's covered columns get the theme's
/// translucent match rect — the current one in the stronger color so
/// it stands out in a row of hits. Hits already carry grid lines, so
/// the mapping is the same `line + offset` rule the planner used.
fn paint_find(
    surface: &GridSurface<'_>,
    overlay: FindOverlay<'_>,
    scroll_offset: usize,
    rows: usize,
) {
    let offset = i64::try_from(scroll_offset).unwrap_or(i64::MAX);
    for (index, hit) in overlay.hits.iter().enumerate() {
        let row = hit.line as i64 + offset;
        if row < 0 || row >= rows as i64 {
            continue;
        }
        let color = if Some(index) == overlay.current {
            surface.theme.find_current
        } else {
            surface.theme.find_match
        };
        surface.painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(surface.x(hit.column), surface.y(row as usize)),
                Vec2::new(hit.cells as f32 * surface.cell.width, surface.cell.height),
            ),
            0.0,
            color,
        );
    }
}

/// Command-block chrome (Spec R13-T4): a divider at each block's start
/// row plus a right-aligned exit-status chip straddling it — the
/// `✓/✗ <code> · <dur>` label `block_text` formats for the status
/// line, in the theme's ok/err colors. `chips` arrive as screen rows
/// from `layout::plan_block_chrome`; the caller feeds it resolved
/// `Block`s only on `Integrated` tabs, so marks-only and unintegrated
/// sessions get nothing here (Spec R05's graceful degrade).
fn paint_block_chrome(surface: &GridSurface<'_>, chips: &[BlockChip]) {
    let painter = surface.painter;
    for chip in chips {
        let y = surface.y(chip.row);
        painter.line_segment(
            [
                Pos2::new(surface.pane.left(), y),
                Pos2::new(surface.pane.right(), y),
            ],
            Stroke::new(1.0, surface.theme.block_edge),
        );
        if let Some(label) = &chip.label {
            let color = if label.ok {
                surface.theme.block_ok
            } else {
                surface.theme.block_err
            };
            let galley =
                painter.layout_no_wrap(label.text.clone(), surface.font.clone(), Color32::WHITE);
            // The chip floats on the divider's right end — a rounded
            // rect straddling the boundary line, bg-filled so the rows
            // above and below stay readable under it.
            let chip_w = galley.size().x + 10.0;
            let chip_h = surface.cell.height + 4.0;
            let rect = Rect::from_center_size(
                Pos2::new(surface.pane.right() - chip_w / 2.0 - 4.0, y),
                Vec2::new(chip_w, chip_h),
            );
            painter.rect_filled(rect, chip_h / 2.0, surface.theme.bg);
            painter.rect_stroke(
                rect,
                chip_h / 2.0,
                Stroke::new(1.0, color),
                eframe::egui::StrokeKind::Inside,
            );
            painter.galley(
                Pos2::new(rect.left() + 5.0, rect.center().y - galley.size().y / 2.0),
                galley,
                color,
            );
        }
    }
}

/// Paint the whole grid pane: theme-bg fill, then planned rows, the
/// find wash, selection wash, block chrome, cursor — the order that
/// keeps text over backgrounds, both washes over text, the chrome over
/// the washes, and the cursor over everything.
pub fn paint_grid(
    surface: &GridSurface<'_>,
    grid: &GridView<'_>,
    scroll_offset: usize,
    cursor: Cursor,
    selection: Option<SelectionRange>,
    overlays: Overlays<'_>,
) {
    surface
        .painter
        .rect_filled(surface.pane, 0.0, surface.theme.bg);
    let (width, rows) = surface.dimensions();
    let plans = plan_viewport(grid, scroll_offset, width, rows, surface.theme);
    let offset = i64::try_from(scroll_offset).unwrap_or(i64::MAX);
    for plan in &plans {
        let row = plan.line as i64 + offset;
        if row < 0 || row >= rows as i64 {
            continue;
        }
        paint_row(surface, surface.y(row as usize), plan);
    }
    if let Some(find) = overlays.find {
        paint_find(surface, find, scroll_offset, rows);
    }
    if let Some(range) = selection {
        paint_selection(surface, range, scroll_offset, width, rows);
    }
    if let Some(chips) = overlays.block_chips {
        paint_block_chrome(surface, chips);
    }
    paint_cursor(surface, cursor, scroll_offset, rows);
}
