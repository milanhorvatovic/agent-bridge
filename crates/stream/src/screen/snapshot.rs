//! Materializing the grid into the snapshot that travels on the wire.
//!
//! This is the expensive half of the feed/render split and the reason the
//! split exists: walking every cell of a 200×100 screen and building an owned
//! structure out of it is work worth doing when someone asks for it and worth
//! doing never otherwise.
//!
//! Rows are cut at their last written cell. A terminal screen is mostly empty
//! most of the time, and a snapshot that spelled out every trailing blank
//! would be dominated by them — the size of a snapshot would track the size
//! of the terminal rather than the amount of text on it, and the reconnect
//! payload is one of the larger things this runtime ever sends.

use agent_bridge_events::{ScreenCell, ScreenSnapshot};

use super::vt::Grid;

/// Builds the snapshot for the screen as it stands.
pub(crate) fn render(grid: &Grid) -> ScreenSnapshot {
    let (cols, rows) = grid.size();
    ScreenSnapshot {
        cols: u32::from(cols),
        rows: u32::from(rows),
        cursor: grid.cursor(),
        cells: (0..grid.row_count())
            .map(|index| row(grid, index))
            .collect(),
    }
}

/// One row, trimmed to its last written cell.
fn row(grid: &Grid, index: usize) -> Vec<ScreenCell> {
    let mut cells: Vec<ScreenCell> = grid.row(index).cells().collect();
    let written = cells.iter().rposition(|cell| !is_blank(cell));
    cells.truncate(written.map_or(0, |last| last + 1));
    cells
}

/// Whether a cell shows nothing: a space, one column wide, in the default
/// style. Anything else — a styled space, the covered half of a wide glyph —
/// carries information a consumer would notice missing.
fn is_blank(cell: &ScreenCell) -> bool {
    cell.ch == ' ' && cell.width == 1 && cell.style.is_plain()
}

#[cfg(test)]
mod tests {
    use super::{Grid, render};
    use serde_json::json;

    #[test]
    fn a_blank_screen_is_rows_of_nothing_rather_than_no_rows() {
        // Row indices have to mean the same thing on every snapshot, so a
        // blank row is an empty array and not an absent one.
        let snapshot = render(&Grid::new(80, 24));
        assert_eq!(snapshot.cells.len(), 24);
        assert!(snapshot.cells.iter().all(Vec::is_empty));
    }

    #[test]
    fn a_row_stops_at_its_last_written_cell() {
        let mut grid = Grid::new(80, 24);
        grid.feed("hi");
        let snapshot = render(&grid);
        assert_eq!(snapshot.cells[0].len(), 2);
        assert_eq!(snapshot.cells[1].len(), 0);
    }

    #[test]
    fn a_blank_inside_a_row_is_kept_because_the_column_after_it_is_not() {
        let mut grid = Grid::new(80, 24);
        grid.feed("\u{1b}[1;1HDo\u{1b}[1;4Hyou");
        let snapshot = render(&grid);
        let text: String = snapshot.cells[0].iter().map(|cell| cell.ch).collect();
        assert_eq!(text, "Do you");
    }

    #[test]
    fn the_serialized_shape_is_the_documented_one() {
        let mut grid = Grid::new(4, 2);
        grid.feed("\u{1b}[1mA\u{1b}[0mb\r\n");
        let snapshot = render(&grid);
        assert_eq!(
            serde_json::to_value(&snapshot).expect("a snapshot serializes"),
            json!({
                "cols": 4,
                "rows": 2,
                "cursor": { "row": 1, "col": 0 },
                "cells": [
                    [{ "ch": "A", "style": { "intensity": "bold" } }, { "ch": "b" }],
                    []
                ]
            })
        );
    }

    #[test]
    fn the_covered_half_of_a_wide_glyph_is_carried_so_columns_still_index() {
        let mut grid = Grid::new(6, 1);
        grid.feed("漢x");
        let snapshot = render(&grid);
        assert_eq!(
            serde_json::to_value(&snapshot.cells).expect("cells serialize"),
            json!([[{ "ch": "漢", "width": 2 }, { "ch": " ", "width": 0 }, { "ch": "x" }]])
        );
    }

    #[test]
    fn a_styled_space_is_not_a_blank_and_survives_the_trim() {
        // A highlighted run of spaces is how a TUI draws a selected row; a
        // trim that treated it as empty would erase the selection.
        let mut grid = Grid::new(8, 1);
        grid.feed("\u{1b}[7m  \u{1b}[0m");
        let snapshot = render(&grid);
        assert_eq!(snapshot.cells[0].len(), 2);
        assert!(snapshot.cells[0].iter().all(|cell| cell.style.inverse));
    }

    #[test]
    fn the_dimensions_reported_are_the_grids_own() {
        let snapshot = render(&Grid::new(200, 100));
        assert_eq!((snapshot.cols, snapshot.rows), (200, 100));
        assert_eq!(snapshot.cells.len(), 100);
    }
}
