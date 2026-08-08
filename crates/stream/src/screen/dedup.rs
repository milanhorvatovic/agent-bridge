//! Telling new content from content that has only moved or been redrawn.
//!
//! A TUI redraws. It rewrites a row it has already written, often with
//! exactly the same characters, because redrawing the region is cheaper for
//! it than working out which cells actually moved. The emulator faithfully
//! reports every one of those rows as written, and a pipeline that turned
//! each report into an event would emit the same line over and over — the
//! failure this filter exists to prevent.
//!
//! **Position is not enough to recognise it.** The obvious filter — remember
//! what each row last said, and pass a row on only when its text changes —
//! catches a region redrawn in place and misses the far more common case,
//! which is a screen that scrolls. When an interface appends a line, every
//! row's text moves up one, so every row differs from what that row last
//! said while nothing on the screen is new. Measured against the recorded
//! sessions, a position-only filter passed **40 % of its output as new
//! content that had already been emitted**, and worse on the narrow screens
//! that scroll most. The terminal emulator cannot help here either: it
//! reports lines leaving the buffer only on the primary screen, and an
//! interface like this one spends its whole session on the alternate screen,
//! where a scroll produces no signal at all.
//!
//! So the question asked is about the text rather than about the row: has
//! this line been reported recently, anywhere on the screen? Two records
//! answer it together, and both are needed.
//!
//! - **Per row, what it last said.** Never expires, so a header or a border
//!   that is repainted for the whole of a long session is suppressed for the
//!   whole of it.
//! - **Recently reported text, wherever it was.** A bounded window, so a line
//!   that shifts up while the screen scrolls is recognised as the line it
//!   already was — and a line that genuinely comes back much later, long
//!   after it left the screen, is reported again rather than silently
//!   swallowed forever.
//!
//! Digests rather than the text itself: a copy of every row would cost about
//! as much memory as the screen it shadows, and the only question ever asked
//! of it is whether two strings are equal.

use std::collections::VecDeque;
use std::hash::{DefaultHasher, Hash, Hasher};

use super::vt::Grid;

/// Text that has appeared on the screen and had not been reported recently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NovelSpan {
    /// The row it is on, zero-based from the top.
    pub row: u16,
    /// The row's text, with trailing blanks removed — what a reader would
    /// see on that line. Never empty: a row that has been cleared shows up
    /// as damage, because a matcher may care that a dialog is gone, but
    /// emptiness is not content and there is no token in it.
    pub text: String,
}

/// How many recently-reported lines are remembered, as a multiple of the
/// screen's height.
///
/// It has to exceed one screenful, or a line scrolling from the bottom row
/// to the top would age out of the window on the way and be reported twice.
/// Beyond that the only cost of a larger window is how long a genuinely
/// repeated line stays suppressed, so this is deliberately a small multiple
/// rather than an unbounded history.
const RECENT_SCREENFULS: usize = 4;

/// Remembers what has been reported, by row and by content.
#[derive(Debug, Default)]
pub(crate) struct RepaintDedup {
    /// One digest per row, indexed by row. A row nobody has written to is
    /// blank, and blank is what it last said — so a row starts out holding
    /// the digest of the empty string rather than a "not yet known", and a
    /// clear applied to an already-blank row reports nothing.
    seen: Vec<u64>,
    /// Digests of recently reported lines, oldest first. Scanned rather than
    /// hashed into a set: it holds a few hundred integers at most, which is
    /// nothing beside digesting the rows being compared against it, and a
    /// set would need occurrence counts to stay in step with the eviction
    /// order.
    recent: VecDeque<u64>,
}

impl RepaintDedup {
    /// Filters `damaged` down to the rows carrying text that has not been
    /// reported recently, and records what it returns as reported.
    ///
    /// The work is bounded by what was written, not by the size of the
    /// screen: an untouched row costs nothing, and a full repaint costs one
    /// pass over the screen — which the evaluation-point cadence already
    /// spaces out.
    pub(crate) fn novel(&mut self, grid: &Grid, damaged: &[u16]) -> Vec<NovelSpan> {
        self.seen.resize(grid.row_count(), digest(""));
        let capacity = grid.row_count() * RECENT_SCREENFULS;
        let mut novel = Vec::new();
        for &row in damaged {
            let Some(slot) = self.seen.get_mut(usize::from(row)) else {
                // A row the last reflow took away. The emulator reported it
                // before the screen shrank; there is nothing there to read.
                continue;
            };
            let text = grid.row(usize::from(row)).text();
            let digest = digest(&text);
            // The row says what it said before: a redraw in place.
            if *slot == digest {
                continue;
            }
            *slot = digest;
            // Nothing is not content, whatever row it is on.
            if text.is_empty() {
                continue;
            }
            // The line itself was reported recently, somewhere: the screen
            // moved under it.
            if self.recent.contains(&digest) {
                continue;
            }
            self.recent.push_back(digest);
            while self.recent.len() > capacity {
                self.recent.pop_front();
            }
            novel.push(NovelSpan { row, text });
        }
        novel
    }

    /// Takes the screen as it now stands to be what every row last said,
    /// reporting nothing.
    ///
    /// This is what a reflow needs. Re-laying-out a screen moves text between
    /// rows without any of it being new, so leaving the old digests in place
    /// would report the whole screen as fresh content — and dropping them
    /// entirely would do the same on the next repaint. Reading the reflowed
    /// screen back in as the new baseline is the only one of the three that
    /// says what actually happened, which is that nothing was written.
    ///
    /// The recent-text window is left alone: a reflow rewraps lines, so the
    /// text on the screen afterwards is not quite the text that was reported
    /// before it, and what was reported is still what was reported.
    pub(crate) fn rebaseline(&mut self, grid: &Grid) {
        self.seen.clear();
        self.seen
            .extend((0..grid.row_count()).map(|row| digest(&grid.row(row).text())));
    }
}

/// A row's text as one number.
///
/// Two different rows digesting alike would suppress a line that should have
/// been emitted, so the width is what matters here: across a session's worth
/// of repaints on a screen of a hundred rows, a 64-bit collision is many
/// orders of magnitude rarer than the recording drift the fixtures are
/// re-checked for.
fn digest(text: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::{Grid, NovelSpan, RepaintDedup};

    /// Feed `text` and report whatever the filter finds novel.
    fn feed(grid: &mut Grid, dedup: &mut RepaintDedup, text: &str) -> Vec<NovelSpan> {
        let damaged: Vec<u16> = grid
            .feed(text)
            .into_iter()
            .map(|row| u16::try_from(row).expect("a screen has fewer than 65 536 rows"))
            .collect();
        dedup.novel(grid, &damaged)
    }

    fn texts(spans: &[NovelSpan]) -> Vec<&str> {
        spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn the_same_content_painted_many_times_is_reported_once() {
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        assert_eq!(
            texts(&feed(&mut grid, &mut dedup, "\u{1b}[1;1Hstatus line")),
            vec!["status line"]
        );
        for _ in 0..16 {
            assert!(
                feed(&mut grid, &mut dedup, "\u{1b}[1;1Hstatus line").is_empty(),
                "a repaint of identical text is not new content"
            );
        }
    }

    #[test]
    fn a_row_that_really_changes_is_reported_again() {
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        feed(&mut grid, &mut dedup, "\u{1b}[1;1Hworking");
        assert_eq!(
            texts(&feed(&mut grid, &mut dedup, "\u{1b}[2K\u{1b}[1;1Hdone")),
            vec!["done"]
        );
    }

    #[test]
    fn each_row_is_remembered_separately() {
        // A spinner on one row must not suppress a message arriving on
        // another, and the message must not make the spinner interesting.
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        feed(&mut grid, &mut dedup, "\u{1b}[1;1Hspinning\u{1b}[2;1Hquiet");
        let spans = feed(
            &mut grid,
            &mut dedup,
            "\u{1b}[1;1Hspinning\u{1b}[2;1H\u{1b}[2Knews",
        );
        assert_eq!(
            spans,
            vec![NovelSpan {
                row: 1,
                text: "news".to_owned()
            }]
        );
    }

    #[test]
    fn the_same_line_arriving_on_another_row_is_not_new_content() {
        // The scroll, in miniature, and the case a position-only filter gets
        // wrong: the words moved, nothing was said.
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        feed(&mut grid, &mut dedup, "\u{1b}[1;1Hhello");
        assert!(feed(&mut grid, &mut dedup, "\u{1b}[3;1Hhello").is_empty());
    }

    #[test]
    fn a_scrolling_stream_reports_each_line_once() {
        // Twelve lines through a four-row screen, examined after every one,
        // which is the cadence that exposes the problem: each line is
        // written to the bottom row and then shifts up through all four.
        let mut grid = Grid::new(20, 4);
        let mut dedup = RepaintDedup::default();
        let mut reported = Vec::new();
        for line in 1..=12 {
            for span in feed(&mut grid, &mut dedup, &format!("line{line}\r\n")) {
                reported.push(span.text);
            }
        }
        let expected: Vec<String> = (1..=12).map(|line| format!("line{line}")).collect();
        assert_eq!(reported, expected);
    }

    #[test]
    fn a_line_that_returns_long_after_leaving_the_screen_is_new_again() {
        // The window is bounded so that a repeat far enough apart is still
        // an event. Anything else would suppress a line for the whole of a
        // session because it once appeared.
        let mut grid = Grid::new(20, 4);
        let mut dedup = RepaintDedup::default();
        assert_eq!(
            texts(&feed(&mut grid, &mut dedup, "once\r\n")),
            vec!["once"]
        );
        for line in 0..64 {
            feed(&mut grid, &mut dedup, &format!("filler{line}\r\n"));
        }
        assert_eq!(
            texts(&feed(&mut grid, &mut dedup, "once\r\n")),
            vec!["once"],
            "a line long gone from the window is content again when it returns"
        );
    }

    #[test]
    fn a_row_going_blank_is_damage_rather_than_content() {
        // A dismissed dialog is worth examining — the row is reported as
        // damaged — but there is no token in emptiness.
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        feed(&mut grid, &mut dedup, "\u{1b}[1;1HProceed?");
        assert!(feed(&mut grid, &mut dedup, "\u{1b}[2K").is_empty());
    }

    #[test]
    fn a_blank_row_that_was_never_written_is_not_reported_as_going_blank() {
        // Damage without a change: the emulator reports a row that was
        // written with spaces, and it was already blank.
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        assert!(feed(&mut grid, &mut dedup, "\u{1b}[1;1H\u{1b}[2K").is_empty());
    }

    #[test]
    fn a_style_change_alone_is_not_new_content() {
        // Highlighting a menu row changes how it is drawn, not what it says,
        // and nothing new should be emitted for it. The screen still records
        // the change — a matcher reading the grid sees the highlight.
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        feed(&mut grid, &mut dedup, "\u{1b}[1;1H1. Yes");
        assert!(feed(&mut grid, &mut dedup, "\u{1b}[1;1H\u{1b}[7m1. Yes\u{1b}[0m").is_empty());
    }

    #[test]
    fn rebaselining_takes_the_screen_as_it_stands_and_reports_nothing() {
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        // Text that arrived without the filter ever being told about it —
        // which is what a reflow produces.
        grid.feed("\u{1b}[1;1Harrived during the reflow");
        dedup.rebaseline(&grid);
        assert!(
            feed(
                &mut grid,
                &mut dedup,
                "\u{1b}[1;1Harrived during the reflow"
            )
            .is_empty(),
            "a repaint of the rebaselined text is not new"
        );
        assert_eq!(
            texts(&feed(
                &mut grid,
                &mut dedup,
                "\u{1b}[2K\u{1b}[1;1Hthen this"
            )),
            vec!["then this"],
            "and what comes after it still is"
        );
    }

    #[test]
    fn a_row_the_screen_no_longer_has_is_skipped_rather_than_panicking() {
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        feed(&mut grid, &mut dedup, "\u{1b}[20;1Hlow down the screen");
        grid.resize(80, 10);
        assert!(dedup.novel(&grid, &[19]).is_empty());
    }
}
