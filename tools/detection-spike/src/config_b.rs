//! Pipeline configuration (b): the screen-state pipeline.
//!
//! Bytes feed a headless virtual terminal instead of a line stripper, and
//! classification happens on the materialized screen at evaluation points —
//! quiet-period boundaries and feed quiescence — never per byte. At each
//! point, every non-blank viewport row whose content was not already on the
//! screen at the previous point is one **emission**: repainted content is
//! deduplicated against the screen buffer, so a TUI redrawing the same
//! dialog five times produces one set of emissions, not five. The dedup
//! remembers exactly one screen — what a runtime holding a screen-state
//! side buffer actually knows — so content that leaves the viewport and is
//! painted again later (an exit-time replay of the startup screen, say)
//! re-emits, and that cost is measured rather than hidden. The denominator
//! this configuration is measured by is therefore *screen-evaluation
//! emissions*, which is not the same population as configuration (a)'s
//! stripped lines — the two ratios sit side by side in the report but are
//! never mixed.
//!
//! Two matcher kinds run over each screen. The line patterns evaluate every
//! new row — the same engine as configuration (a), tuned to the spaced text
//! a screen shows rather than the cursor-mashed text a stream carries. The
//! menu-dialog detector reads whole regions and reports open dialogs with
//! their extracted options; a dialog that stays open across consecutive
//! evaluation points is one appearance, counted once until it leaves the
//! screen.

use std::collections::{BTreeMap, BTreeSet};

use crate::dialog::{self, DialogSpec};
use crate::pacing::PacedInput;
use crate::patterns::{CompiledPatterns, GuardTrip};
use crate::screen;

/// Screen-side counters of one fixture replay. `emissions` is the
/// denominator of the unrecognized ratio: distinct new row contents across
/// all evaluation points.
#[derive(Debug, Default, serde::Serialize)]
pub struct ScreenStats {
    pub eval_points: u64,
    /// Non-blank viewport rows across all evaluation points, before dedup.
    pub rows_seen: u64,
    /// Rows skipped because their content was already emitted — the repaint
    /// share the dedup absorbed.
    pub repainted: u64,
    pub emissions: u64,
    pub matched: u64,
    pub unrecognized: u64,
}

/// One dialog appearance: the evaluation point where a dialog arrived on
/// screen, with the fields the detector extracted there.
#[derive(Debug, serde::Serialize)]
pub struct DialogSighting {
    pub id: &'static str,
    pub eval_point: u64,
    pub title_row: usize,
    pub options: Vec<dialog::DialogOption>,
}

/// Everything one replay of one fixture produced.
#[derive(Debug)]
pub struct ReplayOutcome {
    pub stats: ScreenStats,
    /// Emission-level firings per pattern id, dialog detections included —
    /// dedup means a pattern fires once per distinct row content, and a
    /// dialog counts once per appearance.
    pub pattern_hits: BTreeMap<&'static str, u64>,
    pub guard_trips: Vec<GuardTrip>,
    /// Distinct unmatched row contents with occurrence counts, for growing
    /// the pattern set from what the screen showed and nothing classified.
    pub unmatched: BTreeMap<String, u64>,
    pub dialogs: Vec<DialogSighting>,
}

/// Replay one fixture's byte stream through the screen-state pipeline. The
/// engine carries per-session state (the safety guard's disabled set), so
/// callers hand in a fresh one per fixture. Errors mean the stream could
/// not replay through the virtual terminal at all — never a shorter replay.
pub fn replay(
    input: &PacedInput,
    cols: u16,
    rows: u16,
    engine: &mut CompiledPatterns,
    dialogs: &[&'static DialogSpec],
) -> Result<ReplayOutcome, String> {
    let points = screen::eval_points(input, cols, rows)?;

    let mut stats = ScreenStats::default();
    let mut pattern_hits: BTreeMap<&'static str, u64> = BTreeMap::new();
    for spec in engine.specs() {
        pattern_hits.insert(spec.id, 0);
    }
    for spec in dialogs {
        pattern_hits.insert(spec.id, 0);
    }
    let mut guard_trips = Vec::new();
    let mut unmatched: BTreeMap<String, u64> = BTreeMap::new();
    let mut sightings = Vec::new();

    // The dedup memory: the distinct row contents on screen at the previous
    // evaluation point. One screen deep, deliberately.
    let mut previous: BTreeSet<String> = BTreeSet::new();
    // Dialogs open at the previous evaluation point, for appearance
    // tracking.
    let mut open_dialogs: BTreeSet<&'static str> = BTreeSet::new();

    for point in &points {
        stats.eval_points += 1;
        let mut current: BTreeSet<String> = BTreeSet::new();

        for row in &point.rows {
            // Rows are trailing-trimmed at snapshot time, so a blank row is
            // the empty string; blanks are viewport padding, not paints, and
            // never enter the accounting.
            if row.is_empty() {
                continue;
            }
            stats.rows_seen += 1;
            // A duplicate row on the same screen is one paint of that
            // content, like a carried-over row is.
            if current.contains(row) {
                stats.repainted += 1;
                continue;
            }
            current.insert(row.clone());
            if previous.contains(row) {
                stats.repainted += 1;
                continue;
            }
            stats.emissions += 1;

            // Guard trips are keyed by emission ordinal here — the screen
            // pipeline has no stream line numbers.
            let fired = engine.evaluate(row, stats.emissions, &mut guard_trips);
            if fired.is_empty() {
                stats.unrecognized += 1;
                *unmatched
                    .entry(crate::metrics::truncate_sample(row))
                    .or_insert(0) += 1;
            } else {
                stats.matched += 1;
                for index in fired {
                    *pattern_hits.entry(engine.specs()[index].id).or_insert(0) += 1;
                }
            }
        }

        let detections = dialog::detect(dialogs, &point.rows);
        let now_open: BTreeSet<&'static str> = detections
            .iter()
            .map(|detection| detection.spec.id)
            .collect();
        for detection in detections {
            if !open_dialogs.contains(detection.spec.id) {
                *pattern_hits.entry(detection.spec.id).or_insert(0) += 1;
                sightings.push(DialogSighting {
                    id: detection.spec.id,
                    eval_point: point.ordinal,
                    title_row: detection.title_row,
                    options: detection.options,
                });
            }
        }
        open_dialogs = now_open;
        previous = current;
    }

    Ok(ReplayOutcome {
        stats,
        pattern_hits,
        guard_trips,
        unmatched,
        dialogs: sightings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing::ChunkBoundary;
    use crate::patterns::Cli;
    use crate::screen::QUIET_GAP_NS;

    /// Build a paced input from (bytes, arrival-time) pairs.
    fn paced(chunks: &[(&[u8], u64)]) -> PacedInput {
        let mut bytes = Vec::new();
        let mut boundaries = Vec::new();
        for (chunk, monotonic_ns) in chunks {
            boundaries.push(ChunkBoundary {
                offset: bytes.len(),
                len: chunk.len(),
                monotonic_ns: *monotonic_ns,
            });
            bytes.extend_from_slice(chunk);
        }
        PacedInput {
            bytes,
            chunks: boundaries,
        }
    }

    fn replay_claude(input: &PacedInput) -> ReplayOutcome {
        let mut engine = CompiledPatterns::for_screen(Cli::Claude).expect("compiles");
        replay(input, 80, 24, &mut engine, &dialog::for_cli(Cli::Claude))
            .expect("valid stream replays")
    }

    #[test]
    fn a_repainted_row_is_one_emission_not_two() {
        // The same status row painted before and after a quiet boundary:
        // two evaluation points see it, the dedup counts it once.
        let input = paced(&[
            (b"\x1b[1;1Hesc to interrupt", 0),
            (b"\x1b[1;1Hesc to interrupt", QUIET_GAP_NS),
        ]);
        let outcome = replay_claude(&input);

        assert_eq!(outcome.stats.eval_points, 2);
        assert_eq!(outcome.stats.rows_seen, 2);
        assert_eq!(outcome.stats.emissions, 1);
        assert_eq!(outcome.stats.repainted, 1);
        assert_eq!(outcome.pattern_hits["claude/screen-status-esc-hint"], 1);
    }

    #[test]
    fn content_returning_after_leaving_the_screen_reemits() {
        // The dedup is one screen deep: content wiped at one evaluation
        // point and painted again later is a fresh paint the runtime would
        // re-emit, so the replay counts it again.
        let input = paced(&[
            (b"\x1b[1;1H\xe2\x8f\xba ok", 0),
            (b"\x1b[2J\x1b[1;1Hesc to interrupt", QUIET_GAP_NS),
            (b"\x1b[2J\x1b[1;1H\xe2\x8f\xba ok", 2 * QUIET_GAP_NS),
        ]);
        let outcome = replay_claude(&input);

        assert_eq!(outcome.stats.emissions, 3);
        assert_eq!(outcome.pattern_hits["claude/screen-response-bullet"], 2);
    }

    #[test]
    fn a_changed_row_is_a_new_emission() {
        let input = paced(&[
            (b"\x1b[1;1Hstreaming wor", 0),
            (b"\x1b[1;1Hstreaming words settle", QUIET_GAP_NS),
        ]);
        let outcome = replay_claude(&input);

        assert_eq!(outcome.stats.emissions, 2, "each settled content counts");
        assert_eq!(outcome.stats.unrecognized, 2);
        assert_eq!(
            outcome.unmatched.keys().collect::<Vec<_>>(),
            ["streaming wor", "streaming words settle"]
        );
    }

    #[test]
    fn a_dialog_spanning_evaluation_points_is_one_appearance() {
        let dialog_paint =
            b"\x1b[2;1H Do you want to proceed?\x1b[3;1H \xe2\x9d\xaf 1. Yes\x1b[4;1H   2. No";
        let input = paced(&[
            (&dialog_paint[..], 0),
            // Still open at the next quiet boundary: same appearance.
            (b"\x1b[1;1H", QUIET_GAP_NS),
            // Answered: the dialog rows are overwritten.
            (b"\x1b[2J\x1b[1;1Hdone", 2 * QUIET_GAP_NS),
        ]);
        let outcome = replay_claude(&input);

        assert_eq!(outcome.stats.eval_points, 3);
        assert_eq!(outcome.pattern_hits["claude/screen-dialog-permission"], 1);
        assert_eq!(outcome.dialogs.len(), 1);
        assert_eq!(outcome.dialogs[0].eval_point, 1);
        assert_eq!(outcome.dialogs[0].title_row, 1);
        assert_eq!(outcome.dialogs[0].options.len(), 2);
    }

    #[test]
    fn a_dialog_that_reopens_counts_again() {
        let dialog_paint =
            b"\x1b[2;1H Do you want to proceed?\x1b[3;1H \xe2\x9d\xaf 1. Yes\x1b[4;1H   2. No";
        let input = paced(&[
            (&dialog_paint[..], 0),
            (b"\x1b[2J\x1b[1;1Hworking...", QUIET_GAP_NS),
            (&dialog_paint[..], 2 * QUIET_GAP_NS),
        ]);
        let outcome = replay_claude(&input);

        assert_eq!(outcome.pattern_hits["claude/screen-dialog-permission"], 2);
        assert_eq!(outcome.dialogs.len(), 2);
    }

    #[test]
    fn every_pattern_and_dialog_id_appears_in_the_hit_map_even_at_zero() {
        let outcome = replay_claude(&paced(&[(b"nothing recognizable", 0)]));
        let engine = CompiledPatterns::for_screen(Cli::Claude).expect("compiles");
        for spec in engine.specs() {
            assert!(
                outcome.pattern_hits.contains_key(spec.id),
                "{} missing from the hit map",
                spec.id
            );
        }
        for spec in dialog::for_cli(Cli::Claude) {
            assert!(
                outcome.pattern_hits.contains_key(spec.id),
                "{} missing from the hit map",
                spec.id
            );
        }
    }

    #[test]
    fn an_unreplayable_stream_is_an_error_not_a_short_outcome() {
        let input = paced(&[(&[0xFF, 0xFE], 0)]);
        let mut engine = CompiledPatterns::for_screen(Cli::Claude).expect("compiles");
        let err = replay(&input, 80, 24, &mut engine, &dialog::for_cli(Cli::Claude))
            .expect_err("undecodable bytes must abort");
        assert!(err.contains("UTF-8"), "must name the cause: {err}");
    }
}
