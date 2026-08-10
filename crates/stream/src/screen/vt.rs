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

/// How many buffers exist at once at the worst moment ordinary output can
/// reach, which is not the same as how many a terminal holds.
///
/// Two of the sequences every terminal must answer replace a buffer, and both
/// build the replacement before releasing what it replaces:
///
/// - **Entering the alternate screen** swaps the two buffers and then builds
///   a fresh one over the old alternate, so three are live for the length of
///   that call. `ESC[?1049h` is what every full-screen interface sends on
///   startup, and the ones this component exists for send it within the first
///   couple of kilobytes and stay there.
/// - **A reset** builds *both* replacements before assigning either, so four
///   are live. `ESC c` is rarer but is ordinary output, not a fault.
///
/// Measured on a 1 200×200 screen holding 7.7 MiB settled: 11.0 MiB entering
/// the alternate screen, 14.7 MiB on a reset — exactly three and four times
/// one buffer. A screen admitted on what it holds at rest is therefore
/// admitted on a figure that any TUI startup passes straight through, with no
/// resize involved and nothing to notice it. Admission uses this instead.
const BUFFERS_AT_A_RESET: usize = 4;

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
    /// The most columns this screen has ever had.
    ///
    /// A row narrowed by a reflow keeps the room its cells occupied — the
    /// emulator truncates the row and a truncation does not give memory
    /// back — so a row that was once wide still owns that much whatever the
    /// screen's width says now.
    widest_cols: usize,
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
            widest_cols: cols,
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
        self.widest_cols = self.widest_cols.max(cols);
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

    /// What both buffers would cost after a resize to this size.
    ///
    /// What this grid would settle at after a reshape, in bytes.
    ///
    /// Not simply two buffers at the new size: a resize reaches the active
    /// one only, so the parked buffer stays as large as it has ever been.
    /// Judging a resize on two-at-the-new-size lets a wide screen become a
    /// tall one when each would be affordable alone and the pair is not.
    pub(crate) fn settled_after_resize(&self, cols: u16, rows: u16) -> usize {
        let (cols, rows) = habitable(cols, rows);
        let widest = self.widest_cols.max(cols);
        one_buffer_bytes(widest, rows) + self.largest_buffer.max(one_buffer_bytes(cols, rows))
    }

    /// The most this grid could occupy at once after a reshape, in bytes.
    ///
    /// A session goes on being a session after it is resized, so the shape it
    /// lands in has to survive the same buffer replacements a fresh one does.
    /// Two more at the new size, on top of the pair it settles at — see
    /// [`BUFFERS_AT_A_RESET`].
    pub(crate) fn projected_after_resize(&self, cols: u16, rows: u16) -> usize {
        let (habitable_cols, habitable_rows) = habitable(cols, rows);
        self.settled_after_resize(cols, rows)
            + one_buffer_bytes(habitable_cols, habitable_rows)
                * (BUFFERS_AT_A_RESET - BUFFERS_PER_TERMINAL)
    }

    /// What reshaping to `cols` would allocate on top of what is already
    /// held, in bytes.
    ///
    /// On top of, not instead of: the parked buffer stays where it is and the
    /// active one is still there while its replacement is built, so a caller
    /// deciding whether a reflow fits has to add this to what the screen
    /// already costs. It is the difference, and nothing here is the total.
    ///
    /// A reflow transforms every row the buffer holds and collects all of
    /// them before the rows past the bottom of the new screen are discarded,
    /// so what it allocates is governed by the *old* row count and the *new*
    /// width. Three cases, which measurement separates and arithmetic alone
    /// would not:
    ///
    /// - **The width does not change.** Rows are moved into the new buffer
    ///   rather than rebuilt, and their cells are never touched. Measured at
    ///   33 KiB for a 1 200×200 screen gaining a row — where charging it a
    ///   buffer, as an earlier version of this did, would have thrown the
    ///   screen away to save memory that was never going to be spent.
    /// - **It grows past any width this screen has held.** Every row is
    ///   expanded to the new width, and expanding reallocates.
    /// - **It shrinks.** Rows are split with `Vec::split_off`, which leaves
    ///   the vector it splits holding its old capacity and returns a new one
    ///   holding the rest — and the piece left behind is the one emitted, so
    ///   the emitted segments own a descending staircase of capacities that
    ///   sums as a triangle rather than a rectangle. Narrowing by a factor of
    ///   two hundred therefore costs about a hundred times the grid it
    ///   started from. A gentler narrowing is dominated instead by the
    ///   rewrap, which carries each row's remainder forward into the next and
    ///   rebuilds the buffer at the new width, so the larger of the two is
    ///   what a narrowing costs.
    ///
    /// Widths this screen has held, rather than its width now, because a row
    /// narrowed once keeps the room it had — so a screen that has already
    /// narrowed can widen back into that room without paying for it, and a
    /// screen that narrows again splits from the larger figure.
    pub(crate) fn reflow_peak(&self, cols: u16) -> usize {
        let (current_cols, rows) = self.vt.size();
        let (cols, _) = habitable(cols, 1);
        let widest = self.widest_cols.max(current_cols);
        let (cells_per_row, lines_per_row) = if cols > current_cols {
            // Charged whatever this screen has been before. A row narrowed
            // once may still own the room it had, and widening back into that
            // room would cost nothing — but "may" is the whole problem: the
            // width this screen once reached says nothing about the rows it
            // holds now. Rows added by a later change of height never had it,
            // and a narrowing rebuilds rows rather than only truncating them,
            // so some of the rest will have lost it too. Reading a historical
            // maximum as a promise about present capacity is the assumption
            // this bound has already been wrong about twice.
            (cols, 1)
        } else if cols == current_cols {
            (0, 1)
        } else {
            let segments = widest.div_ceil(cols);
            // The staircase, summed exactly rather than walked: `segments`
            // terms starting at `widest` and stepping down by `cols`.
            let staircase = segments * widest - cols * segments * (segments - 1) / 2;
            // Against the rewrap, which rebuilds the buffer at the new width.
            (staircase.max(cols), segments)
        };
        rows * (cells_per_row * size_of::<avt::Cell>() + lines_per_row * size_of::<avt::Line>())
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
        let (_, rows) = self.vt.size();
        // The active buffer's rows are counted at the widest this screen has
        // ever been, not at its width now. Narrowing truncates each row and
        // a truncation keeps the room, so a row that was once wide still
        // owns that much — the emulator's doing, and not something it
        // promises either way, which is why this errs high rather than
        // trying to predict it.
        one_buffer_bytes(self.widest_cols, rows) + self.largest_buffer
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

/// The most a grid of this size can occupy at once, without building one.
///
/// What a session is admitted on. See [`BUFFERS_AT_A_RESET`] for why this is
/// twice what the same terminal holds while nothing is happening to it.
pub(crate) fn projected_grid_peak_bytes(cols: usize, rows: usize) -> usize {
    one_buffer_bytes(cols, rows) * BUFFERS_AT_A_RESET
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
///
/// **Conceal is absent, and its absence is not cosmetic.** A terminal told
/// `ESC[8m` stops showing what follows — it is what a CLI reaches for to
/// keep something off the screen — and this emulator has no state for it, so
/// the text is stored and read back like any other. Concealed output
/// therefore reaches a snapshot, and reaches the reported content that
/// becomes tokens, as though it had been displayed.
///
/// It cannot be fixed at this seam: nothing distinguishes those cells once
/// the emulator has taken them, so there is no flag here to carry and no way
/// to recover one. A fixture test fails if any recorded session ever emits
/// the sequence, which is what turns this from a paragraph into a signal.
///
/// # Intensity is one axis, and that is an emulator property
///
/// `CellIntensity` has room for one of bold, faint or normal, because
/// ECMA-48 defines SGR 1 and SGR 2 as increased and decreased intensity —
/// two ends of a single attribute, not two attributes. This emulator reads
/// them that way: setting either clears the other, so `\x1b[1;2m` leaves
/// faint alone and `\x1b[2;1m` leaves bold alone, and no cell ever reports
/// both. The branch below relies on exactly that, and would drop faint
/// silently if it stopped being true.
///
/// It is worth naming because it is not universal. Emulators that keep the
/// two as independent bits exist, and this component is written to allow the
/// one behind it to be swapped. A test asserts the exclusivity rather than
/// leaving the branch to encode it, so a swap or an upgrade that changes it
/// fails there instead of quietly publishing half a style.
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

    /// Every way of asking for both intensities at once, and what this
    /// emulator settles on for each.
    ///
    /// Separate sequences as well as one parameter list, and both orders of
    /// each, because "the last one wins" and "they cannot coexist" are
    /// different rules that agree on the simplest spellings.
    const BOTH_INTENSITIES: &[(&str, CellIntensity)] = &[
        ("\x1b[1;2m", CellIntensity::Faint),
        ("\x1b[2;1m", CellIntensity::Bold),
        ("\x1b[1m\x1b[2m", CellIntensity::Faint),
        ("\x1b[2m\x1b[1m", CellIntensity::Bold),
        ("\x1b[1;2;1;2m", CellIntensity::Faint),
    ];

    #[test]
    fn the_emulator_never_reports_a_cell_as_both_bold_and_faint() {
        // What the style mapping relies on, asserted where it can fail
        // loudly. `CellIntensity` carries one of the three, following
        // ECMA-48, where SGR 1 and SGR 2 are the two ends of one attribute
        // rather than two attributes — and this emulator agrees, clearing
        // either when the other is set.
        //
        // Asked of the emulator rather than of a snapshot, because a
        // snapshot cannot answer it: the mapping is what collapses the two,
        // so a cell reporting both would arrive here already reduced to one
        // and the test would pass on exactly the state it exists to catch.
        //
        // An emulator holding them as independent bits would make that
        // mapping drop faint without saying so, which is a published style
        // silently losing an attribute. Swapping the emulator is a live
        // possibility here, so the assumption is checked rather than
        // commented.
        for (spelling, _) in BOTH_INTENSITIES {
            let mut vt = avt::Vt::builder().size(4, 1).build();
            vt.feed_str(&format!("{spelling}X"));
            let line = vt.view().next().expect("the grid has a first row");
            let pen = line.cells()[0].pen();
            assert!(
                !(pen.is_bold() && pen.is_faint()),
                "{spelling:?} left the cell both bold and faint, which the published \
                 style has no way to say"
            );
        }
    }

    #[test]
    fn asking_for_both_intensities_keeps_the_one_asked_for_last() {
        // The rule underneath the exclusivity, pinned separately: it is not
        // that one of the two is dropped arbitrarily, it is that the later
        // one replaces the earlier. A change from last-wins to first-wins
        // would keep the test above passing and still change what every
        // snapshot of such a cell says.
        for (spelling, expected) in BOTH_INTENSITIES {
            let mut grid = Grid::new(4, 1);
            grid.feed(&format!("{spelling}X"));
            let cells: Vec<_> = grid.row(0).cells().collect();
            assert_eq!(cells[0].style.intensity, *expected, "{spelling:?}");
        }
    }

    #[test]
    fn a_cell_can_still_be_bold_or_faint_on_its_own() {
        // Without this, both tests above pass on an emulator that has
        // stopped tracking intensity at all.
        for (spelling, expected) in [
            ("\x1b[1m", CellIntensity::Bold),
            ("\x1b[2m", CellIntensity::Faint),
        ] {
            let mut grid = Grid::new(4, 1);
            grid.feed(&format!("{spelling}X"));
            let cells: Vec<_> = grid.row(0).cells().collect();
            assert_eq!(cells[0].style.intensity, expected, "{spelling:?}");
        }
    }

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
