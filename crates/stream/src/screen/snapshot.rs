//! Materializing the grid into the snapshot that travels on the wire.
//!
//! This is the expensive half of the feed/render split and the reason the
//! split exists: walking every cell of a 200×100 screen and building an owned
//! structure out of it is work worth doing when someone asks for it and worth
//! doing never otherwise.
//!
//! Two things keep a snapshot proportional to what is on a screen rather than
//! to the size of the screen, which matters because it travels whole and is
//! one of the larger things this runtime ever sends.
//!
//! - **Rows are cut at their last written cell.** A terminal screen is mostly
//!   empty most of the time, and spelling out every trailing blank would make
//!   a snapshot's size track the terminal's area instead of its text.
//! - **Styles are named, not repeated.** Nearly every cell of a drawn
//!   interface carries a colour, and a screen is drawn from very few of them —
//!   four to fifteen across the recorded sessions. Writing the style into
//!   every cell that uses it is the same short object a thousand times over;
//!   measured, it was half of the payload.
//!
//! Both were arrived at by measuring the recorded corpus rather than by
//! reasoning about it, and the second one only after the first estimate of a
//! cell's cost turned out to be a third of the real figure.

use std::collections::HashMap;

use agent_bridge_events::{CellStyle, ScreenCell, ScreenSnapshot};

use super::vt::{Grid, VtCell};

/// Builds the snapshot for the screen as it stands.
pub(crate) fn render(grid: &Grid) -> ScreenSnapshot {
    let (cols, rows) = grid.size();
    let mut styles = StyleTable::default();
    let cells = (0..grid.row_count())
        .map(|index| row(grid, index, &mut styles))
        .collect();
    ScreenSnapshot {
        cols: u32::from(cols),
        rows: u32::from(rows),
        cursor: grid.cursor(),
        styles: styles.listed,
        cells,
    }
}

/// One row, trimmed to its last written cell, its styles named rather than
/// spelled out.
fn row(grid: &Grid, index: usize, styles: &mut StyleTable) -> Vec<ScreenCell> {
    let mut cells: Vec<VtCell> = grid.row(index).cells().collect();
    let written = cells.iter().rposition(|cell| !is_blank(cell));
    cells.truncate(written.map_or(0, |last| last + 1));
    cells
        .into_iter()
        .map(|cell| ScreenCell {
            ch: cell.ch,
            width: cell.width,
            style: styles.name(cell.style),
        })
        .collect()
}

/// The styles a screen is drawn from, each given a number the cells can use.
///
/// A screen normally uses a handful, so a scan over a short list would nearly
/// always win. It is a map anyway, because the exception is not exotic:
/// anything painting a true colour per cell — an image viewer, a gradient, a
/// dashboard — gives every cell a style of its own, and a scan then compares
/// every cell against every style already found. Rendering one 200×100 screen
/// of that kind measured at **152 ms** against a fraction of a millisecond
/// for an ordinary one, on a path a caller reaches by reconnecting. The map
/// gives up a little on the common screen to make the bad one ordinary.
struct StyleTable {
    /// The styles in the order they were first seen, which is what the
    /// snapshot carries.
    listed: Vec<CellStyle>,
    /// Where each of them sits in that list.
    numbered: HashMap<CellStyle, u32>,
}

impl Default for StyleTable {
    fn default() -> Self {
        // Index 0 is the default style on every snapshot, used or not, so
        // reading a cell's style is one unconditional lookup rather than a
        // lookup and a fallback.
        let default = CellStyle::default();
        Self {
            listed: vec![default.clone()],
            numbered: HashMap::from([(default, 0)]),
        }
    }
}

impl StyleTable {
    /// The number for `style`, giving it one if it does not have it yet.
    fn name(&mut self, style: CellStyle) -> u32 {
        if let Some(known) = self.numbered.get(&style) {
            return *known;
        }
        let number = u32::try_from(self.listed.len())
            .expect("a screen is drawn from fewer than 4 billion styles");
        self.listed.push(style.clone());
        self.numbered.insert(style, number);
        number
    }
}

/// Whether a cell shows nothing: a space, one column wide, in the default
/// style. Anything else — a styled space, the covered half of a wide glyph —
/// carries information a consumer would notice missing.
fn is_blank(cell: &VtCell) -> bool {
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
                "styles": [{}, { "intensity": "bold" }],
                "cells": [[{ "ch": "A", "style": 1 }, { "ch": "b" }], []]
            })
        );
    }

    #[test]
    fn the_default_style_is_always_the_first_entry_even_when_unused() {
        // So `styles[cell.style]` is one lookup with no special case, on
        // every snapshot, including a screen with nothing plain on it.
        let mut grid = Grid::new(4, 1);
        grid.feed("\u{1b}[1mAB");
        let snapshot = render(&grid);
        assert_eq!(
            snapshot.styles[0],
            agent_bridge_events::CellStyle::default()
        );
        assert!(snapshot.cells[0].iter().all(|cell| cell.style == 1));
    }

    #[test]
    fn one_style_used_many_times_is_listed_once() {
        // The reason the table exists: a screen is drawn out of a handful of
        // styles, and writing one into every cell that uses it is the same
        // short object repeated a thousand times.
        let mut grid = Grid::new(40, 3);
        grid.feed("\u{1b}[31m");
        for row in 1..=3 {
            grid.feed(&format!("\u{1b}[{row};1Ha line of red text"));
        }
        let snapshot = render(&grid);
        assert_eq!(snapshot.styles.len(), 2, "the default, and red");
        assert!(snapshot.cells.iter().flatten().all(|cell| cell.style == 1));
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
        assert!(
            snapshot.cells[0]
                .iter()
                .all(|cell| snapshot.styles[cell.style as usize].inverse)
        );
    }

    #[test]
    fn the_dimensions_reported_are_the_grids_own() {
        let snapshot = render(&Grid::new(200, 100));
        assert_eq!((snapshot.cols, snapshot.rows), (200, 100));
        assert_eq!(snapshot.cells.len(), 100);
    }
}
