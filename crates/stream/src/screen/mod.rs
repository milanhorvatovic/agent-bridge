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

/// What a screen of this size would cost, before one is built.
///
/// Deliberately the same accounting [`ScreenState::footprint`] reports, so
/// the number a session is refused on and the number it is later measured
/// against cannot drift apart.
fn projected_footprint(cols: u16, rows: u16) -> usize {
    let (cols, rows) = (usize::from(cols.max(1)), usize::from(rows.max(1)));
    let grid = vt::projected_grid_bytes(cols, rows);
    grid + dedup::projected_bytes(rows) + rows
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
        if screen.decoded.is_empty() {
            return;
        }
        for row in screen.grid.feed(&screen.decoded) {
            if let Some(flag) = screen.damaged.get_mut(row) {
                *flag = true;
            }
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
        if projected_footprint(cols, rows) > LARGEST_SCREEN_BYTES {
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
        let reflowed = screen.grid.resize(cols, rows);
        // Rows already waiting to be examined stay waiting. The emulator in
        // use happens to report every row as changed on any resize, which
        // would make re-deriving the whole set from `reflowed` come out the
        // same — but that is its choice and not a promise, and the guarantee
        // above is that output written before a resize survives it. Keeping
        // the flags makes that true here rather than true elsewhere.
        screen.damaged.resize(screen.grid.row_count(), false);
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
    use super::{Evaluation, NovelSpan, ScreenState};

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

    /// The per-session budget the design corpus records for this component.
    const BUDGET: usize = 64 * 1024;

    /// What the largest screen a caller may ask for actually costs, measured
    /// rather than budgeted, with room for a cell to grow by a byte or two
    /// before anyone needs to hear about it.
    const LARGEST_SCREEN: usize = 680 * 1024;

    #[test]
    fn the_default_screen_fits_the_budget_and_the_largest_one_cannot() {
        // The budget row reads "~64 KiB (200 cols × 100 rows × cell
        // overhead)", and its two halves do not describe the same object.
        // 20 000 cells cannot fit in 64 KiB while each holds a `char`: that
        // field alone is 78 KiB before a single attribute joins it, and no
        // emulator storing a character per cell can do better. The figure is
        // right for the screen a session usually gets — which is the one
        // sizing 32 concurrent sessions actually turns on — and the
        // dimensions beside it are not. Both are recorded here rather than
        // asserting a number that arithmetic rules out.
        //
        // A session holds *two* full grids, not one: the emulator allocates a
        // primary buffer and an alternate one up front and keeps both, which
        // is how switching screens and back restores what was underneath.
        // The default screen still fits the budget, but at 63.6 of 64 KiB it
        // fits by under two per cent — worth knowing before anyone treats
        // that headroom as somewhere to spend. The figure counts everything
        // a session keeps, the repaint filter's window included, not the
        // grid alone.
        let default_screen = ScreenState::new(80, 24, true).footprint();
        assert!(
            default_screen <= BUDGET,
            "an 80×24 screen is {default_screen} B, over the {BUDGET} B budget"
        );
        let largest = ScreenState::new(200, 100, true).footprint();
        assert!(
            largest <= LARGEST_SCREEN,
            "a 200×100 screen is {largest} B, over the {LARGEST_SCREEN} B recorded for it"
        );
        assert!(
            largest > BUDGET,
            "if a 200×100 screen now fits {BUDGET} B, the budget row is right after all \
             and this test is the thing that needs correcting"
        );
    }
}
