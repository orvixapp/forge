//! Headless terminal-grid state consumed by the GPUI frontend.

pub mod config;
pub mod shell;
pub mod theme;

use proto_ipc::{
    CursorStyle, KeyAction, KeyEvent, KeyMods, Rgb, ScreenCell, ScreenCursor, ScreenRow,
    ServerMessage, TerminalKey, Viewport,
};
use std::ops::Range;
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GridError {
    #[error("patch row {row} exceeds grid height {rows}")]
    RowOutOfBounds { row: u16, rows: u16 },
    #[error("patch row {row} has {actual} cells, expected {expected}")]
    InvalidRowWidth {
        row: u16,
        actual: usize,
        expected: usize,
    },
    #[error("full patch omitted row {row}")]
    MissingFullRow { row: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalGrid {
    cols: u16,
    rows: u16,
    revision: u64,
    cells: Vec<ScreenCell>,
    cursor: Option<ScreenCursor>,
    viewport: Viewport,
}

impl TerminalGrid {
    #[must_use]
    pub fn new(cols: u16, rows: u16) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Self {
            cols,
            rows,
            revision: 0,
            cells: vec![blank_cell(); usize::from(cols) * usize::from(rows)],
            cursor: None,
            viewport: Viewport::default(),
        }
    }

    /// Where the visible rows sit inside the scrollback.
    #[must_use]
    pub fn viewport(&self) -> Viewport {
        self.viewport
    }

    /// Applies a complete or incremental screen patch. Stale revisions are
    /// ignored so delayed IPC messages cannot roll the visible grid backward.
    /// Rows are moved into the grid, not cloned: a full 200×60 frame carries
    /// 12 000 strings and copying them costs as much as painting them.
    ///
    /// # Errors
    ///
    /// Rejects malformed row coordinates, widths, or incomplete full frames.
    pub fn apply_patch(
        &mut self,
        revision: u64,
        cols: u16,
        rows: u16,
        full: bool,
        mut dirty_rows: Vec<ScreenRow>,
    ) -> Result<bool, GridError> {
        if revision <= self.revision {
            return Ok(false);
        }
        let (cols, rows) = (cols.max(1), rows.max(1));
        validate_rows(cols, rows, full, &dirty_rows)?;
        if self.cols != cols || self.rows != rows {
            self.resize(cols, rows);
        }
        for row in &mut dirty_rows {
            let start = usize::from(row.y) * usize::from(cols);
            self.cells[start..start + usize::from(cols)].swap_with_slice(&mut row.cells);
        }
        self.revision = revision;
        Ok(true)
    }

    #[must_use]
    pub fn dimensions(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    #[must_use]
    pub fn cell(&self, x: u16, y: u16) -> Option<&ScreenCell> {
        if x >= self.cols || y >= self.rows {
            return None;
        }
        self.cells
            .get(usize::from(y) * usize::from(self.cols) + usize::from(x))
    }

    #[must_use]
    pub fn row_text(&self, y: u16) -> Option<String> {
        if y >= self.rows {
            return None;
        }
        let start = usize::from(y) * usize::from(self.cols);
        Some(
            self.cells[start..start + usize::from(self.cols)]
                .iter()
                .map(|cell| cell.text.as_str())
                .collect(),
        )
    }

    #[must_use]
    pub fn row(&self, y: u16) -> Option<&[ScreenCell]> {
        if y >= self.rows {
            return None;
        }
        let start = usize::from(y) * usize::from(self.cols);
        Some(&self.cells[start..start + usize::from(self.cols)])
    }

    #[must_use]
    pub fn cursor(&self) -> Option<ScreenCursor> {
        self.cursor
    }

    /// Applies a screen-bearing IPC message and ignores unrelated messages.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::apply_patch`].
    pub fn apply_server_message(&mut self, message: ServerMessage) -> Result<bool, GridError> {
        let ServerMessage::ScreenPatch {
            revision,
            cols,
            rows,
            full,
            dirty_rows,
            cursor,
            viewport,
            ..
        } = message
        else {
            return Ok(false);
        };
        let changed = self.apply_patch(revision, cols, rows, full, dirty_rows)?;
        if changed {
            self.cursor = cursor;
            self.viewport = viewport;
        }
        Ok(changed)
    }

    /// Text covered by `selection`, rows joined with `\n` and trailing blanks
    /// trimmed per row. Blank cells become spaces so columns line up.
    #[must_use]
    pub fn selected_text(&self, selection: Selection) -> String {
        let (start, end) = selection.ordered();
        let mut text = String::new();
        for y in start.y..=end.y.min(self.rows.saturating_sub(1)) {
            let Some(cells) = self.row(y) else { break };
            let Some(span) = selection.row_span(y, self.cols) else {
                continue;
            };
            if y > start.y {
                text.push('\n');
            }
            let line: String = cells[usize::from(span.start)..usize::from(span.end)]
                .iter()
                .map(|cell| {
                    if cell.text.is_empty() {
                        " "
                    } else {
                        &cell.text
                    }
                })
                .collect();
            text.push_str(line.trim_end());
        }
        text
    }

    /// Selection covering the word under `pos`, delimited by blanks and
    /// common punctuation. A blank cell selects just itself.
    #[must_use]
    pub fn word_at(&self, pos: CellPos) -> Selection {
        let Some(cells) = self.row(pos.y) else {
            return Selection::collapsed(pos);
        };
        let x = usize::from(pos.x).min(cells.len().saturating_sub(1));
        if !is_word_cell(&cells[x]) {
            return Selection::collapsed(pos);
        }
        let mut start = x;
        while start > 0 && is_word_cell(&cells[start - 1]) {
            start -= 1;
        }
        let mut end = x;
        while end + 1 < cells.len() && is_word_cell(&cells[end + 1]) {
            end += 1;
        }
        Selection {
            anchor: CellPos::new(to_column(start), pos.y),
            head: CellPos::new(to_column(end), pos.y),
        }
    }

    /// Selection covering the whole row `y`.
    #[must_use]
    pub fn line_at(&self, y: u16) -> Selection {
        Selection {
            anchor: CellPos::new(0, y),
            head: CellPos::new(self.cols.saturating_sub(1), y),
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.cells = vec![blank_cell(); usize::from(cols) * usize::from(rows)];
        self.cursor = None;
    }
}

/// Grid coordinates; ordered by row first so `Ord` follows reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellPos {
    pub y: u16,
    pub x: u16,
}

impl CellPos {
    #[must_use]
    pub const fn new(x: u16, y: u16) -> Self {
        Self { y, x }
    }
}

/// Inclusive selection between two cells in reading order, like a terminal
/// emulator: the head cell is part of the selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: CellPos,
    pub head: CellPos,
}

impl Selection {
    #[must_use]
    pub const fn collapsed(pos: CellPos) -> Self {
        Self {
            anchor: pos,
            head: pos,
        }
    }

    /// `(start, end)` with `start <= end` in reading order.
    #[must_use]
    pub fn ordered(&self) -> (CellPos, CellPos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Columns selected on row `y` as a half-open range, if any.
    #[must_use]
    pub fn row_span(&self, y: u16, cols: u16) -> Option<Range<u16>> {
        let (start, end) = self.ordered();
        if y < start.y || y > end.y || cols == 0 {
            return None;
        }
        let first = if y == start.y { start.x } else { 0 };
        let last = if y == end.y { end.x } else { cols - 1 };
        let last = last.min(cols - 1);
        (first <= last).then(|| first..last + 1)
    }
}

fn is_word_cell(cell: &ScreenCell) -> bool {
    !cell.text.is_empty()
        && !cell
            .text
            .chars()
            .all(|c| c.is_whitespace() || "()[]{}<>'\"`,;".contains(c))
}

fn to_column(index: usize) -> u16 {
    u16::try_from(index).expect("grid rows are at most u16::MAX cells wide")
}

fn validate_rows(
    cols: u16,
    rows: u16,
    full: bool,
    dirty_rows: &[ScreenRow],
) -> Result<(), GridError> {
    let mut present = vec![false; usize::from(rows)];
    for row in dirty_rows {
        if row.y >= rows {
            return Err(GridError::RowOutOfBounds { row: row.y, rows });
        }
        if row.cells.len() != usize::from(cols) {
            return Err(GridError::InvalidRowWidth {
                row: row.y,
                actual: row.cells.len(),
                expected: usize::from(cols),
            });
        }
        present[usize::from(row.y)] = true;
    }
    if full {
        for (row, exists) in present.into_iter().enumerate() {
            if !exists {
                let row = u16::try_from(row).expect("row index came from a u16 grid height");
                return Err(GridError::MissingFullRow { row });
            }
        }
    }
    Ok(())
}

/// Consecutive cells on one row that share an explicit background colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundRun {
    pub start: u16,
    pub len: u16,
    pub color: Rgb,
}

/// Merges neighbouring cells with the same explicit background so the
/// renderer paints one quad per run instead of one per cell. Cells without a
/// background are skipped: the surface behind the grid shows through.
///
/// # Panics
///
/// Panics if `cells` is wider than a terminal row can be (`u16::MAX`).
pub fn background_runs(cells: &[ScreenCell]) -> impl Iterator<Item = BackgroundRun> + '_ {
    let mut x = 0;
    std::iter::from_fn(move || {
        while x < cells.len() {
            let Some(color) = cells[x].background else {
                x += 1;
                continue;
            };
            let start = x;
            while x < cells.len() && cells[x].background == Some(color) {
                x += 1;
            }
            return Some(BackgroundRun {
                start: u16::try_from(start).expect("grid rows are at most u16::MAX cells wide"),
                len: u16::try_from(x - start).expect("run fits inside a u16-wide row"),
                color,
            });
        }
        None
    })
}

/// Cursor rectangle relative to the top-left corner of its cell, in pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorShape {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Outline only; the cell text stays visible in its own colour.
    pub hollow: bool,
}

/// Thickness of bar and underline cursors.
pub const THIN_CURSOR_PX: f32 = 2.0;

#[must_use]
pub fn cursor_shape(style: CursorStyle, cell_width: f32, cell_height: f32) -> CursorShape {
    match style {
        CursorStyle::Block | CursorStyle::HollowBlock => CursorShape {
            x: 0.0,
            y: 0.0,
            width: cell_width,
            height: cell_height,
            hollow: style == CursorStyle::HollowBlock,
        },
        CursorStyle::Underline => CursorShape {
            x: 0.0,
            y: (cell_height - THIN_CURSOR_PX).max(0.0),
            width: cell_width,
            height: THIN_CURSOR_PX.min(cell_height),
            hollow: false,
        },
        CursorStyle::Bar => CursorShape {
            x: 0.0,
            y: 0.0,
            width: THIN_CURSOR_PX.min(cell_width),
            height: cell_height,
            hollow: false,
        },
    }
}

fn blank_cell() -> ScreenCell {
    ScreenCell {
        text: String::new(),
        foreground: None,
        background: None,
        styled: false,
    }
}

/// Modifier state of a key press, independent of the windowing toolkit.
pub type KeyModifiers = KeyMods;

/// Builds the key event the daemon encodes, from a toolkit key name (the
/// W3C-style names GPUI uses: `a`, `enter`, `f5`, `pageup`…) and the text the
/// key produced. Text is only attached when no control/alt/super modifier is
/// held: the daemon derives control sequences from the key itself.
#[must_use]
pub fn key_event(name: &str, key_char: Option<&str>, mods: KeyMods, action: KeyAction) -> KeyEvent {
    let text = key_char
        .filter(|_| !(mods.control || mods.alt || mods.super_key))
        .map(str::to_string);
    let unshifted_codepoint = match name.chars().collect::<Vec<_>>().as_slice() {
        [single] => u32::from(single.to_ascii_lowercase()),
        _ => 0,
    };
    KeyEvent {
        action,
        key: terminal_key(name),
        mods,
        text,
        unshifted_codepoint,
    }
}

/// Toolkit key name to the physical key the daemon understands.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn terminal_key(name: &str) -> TerminalKey {
    use TerminalKey as K;
    match name {
        "a" => K::A,
        "b" => K::B,
        "c" => K::C,
        "d" => K::D,
        "e" => K::E,
        "f" => K::F,
        "g" => K::G,
        "h" => K::H,
        "i" => K::I,
        "j" => K::J,
        "k" => K::K,
        "l" => K::L,
        "m" => K::M,
        "n" => K::N,
        "o" => K::O,
        "p" => K::P,
        "q" => K::Q,
        "r" => K::R,
        "s" => K::S,
        "t" => K::T,
        "u" => K::U,
        "v" => K::V,
        "w" => K::W,
        "x" => K::X,
        "y" => K::Y,
        "z" => K::Z,
        "0" => K::Digit0,
        "1" => K::Digit1,
        "2" => K::Digit2,
        "3" => K::Digit3,
        "4" => K::Digit4,
        "5" => K::Digit5,
        "6" => K::Digit6,
        "7" => K::Digit7,
        "8" => K::Digit8,
        "9" => K::Digit9,
        "`" => K::Backquote,
        "\\" => K::Backslash,
        "[" => K::BracketLeft,
        "]" => K::BracketRight,
        "," => K::Comma,
        "=" => K::Equal,
        "-" => K::Minus,
        "." => K::Period,
        "'" => K::Quote,
        ";" => K::Semicolon,
        "/" => K::Slash,
        "space" | " " => K::Space,
        "tab" => K::Tab,
        "enter" => K::Enter,
        "escape" => K::Escape,
        "backspace" => K::Backspace,
        "delete" => K::Delete,
        "insert" => K::Insert,
        "home" => K::Home,
        "end" => K::End,
        "pageup" => K::PageUp,
        "pagedown" => K::PageDown,
        "up" => K::ArrowUp,
        "down" => K::ArrowDown,
        "left" => K::ArrowLeft,
        "right" => K::ArrowRight,
        "shift" => K::ShiftLeft,
        "control" => K::ControlLeft,
        "alt" => K::AltLeft,
        "platform" => K::MetaLeft,
        "capslock" => K::CapsLock,
        "numlock" => K::NumLock,
        "scrolllock" => K::ScrollLock,
        "printscreen" => K::PrintScreen,
        "pause" => K::Pause,
        "menu" => K::ContextMenu,
        "f1" => K::F1,
        "f2" => K::F2,
        "f3" => K::F3,
        "f4" => K::F4,
        "f5" => K::F5,
        "f6" => K::F6,
        "f7" => K::F7,
        "f8" => K::F8,
        "f9" => K::F9,
        "f10" => K::F10,
        "f11" => K::F11,
        "f12" => K::F12,
        "f13" => K::F13,
        "f14" => K::F14,
        "f15" => K::F15,
        "f16" => K::F16,
        "f17" => K::F17,
        "f18" => K::F18,
        "f19" => K::F19,
        "f20" => K::F20,
        "f21" => K::F21,
        "f22" => K::F22,
        "f23" => K::F23,
        "f24" => K::F24,
        _ => K::Unidentified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(text: &str) -> ScreenCell {
        ScreenCell {
            text: text.into(),
            foreground: Some(Rgb { r: 1, g: 2, b: 3 }),
            background: None,
            styled: true,
        }
    }

    fn row(y: u16, values: &[&str]) -> ScreenRow {
        ScreenRow {
            y,
            cells: values.iter().map(|value| cell(value)).collect(),
        }
    }

    #[test]
    fn applies_full_frame_and_preserves_cell_metadata() {
        let mut grid = TerminalGrid::new(1, 1);
        assert!(
            grid.apply_patch(
                1,
                2,
                2,
                true,
                vec![row(0, &["a", "b"]), row(1, &["c", "d"])]
            )
            .unwrap()
        );
        assert_eq!(grid.dimensions(), (2, 2));
        assert_eq!(grid.cell(1, 1), Some(&cell("d")));
    }

    #[test]
    fn partial_patch_only_replaces_dirty_row() {
        let mut grid = TerminalGrid::new(2, 2);
        grid.apply_patch(
            1,
            2,
            2,
            true,
            vec![row(0, &["a", "b"]), row(1, &["c", "d"])],
        )
        .unwrap();
        grid.apply_patch(2, 2, 2, false, vec![row(1, &["x", "y"])])
            .unwrap();
        assert_eq!(grid.cell(0, 0).unwrap().text, "a");
        assert_eq!(grid.cell(0, 1).unwrap().text, "x");
    }

    #[test]
    fn ignores_stale_patch() {
        let mut grid = TerminalGrid::new(1, 1);
        grid.apply_patch(2, 1, 1, true, vec![row(0, &["new"])])
            .unwrap();
        assert!(
            !grid
                .apply_patch(1, 1, 1, true, vec![row(0, &["old"])])
                .unwrap()
        );
        assert_eq!(grid.cell(0, 0).unwrap().text, "new");
    }

    #[test]
    fn exposes_a_renderable_row_without_losing_graphemes() {
        let mut grid = TerminalGrid::new(2, 1);
        grid.apply_patch(1, 2, 1, true, vec![row(0, &["🦀", "e\u{301}"])])
            .unwrap();
        assert_eq!(grid.row_text(0).as_deref(), Some("🦀e\u{301}"));
        assert_eq!(grid.row_text(1), None);
    }

    #[test]
    fn resize_clears_cells_missing_from_partial_patch() {
        let mut grid = TerminalGrid::new(2, 1);
        grid.apply_patch(1, 2, 1, true, vec![row(0, &["a", "b"])])
            .unwrap();
        grid.apply_patch(2, 3, 2, false, vec![row(1, &["x", "y", "z"])])
            .unwrap();
        assert_eq!(grid.dimensions(), (3, 2));
        assert_eq!(grid.cell(0, 0).unwrap().text, "");
    }

    #[test]
    fn rejects_malformed_patches_without_mutating_grid() {
        let mut grid = TerminalGrid::new(2, 2);
        let before = grid.clone();
        assert_eq!(
            grid.apply_patch(1, 2, 2, false, vec![row(2, &["x", "y"])]),
            Err(GridError::RowOutOfBounds { row: 2, rows: 2 })
        );
        assert_eq!(grid, before);
        assert!(matches!(
            grid.apply_patch(1, 2, 2, true, vec![row(0, &["x", "y"])]),
            Err(GridError::MissingFullRow { row: 1 })
        ));
    }

    #[test]
    fn reducer_ignores_non_screen_messages() {
        let mut grid = TerminalGrid::new(1, 1);
        let changed = grid
            .apply_server_message(ServerMessage::Initialized {
                protocol_version: 2,
                daemon_instance: 1,
            })
            .unwrap();
        assert!(!changed);
        assert_eq!(grid.revision(), 0);
    }

    #[test]
    fn reducer_updates_cursor_with_screen_revision() {
        let mut grid = TerminalGrid::new(1, 1);
        let cursor = ScreenCursor {
            x: 0,
            y: 0,
            visible: true,
            blinking: false,
            style: CursorStyle::Block,
        };
        grid.apply_server_message(ServerMessage::ScreenPatch {
            session_id: 1,
            revision: 1,
            cols: 1,
            rows: 1,
            full: true,
            dirty_rows: vec![row(0, &["x"])],
            cursor: Some(cursor),
            viewport: Viewport::default(),
        })
        .unwrap();
        assert_eq!(grid.cursor(), Some(cursor));
    }

    #[test]
    fn selection_orders_ends_and_spans_rows() {
        let selection = Selection {
            anchor: CellPos::new(5, 2),
            head: CellPos::new(1, 0),
        };
        assert_eq!(
            selection.ordered(),
            (CellPos::new(1, 0), CellPos::new(5, 2))
        );
        assert_eq!(selection.row_span(0, 10), Some(1..10));
        assert_eq!(selection.row_span(1, 10), Some(0..10));
        assert_eq!(selection.row_span(2, 10), Some(0..6));
        assert_eq!(selection.row_span(3, 10), None);
        let single = Selection {
            anchor: CellPos::new(4, 1),
            head: CellPos::new(2, 1),
        };
        assert_eq!(single.row_span(1, 10), Some(2..5));
        assert_eq!(
            Selection::collapsed(CellPos::new(3, 0)).row_span(0, 4),
            Some(3..4)
        );
    }

    #[test]
    fn selected_text_joins_rows_and_trims_trailing_blanks() {
        let mut grid = TerminalGrid::new(4, 3);
        grid.apply_patch(
            1,
            4,
            3,
            true,
            vec![
                row(0, &["l", "s", "", ""]),
                row(1, &["a", " ", "b", ""]),
                row(2, &["x", "y", "z", "w"]),
            ],
        )
        .unwrap();
        let all = Selection {
            anchor: CellPos::new(0, 0),
            head: CellPos::new(3, 2),
        };
        assert_eq!(grid.selected_text(all), "ls\na b\nxyzw");
        let partial = Selection {
            anchor: CellPos::new(2, 1),
            head: CellPos::new(1, 2),
        };
        assert_eq!(grid.selected_text(partial), "b\nxy");
        let past_the_end = Selection {
            anchor: CellPos::new(0, 2),
            head: CellPos::new(9, 9),
        };
        assert_eq!(grid.selected_text(past_the_end), "xyzw");
    }

    #[test]
    fn word_and_line_selection_follow_terminal_conventions() {
        let mut grid = TerminalGrid::new(9, 1);
        grid.apply_patch(
            1,
            9,
            1,
            true,
            vec![row(0, &["c", "d", " ", "s", "r", "c", "/", "", ""])],
        )
        .unwrap();
        assert_eq!(
            grid.word_at(CellPos::new(4, 0)),
            Selection {
                anchor: CellPos::new(3, 0),
                head: CellPos::new(6, 0),
            }
        );
        assert_eq!(
            grid.word_at(CellPos::new(2, 0)),
            Selection::collapsed(CellPos::new(2, 0))
        );
        assert_eq!(grid.selected_text(grid.word_at(CellPos::new(0, 0))), "cd");
        assert_eq!(grid.selected_text(grid.line_at(0)), "cd src/");
    }

    #[test]
    fn background_runs_merge_neighbours_and_skip_transparent_cells() {
        let red = Rgb { r: 255, g: 0, b: 0 };
        let blue = Rgb { r: 0, g: 0, b: 255 };
        let mut cells = vec![cell("a"), cell("b"), cell("c"), cell("d"), cell("e")];
        cells[0].background = Some(red);
        cells[1].background = Some(red);
        cells[3].background = Some(blue);
        cells[4].background = Some(red);
        let runs = background_runs(&cells).collect::<Vec<_>>();
        assert_eq!(
            runs,
            vec![
                BackgroundRun {
                    start: 0,
                    len: 2,
                    color: red
                },
                BackgroundRun {
                    start: 3,
                    len: 1,
                    color: blue
                },
                BackgroundRun {
                    start: 4,
                    len: 1,
                    color: red
                },
            ]
        );
        assert_eq!(background_runs(&[]).count(), 0);
    }

    #[test]
    fn cursor_shapes_stay_inside_their_cell() {
        let block = cursor_shape(CursorStyle::Block, 9.0, 18.0);
        assert_eq!(
            (block.width, block.height, block.hollow),
            (9.0, 18.0, false)
        );
        assert!(cursor_shape(CursorStyle::HollowBlock, 9.0, 18.0).hollow);
        let underline = cursor_shape(CursorStyle::Underline, 9.0, 18.0);
        assert_eq!((underline.y, underline.height), (16.0, 2.0));
        let bar = cursor_shape(CursorStyle::Bar, 9.0, 18.0);
        assert_eq!((bar.width, bar.height), (2.0, 18.0));
        let tiny = cursor_shape(CursorStyle::Underline, 1.0, 1.0);
        assert_eq!((tiny.y, tiny.height), (0.0, 1.0));
    }

    #[test]
    fn key_events_carry_text_only_without_control_modifiers() {
        let plain = KeyMods::default();
        let control = KeyMods {
            control: true,
            ..plain
        };
        let typed = key_event("x", Some("ñ"), plain, KeyAction::Press);
        assert_eq!(typed.key, TerminalKey::X);
        assert_eq!(typed.text.as_deref(), Some("ñ"));
        assert_eq!(typed.unshifted_codepoint, u32::from('x'));
        let ctrl = key_event("c", Some("c"), control, KeyAction::Press);
        assert_eq!(ctrl.text, None);
        assert_eq!(ctrl.key, TerminalKey::C);
        assert_eq!(terminal_key("pageup"), TerminalKey::PageUp);
        assert_eq!(terminal_key("f12"), TerminalKey::F12);
        assert_eq!(terminal_key("ñ"), TerminalKey::Unidentified);
        assert_eq!(
            key_event("ñ", Some("ñ"), plain, KeyAction::Press).unshifted_codepoint,
            u32::from('ñ')
        );
    }
}
