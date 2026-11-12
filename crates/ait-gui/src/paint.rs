//! The egui painting pass: [`RowPlan`]s + the cursor + the selection
//! → `Painter` calls. Every draw is clipped to the grid pane; nothing
//! here decides WHAT to paint (that is [`grid::plan_viewport`]'s pure
//! planning half — the split the spec's testing decisions ask for).
//!
//! Text paints one galley per ROW: each span becomes a `TextFormat`
//! section of one `LayoutJob`, shaped once and cached by
//! [`RowGalleys`] — `painter.galley` then emits the row in a single
//! call (REQUIREMENTS §6's no-per-cell-work rule). The pump's damage
//! filter decides whether a frame repaints at all; the cache decides
//! how cheap the frame is: an unchanged row is a hash lookup, not a
//! re-shape.
//!
//! [Spec R13 §5.12, REQUIREMENTS.md §5.12]

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use ait_app::search::SearchHit;
use ait_app::selection::SelectionRange;
use ait_term::{Cursor, CursorShape, GridView, Underline};
use eframe::egui::text::{LayoutJob, TextFormat};
use eframe::egui::{Color32, FontFamily, FontId, Galley, Painter, Pos2, Rect, Stroke, Vec2};

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
    /// The Ctrl+hover link affordance (Spec R14 §5.13 us-24): the
    /// detected token under the pointer, painted as a single
    /// underline over its cells — `None` unless Ctrl is held and the
    /// hover sits on a token.
    pub detect: Option<DetectOverlay>,
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

/// The detected token's paint inputs — the cell span the underline
/// covers, in `GridView` line space like a find hit (negative lines
/// are scrollback).
#[derive(Debug, Clone, Copy)]
pub struct DetectOverlay {
    /// The token's grid line.
    pub line: i32,
    /// The first covered column.
    pub column: usize,
    /// The covered cell count — the underline's width.
    pub cells: usize,
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

/// How many laid-out rows the cache holds before it clears — ~25
/// viewports' worth at 80 rows. Scrollback churn and zoom/theme
/// changes fill and rebuild rather than growing unbounded.
const GALLEY_CAP: usize = 2048;

/// The per-row galley cache (REQUIREMENTS §6): `LayoutJob` shaping —
/// glyph lookup, advance summation, tessellation prep — is the paint
/// pass's expensive call, so each planned row is laid out once and
/// the `Arc<Galley>` reused while everything the shape depends on
/// stays equal (the key hashes every span's text and style plus the
/// font, so a restyle, a hidden-cell flip or a zoom rebuilds exactly
/// the rows it changed). The pump's damage filter decides whether a
/// frame repaints at all; this decides how cheap the frame is.
#[derive(Debug, Default)]
pub struct RowGalleys {
    cache: HashMap<u64, Arc<Galley>>,
}

impl RowGalleys {
    /// The row's galley — a cached `Arc` while `plan` and `font` hash
    /// to a seen key.
    fn get(&mut self, painter: &Painter, plan: &RowPlan, font: &FontId) -> Arc<Galley> {
        let key = row_key(plan, font);
        if let Some(galley) = self.cache.get(&key) {
            return Arc::clone(galley);
        }
        if self.cache.len() >= GALLEY_CAP {
            self.cache.clear();
        }
        let galley = painter.layout_job(row_layout(plan, font));
        self.cache.insert(key, Arc::clone(&galley));
        galley
    }
}

/// What the galley depends on, hashed — exactly `row_layout`'s
/// inputs: the font (a zoom's changed pixel size reshapes
/// everything), and per span its text, resolved foreground, the
/// `bold`/`italic`/`hidden` flags that transform both, and `cells`
/// (a hidden span's column count IS its space run). The fields the
/// galley never reads — `line`, `bg` and the decoration flags the
/// shape pass paints — stay out, so a bg-only churn doesn't force a
/// re-shape.
fn row_key(plan: &RowPlan, font: &FontId) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    font.size.to_bits().hash(&mut hasher);
    std::mem::discriminant(&font.family).hash(&mut hasher);
    if let FontFamily::Name(name) = &font.family {
        name.hash(&mut hasher);
    }
    for span in &plan.spans {
        span.cells.hash(&mut hasher);
        span.text.hash(&mut hasher);
        span.fg.hash(&mut hasher);
        span.hidden.hash(&mut hasher);
        span.bold.hash(&mut hasher);
        span.italic.hash(&mut hasher);
    }
    hasher.finish()
}

/// One row's `LayoutJob`: every span's text under its own
/// `TextFormat` (foreground resolved, bold pre-lifted — the bundled
/// monospace has no bold face, `italic` honored natively). The
/// planner tiles the row contiguously from column 0 — a span per
/// cell, blanks included — so the galley needs no per-span x
/// anchoring. `hidden` spans still occupy their columns as spaces,
/// keeping everything after them aligned. Cell-exact columns hold
/// because every section shares the row's monospace font: each
/// glyph's advance IS the cell width the winsize math measured.
fn row_layout(plan: &RowPlan, font: &FontId) -> LayoutJob {
    let mut job = LayoutJob::default();
    for span in &plan.spans {
        // `hidden` cells paint no text — SGR 8's "invisible" is a
        // blank with its colors — but their columns still exist.
        let text = if span.hidden {
            " ".repeat(span.cells)
        } else {
            span.text.clone()
        };
        if text.is_empty() {
            continue;
        }
        let fg = if span.bold && !span.hidden {
            bold_color(span.fg)
        } else {
            span.fg
        };
        job.append(
            &text,
            0.0,
            TextFormat {
                font_id: font.clone(),
                color: fg,
                italics: span.italic,
                ..Default::default()
            },
        );
    }
    job
}

/// Paint one planned row: background rects first (cell-exact, only
/// where the resolved bg differs from the theme's), then the row's
/// one cached galley, then the decoration lines — underlines and
/// strikeout stay explicit shapes: egui's `TextFormat` covers single-
/// underline only, and the cell-exact segment widths are already
/// computed per span.
fn paint_row(surface: &GridSurface<'_>, galleys: &mut RowGalleys, row_top: f32, plan: &RowPlan) {
    for span in &plan.spans {
        if let Some(bg) = span.bg {
            surface.painter.rect_filled(
                Rect::from_min_size(
                    Pos2::new(surface.x(span.col), row_top),
                    Vec2::new(span.cells as f32 * surface.cell.width, surface.cell.height),
                ),
                0.0,
                bg,
            );
        }
    }
    let galley = galleys.get(surface.painter, plan, surface.font);
    surface.painter.galley(
        Pos2::new(surface.pane.left(), row_top),
        galley,
        surface.theme.fg,
    );
    for span in &plan.spans {
        if !span.hidden && !span.text.is_empty() {
            let fg = if span.bold {
                bold_color(span.fg)
            } else {
                span.fg
            };
            paint_decoration(
                surface,
                span,
                surface.x(span.col),
                row_top,
                span.cells as f32 * surface.cell.width,
                fg,
            );
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

/// The detected token's underline — one segment at the underline's
/// usual offset under the token's cell span, in the theme's `link`
/// color so it reads as clickable rather than as SGR-underlined text.
/// `line` maps to a screen row by the same `line + offset` rule the
/// planner and the find wash share.
fn paint_detect(
    surface: &GridSurface<'_>,
    detect: DetectOverlay,
    scroll_offset: usize,
    rows: usize,
) {
    let offset = i64::try_from(scroll_offset).unwrap_or(i64::MAX);
    let row = detect.line as i64 + offset;
    if row < 0 || row >= rows as i64 {
        return;
    }
    let y = underline_y(surface.y(row as usize), surface.cell);
    let x = surface.x(detect.column);
    surface.painter.line_segment(
        [
            Pos2::new(x, y),
            Pos2::new(x + detect.cells as f32 * surface.cell.width, y),
        ],
        Stroke::new(1.0, surface.theme.link),
    );
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
/// find wash, selection wash, the detect underline, block chrome,
/// cursor — the order that keeps text over backgrounds, both washes
/// over text, the affordance over the washes, the chrome over that,
/// and the cursor over everything. `galleys` is the
/// caller's cross-frame row cache — the pump's damage filter decides
/// whether this runs at all; the cache keeps the rows it didn't touch
/// free.
pub fn paint_grid(
    surface: &GridSurface<'_>,
    galleys: &mut RowGalleys,
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
        paint_row(surface, galleys, surface.y(row as usize), plan);
    }
    if let Some(find) = overlays.find {
        paint_find(surface, find, scroll_offset, rows);
    }
    if let Some(range) = selection {
        paint_selection(surface, range, scroll_offset, width, rows);
    }
    if let Some(detect) = overlays.detect {
        paint_detect(surface, detect, scroll_offset, rows);
    }
    if let Some(chips) = overlays.block_chips {
        paint_block_chrome(surface, chips);
    }
    paint_cursor(surface, cursor, scroll_offset, rows);
}

#[cfg(test)]
mod tests {
    //! The row-job seam: `row_layout` maps planned spans to one
    //! `LayoutJob`'s sections and `row_key` covers everything the
    //! galley depends on — the two halves of "one galley per row,
    //! cached" (REQUIREMENTS §6).

    use super::*;

    /// A default-styled span at `col`/`cells` carrying `text`.
    fn span(col: usize, cells: usize, text: &str) -> CellSpan {
        CellSpan {
            col,
            cells,
            text: text.to_string(),
            fg: Color32::WHITE,
            bg: None,
            hidden: false,
            bold: false,
            italic: false,
            strikeout: false,
            underline: Underline::None,
        }
    }

    fn font() -> FontId {
        FontId::monospace(15.0)
    }

    /// One section per span — text concatenates into `job.text`, and
    /// each section's `TextFormat` carries the span's font, resolved
    /// color and italics.
    #[test]
    fn the_row_job_maps_spans_to_sections() {
        let mut red = span(0, 2, "ab");
        red.fg = Color32::RED;
        let italic = CellSpan {
            italic: true,
            ..span(2, 2, "cd")
        };
        let plan = RowPlan {
            line: 0,
            spans: vec![red, italic],
        };

        let job = row_layout(&plan, &font());
        assert_eq!(job.text, "abcd");
        assert_eq!(job.sections.len(), 2);
        assert_eq!(job.sections[0].format.color, Color32::RED);
        assert!(!job.sections[0].format.italics);
        assert_eq!(job.sections[1].format.color, Color32::WHITE);
        assert!(job.sections[1].format.italics);
    }

    /// `hidden` paints no glyphs — SGR 8's blank — but its columns
    /// still exist, so the job carries spaces and keeps the next
    /// span's text at its column.
    #[test]
    fn hidden_spans_hold_their_columns_as_spaces() {
        let mut hidden = span(0, 3, "abc");
        hidden.hidden = true;
        let plan = RowPlan {
            line: 0,
            spans: vec![hidden, span(3, 1, "d")],
        };

        let job = row_layout(&plan, &font());
        assert_eq!(job.text, "   d", "the hidden span's cells stay occupied");
    }

    /// Bold rides the galley as the lifted-foreground degrade — the
    /// bundled monospace has no bold face.
    #[test]
    fn bold_lifts_the_section_foreground() {
        let gray = Color32::from_rgb(0x80, 0x80, 0x80);
        let plan = RowPlan {
            line: 0,
            spans: vec![CellSpan {
                bold: true,
                fg: gray,
                ..span(0, 1, "x")
            }],
        };
        let job = row_layout(&plan, &font());
        assert_eq!(job.sections[0].format.color, bold_color(gray));
        assert_ne!(job.sections[0].format.color, gray);
    }

    /// The cache key covers everything the galley depends on: a text,
    /// color, style-flag, hidden or font-size change lands a new key;
    /// fields the galley never reads (`line`, `bg`, `strikeout` and
    /// `underline` — both stay explicit shapes outside the galley,
    /// `bg` stays a rect) can change without invalidating it.
    #[test]
    fn the_key_covers_everything_the_galley_depends_on() {
        let plan = RowPlan {
            line: 0,
            spans: vec![span(0, 3, "abc")],
        };
        let base = row_key(&plan, &font());

        // The galley's inputs all change the key.
        for mutate in [
            (|p: &mut RowPlan| p.spans[0].text = "abd".to_string()) as fn(&mut RowPlan),
            |p| p.spans[0].fg = Color32::RED,
            |p| p.spans[0].bold = true,
            |p| p.spans[0].italic = true,
            |p| p.spans[0].hidden = true,
            |p| p.spans[0].cells = 4,
        ] {
            let mut changed = plan.clone();
            mutate(&mut changed);
            assert_ne!(
                row_key(&changed, &font()),
                base,
                "a galley-visible change must miss the cache"
            );
        }
        assert_ne!(
            row_key(&plan, &FontId::monospace(16.0)),
            base,
            "a zoom's changed font size reshapes every row"
        );

        // The out-of-galley fields do not: `line`/`bg`/decorations are
        // painted by the rect/shape passes, not the text run.
        for mutate in [
            (|p: &mut RowPlan| p.line = 7) as fn(&mut RowPlan),
            |p| p.spans[0].bg = Some(Color32::DARK_GRAY),
            |p| p.spans[0].strikeout = true,
            |p| p.spans[0].underline = Underline::Curly,
        ] {
            let mut changed = plan.clone();
            mutate(&mut changed);
            assert_eq!(
                row_key(&changed, &font()),
                base,
                "a shape-pass change must not force a re-shape"
            );
        }
    }
}
