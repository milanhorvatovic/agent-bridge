//! Telling a repaint from a change.
//!
//! A TUI redraws. It rewrites a row it has already written, often with
//! exactly the same characters, because redrawing the region is cheaper for
//! it than working out which cells actually moved. The emulator faithfully
//! reports every one of those rows as written, and a pipeline that turned
//! each report into an event would emit the same line over and over — the
//! failure this filter exists to prevent.
//!
//! So each row is remembered by a digest of its text as of the last time it
//! was reported, and a row whose text still digests the same has nothing new
//! to say. Digests rather than the text itself: a copy of every row would
//! cost about as much memory as the screen it shadows, and the only question
//! being asked of it is whether two strings are equal.

use std::hash::{DefaultHasher, Hash, Hasher};

use super::vt::Grid;

/// Text that appeared on a row and had not appeared there before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NovelSpan {
    /// The row it appeared on, zero-based from the top.
    pub row: u16,
    /// The row's text, with trailing blanks removed — what a reader would
    /// see on that line.
    pub text: String,
}

/// Remembers what each row last said.
#[derive(Debug, Default)]
pub(crate) struct RepaintDedup {
    /// One digest per row, indexed by row. A row nobody has written to is
    /// blank, and blank is what it last said — so a row starts out holding
    /// the digest of the empty string rather than a "not yet known", and a
    /// clear applied to an already-blank row reports nothing.
    seen: Vec<u64>,
}

impl RepaintDedup {
    /// Filters `damaged` down to the rows whose text actually differs from
    /// the last report, and records the new text as reported.
    ///
    /// The work is bounded by what was written, not by the size of the
    /// screen: an untouched row costs nothing, and a full repaint costs one
    /// pass over the screen — which the evaluation-point cadence already
    /// spaces out.
    ///
    /// A row that has just been emptied is a change like any other and comes
    /// back with empty text: something that was on the screen is no longer
    /// there, which a dialog being dismissed looks exactly like.
    pub(crate) fn novel(&mut self, grid: &Grid, damaged: &[u16]) -> Vec<NovelSpan> {
        self.seen.resize(grid.row_count(), digest(""));
        let mut novel = Vec::new();
        for &row in damaged {
            let Some(slot) = self.seen.get_mut(usize::from(row)) else {
                // A row the last reflow took away. The emulator reported it
                // before the screen shrank; there is nothing there to read.
                continue;
            };
            let text = grid.row(usize::from(row)).text();
            let digest = digest(&text);
            if *slot == digest {
                continue;
            }
            *slot = digest;
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
    fn the_same_text_on_a_different_row_is_new_there() {
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        feed(&mut grid, &mut dedup, "\u{1b}[1;1Hhello");
        assert_eq!(
            texts(&feed(&mut grid, &mut dedup, "\u{1b}[3;1Hhello")),
            vec!["hello"]
        );
    }

    #[test]
    fn text_that_comes_back_after_being_cleared_is_new_again() {
        // The dialog case: a prompt is painted, dismissed, and painted again.
        // The second painting is a real event even though the words match.
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        feed(&mut grid, &mut dedup, "\u{1b}[1;1HProceed?");
        assert_eq!(texts(&feed(&mut grid, &mut dedup, "\u{1b}[2K")), vec![""]);
        assert_eq!(
            texts(&feed(&mut grid, &mut dedup, "\u{1b}[1;1HProceed?")),
            vec!["Proceed?"]
        );
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
