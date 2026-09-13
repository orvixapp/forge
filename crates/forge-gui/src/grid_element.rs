//! Direct-paint GPUI element for the terminal grid.
//!
//! Instead of building one retained element per cell or run, the grid paints
//! straight into GPUI's scene: one quad per background run, one quad for the
//! cursor and one sprite per visible glyph, all inside a single paint layer so
//! the renderer batches them into a handful of draw calls. Glyph lookups are
//! served from a per-grapheme cache, so shaping happens once per distinct cell
//! text instead of once per cell per frame.

use forge_gui::{
    CellPos, Selection, TerminalGrid, background_runs, config::HexColor, cursor_shape,
    theme::ThemeColors,
};
use gpui::{
    App, BorderStyle, Bounds, Element, ElementId, ElementInputHandler, Entity, EntityInputHandler,
    FocusHandle, Font, FontId, GlobalElementId, GlyphId, Hsla, InspectorElementId, IntoElement,
    LayoutId, Pixels, Point, Rgba, Size, Style, TextRun, Window, WindowTextSystem, black, fill,
    outline, point, px, rgb, size,
};
use proto_ipc::{CursorStyle, Rgb};
use std::{collections::HashMap, ops::Range};

/// Colours the grid paints itself; everything else comes from the cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    pub background: Rgba,
    pub foreground: Rgba,
    pub cursor: Rgba,
    /// Overlay painted on selected cells; carries its own opacity.
    pub selection: Rgba,
    pub accent: Rgba,
}

impl From<&ThemeColors> for Palette {
    fn from(colors: &ThemeColors) -> Self {
        let mut selection = color(colors.selection);
        selection.a = colors.selection_opacity;
        Self {
            background: color(colors.background),
            foreground: color(colors.foreground),
            cursor: color(colors.cursor),
            selection,
            accent: color(colors.accent),
        }
    }
}

/// GPUI colour for a configured `#rrggbb`.
#[must_use]
pub fn color(value: HexColor) -> Rgba {
    rgb(rgb_value(value.0))
}

/// Fixed cell geometry shared by layout, window resizing and painting.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    pub font_size: f32,
}

impl CellMetrics {
    /// Small enough for a 200×60 grid to fit a 1366×768 display, so the
    /// benchmark paints every cell instead of culling the off-screen ones.
    pub const BENCHMARK: Self = Self {
        width: 6.0,
        height: 10.0,
        font_size: 8.0,
    };

    #[must_use]
    pub fn grid_size(self, cols: u16, rows: u16) -> Size<Pixels> {
        size(
            px(self.width * f32::from(cols)),
            px(self.height * f32::from(rows)),
        )
    }
}

/// Grid state plus the render caches the element keeps between frames.
pub struct TerminalSurface {
    pub grid: TerminalGrid,
    pub metrics: CellMetrics,
    pub palette: Palette,
    pub selection: Option<Selection>,
    glyphs: GlyphCache,
    scratch: PaintScratch,
    /// Where the grid was last painted, in window coordinates; mouse events
    /// arrive in that space.
    last_bounds: Option<Bounds<Pixels>>,
    /// Cells inside the clip region during the last paint; lets benchmarks
    /// prove that nothing was culled.
    pub painted_cells: usize,
}

impl TerminalSurface {
    #[must_use]
    pub fn new(grid: TerminalGrid, metrics: CellMetrics, palette: Palette) -> Self {
        Self {
            grid,
            metrics,
            palette,
            selection: None,
            glyphs: GlyphCache::default(),
            scratch: PaintScratch::default(),
            last_bounds: None,
            painted_cells: 0,
        }
    }

    /// Cell under a window position, clamped to the grid so a drag that
    /// leaves the grid keeps extending the selection towards the edge it
    /// crossed. `None` before the first paint.
    #[must_use]
    pub fn cell_at(&self, position: Point<Pixels>) -> Option<CellPos> {
        let bounds = self.last_bounds?;
        let (cols, rows) = self.grid.dimensions();
        let x = cell_index(
            position.x - bounds.origin.x,
            px(self.metrics.width),
            cols.saturating_sub(1),
            false,
        );
        let y = cell_index(
            position.y - bounds.origin.y,
            px(self.metrics.height),
            rows.saturating_sub(1),
            false,
        );
        Some(CellPos::new(x, y))
    }

    /// Whether a window position lies inside the painted grid.
    #[must_use]
    pub fn contains(&self, position: Point<Pixels>) -> bool {
        self.last_bounds
            .is_some_and(|bounds| bounds.contains(&position))
    }

    /// Window-space rectangle of the cursor cell, where IME candidate
    /// windows should appear. `None` before the first paint.
    #[must_use]
    pub fn cursor_bounds(&self) -> Option<Bounds<Pixels>> {
        let bounds = self.last_bounds?;
        let cursor = self.grid.cursor()?;
        let cell = size(px(self.metrics.width), px(self.metrics.height));
        let origin = bounds.origin
            + point(
                cell.width * f32::from(cursor.x),
                cell.height * f32::from(cursor.y),
            );
        Some(Bounds::new(origin, cell))
    }
}

#[derive(Debug, Clone, Copy)]
struct CellGlyph {
    font_id: FontId,
    id: GlyphId,
    offset: Pixels,
    is_emoji: bool,
}

/// Index of a distinct cell text inside [`GlyphCache::entries`].
type GlyphRank = u32;

/// Shaped glyphs per cell text, addressed by rank. ASCII gets a direct table;
/// everything else (combining sequences, emoji, wide characters) goes through
/// a map. Ranks are stable for the lifetime of the cache.
#[derive(Default)]
struct GlyphCache {
    font: Option<Font>,
    font_size: Pixels,
    line_height: Pixels,
    baseline: Pixels,
    ascii: Vec<Option<GlyphRank>>,
    other: HashMap<String, GlyphRank>,
    entries: Vec<Vec<CellGlyph>>,
}

impl GlyphCache {
    fn prepare(&mut self, font: &Font, metrics: CellMetrics, text_system: &WindowTextSystem) {
        let font_size = px(metrics.font_size);
        let line_height = px(metrics.height);
        if self.font.as_ref() == Some(font)
            && self.font_size == font_size
            && self.line_height == line_height
        {
            return;
        }
        let font_id = text_system.resolve_font(font);
        self.baseline = text_system.baseline_offset(font_id, font_size, line_height);
        self.font = Some(font.clone());
        self.font_size = font_size;
        self.line_height = line_height;
        self.ascii = vec![None; 128];
        self.other.clear();
        self.entries.clear();
    }

    fn rank(&mut self, text: &str, text_system: &WindowTextSystem) -> GlyphRank {
        if let [byte] = text.as_bytes()
            && byte.is_ascii()
        {
            let index = usize::from(*byte);
            if let Some(rank) = self.ascii[index] {
                return rank;
            }
            let rank = self.push(text, text_system);
            self.ascii[index] = Some(rank);
            return rank;
        }
        if let Some(rank) = self.other.get(text) {
            return *rank;
        }
        let rank = self.push(text, text_system);
        self.other.insert(text.to_owned(), rank);
        rank
    }

    fn push(&mut self, text: &str, text_system: &WindowTextSystem) -> GlyphRank {
        let shaped = self.shape(text, text_system);
        self.entries.push(shaped);
        GlyphRank::try_from(self.entries.len() - 1).expect("fewer than u32::MAX distinct cells")
    }

    fn glyphs(&self, rank: GlyphRank) -> &[CellGlyph] {
        self.entries.get(rank as usize).map_or(&[], Vec::as_slice)
    }

    fn shape(&self, text: &str, text_system: &WindowTextSystem) -> Vec<CellGlyph> {
        let Some(font) = &self.font else {
            return Vec::new();
        };
        let run = TextRun {
            len: text.len(),
            font: font.clone(),
            color: black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let layout = text_system.layout_line(text, self.font_size, &[run], None);
        layout
            .runs
            .iter()
            .flat_map(|run| {
                run.glyphs.iter().map(move |glyph| CellGlyph {
                    font_id: run.font_id,
                    id: glyph.id,
                    offset: glyph.position.x,
                    is_emoji: glyph.is_emoji,
                })
            })
            .collect()
    }
}

/// A cell whose glyphs will be painted this frame.
#[derive(Debug, Clone, Copy)]
struct PendingCell {
    x: u16,
    y: u16,
    rank: GlyphRank,
    color: Hsla,
}

/// Per-frame buffers, kept between frames to avoid reallocating them.
#[derive(Default)]
struct PaintScratch {
    pending: Vec<PendingCell>,
    /// Start offset of each rank inside `grouped` (counting sort).
    starts: Vec<u32>,
    /// `pending` reordered so cells with the same rank are contiguous.
    grouped: Vec<PendingCell>,
    cursor: Vec<u32>,
    colors: ColorCache,
}

impl PaintScratch {
    /// Groups `pending` by rank without sorting: O(cells + ranks).
    fn group_by_rank(&mut self, ranks: usize) {
        self.starts.clear();
        self.starts.resize(ranks + 1, 0);
        for cell in &self.pending {
            self.starts[cell.rank as usize + 1] += 1;
        }
        for rank in 0..ranks {
            self.starts[rank + 1] += self.starts[rank];
        }
        self.cursor.clear();
        self.cursor.extend_from_slice(&self.starts);
        self.grouped.clear();
        self.grouped
            .resize(self.pending.len(), PendingCell::PLACEHOLDER);
        for cell in &self.pending {
            let slot = &mut self.cursor[cell.rank as usize];
            self.grouped[*slot as usize] = *cell;
            *slot += 1;
        }
    }
}

impl PendingCell {
    const PLACEHOLDER: Self = Self {
        x: 0,
        y: 0,
        rank: 0,
        color: Hsla {
            h: 0.0,
            s: 0.0,
            l: 0.0,
            a: 0.0,
        },
    };
}

/// Direct-mapped memo of RGB → HSLA conversions. Terminal output cycles
/// through a small palette, so nearly every lookup hits.
struct ColorCache {
    slots: Vec<(u32, Hsla)>,
}

impl Default for ColorCache {
    fn default() -> Self {
        Self {
            slots: vec![(u32::MAX, Hsla::default()); Self::SLOTS],
        }
    }
}

impl ColorCache {
    const SLOTS: usize = 1024;

    fn get(&mut self, color: Rgb) -> Hsla {
        let packed = rgb_value(color);
        let slot = (packed.wrapping_mul(0x9E37_79B1) >> 22) as usize % Self::SLOTS;
        let entry = &mut self.slots[slot];
        if entry.0 != packed {
            *entry = (packed, rgb(packed).into());
        }
        entry.1
    }
}

/// Element that paints a [`TerminalSurface`] owned by view `V`.
pub struct TerminalGridElement<V: EntityInputHandler> {
    view: Entity<V>,
    index: usize,
    surface: fn(&mut V, usize) -> &mut TerminalSurface,
    /// When set, this pane receives IME text (dead keys, CJK composition)
    /// through the view's [`EntityInputHandler`].
    input_focus: Option<FocusHandle>,
}

impl<V: EntityInputHandler> TerminalGridElement<V> {
    /// Selects one surface from a view. Keeping the index in the element (and
    /// not in a closure) lets one Forge window paint several live terminals.
    pub fn new(
        view: Entity<V>,
        index: usize,
        surface: fn(&mut V, usize) -> &mut TerminalSurface,
    ) -> Self {
        Self {
            view,
            index,
            surface,
            input_focus: None,
        }
    }

    /// Registers the pane as the window's text input target while `focus`
    /// is focused.
    #[must_use]
    pub fn with_input_focus(mut self, focus: FocusHandle) -> Self {
        self.input_focus = Some(focus);
        self
    }
}

impl<V: EntityInputHandler> IntoElement for TerminalGridElement<V> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<V: EntityInputHandler> Element for TerminalGridElement<V> {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let surface = self.surface;
        let index = self.index;
        let grid_size = self.view.update(cx, |view, _| {
            let surface = surface(view, index);
            let (cols, rows) = surface.grid.dimensions();
            surface.metrics.grid_size(cols, rows)
        });
        let mut style = Style::default();
        style.size.width = grid_size.width.into();
        style.size.height = grid_size.height.into();
        style.flex_shrink = 0.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let surface = self.surface;
        let index = self.index;
        if let Some(focus) = &self.input_focus {
            window.handle_input(
                focus,
                ElementInputHandler::new(bounds, self.view.clone()),
                cx,
            );
        }
        self.view.update(cx, |view, _| {
            paint_grid(surface(view, index), bounds, window);
        });
    }
}

/// Geometry shared by the paint passes of one frame.
struct GridFrame {
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    cols: Range<u16>,
    rows: Range<u16>,
}

impl GridFrame {
    fn cell_origin(&self, x: u16, y: u16) -> Point<Pixels> {
        self.origin
            + point(
                self.cell.width * f32::from(x),
                self.cell.height * f32::from(y),
            )
    }
}

fn paint_grid(surface: &mut TerminalSurface, bounds: Bounds<Pixels>, window: &mut Window) {
    let text_system = window.text_system().clone();
    let font = window.text_style().font();
    let metrics = surface.metrics;
    let palette = surface.palette;
    let selection = surface.selection;
    surface.last_bounds = Some(bounds);
    let TerminalSurface {
        grid,
        glyphs,
        scratch,
        painted_cells,
        ..
    } = surface;
    glyphs.prepare(&font, metrics, &text_system);

    let (cols, rows) = grid.dimensions();
    let cell = size(px(metrics.width), px(metrics.height));
    let visible = window.content_mask().bounds.intersect(&bounds);
    let (col_range, row_range) = visible_cells(bounds, visible, cell, cols, rows);
    *painted_cells = col_range.len() * row_range.len();
    if col_range.is_empty() || row_range.is_empty() {
        return;
    }
    let frame = GridFrame {
        origin: bounds.origin,
        cell,
        cols: col_range,
        rows: row_range,
    };

    window.paint_layer(bounds, |window| {
        paint_backgrounds(grid, &frame, &mut scratch.colors, window);
        if let Some(selection) = selection {
            paint_selection(selection, &frame, palette.selection, window);
        }
        paint_cursor(grid, &frame, metrics, palette.cursor, window);
        collect_glyph_cells(grid, &frame, glyphs, scratch, &palette, &text_system);
        paint_glyphs(
            &frame,
            glyphs,
            scratch,
            px(metrics.font_size),
            bounds,
            window,
        );
    });
}

/// One translucent quad per selected row segment, over the backgrounds and
/// under the glyphs.
fn paint_selection(selection: Selection, frame: &GridFrame, color: Rgba, window: &mut Window) {
    let cols = frame.cols.end;
    for y in frame.rows.clone() {
        let Some(span) = selection.row_span(y, cols) else {
            continue;
        };
        let start = span.start.max(frame.cols.start);
        let end = span.end.min(frame.cols.end);
        if start >= end {
            continue;
        }
        let origin = frame.cell_origin(start, y);
        let extent = size(frame.cell.width * f32::from(end - start), frame.cell.height);
        window.paint_quad(fill(Bounds::new(origin, extent), color));
    }
}

fn paint_backgrounds(
    grid: &TerminalGrid,
    frame: &GridFrame,
    colors: &mut ColorCache,
    window: &mut Window,
) {
    let cols = usize::from(frame.cols.start)..usize::from(frame.cols.end);
    for y in frame.rows.clone() {
        let Some(cells) = grid.row(y) else { continue };
        for run in background_runs(&cells[cols.clone()]) {
            let origin = frame.cell_origin(frame.cols.start + run.start, y);
            let extent = size(frame.cell.width * f32::from(run.len), frame.cell.height);
            window.paint_quad(fill(Bounds::new(origin, extent), colors.get(run.color)));
        }
    }
}

fn paint_cursor(
    grid: &TerminalGrid,
    frame: &GridFrame,
    metrics: CellMetrics,
    color: Rgba,
    window: &mut Window,
) {
    let Some(cursor) = grid.cursor().filter(|cursor| cursor.visible) else {
        return;
    };
    if !frame.cols.contains(&cursor.x) || !frame.rows.contains(&cursor.y) {
        return;
    }
    let shape = cursor_shape(cursor.style, metrics.width, metrics.height);
    let origin = frame.cell_origin(cursor.x, cursor.y) + point(px(shape.x), px(shape.y));
    let rect = Bounds::new(origin, size(px(shape.width), px(shape.height)));
    window.paint_quad(if shape.hollow {
        outline(rect, color, BorderStyle::Solid)
    } else {
        fill(rect, color)
    });
}

/// Collects the visible cells that carry a glyph, resolving each cell's text
/// to a rank and its foreground to a colour once.
fn collect_glyph_cells(
    grid: &TerminalGrid,
    frame: &GridFrame,
    glyphs: &mut GlyphCache,
    scratch: &mut PaintScratch,
    palette: &Palette,
    text_system: &WindowTextSystem,
) {
    let default_foreground: Hsla = palette.foreground.into();
    let block_cursor_text: Hsla = palette.background.into();
    let block_cursor = grid
        .cursor()
        .filter(|cursor| cursor.visible && cursor.style == CursorStyle::Block)
        .map(|cursor| (cursor.x, cursor.y));
    scratch.pending.clear();
    for y in frame.rows.clone() {
        let Some(cells) = grid.row(y) else { continue };
        let mut last_color: Option<(Option<Rgb>, Hsla)> = None;
        for x in frame.cols.clone() {
            let data = &cells[usize::from(x)];
            if data.text.is_empty() || data.text == " " {
                continue;
            }
            let color = if block_cursor == Some((x, y)) {
                block_cursor_text
            } else {
                match last_color {
                    Some((foreground, color)) if foreground == data.foreground => color,
                    _ => {
                        let color = data
                            .foreground
                            .map_or(default_foreground, |color| scratch.colors.get(color));
                        last_color = Some((data.foreground, color));
                        color
                    }
                }
            };
            scratch.pending.push(PendingCell {
                x,
                y,
                rank: glyphs.rank(&data.text, text_system),
                color,
            });
        }
    }
}

/// GPUI sorts every sprite of the frame by (layer order, atlas tile) before
/// encoding. Emitting each distinct glyph as its own layer hands that sort an
/// already-ordered input, which turns an O(n log n) shuffle of 12k sprites
/// into a linear scan.
fn paint_glyphs(
    frame: &GridFrame,
    glyphs: &GlyphCache,
    scratch: &mut PaintScratch,
    font_size: Pixels,
    bounds: Bounds<Pixels>,
    window: &mut Window,
) {
    let ranks = glyphs.entries.len();
    scratch.group_by_rank(ranks);
    for rank in 0..ranks {
        let range = scratch.starts[rank] as usize..scratch.starts[rank + 1] as usize;
        if range.is_empty() {
            continue;
        }
        let shaped = glyphs.glyphs(GlyphRank::try_from(rank).expect("rank came from a u32"));
        window.paint_layer(bounds, |window| {
            for pending in &scratch.grouped[range] {
                let cell_origin = frame.cell_origin(pending.x, pending.y);
                let baseline = cell_origin.y + glyphs.baseline;
                for glyph in shaped {
                    let origin = point(cell_origin.x + glyph.offset, baseline);
                    // A glyph the platform cannot rasterize leaves its cell blank.
                    let _ = if glyph.is_emoji {
                        window.paint_emoji(origin, glyph.font_id, glyph.id, font_size)
                    } else {
                        window.paint_glyph(
                            origin,
                            glyph.font_id,
                            glyph.id,
                            font_size,
                            pending.color,
                        )
                    };
                }
            }
        });
    }
}

/// Column and row ranges of the grid that intersect `visible`.
fn visible_cells(
    bounds: Bounds<Pixels>,
    visible: Bounds<Pixels>,
    cell: Size<Pixels>,
    cols: u16,
    rows: u16,
) -> (Range<u16>, Range<u16>) {
    if visible.is_empty() {
        return (0..0, 0..0);
    }
    let first_col = cell_index(visible.origin.x - bounds.origin.x, cell.width, cols, false);
    let last_col = cell_index(visible.right() - bounds.origin.x, cell.width, cols, true);
    let first_row = cell_index(visible.origin.y - bounds.origin.y, cell.height, rows, false);
    let last_row = cell_index(visible.bottom() - bounds.origin.y, cell.height, rows, true);
    (
        first_col..last_col.max(first_col),
        first_row..last_row.max(first_row),
    )
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn cell_index(offset: Pixels, cell: Pixels, limit: u16, round_up: bool) -> u16 {
    let index = offset / cell;
    let index = if round_up {
        index.ceil()
    } else {
        index.floor()
    };
    index.clamp(0.0, f32::from(limit)) as u16
}

#[must_use]
pub fn rgb_value(color: Rgb) -> u32 {
    (u32::from(color.r) << 16) | (u32::from(color.g) << 8) | u32::from(color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_cells_clip_to_the_intersection_with_the_content_mask() {
        let cell = size(px(9.0), px(18.0));
        let bounds = Bounds::new(point(px(16.0), px(40.0)), size(px(90.0), px(180.0)));
        // Fully visible grid.
        assert_eq!(visible_cells(bounds, bounds, cell, 10, 10), (0..10, 0..10));
        // Mask cuts off the right and bottom halves, partially covering cells.
        let mask = Bounds::new(point(px(16.0), px(40.0)), size(px(50.0), px(100.0)));
        assert_eq!(visible_cells(bounds, mask, cell, 10, 10), (0..6, 0..6));
        // Mask starting inside the grid skips leading cells.
        let mask = Bounds::new(point(px(30.0), px(80.0)), size(px(500.0), px(500.0)));
        assert_eq!(visible_cells(bounds, mask, cell, 10, 10), (1..10, 2..10));
        // Empty intersection paints nothing.
        let empty = Bounds::new(point(px(0.0), px(0.0)), size(px(0.0), px(0.0)));
        assert_eq!(visible_cells(bounds, empty, cell, 10, 10), (0..0, 0..0));
    }

    #[test]
    fn cell_metrics_size_the_element_from_the_grid_dimensions() {
        let grid = CellMetrics {
            width: 9.0,
            height: 18.0,
            font_size: 14.0,
        }
        .grid_size(80, 24);
        assert_eq!(grid, size(px(720.0), px(432.0)));
        let benchmark = CellMetrics::BENCHMARK.grid_size(200, 60);
        assert!(benchmark.width < px(1366.0) && benchmark.height < px(700.0));
    }

    #[test]
    fn converts_ipc_colors_to_packed_rgb() {
        assert_eq!(
            rgb_value(Rgb {
                r: 0x12,
                g: 0x34,
                b: 0x56
            }),
            0x12_34_56
        );
    }
}
