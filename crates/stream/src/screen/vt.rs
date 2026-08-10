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

use agent_bridge_events::{CellColor, CellIntensity, CellStyle, CursorPosition};

/// How many full-size cell grids one terminal holds.
///
/// A primary buffer and an alternate one, both allocated when the terminal is
/// created and both kept for its lifetime — switching to the alternate screen
/// and back restores what was underneath precisely because the other grid was
/// never released.
const BUFFERS_PER_TERMINAL: usize = 2;

/// A terminal screen with no scrollback: exactly the rows that are visible,
/// which is all a reconstruction of "what is on screen now" can mean.
pub(crate) struct Grid {
    vt: avt::Vt,
    /// The most one buffer of this screen has ever cost, in bytes.
    ///
    /// Bytes rather than the largest width and the largest height, which are
    /// not a shape: a screen that was tall and narrow and then became short
    /// and wide never occupied the product of the two, and remembering it
    /// that way would report a grid that never existed.
    largest_buffer: usize,
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
            largest_buffer: one_buffer_bytes(cols, rows),
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
        self.largest_buffer = self.largest_buffer.max(one_buffer_bytes(cols, rows));
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
    ///
    /// The column is clamped to the last real one. After a character is
    /// printed in the final column with wrapping on, the emulator parks the
    /// cursor one past the end as its "wrap on the next character" marker —
    /// a position that is meaningful inside the emulator and out of range on
    /// the wire, where a consumer indexes the row by it. Visually the caret
    /// is in the last column, so that is what gets reported.
    pub(crate) fn cursor(&self) -> CursorPosition {
        let cursor = self.vt.cursor();
        let (cols, _) = self.vt.size();
        let col = cursor.col.min(cols.saturating_sub(1));
        CursorPosition {
            row: u32::try_from(cursor.row).unwrap_or(u32::MAX),
            col: u32::try_from(col).unwrap_or(u32::MAX),
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
    ///
    /// **Two grids, not one, and only one of them is known to be current.**
    /// The emulator holds a primary buffer and an alternate one and keeps
    /// both for the life of the session — that is how switching screens and
    /// back restores what was underneath — but a resize only reaches the
    /// buffer that is active. A session that shrinks while drawing on the
    /// alternate screen leaves the parked one allocated at the size it had,
    /// and the size the emulator reports is the active one's.
    ///
    /// So the parked buffer is counted at the largest this screen has ever
    /// been. That over-reports after a session shrinks for good — the parked
    /// buffer is reconciled when it is swapped back in, and this cannot see
    /// that happen — and over-reporting is the direction a memory figure
    /// should err in, since something budgets sessions from it.
    pub(crate) fn footprint(&self) -> usize {
        let (cols, rows) = self.vt.size();
        one_buffer_bytes(cols, rows) + self.largest_buffer
    }
}

/// What a grid of this size costs, without building one.
///
/// Both buffers, and the per-row vector header as well as the cells — that
/// header is what makes a tall narrow screen expensive out of proportion to
/// its area.
pub(crate) fn projected_grid_bytes(cols: usize, rows: usize) -> usize {
    one_buffer_bytes(cols, rows) * BUFFERS_PER_TERMINAL
}

/// What one buffer of this size costs.
///
/// A row is an `avt::Line`, not a bare vector of cells — it carries whether
/// it was wrapped, and the padding that alignment adds around that. Counting
/// the vector alone understates every row of both buffers, which on a tall
/// screen is the difference between a backstop that holds and one that is
/// merely near.
fn one_buffer_bytes(cols: usize, rows: usize) -> usize {
    rows * (cols * size_of::<avt::Cell>() + size_of::<avt::Line>())
}

impl std::fmt::Debug for Grid {
    /// Its size, not what is written on it. The emulator's derived `Debug`
    /// prints every cell, which is the CLI's output — and output does not go
    /// into a log line unless somebody asked for it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (cols, rows) = self.size();
        formatter
            .debug_struct("Grid")
            .field("cols", &cols)
            .field("rows", &rows)
            .finish_non_exhaustive()
    }
}

/// One cell as the seam reports it: what it shows and how, with the style
/// spelled out rather than named.
pub(crate) struct VtCell {
    pub(crate) ch: char,
    pub(crate) width: u8,
    pub(crate) style: CellStyle,
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
    ///
    /// Each carries its style outright. Collapsing the repeats into a table
    /// is the snapshot's job, because only the snapshot knows what the other
    /// rows used.
    pub(crate) fn cells(&self) -> impl Iterator<Item = VtCell> + '_ {
        self.0.cells().iter().map(|cell| VtCell {
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
        // Only the blank the terminal fills unwritten cells with, not every
        // scalar Unicode calls whitespace. A row ending in a no-break or
        // ideographic space ends in something the CLI drew and the snapshot
        // keeps, and trimming it here would leave the text this component
        // reports disagreeing with the screen it reports alongside — a row
        // holding one of them would read as empty and be dropped as
        // contentless.
        text.truncate(text.trim_end_matches(' ').len());
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
    fn the_cursor_never_reports_a_column_the_row_does_not_have() {
        // Printing into the final column with wrapping on leaves the
        // emulator's cursor one past the end, as its marker for "wrap before
        // the next character". That is a sensible internal state and an
        // out-of-range answer for anyone indexing the row by it.
        let mut grid = Grid::new(5, 3);
        grid.feed("abcde");
        let cursor = grid.cursor();
        assert_eq!(cursor.row, 0);
        assert_eq!(cursor.col, 4, "the caret is visually in the last column");
    }

    #[test]
    fn a_combining_mark_takes_a_column_the_display_would_not_give_it() {
        // A **known divergence from a real terminal**, recorded so it is a
        // thing somebody decided rather than a thing nobody noticed.
        //
        // A terminal composes a combining mark into the cell before it and
        // shows `é` in one column. This emulator gives the mark a column of
        // its own, so a decomposed letter takes two cells and everything to
        // its right sits one column further along than it would on screen.
        // Joined emoji shift further still.
        //
        // It is not fixed here because it cannot be: the emulator allocates
        // the column before this layer sees the grid, so neither a
        // grapheme-valued cell nor a merge at this seam recovers the
        // geometry — that would take an emulator that groups combining
        // scalars. What this layer can do is not claim otherwise, which the
        // cell's own documentation now does.
        //
        // Text is unaffected, and that is the property matching depends on.
        let mut grid = Grid::new(20, 1);
        grid.feed("e\u{301}X");
        let row = grid.row(0);
        let cells: Vec<(char, u8)> = row.cells().take(3).map(|c| (c.ch, c.width)).collect();
        assert_eq!(
            cells,
            vec![('e', 1), ('\u{301}', 1), ('X', 1)],
            "if this ever reads as one cell, the emulator started composing and the \
             divergence documented on `ScreenCell::ch` is gone"
        );
        assert_eq!(
            row.text(),
            "e\u{301}X",
            "the scalars still read back in order"
        );
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
