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
//! examining the screen at an [evaluation point](sched). A run that only
//! feeds materializes nothing at all, and [`ScreenState::renders`] is there
//! so a test can hold this crate to that.
//!
//! # Sessions that do not need it pay nothing
//!
//! The grid costs memory per session and parses every byte, which is worth it
//! for a CLI that draws and worth nothing for one that prints lines. Whether
//! to keep one is therefore a per-session decision — an adapter's setting
//! where it has one, the runtime-wide default otherwise — taken once, at
//! construction. A session without one holds no grid, does no work on feed,
//! and has no snapshot to give: [`ScreenState::render`] returns `None`, which
//! is what a reconnecting caller receives as a null snapshot.

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
    /// The subset of those rows whose text had not been reported before —
    /// what may be emitted without saying the same thing twice.
    pub novel: Vec<NovelSpan>,
}

/// The screen buffer for one session.
///
/// Construct it with the session's effective setting; everything else follows
/// from that one decision.
#[derive(Debug)]
pub struct ScreenState {
    /// `None` for a session that keeps no screen. The absence is the whole
    /// mechanism: there is no grid to feed, so there is no per-byte cost to
    /// skip and no branch that could be got wrong twice.
    kept: Option<Screen>,
}

/// Everything a session that keeps a screen needs.
#[derive(Debug)]
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
    pub fn new(cols: u16, rows: u16, tui_aware_effective: bool) -> Self {
        Self {
            kept: tui_aware_effective.then(|| {
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
    /// `None` when the session keeps no screen — the null snapshot a
    /// reconnecting caller receives, and the one place that absence is
    /// decided.
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
    /// look again at a screen that now breaks in different places. None of it
    /// counts as new content: the same text laid out at a new width is the
    /// same text, and re-reporting it would put a screenful on the wire every
    /// time a caller dragged a window edge.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let Some(screen) = self.kept.as_mut() else {
            return;
        };
        tracing::debug!(cols, rows, "reflowing the screen");
        let reflowed = screen.grid.resize(cols, rows);
        screen.dedup.rebaseline(&screen.grid);
        screen.damaged.clear();
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
    /// An accounting rather than a measurement: the grid dominates and its
    /// size is exactly known, while the odd bookkeeping vector around it is
    /// not worth an allocator hook to chase. A runtime that budgets memory
    /// per session and caps how many it will run has a use for the number,
    /// and so does anything reporting on a live session.
    pub fn footprint(&self) -> usize {
        self.kept
            .as_ref()
            .map_or(0, |screen| screen.grid.footprint())
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
            "which is the null snapshot on the wire"
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
    const LARGEST_SCREEN: usize = 340 * 1024;

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
