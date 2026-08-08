//! The seam over the terminal emulator.
//!
//! This is the only file in the workspace that names the emulator crate, and
//! that is the whole design: swapping it for the other candidate means
//! rewriting this file and nothing else, because every type that crosses out
//! of here belongs to us. The alternative shape — a trait with one
//! implementor — would say the same thing in more words and would put a
//! lifetime-carrying associated type between the grid and the two callers
//! that read it, for a second implementation nobody is writing today.
//!
//! What the emulator is asked for is deliberately small: give it text, get
//! back which rows changed, and read the rows and the cursor. Anything richer
//! — cell-level damage spans, scrollback, palette resolution — is either not
//! needed here or is information this layer has no business inventing.

use agent_bridge_events::{CellColor, CellIntensity, CellStyle, CursorPosition, ScreenCell};

/// A terminal screen with no scrollback: exactly the rows that are visible,
/// which is all a reconstruction of "what is on screen now" can mean.
#[derive(Debug)]
pub(crate) struct Grid {
    vt: avt::Vt,
}

impl Grid {
    /// A blank screen of the given size.
    pub(crate) fn new(cols: u16, rows: u16) -> Self {
        let (cols, rows) = habitable(cols, rows);
        let mut grid = Self {
            vt: avt::Vt::builder()
                .size(cols, rows)
                // No scrollback. What scrolls off the top is gone from the
                // screen, and the events already carry the history — keeping
                // a second copy here would double the cost of the buffer to
                // hold what nothing reads.
                .scrollback_limit(0)
                .build(),
        };
        // A fresh emulator considers every row changed, on the reasoning that
        // a renderer has not drawn any of them yet. This is not a renderer,
        // and "the screen is blank" is not news, so the opening report is
        // taken and dropped — otherwise the first feed of every session would
        // arrive carrying the whole screen behind it.
        grid.feed("");
        grid
    }

    /// Interprets `text` and returns the rows it changed, top-relative.
    ///
    /// "Changed" is the emulator's word, not ours: a row it reports is a row
    /// something was *written to*, which is not the same as a row that now
    /// looks different. Telling those apart is what the repaint filter is
    /// for, and it needs this list to know which rows are even worth asking
    /// about.
    pub(crate) fn feed(&mut self, text: &str) -> Vec<usize> {
        self.vt.feed_str(text).lines
    }

    /// Reflows to a new size and returns the rows that changed.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) -> Vec<usize> {
        let (cols, rows) = habitable(cols, rows);
        self.vt.resize(cols, rows).lines
    }

    /// The screen size, in columns and rows.
    pub(crate) fn size(&self) -> (u16, u16) {
        let (cols, rows) = self.vt.size();
        (clamp_to_u16(cols), clamp_to_u16(rows))
    }

    /// Where the cursor sits, zero-based from the top left.
    ///
    /// A hidden cursor still has a position, and that position is what is
    /// reported: the snapshot describes where the terminal would put the
    /// caret, and whether it is currently drawn is a rendering question for
    /// whoever displays the snapshot.
    pub(crate) fn cursor(&self) -> CursorPosition {
        let cursor = self.vt.cursor();
        CursorPosition {
            row: u32::try_from(cursor.row).unwrap_or(u32::MAX),
            col: u32::try_from(cursor.col).unwrap_or(u32::MAX),
        }
    }

    /// How many rows are visible.
    pub(crate) fn row_count(&self) -> usize {
        self.vt.view().count()
    }

    /// One visible row, top-relative.
    ///
    /// Rows are addressed one at a time rather than handed out as an
    /// iterator because both callers want a specific row: the repaint filter
    /// asks only about the rows the feed touched, and the snapshot walks them
    /// in order.
    pub(crate) fn row(&self, index: usize) -> Row<'_> {
        Row(self.vt.line(index))
    }

    /// Roughly how much memory the grid occupies, in bytes.
    ///
    /// Counted rather than measured: with no scrollback the grid is a known
    /// number of cells of a known size, and the emulator's own bookkeeping
    /// around them is small and constant. An allocator hook would give a
    /// figure to the byte and would tie the number to which allocator the
    /// test ran under.
    pub(crate) fn footprint(&self) -> usize {
        let (cols, rows) = self.vt.size();
        rows * (cols * size_of::<avt::Cell>() + size_of::<Vec<avt::Cell>>())
    }
}

/// One row of the screen.
pub(crate) struct Row<'a>(&'a avt::Line);

impl Row<'_> {
    /// The row's cells, left to right, one per column.
    ///
    /// A double-width glyph occupies two of them: the character in the first
    /// and a zero-width blank in the second. Carrying that second cell rather
    /// than dropping it is what keeps a column index an index into this
    /// sequence, which is the property a region-anchored matcher is built on.
    pub(crate) fn cells(&self) -> impl Iterator<Item = ScreenCell> + '_ {
        self.0.cells().iter().map(|cell| ScreenCell {
            ch: cell.char(),
            width: cell.width(),
            style: style_of(cell.pen()),
        })
    }

    /// The row as text: the blank half of each double-width glyph left out,
    /// and the unwritten remainder of the row cut off, so the string reads
    /// the way the row looks rather than the way it is stored.
    pub(crate) fn text(&self) -> String {
        let mut text = self.0.text();
        text.truncate(text.trim_end().len());
        text
    }
}

/// Translates the emulator's pen into the published cell style.
fn style_of(pen: &avt::Pen) -> CellStyle {
    CellStyle {
        foreground: pen.foreground().map(color_of),
        background: pen.background().map(color_of),
        intensity: if pen.is_bold() {
            CellIntensity::Bold
        } else if pen.is_faint() {
            CellIntensity::Faint
        } else {
            CellIntensity::Normal
        },
        italic: pen.is_italic(),
        underline: pen.is_underline(),
        strikethrough: pen.is_strikethrough(),
        blink: pen.is_blink(),
        inverse: pen.is_inverse(),
    }
}

fn color_of(color: avt::Color) -> CellColor {
    match color {
        avt::Color::Indexed(index) => CellColor::Indexed(index),
        avt::Color::RGB(rgb) => CellColor::Rgb([rgb.r, rgb.g, rgb.b]),
    }
}

/// A size the emulator can actually hold.
///
/// A screen with no columns or no rows has no cells, and the emulator
/// indexes into them without checking — a zero in either dimension is an
/// out-of-bounds panic a few calls later, not an empty screen. Terminal
/// dimensions reach this runtime from a caller over the wire, so zero is a
/// value that will arrive; the session it would take down has done nothing
/// wrong, and one degenerate cell is a more useful answer than none.
fn habitable(cols: u16, rows: u16) -> (usize, usize) {
    (usize::from(cols.max(1)), usize::from(rows.max(1)))
}

/// Terminal dimensions arrive as `u16` and are handed back as `u16`; the
/// emulator counts in `usize` in between. A screen larger than 65 535 rows
/// cannot be asked for through this crate, so the saturation is unreachable
/// rather than lossy — but saturating beats a panic on a value the caller
/// never supplied.
fn clamp_to_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::Grid;
    use agent_bridge_events::{CellColor, CellIntensity, CursorPosition};

    #[test]
    fn a_new_grid_has_the_size_it_was_asked_for() {
        let grid = Grid::new(120, 40);
        assert_eq!(grid.size(), (120, 40));
        assert_eq!(grid.row_count(), 40);
    }

    #[test]
    fn cursor_addressing_lands_text_in_the_addressed_columns() {
        // The surface that defeats a matcher reading the stripped stream:
        // the words are written with cursor moves between them, so the
        // stripped text runs them together and only the grid spaces them.
        let mut grid = Grid::new(80, 24);
        grid.feed("\u{1b}[1;1HDo\u{1b}[1;4Hyou\u{1b}[1;8Hwant");
        assert_eq!(grid.row(0).text(), "Do you want");
    }

    #[test]
    fn a_repaint_overwrites_the_row_instead_of_adding_one() {
        let mut grid = Grid::new(80, 24);
        grid.feed("\u{1b}[1;1Hstatus\u{1b}[1;1Hstatus");
        assert_eq!(grid.row(0).text(), "status");
        assert_eq!(grid.row(1).text(), "");
    }

    #[test]
    fn a_feed_reports_the_rows_it_wrote_to() {
        let mut grid = Grid::new(80, 24);
        assert_eq!(grid.feed("one\r\ntwo"), vec![0, 1]);
        // And having reported them, it does not report them again: the list
        // is what changed since the last feed, not what has ever changed.
        assert_eq!(grid.feed(""), Vec::<usize>::new());
    }

    #[test]
    fn a_row_written_with_the_same_text_is_still_reported_as_changed() {
        // The reason the repaint filter exists at all. The emulator reports
        // what was written, not what now differs, so an identical repaint
        // looks exactly like a real change from here.
        let mut grid = Grid::new(80, 24);
        grid.feed("\u{1b}[1;1Hstatus");
        assert_eq!(grid.feed("\u{1b}[1;1Hstatus"), vec![0]);
    }

    #[test]
    fn the_cursor_is_where_the_addressing_left_it() {
        let mut grid = Grid::new(80, 24);
        grid.feed("\u{1b}[4;13H");
        assert_eq!(grid.cursor(), CursorPosition { row: 3, col: 12 });
    }

    #[test]
    fn display_attributes_reach_the_cells_that_carry_them() {
        let mut grid = Grid::new(80, 24);
        grid.feed("\u{1b}[1;4;38;5;9mred\u{1b}[0m plain");
        let row = grid.row(0);
        let cells: Vec<_> = row.cells().take(10).collect();
        assert_eq!(cells[0].ch, 'r');
        assert_eq!(cells[0].style.intensity, CellIntensity::Bold);
        assert!(cells[0].style.underline);
        assert_eq!(cells[0].style.foreground, Some(CellColor::Indexed(9)));
        assert_eq!(cells[4].ch, 'p');
        assert!(cells[4].style.is_plain(), "the reset must end the run");
    }

    #[test]
    fn a_truecolor_background_survives_as_its_components() {
        let mut grid = Grid::new(80, 24);
        grid.feed("\u{1b}[48;2;10;20;30mx");
        let row = grid.row(0);
        let cell = row.cells().next().expect("the row has cells");
        assert_eq!(cell.style.background, Some(CellColor::Rgb([10, 20, 30])));
    }

    #[test]
    fn a_double_width_glyph_takes_two_cells_so_columns_still_line_up() {
        let mut grid = Grid::new(80, 24);
        grid.feed("漢x");
        let row = grid.row(0);
        let cells: Vec<_> = row.cells().take(3).collect();
        assert_eq!((cells[0].ch, cells[0].width), ('漢', 2));
        assert_eq!(
            cells[1].width, 0,
            "the covered column is carried, not dropped"
        );
        assert_eq!(cells[2].ch, 'x', "so `x` is at column 2, where it is drawn");
        assert_eq!(row.text(), "漢x", "and the text reads without the filler");
    }

    #[test]
    fn resizing_reflows_and_the_size_follows() {
        let mut grid = Grid::new(80, 24);
        grid.feed("first\r\nsecond");
        grid.resize(120, 40);
        assert_eq!(grid.size(), (120, 40));
        assert_eq!(grid.row_count(), 40);
        assert_eq!(grid.row(0).text(), "first");
    }
}
