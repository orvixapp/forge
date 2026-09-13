//! Headless terminal-grid state consumed by the GPUI frontend.

use proto_ipc::{Rgb, ScreenCell, ScreenRow};
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
        }
    }

    /// Applies a complete or incremental screen patch. Stale revisions are
    /// ignored so delayed IPC messages cannot roll the visible grid backward.
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
        dirty_rows: &[ScreenRow],
    ) -> Result<bool, GridError> {
        if revision <= self.revision {
            return Ok(false);
        }
        let (cols, rows) = (cols.max(1), rows.max(1));
        validate_rows(cols, rows, full, dirty_rows)?;
        if self.cols != cols || self.rows != rows {
            self.resize(cols, rows);
        }
        for row in dirty_rows {
            let start = usize::from(row.y) * usize::from(cols);
            self.cells[start..start + usize::from(cols)].clone_from_slice(&row.cells);
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

    fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.cells = vec![blank_cell(); usize::from(cols) * usize::from(rows)];
    }
}

fn validate_rows(cols: u16, rows: u16, full: bool, dirty_rows: &[ScreenRow]) -> Result<(), GridError> {
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
                return Err(GridError::MissingFullRow { row: row as u16 });
            }
        }
    }
    Ok(())
}

fn blank_cell() -> ScreenCell {
    ScreenCell {
        text: String::new(),
        foreground: None,
        background: None,
        styled: false,
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
        assert!(grid
            .apply_patch(1, 2, 2, true, &[row(0, &["a", "b"]), row(1, &["c", "d"])])
            .unwrap());
        assert_eq!(grid.dimensions(), (2, 2));
        assert_eq!(grid.cell(1, 1), Some(&cell("d")));
    }

    #[test]
    fn partial_patch_only_replaces_dirty_row() {
        let mut grid = TerminalGrid::new(2, 2);
        grid.apply_patch(1, 2, 2, true, &[row(0, &["a", "b"]), row(1, &["c", "d"])])
            .unwrap();
        grid.apply_patch(2, 2, 2, false, &[row(1, &["x", "y"])])
            .unwrap();
        assert_eq!(grid.cell(0, 0).unwrap().text, "a");
        assert_eq!(grid.cell(0, 1).unwrap().text, "x");
    }

    #[test]
    fn ignores_stale_patch() {
        let mut grid = TerminalGrid::new(1, 1);
        grid.apply_patch(2, 1, 1, true, &[row(0, &["new"])]).unwrap();
        assert!(!grid.apply_patch(1, 1, 1, true, &[row(0, &["old"])]).unwrap());
        assert_eq!(grid.cell(0, 0).unwrap().text, "new");
    }

    #[test]
    fn resize_clears_cells_missing_from_partial_patch() {
        let mut grid = TerminalGrid::new(2, 1);
        grid.apply_patch(1, 2, 1, true, &[row(0, &["a", "b"])]).unwrap();
        grid.apply_patch(2, 3, 2, false, &[row(1, &["x", "y", "z"])])
            .unwrap();
        assert_eq!(grid.dimensions(), (3, 2));
        assert_eq!(grid.cell(0, 0).unwrap().text, "");
    }

    #[test]
    fn rejects_malformed_patches_without_mutating_grid() {
        let mut grid = TerminalGrid::new(2, 2);
        let before = grid.clone();
        assert_eq!(
            grid.apply_patch(1, 2, 2, false, &[row(2, &["x", "y"])]),
            Err(GridError::RowOutOfBounds { row: 2, rows: 2 })
        );
        assert_eq!(grid, before);
        assert!(matches!(
            grid.apply_patch(1, 2, 2, true, &[row(0, &["x", "y"])]),
            Err(GridError::MissingFullRow { row: 1 })
        ));
    }
}
