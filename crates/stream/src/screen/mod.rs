//! The reconstructed screen: what a terminal attached to the CLI would show.
//!
//! Some CLIs do not stream text, they draw. Claude Code's interactive mode is
//! one: an Ink-rendered interface that positions the cursor, repaints
//! regions, and redraws whole lines as it goes. Read as a stream of lines,
//! that output is nearly meaningless — the same words arrive several times,
//! a menu-rendered prompt never appears as a line at all, and the width the
//! session happens to have changes where the text breaks. Read as a screen,
//! it is exactly what a person sitting at the terminal would see.
//!
//! So the bytes are interpreted twice, in parallel and for different
//! purposes: once stripped of control sequences for the text a caller reads,
//! and once into the grid here for the structure a matcher needs — cursor
//! position, region contents, what a paint left behind once it finished.
//! This is the fallback route to understanding a session, not the preferred
//! one; where a CLI will tell the runtime what it is doing through a channel
//! of its own, that account beats any inference drawn from what it drew. What
//! the grid answers is everything no such channel covers: a first-run trust
//! dialog, a login prompt, a permission dialog that fell back to the
//! interface, whether anything is being painted at all, and what the screen
//! looked like when a caller reconnects and finds its history gone.
//!
//! # Feeding and rendering are separate
//!
//! Bytes go in continuously; snapshots come out on request. That split is the
//! design, not an optimization: feeding is proportional to the bytes and
//! happens on every read, while rendering walks the whole grid and builds an
//! owned structure, and there are only three moments that need one — a caller
//! reconnecting, an operator inspecting a live session, and a matcher
//! examining the screen at an [evaluation point](EvalPointScheduler). A run
//! that only
//! feeds materializes nothing at all, and [`ScreenState::renders`] is there
//! so a test can hold this crate to that.
//!
//! # What a screen cannot tell you
//!
//! A screen holds what is visible, and it is examined at intervals. Both of
//! those bound what it can report, and a consumer that mistakes it for a
//! transcript will quietly lose content:
//!
//! - **Text that arrives and scrolls off between two evaluation points was
//!   never on the screen when anyone looked, and is not reported at all.**
//!   Sampling cannot see what passed between samples, and this buffer keeps
//!   no scrollback to go back to. Where a CLI offers an account of its own
//!   output, that account is the one to read; this is the fallback for the
//!   parts no such account covers.
//! - A line reported once is not reported again while it is still recent,
//!   even if the CLI genuinely printed it twice — the cost of recognising
//!   the same line after it has moved up the screen. [`NovelSpan`] says how
//!   far "recent" reaches.
//! - **A reshape too expensive to perform starts an empty screen.** A
//!   reflow costs more while it runs than either shape costs settled, and
//!   past [`LARGEST_SCREEN_BYTES`] the screen is rebuilt at the new size
//!   rather than reflowed — so what was showing is gone, and every row comes
//!   back as damage. It takes an extreme change of shape to reach, and a
//!   reflow that extreme discards nearly everything anyway: with no
//!   scrollback, a reshape that multiplies the rows keeps only the last
//!   screenful of them.
//! - **A session may keep no screen even having asked for one.** A terminal
//!   past [`LARGEST_SCREEN_BYTES`] is refused rather than allocated, so
//!   [`ScreenState::is_kept`] is the question to ask, not whether the setting
//!   was on.
//!
//! # Sessions that do not need it pay nothing
//!
//! The grid costs memory per session and parses every byte, which is worth it
//! for a CLI that draws and worth nothing for one that prints lines. Whether
//! to keep one is therefore a per-session decision — an adapter's setting
//! where it has one, the runtime-wide default otherwise — taken once, at
//! construction. A session without one holds no grid, does no work on feed,
//! and has no snapshot to give: [`ScreenState::render`] returns `None`, and
//! a reconnecting caller's payload leaves `screen_snapshot` out — the field
//! is absent, not present and null.

mod dedup;
mod sched;
mod snapshot;
mod text;
mod vt;

use agent_bridge_events::ScreenSnapshot;

use dedup::RepaintDedup;
use text::Decoder;
use vt::Grid;

pub use dedup::NovelSpan;
pub use sched::{EvalPointScheduler, EvalTrigger, QUIET_PERIOD};

/// The most memory one session's screen may hold, in bytes.
///
/// A screen's cost comes from a caller: a runtime's caller is in the same
/// trust domain but is not trusted *input*, and a buggy client asking for
/// 65 535 × 65 535 asks for tens of gibibytes — an allocation that takes the
/// process down, and every other session with it.
///
/// **The bound is bytes rather than cells because the cost is not
/// proportional to cells.** Every row carries a fixed overhead whatever its
/// width — a vector header for the cells, and the repaint filter's four
/// entries per row — so a screen can be small by area and enormous in
/// memory. A bound of a million cells looks generous and admits 15 × 65 535,
/// which is under it by area and costs 37 MiB: more across the session cap
/// than the whole runtime is sized for. Projecting the cost and comparing
/// that closes the gap the shape opened.
///
/// Eight mebibytes is an order of magnitude above the largest screen anyone
/// has in front of them — a very large real terminal is some 500 × 150, at
/// 2.4 MiB — and leaves the worst case across a full session cap in the same
/// order as the runtime's own resident target.
///
/// Past it the session keeps **no screen**, rather than a trimmed one.
/// Trimming would be the quiet mistake: the terminal the CLI is drawing into
/// really is the size that was asked for, so a smaller grid would reconstruct
/// a screen that never existed and hand it to a matcher as fact. Keeping none
/// is a state the whole system already handles — it is what a line-oriented
/// session does all day, and it reaches a caller the same way: with the
/// `screen_snapshot` field simply absent.
///
/// This is a backstop, not the fix. Dimensions should be rejected where a
/// session's parameters are validated, with an error the caller can read;
/// this only makes the unrejected case survivable.
pub const LARGEST_SCREEN_BYTES: usize = 8 * 1024 * 1024;

/// The most the reused decode buffer keeps between feeds.
///
/// Decoding into one buffer and reusing it saves an allocation on every
/// read, and a session's reads are small. The buffer follows whatever the
/// largest single feed was, though, and `feed` takes a slice of any length —
/// so without a ceiling one oversized call would have a session holding that
/// much for the rest of its life, uncounted by anything projecting cost from
/// the screen's dimensions. Sixty-four kibibytes is the figure the runtime
/// budgets for a session's terminal read buffer, which is the size of feed
/// this is built for; past it the buffer is handed back rather than kept.
const RETAINED_DECODE_BYTES: usize = 64 * 1024;

/// What a screen of this size would cost, before one is built.
///
/// Deliberately the same accounting [`ScreenState::footprint`] reports, so
/// the number a session is refused on and the number it is later measured
/// against cannot drift apart.
fn projected_footprint(cols: u16, rows: u16) -> usize {
    let (cols, rows) = (usize::from(cols.max(1)), usize::from(rows.max(1)));
    vt::projected_grid_bytes(cols, rows) + everything_but_the_grid(rows)
}

/// What a screen of this height costs apart from its cells.
fn everything_but_the_grid(rows: usize) -> usize {
    dedup::projected_bytes(rows) + rows + RETAINED_DECODE_BYTES
}

/// What examining the screen at an evaluation point found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evaluation {
    /// The rows written to since the last evaluation point, in order.
    ///
    /// Written to, which is not the same as different: an identical repaint
    /// appears here. It is the right list for a matcher deciding which rows
    /// are worth examining, because a row redrawn in a new colour says
    /// nothing new in [`novel`](Self::novel) and may still be the change a
    /// matcher is waiting for.
    pub damaged: Vec<u16>,
    /// The rows carrying text that has not been reported recently — what may
    /// be emitted without saying the same thing twice.
    ///
    /// A subset of [`damaged`](Self::damaged) by row, but not a filter on it
    /// by row alone: a line that has only moved up the screen was written to
    /// a row it was never on, and is still the line it already was. See
    /// [`NovelSpan`].
    pub novel: Vec<NovelSpan>,
}

/// The screen buffer for one session.
///
/// Construct it with the session's effective setting; everything else follows
/// from that one decision.
///
/// **Its [`Debug`] deliberately shows no content.** What this holds is
/// whatever the CLI has drawn — prompts, file contents, an API key someone
/// echoed — and default logs do not carry CLI output. A derived `Debug` would
/// put the whole screen into a log line the moment anyone wrote
/// `tracing::debug!(?screen)`, here or in a struct further up that derives
/// `Debug` and happens to contain one. Reading the screen is what
/// [`render`](Self::render) is for, and that is a call somebody makes on
/// purpose.
pub struct ScreenState {
    /// `None` for a session that keeps no screen. The absence is the whole
    /// mechanism: there is no grid to feed, so there is no per-byte cost to
    /// skip and no branch that could be got wrong twice.
    kept: Option<Screen>,
}

impl std::fmt::Debug for ScreenState {
    /// Shape and counters, never content — see the type's documentation.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut shown = formatter.debug_struct("ScreenState");
        match self.kept.as_ref() {
            None => shown.field("kept", &false),
            Some(screen) => {
                let (cols, rows) = screen.grid.size();
                shown
                    .field("kept", &true)
                    .field("cols", &cols)
                    .field("rows", &rows)
                    .field("renders", &screen.renders)
                    .field(
                        "damaged_rows",
                        &screen.damaged.iter().filter(|flag| **flag).count(),
                    )
            }
        }
        .finish()
    }
}

/// Everything a session that keeps a screen needs.
struct Screen {
    grid: Grid,
    decoder: Decoder,
    dedup: RepaintDedup,
    /// Which rows have been written to since the last evaluation point, one
    /// flag per row. A set of indices would allocate on the first write of
    /// every burst; a flag per row is a hundred bytes that never move.
    damaged: Vec<bool>,
    /// How many snapshots have been materialized. The witness for the
    /// feed/render split — a pure feed run leaves it at zero.
    renders: u64,
    /// Reused across feeds so a steady stream of small reads does not
    /// allocate a decode buffer each time.
    decoded: String,
}

impl ScreenState {
    /// The setting that applies to a session: the adapter's where it has one,
    /// the runtime-wide default otherwise.
    ///
    /// The runtime-wide default is off, because most of what a v1 caller runs
    /// prints lines; an adapter for a CLI that draws turns it on for its own
    /// sessions and leaves everyone else's alone.
    pub fn effective_tui_aware(runtime_default: bool, adapter_override: Option<bool>) -> bool {
        adapter_override.unwrap_or(runtime_default)
    }

    /// A screen for a session, or the do-nothing handle when the session
    /// keeps none.
    ///
    /// A screen is allocated in proportion to the size asked for, and the
    /// size comes from a caller. Past [`LARGEST_SCREEN_BYTES`] this keeps no
    /// screen at all rather than the memory: see that constant for why
    /// refusing beats both allocating and trimming, and why the bound counts
    /// bytes rather than cells.
    pub fn new(cols: u16, rows: u16, tui_aware_effective: bool) -> Self {
        let projected = projected_footprint(cols, rows);
        let affordable = projected <= LARGEST_SCREEN_BYTES;
        if tui_aware_effective && !affordable {
            tracing::warn!(
                cols,
                rows,
                projected,
                limit = LARGEST_SCREEN_BYTES,
                "terminal too large to reconstruct; this session keeps no screen"
            );
        }
        Self {
            kept: (tui_aware_effective && affordable).then(|| {
                let grid = Grid::new(cols, rows);
                Screen {
                    // The grid's own count, not the requested one: a caller
                    // may ask for a screen with no rows, and the grid it gets
                    // has the one row it takes to be a screen at all.
                    damaged: vec![false; grid.row_count()],
                    grid,
                    decoder: Decoder::default(),
                    dedup: RepaintDedup::default(),
                    renders: 0,
                    decoded: String::new(),
                }
            }),
        }
    }

    /// Whether this session keeps a screen at all.
    ///
    /// Ask this rather than assuming the setting decided it. A session can
    /// have asked for a screen and not have one — a terminal past
    /// [`LARGEST_SCREEN_BYTES`] is refused rather than allocated — and one
    /// that has a screen can lose it, if it is later resized past that bound.
    pub fn is_kept(&self) -> bool {
        self.kept.is_some()
    }

    /// Interprets output bytes into the screen.
    ///
    /// Proportional to the bytes and nothing more — no snapshot is built
    /// here, at any size of input. Bytes may be cut anywhere, including
    /// through the middle of a character.
    pub fn feed(&mut self, bytes: &[u8]) {
        let Some(screen) = self.kept.as_mut() else {
            return;
        };
        screen.decoded.clear();
        screen.decoder.push(bytes, &mut screen.decoded);
        if !screen.decoded.is_empty() {
            for row in screen.grid.feed(&screen.decoded) {
                if let Some(flag) = screen.damaged.get_mut(row) {
                    *flag = true;
                }
            }
        }
        // After the grid has taken it, not before. Checking on the way in
        // releases the buffer an *earlier* feed grew, and leaves whatever
        // this one grew held until a next feed arrives — which for the last
        // feed of a session is never, so an oversized final call would have
        // it holding that much for good. Replaced rather than shrunk: a
        // shrink follows the length, and the length is what was just
        // decoded.
        if screen.decoded.capacity() > RETAINED_DECODE_BYTES {
            screen.decoded = String::new();
        }
    }

    /// The screen as it stands, for a caller reconnecting or an operator
    /// looking.
    ///
    /// `None` when the session keeps no screen. That is where the absence a
    /// reconnecting caller sees is decided: the payload omits
    /// `screen_snapshot` entirely rather than carrying it as null, so there
    /// is one spelling of "there is no screen" rather than two.
    pub fn render(&mut self) -> Option<ScreenSnapshot> {
        let screen = self.kept.as_mut()?;
        screen.renders += 1;
        tracing::debug!(renders = screen.renders, "materializing a screen snapshot");
        Some(snapshot::render(&screen.grid))
    }

    /// Examines the screen: which rows were written to, and which of them say
    /// something that has not been said before.
    ///
    /// Call it at an evaluation point — see [`EvalPointScheduler`]. Between
    /// them the damage accumulates, so calling it more often costs more calls
    /// and no more information.
    pub fn evaluate(&mut self) -> Evaluation {
        let Some(screen) = self.kept.as_mut() else {
            return Evaluation::default();
        };
        let damaged: Vec<u16> = screen
            .damaged
            .iter_mut()
            .enumerate()
            .filter_map(|(row, flag)| {
                std::mem::take(flag)
                    .then(|| u16::try_from(row).expect("a screen has fewer than 65 536 rows"))
            })
            .collect();
        let novel = screen.dedup.novel(&screen.grid, &damaged);
        Evaluation { damaged, novel }
    }

    /// Reflows the screen to a new size.
    ///
    /// The rows a reflow rearranges come back as damage, so a matcher gets to
    /// look again at a screen that now breaks in different places. Almost
    /// none of it is new content, and nothing here has to arrange that: text
    /// that has already been reported is recognised wherever the reflow put
    /// it, by the same window that recognises a line which has scrolled.
    ///
    /// Taking the reflowed screen as a fresh baseline instead — declaring
    /// every row already-said — looks tidier and loses data. Output written
    /// in the moments before a resize has not been reported yet, and a
    /// baseline would swallow it along with everything else, including the
    /// repaint the CLI sends when it learns of the new size. What a reflow
    /// does re-report is a line it rewrapped, because that line is now
    /// genuinely different text.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        // A session can be grown after it starts, so the size a caller
        // supplies has to be checked here too — and past the bound the screen
        // goes rather than growing. It does not come back if the terminal
        // shrinks again: a session that asked for a terminal that size has
        // left the envelope this component is built for, and quietly
        // resuming would give a caller a screen with a hole in its history
        // where the oversized period was.
        // What this grid would cost after the reflow, not what a fresh one
        // of that size would: the parked buffer stays as large as it has
        // ever been, so a wide screen becoming a tall one can be two shapes
        // each affordable alone and not affordable together.
        let projected = self.kept.as_ref().map_or_else(
            || projected_footprint(cols, rows),
            |screen| {
                screen.grid.projected_after_resize(cols, rows)
                    + everything_but_the_grid(usize::from(rows.max(1)))
            },
        );
        if projected > LARGEST_SCREEN_BYTES {
            if self.kept.take().is_some() {
                tracing::warn!(
                    cols,
                    rows,
                    limit = LARGEST_SCREEN_BYTES,
                    "terminal resized past what can be reconstructed; dropping this \
                     session's screen"
                );
            }
            return;
        }
        let Some(screen) = self.kept.as_mut() else {
            return;
        };
        tracing::debug!(cols, rows, "reflowing the screen");
        // Before the grid grows, not after. A session going from tall to
        // wide builds the new grid while still holding the old shape's
        // records, so releasing them second means both exist at once and the
        // peak passes a bound the settled figure would have met.
        screen.dedup.reshape(usize::from(rows.max(1)));
        screen.decoded = String::new();
        // A reflow can cost far more while it runs than either shape costs
        // settled, and the projection above only judges the shapes. Every row
        // the buffer holds is transformed to the new width and all of them
        // are collected before the ones past the bottom are dropped, so the
        // peak is the old row count at the new width — thirty-one times the
        // bound for a wide screen being narrowed, eleven for a tall one being
        // widened, in both cases on the way to a shape that was affordable.
        //
        // Rebuilding instead is the cheaper half of a bad trade, and in the
        // cases that reach it the trade is nearly free: this screen has no
        // scrollback, so a reshape that multiplies the rows discards all but
        // the last screenful anyway. What survives a reflow like that is a
        // tail of what was showing, and what survives this is nothing.
        // Everything live at once, not the reflow's share of it. The parked
        // buffer stays where it is throughout and the bookkeeping does too,
        // so a screen sitting near the bound can afford no reflow at all
        // while a reflow considered by itself looks affordable — which is
        // how a 1 200×200 screen narrowing to 1 000×200 reached 10.4 MiB
        // with every part of the sum inside the limit.
        let held = screen.grid.footprint()
            + screen.dedup.footprint()
            + screen.damaged.capacity()
            + screen.decoded.capacity();
        let reflowed = if held + screen.grid.reflow_peak(cols) > LARGEST_SCREEN_BYTES {
            tracing::warn!(
                cols,
                rows,
                limit = LARGEST_SCREEN_BYTES,
                "terminal reshaped too far to reflow within the memory bound; \
                 starting an empty screen at the new size"
            );
            screen.grid.rebuild(cols, rows);
            (0..screen.grid.row_count()).collect()
        } else {
            screen.grid.resize(cols, rows)
        };
        // Rows already waiting to be examined stay waiting. The emulator in
        // use happens to report every row as changed on any resize, which
        // would make re-deriving the whole set from `reflowed` come out the
        // same — but that is its choice and not a promise, and the guarantee
        // above is that output written before a resize survives it. Keeping
        // the flags makes that true here rather than true elsewhere.
        screen.damaged.resize(screen.grid.row_count(), false);
        screen.damaged.shrink_to_fit();
        for row in reflowed {
            if let Some(flag) = screen.damaged.get_mut(row) {
                *flag = true;
            }
        }
    }

    /// How many snapshots this session has materialized.
    pub fn renders(&self) -> u64 {
        self.kept.as_ref().map_or(0, |screen| screen.renders)
    }

    /// Roughly how much memory the screen holds, in bytes.
    ///
    /// An accounting rather than a measurement, and one that counts
    /// everything a session keeps rather than only the obvious part. The
    /// grid dominates on a screen shaped like a screen — but the repaint
    /// filter's window is four entries per *row*, in two structures, so on
    /// a tall narrow one it outweighs the cells it shadows. Counting the
    /// grid alone reported a fraction of the truth for exactly the
    /// dimensions this component already has to defend itself against.
    ///
    /// A runtime that budgets memory per session and caps how many it runs
    /// is the caller this exists for, which is why it errs toward counting
    /// too much: the decode buffer and the damage flags are small and are
    /// included anyway, and every container reports what it has *allocated*
    /// rather than what it currently holds.
    pub fn footprint(&self) -> usize {
        self.kept.as_ref().map_or(0, |screen| {
            screen.grid.footprint()
                + screen.dedup.footprint()
                + screen.damaged.capacity()
                + screen.decoded.capacity()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Evaluation, LARGEST_SCREEN_BYTES, NovelSpan, RETAINED_DECODE_BYTES, ScreenState};

    /// Feed a whole string, the way a single read would deliver it.
    fn feed(screen: &mut ScreenState, text: &str) {
        screen.feed(text.as_bytes());
    }

    #[test]
    fn an_adapter_setting_wins_and_the_runtime_default_applies_otherwise() {
        assert!(ScreenState::effective_tui_aware(false, Some(true)));
        assert!(!ScreenState::effective_tui_aware(true, Some(false)));
        assert!(ScreenState::effective_tui_aware(true, None));
        assert!(!ScreenState::effective_tui_aware(false, None));
    }

    #[test]
    fn a_session_that_keeps_no_screen_holds_nothing_and_offers_nothing() {
        let mut screen = ScreenState::new(80, 24, false);
        assert!(!screen.is_kept());
        assert_eq!(screen.footprint(), 0, "no grid is allocated");
        feed(&mut screen, "\u{1b}[1;1Hanything at all");
        assert_eq!(
            screen.render(),
            None,
            "and the payload leaves the field out rather than nulling it"
        );
        assert_eq!(screen.evaluate(), Evaluation::default());
        screen.resize(120, 40);
        assert_eq!(screen.renders(), 0);
    }

    #[test]
    fn a_session_that_keeps_one_has_a_snapshot_of_what_was_drawn() {
        let mut screen = ScreenState::new(80, 24, true);
        assert!(screen.is_kept());
        feed(&mut screen, "\u{1b}[2;3Hhello");
        let snapshot = screen.render().expect("a kept screen renders");
        assert_eq!((snapshot.cols, snapshot.rows), (80, 24));
        assert_eq!(snapshot.cursor.row, 1);
        let text: String = snapshot.cells[1].iter().map(|cell| cell.ch).collect();
        assert_eq!(text, "  hello");
    }

    #[test]
    fn feeding_alone_materializes_no_snapshots() {
        // The feed/render split, stated as a number. Anything that starts
        // building snapshots during a feed shows up here.
        let mut screen = ScreenState::new(200, 100, true);
        for _ in 0..500 {
            feed(
                &mut screen,
                "\u{1b}[1;1Hpainting\u{1b}[2;1Hand painting\r\n",
            );
        }
        assert_eq!(screen.renders(), 0);
        screen.render();
        assert_eq!(screen.renders(), 1);
    }

    #[test]
    fn a_character_split_between_two_feeds_still_reaches_the_screen() {
        let mut screen = ScreenState::new(80, 24, true);
        let arrow = "❯ Yes".as_bytes();
        screen.feed(&arrow[..2]);
        screen.feed(&arrow[2..]);
        let snapshot = screen.render().expect("a kept screen renders");
        let text: String = snapshot.cells[0].iter().map(|cell| cell.ch).collect();
        assert_eq!(text, "❯ Yes");
    }

    #[test]
    fn damage_accumulates_between_evaluation_points() {
        let mut screen = ScreenState::new(80, 24, true);
        feed(&mut screen, "\u{1b}[1;1Hone");
        feed(&mut screen, "\u{1b}[3;1Hthree");
        let evaluation = screen.evaluate();
        assert_eq!(evaluation.damaged, vec![0, 2]);
        assert_eq!(
            evaluation.novel,
            vec![
                NovelSpan {
                    row: 0,
                    text: "one".to_owned()
                },
                NovelSpan {
                    row: 2,
                    text: "three".to_owned()
                },
            ]
        );
        assert_eq!(
            screen.evaluate(),
            Evaluation::default(),
            "and is then spent"
        );
    }

    #[test]
    fn a_repainted_row_is_damaged_but_says_nothing_new() {
        let mut screen = ScreenState::new(80, 24, true);
        feed(&mut screen, "\u{1b}[1;1Hsteady");
        screen.evaluate();
        feed(&mut screen, "\u{1b}[1;1Hsteady");
        let evaluation = screen.evaluate();
        assert_eq!(evaluation.damaged, vec![0], "the row was written to");
        assert!(evaluation.novel.is_empty(), "with nothing new in it");
    }

    #[test]
    fn a_stream_that_scrolls_off_the_top_reports_each_line_once() {
        // Every row of a full screen changes when one line is appended, and
        // none of it is new. This is the case a filter comparing each row
        // against what that row last said gets wrong, and it is the common
        // one — an interface that prints scrolls, and one drawing on the
        // alternate screen scrolls with nothing in the byte stream to say so.
        let mut screen = ScreenState::new(20, 4, true);
        let mut reported = Vec::new();
        for line in 1..=12 {
            feed(&mut screen, &format!("line{line}\r\n"));
            for span in screen.evaluate().novel {
                reported.push(span.text);
            }
        }
        let expected: Vec<String> = (1..=12).map(|line| format!("line{line}")).collect();
        assert_eq!(reported, expected);
    }

    #[test]
    fn a_resize_mid_stream_reflows_and_the_next_snapshot_has_the_new_size() {
        let mut screen = ScreenState::new(80, 24, true);
        feed(&mut screen, "\u{1b}[1;1Hbefore the resize");
        screen.resize(120, 40);
        feed(&mut screen, "\u{1b}[5;1Hafter the resize");
        let snapshot = screen.render().expect("a kept screen renders");
        assert_eq!((snapshot.cols, snapshot.rows), (120, 40));
        assert_eq!(snapshot.cells.len(), 40);
        let text: String = snapshot.cells[4].iter().map(|cell| cell.ch).collect();
        assert_eq!(text, "after the resize");
    }

    #[test]
    fn a_resize_to_a_shorter_screen_leaves_no_damage_pointing_off_the_end() {
        // Rows written before the reflow are gone after it, and an
        // evaluation that still believed in them would read past the grid.
        let mut screen = ScreenState::new(80, 24, true);
        feed(&mut screen, "\u{1b}[20;1Hlow down the screen");
        screen.resize(80, 10);
        let evaluation = screen.evaluate();
        assert!(evaluation.damaged.iter().all(|&row| row < 10));
    }

    #[test]
    fn output_written_just_before_a_resize_is_not_lost_to_it() {
        // A caller resizing while the CLI is mid-paint lands inside the
        // window between output arriving and the screen next being examined.
        // That output has not been reported yet, and a reflow must not be
        // able to swallow it — nor to swallow the repaint the CLI sends when
        // it learns of the new size, which would be the only other chance to
        // see it.
        let mut screen = ScreenState::new(80, 24, true);
        feed(&mut screen, "\u{1b}[1;1Hnobody has seen this yet");
        screen.resize(120, 40);
        assert_eq!(
            screen
                .evaluate()
                .novel
                .into_iter()
                .map(|span| span.text)
                .collect::<Vec<_>>(),
            vec!["nobody has seen this yet".to_owned()],
        );
        // And having been reported once, the repaint does not repeat it.
        feed(&mut screen, "\u{1b}[1;1Hnobody has seen this yet");
        assert!(screen.evaluate().novel.is_empty());
    }

    #[test]
    fn a_reflow_that_moves_a_line_to_another_row_does_not_re_report_it() {
        // Narrowing wraps the line above and pushes this one down. It is the
        // same line, on a row it has never been on.
        let mut screen = ScreenState::new(40, 6, true);
        feed(
            &mut screen,
            "\u{1b}[1;1Ha line that is long enough to wrap\r\nsecond line here",
        );
        screen.evaluate();
        screen.resize(20, 6);
        assert!(
            screen
                .evaluate()
                .novel
                .iter()
                .all(|span| span.text != "second line here"),
            "the line moved down a row; it did not arrive"
        );
    }

    #[test]
    fn a_reflow_is_not_new_content() {
        // Dragging a window edge must not replay the visible screen as
        // freshly arrived text, either at the reflow or at the repaint that
        // usually follows it.
        let mut screen = ScreenState::new(80, 24, true);
        feed(&mut screen, "\u{1b}[1;1HProceed?");
        screen.evaluate();
        screen.resize(120, 40);
        assert!(screen.evaluate().novel.is_empty());
        feed(&mut screen, "\u{1b}[1;1HProceed?");
        assert!(screen.evaluate().novel.is_empty());
    }

    #[test]
    fn a_reflow_asks_the_matcher_to_look_again() {
        // Not new content, but the screen breaks in different places now, so
        // the rows it rearranged are worth re-examining.
        let mut screen = ScreenState::new(20, 4, true);
        feed(
            &mut screen,
            "a line long enough to wrap when the screen narrows",
        );
        screen.evaluate();
        screen.resize(10, 4);
        assert!(!screen.evaluate().damaged.is_empty());
    }

    #[test]
    fn a_terminal_too_large_to_reconstruct_gets_no_screen_rather_than_the_memory() {
        // 65 535 × 65 535 is 63 GiB of grid — an allocation that takes the
        // process down and every other session with it. The size comes from
        // a caller, and a caller is not trusted input.
        let mut screen = ScreenState::new(u16::MAX, u16::MAX, true);
        assert!(!screen.is_kept());
        assert_eq!(screen.footprint(), 0);
        feed(&mut screen, "anything at all");
        assert_eq!(
            screen.render(),
            None,
            "so the reconnect payload omits the field"
        );
        assert_eq!(screen.evaluate(), Evaluation::default());
    }

    #[test]
    fn a_terminal_within_reach_is_reconstructed_normally() {
        // The bound has to sit far above any real terminal, or it becomes a
        // second way to lose a screen nobody asked to lose. A very large one
        // in front of a person is around 500 × 150.
        for (cols, rows) in [(80, 24), (200, 100), (500, 150), (600, 200)] {
            let screen = ScreenState::new(cols, rows, true);
            assert!(
                screen.is_kept(),
                "{cols}×{rows} is a size a person can have"
            );
        }
    }

    #[test]
    fn a_screen_that_is_cheap_by_area_and_costly_by_shape_is_still_refused() {
        // The reason the bound counts bytes. Each of these is well under a
        // million cells — the area a previous version of this bound allowed
        // — and each costs more than the whole runtime is sized for, because
        // every row carries a fixed overhead whatever its width.
        for (cols, rows) in [(1, 65_535), (15, 20_000), (1_000, 500)] {
            let screen = ScreenState::new(cols, rows, true);
            assert!(
                !screen.is_kept(),
                "{cols}×{rows} projects past the bound and must keep no screen"
            );
        }
    }

    #[test]
    fn growing_a_terminal_past_reach_drops_the_screen_instead_of_the_process() {
        let mut screen = ScreenState::new(80, 24, true);
        feed(&mut screen, "\u{1b}[1;1Hbefore");
        screen.resize(u16::MAX, u16::MAX);
        assert!(!screen.is_kept());
        assert_eq!(screen.render(), None);
        // And stays gone: resuming would hand back a screen with a hole in
        // its history where the oversized period was.
        screen.resize(80, 24);
        assert!(!screen.is_kept());
    }

    #[test]
    fn debugging_a_screen_does_not_print_what_is_on_it() {
        // Default logs do not carry CLI output, and a derived `Debug` would
        // put a whole screen into one the moment anything wrote
        // `tracing::debug!(?screen)` — including a struct further up that
        // derives `Debug` and happens to hold a session's screen.
        let mut screen = ScreenState::new(40, 3, true);
        feed(&mut screen, "api key sk-secret-value\r\npassword hunter2");
        let rendered = format!("{screen:?}");
        for secret in ["sk-secret-value", "hunter2", "api key", "password"] {
            assert!(
                !rendered.contains(secret),
                "the screen's Debug leaked {secret:?}: {rendered}"
            );
        }
        // Still worth printing: it says what shape the thing is in.
        assert!(rendered.contains("kept: true"), "{rendered}");
        assert!(rendered.contains("cols: 40"), "{rendered}");
        assert!(format!("{:?}", ScreenState::new(40, 3, false)).contains("kept: false"));
    }

    #[test]
    fn a_screen_with_no_columns_or_no_rows_is_still_a_screen() {
        // Terminal dimensions reach this runtime from a caller over the
        // wire, so a zero will arrive. The emulator indexes into its cells
        // without checking, which makes that an out-of-bounds panic several
        // calls after the mistake rather than an empty screen.
        for (cols, rows) in [(0, 0), (0, 24), (80, 0)] {
            let mut screen = ScreenState::new(cols, rows, true);
            feed(&mut screen, "\u{1b}[1;1Hstill here");
            screen.evaluate();
            screen.resize(cols, rows);
            let snapshot = screen.render().expect("a kept screen renders");
            assert!(
                snapshot.cols >= 1 && snapshot.rows >= 1,
                "{cols}×{rows} produced a screen of {}×{}",
                snapshot.cols,
                snapshot.rows
            );
            assert_eq!(snapshot.cells.len(), snapshot.rows as usize);
        }
    }

    #[test]
    fn a_screen_admitted_under_the_bound_stays_under_it_once_warm() {
        // The bound is checked against a projection, before anything is
        // allocated. That is only worth something if the projection is not
        // optimistic: a screen let in on an estimate and then growing past
        // it would make the cap advisory, and the public footprint — which a
        // runtime budgets from — would disagree with the number the session
        // was admitted on.
        //
        // Every shape here is driven until the repaint filter's window is
        // full, which is the state that costs most. The tall narrow one is
        // the case that matters: it sits just inside the bound, so it is
        // where an under-count would show first.
        for (cols, rows) in [(15, 12_000), (80, 24), (200, 100), (500, 150)] {
            let mut screen = ScreenState::new(cols, rows, true);
            assert!(screen.is_kept(), "{cols}×{rows} projects inside the bound");
            for round in 0..4 {
                let mut paint = String::new();
                for row in 0..rows {
                    paint.push_str(&format!("\u{1b}[{};1Hr{round}c{row}\r\n", row + 1));
                }
                feed(&mut screen, &paint);
                screen.evaluate();
            }
            let warm = screen.footprint();
            assert!(
                warm <= LARGEST_SCREEN_BYTES,
                "{cols}×{rows} was admitted on a projection and warms to {warm} B, past the \
                 {LARGEST_SCREEN_BYTES} B it was admitted under"
            );
        }
    }

    #[test]
    fn a_warm_tall_screen_reshaped_wide_stays_inside_the_bound() {
        // The cross-shape case, which is where the accounting is easiest to
        // get wrong: a session earns a tall screen's worth of bookkeeping,
        // then becomes a wide one whose grid alone is most of the budget. If
        // the old allocation is still held, the total sits past a bound the
        // new shape was admitted under — and nothing would say so, because
        // the admission check looks at a projection of the new shape only.
        let mut screen = ScreenState::new(15, 12_000, true);
        assert!(screen.is_kept());
        for round in 0..4 {
            let mut paint = String::new();
            for row in 0..12_000 {
                paint.push_str(&format!("\u{1b}[{};1Hr{round}c{row}\r\n", row + 1));
            }
            feed(&mut screen, &paint);
            screen.evaluate();
        }
        let tall = screen.footprint();
        assert!(
            tall > 4 * 1024 * 1024,
            "the tall shape should be genuinely large"
        );

        screen.resize(1_200, 200);
        assert!(screen.is_kept(), "this shape projects inside the bound");
        let wide = screen.footprint();
        assert!(
            wide <= LARGEST_SCREEN_BYTES,
            "a screen warmed at 15×12000 ({tall} B) and reshaped to 1200×200 holds {wide} B, \
             past the {LARGEST_SCREEN_BYTES} B the new shape was admitted under"
        );
    }

    #[test]
    fn leaving_the_alternate_screen_after_a_resize_is_coherent() {
        // The emulator resizes only the buffer that is active, so a session
        // that grows while on the alternate screen parks a primary buffer at
        // the old size. Restoring it could in principle report dimensions the
        // cells do not have, or fault on a row only the new size has — it
        // does not, and this is what notices if that ever changes, since the
        // interface this exists for spends its whole session on the
        // alternate screen and a caller may resize at any point in it.
        let mut screen = ScreenState::new(10, 3, true);
        feed(&mut screen, "\u{1b}[1;1Hprimary");
        feed(&mut screen, "\u{1b}[?1049h\u{1b}[1;1Halternate");
        screen.resize(60, 40);
        feed(&mut screen, "\u{1b}[?1049l");
        feed(&mut screen, "\u{1b}[38;1Ha row only the new size has");

        let snapshot = screen.render().expect("a kept screen renders");
        assert_eq!((snapshot.cols, snapshot.rows), (60, 40));
        assert_eq!(
            snapshot.cells.len(),
            snapshot.rows as usize,
            "the row count has to match the size reported beside it"
        );
        let row38: String = snapshot.cells[37].iter().map(|cell| cell.ch).collect();
        assert_eq!(row38, "a row only the new size has");
    }

    #[test]
    fn a_reflow_is_judged_on_the_parked_buffer_too() {
        // A resize reaches the active buffer only, so after one the session
        // holds the new buffer beside whatever the parked one already was.
        // Judging the reflow on two buffers at the *new* size describes a
        // grid that will not exist, and the direction of the error is
        // towards admitting.
        //
        // Honest about what this does and does not show: no pair of shapes
        // was found where the old reckoning actually overran — this one
        // holds 7.8 MiB of its 8 MiB under either — so what is asserted is
        // the reckoning rather than a rescued failure. The cost is
        // conservatism, and it is the cost a projection made before
        // allocating anything has to pay.
        let wide = ScreenState::new(5_800, 40, true);
        let tall = ScreenState::new(20, 10_000, true);
        assert!(
            wide.is_kept() && tall.is_kept(),
            "each shape alone is affordable"
        );

        let mut screen = ScreenState::new(5_800, 40, true);
        let parked = screen.footprint();
        screen.resize(20, 10_000);
        assert!(
            !screen.is_kept(),
            "a screen holding a {parked} B wide buffer was allowed to build a tall one \
             beside it; the reflow was judged on two buffers at the new size rather than \
             on the one still parked"
        );
    }

    #[test]
    fn one_oversized_feed_does_not_leave_its_buffer_behind() {
        // The decode buffer follows the largest feed it is given, and `feed`
        // takes a slice of any length. Releasing it on the way *in* handles
        // the previous feed and never the last one — and the last feed of a
        // session is the one with nothing after it to trigger the release.
        let mut screen = ScreenState::new(80, 24, true);
        let modest = screen.footprint();
        feed(&mut screen, &"x".repeat(4 * 1024 * 1024));
        let after = screen.footprint();
        assert!(
            after <= modest + RETAINED_DECODE_BYTES,
            "an 80×24 screen fed four mebibytes in one call holds {after} B afterwards, \
             against {modest} B before — the buffer was kept rather than handed back"
        );
        assert!(after <= LARGEST_SCREEN_BYTES);
    }

    #[test]
    fn a_screen_that_was_wide_is_still_counted_as_wide() {
        // Narrowing truncates each row, and a truncation keeps the room the
        // cells occupied — so a screen that was 4000 columns across still
        // owns that much per row after becoming 20. Counting the active
        // buffer at the width it reports now misses it entirely, and the
        // shape that exposes it is one that then grows tall: a 4000×40
        // screen reflowed to 20×8000 projects comfortably under the bound
        // while owning megabytes of wide-row capacity on top.
        let mut screen = ScreenState::new(4_000, 40, true);
        assert!(screen.is_kept(), "the wide shape alone fits");
        assert!(
            ScreenState::new(20, 8_000, true).is_kept(),
            "and the tall shape alone fits"
        );
        screen.resize(20, 8_000);
        assert!(
            !screen.is_kept(),
            "a screen that was 4000 columns wide was allowed to become 8000 rows tall; the \
             rows it narrowed still hold their old width, and the reckoning counted them at \
             the new one"
        );
    }

    /// The per-session budget the design corpus records for this component.
    const BUDGET: usize = 64 * 1024;

    /// What a default screen costs once it has been used, measured rather
    /// than budgeted. Over the documented figure, which is the finding.
    const DEFAULT_SCREEN_IN_USE: usize = 80 * 1024;

    /// What the largest screen a caller may ask for actually costs, measured
    /// rather than budgeted, with room for a cell to grow by a byte or two
    /// before anyone needs to hear about it.
    const LARGEST_SCREEN: usize = 680 * 1024;

    #[test]
    fn the_default_screen_does_not_fit_the_budget_either() {
        // The budget row reads "~64 KiB (200 cols × 100 rows × cell
        // overhead)", and neither half of it survives contact.
        //
        // The dimensions were ruled out first: 20 000 cells hold 78 KiB of
        // characters before a single attribute, and no emulator storing a
        // character per cell can do better. What was left standing was the
        // figure, on the reading that it described the screen a session
        // usually gets — and it does not describe that either. A default
        // 80×24 screen costs more than 64 KiB as soon as it is used, and
        // measuring one that had never been fed was how that stayed hidden:
        // the repaint filter has allocated no window on a cold screen, and
        // the decode buffer no room.
        //
        // A session holds two grids, a window of recently reported lines
        // sized from the screen's height, and a decode buffer sized to one
        // read. That is what a per-session figure has to cover.
        let cold = ScreenState::new(80, 24, true).footprint();
        assert!(cold <= BUDGET, "a screen nobody has fed is {cold} B");

        let mut screen = ScreenState::new(80, 24, true);
        for round in 0..6 {
            let mut paint = String::new();
            for row in 0..24 {
                paint.push_str(&format!(
                    "\u{1b}[{};1Hr{round}c{row} some content\r\n",
                    row + 1
                ));
            }
            feed(&mut screen, &paint);
            screen.evaluate();
        }
        let warm = screen.footprint();
        assert!(
            warm > BUDGET,
            "a used 80×24 screen now fits {BUDGET} B, so the budget row holds after all and \
             this test is the thing that needs correcting"
        );
        assert!(
            warm <= DEFAULT_SCREEN_IN_USE,
            "a used 80×24 screen is {warm} B, past the {DEFAULT_SCREEN_IN_USE} B recorded \
             for it"
        );

        let largest = ScreenState::new(200, 100, true).footprint();
        assert!(
            largest <= LARGEST_SCREEN,
            "a 200×100 screen is {largest} B, over the {LARGEST_SCREEN} B recorded for it"
        );
    }
}
