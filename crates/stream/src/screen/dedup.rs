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

use std::collections::{HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};

use super::vt::Grid;

/// Text that has appeared on the screen and had not been reported recently.
///
/// Unlike the screen it came from, this **is** content and its [`Debug`]
/// says so — carrying the text is the whole point of the type. A caller that
/// logs one is logging CLI output, which default logs do not carry; that is
/// a decision for whoever holds it, made knowingly, not something to fall
/// into by printing a handle.
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
    /// Digests of recently reported lines, oldest first — the eviction order.
    ///
    /// Paired with a set rather than scanned, because the window is sized
    /// from the screen's height and the height comes from a caller. A
    /// 15 × 7 000 terminal is the tallest the memory bound admits and still
    /// gives a window of 28 000 digests to check every one of 7 000 rows
    /// against — seconds of comparisons for a handful of repaints, on a
    /// screen the bound lets in. (It was 15 × 12 000 when this was written,
    /// which the bound stopped admitting once it began covering the buffer
    /// replacements an ordinary session performs; the shape moved and the
    /// argument did not.)
    recent: VecDeque<u64>,
    /// The same digests, for asking whether one is in the window without
    /// walking it. No occurrence counts needed: a digest is only ever pushed
    /// when it is *absent*, so the window holds each at most once and the two
    /// stay in step by construction.
    in_recent: HashSet<u64>,
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
        // Trim to the current screen before looking at anything, not only
        // after something new goes in. A reflow to fewer rows shrinks the
        // window, and a reflow where every damaged line is already known
        // never reaches the insert — so a window sized for the tallest the
        // session has ever been would otherwise persist, holding memory for
        // a screen that no longer exists and suppressing lines that should
        // have aged out of it.
        let capacity = grid.row_count() * RECENT_SCREENFULS;
        self.trim_to(capacity);
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
            if self.in_recent.contains(&digest) {
                continue;
            }
            self.recent.push_back(digest);
            self.in_recent.insert(digest);
            self.trim_to(capacity);
            novel.push(NovelSpan { row, text });
        }
        novel
    }

    /// Roughly how much memory this holds, in bytes.
    ///
    /// Not a rounding error beside the grid, which is why it is counted:
    /// the window is four entries per row and lives in two structures, so on
    /// a screen that is mostly rows it can outweigh the cells it shadows.
    /// Reported from what is actually allocated rather than from what is
    /// occupied — a container that grew and then shrank is still holding the
    /// memory.
    pub(crate) fn footprint(&self) -> usize {
        let per_slot = size_of::<u64>();
        self.seen.capacity() * per_slot
            + self.recent.capacity() * per_slot
            + hash_set_bytes(self.in_recent.capacity())
    }

    /// Takes the screen's new height, and gives back what the old one needed.
    ///
    /// Both halves matter and the order is the whole point. Shrinking a
    /// container only releases what is past its *length*, so asking for it
    /// while the records still hold a taller screen's worth of entries
    /// releases nothing at all — the lengths have to come down first. Doing
    /// that lazily at the next evaluation is too late, because the bound is
    /// checked in between: the capacity a tall screen justified would sit
    /// uncounted by any projection of the short one that replaced it, and
    /// the session would hold more than it was admitted under.
    pub(crate) fn reshape(&mut self, rows: usize) {
        self.seen.resize(rows, digest(""));
        self.trim_to(rows * RECENT_SCREENFULS);
        self.seen.shrink_to_fit();
        self.recent.shrink_to_fit();
        self.in_recent.shrink_to_fit();
    }

    /// Drops the oldest entries until the window is no larger than `capacity`.
    fn trim_to(&mut self, capacity: usize) {
        while self.recent.len() > capacity {
            if let Some(evicted) = self.recent.pop_front() {
                self.in_recent.remove(&evicted);
            }
        }
    }
}

/// What the filter costs for a screen of `rows` rows, without building one.
///
/// Four entries per row, held twice — once in eviction order and once for
/// membership — plus one digest per row for what it last said.
pub(crate) fn projected_bytes(rows: usize) -> usize {
    let window = rows * RECENT_SCREENFULS;
    // Rounded up to what a growable container actually asks the allocator
    // for. A vector and a deque both grow by doubling, so each holds a power
    // of two of slots rather than exactly what was put in it, and a
    // projection that counts the entries is short by up to half of the
    // allocation every time. Estimating these exactly has been tried and is
    // a losing game — the growth policy is an implementation detail of the
    // standard library — so the projection rounds the way the containers do
    // and errs high, which is the direction a number that admits a session
    // has to err in.
    grown(rows) * size_of::<u64>() + grown(window) * size_of::<u64>() + hash_set_bytes(window)
}

/// How many slots a growable container holds once it has been filled to
/// `len`, given that it doubles.
fn grown(len: usize) -> usize {
    if len == 0 { 0 } else { len.next_power_of_two() }
}

/// What a hash set holding `capacity` digests actually allocates.
///
/// `capacity` is how many more can go in before it grows, not how much is
/// allocated: the table keeps a power-of-two bucket array sized to stay
/// under seven-eighths full, plus a control byte for every bucket including
/// the empty ones. Counting a slot per element understates that by up to
/// something over twice, which is the wrong direction for a number that
/// decides whether a session is affordable.
fn hash_set_bytes(capacity: usize) -> usize {
    if capacity == 0 {
        return 0;
    }
    // Ceiling division, and no rounding up beyond it: a live capacity is
    // already seven-eighths of a power of two, so landing exactly on the
    // boundary and then adding one would name the next size up and report
    // twice the memory that is there. The same expression has to be right
    // for a count of elements somebody wants and for a capacity a table
    // already has, because both are asked of it.
    let buckets = (capacity * 8).div_ceil(7).next_power_of_two();
    buckets * (size_of::<u64>() + 1)
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
    fn text_the_filter_was_never_shown_is_still_content_when_it_is_repainted() {
        // What a reflow leaves behind: rows the grid changed without the
        // filter being asked about them. Declaring the screen already-said
        // at that moment — the tidy-looking thing to do on a resize — throws
        // this away, and the repaint that would have carried it too.
        let mut grid = Grid::new(80, 24);
        let mut dedup = RepaintDedup::default();
        grid.feed("\u{1b}[1;1Hwritten while nobody was asking");
        assert_eq!(
            texts(&feed(
                &mut grid,
                &mut dedup,
                "\u{1b}[1;1Hwritten while nobody was asking"
            )),
            vec!["written while nobody was asking"],
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
