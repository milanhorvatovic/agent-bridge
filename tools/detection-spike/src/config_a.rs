//! Pipeline configuration (a): the text-matching pipeline.
//!
//! Bytes flow recorded-chunk by recorded-chunk through the stripper into
//! completed lines; every non-blank line is one **emission**, evaluated
//! against the compiled pattern set. An emission with at least one firing is
//! recognized; the rest are the `unrecognized` share this configuration is
//! measured by. Blank lines are counted but excluded from the emission
//! denominator — a repaint-heavy stream pads itself with them, and letting
//! padding dilute the ratio would flatter the pipeline. All counters are
//! reported, so the ratio can be re-sliced later without re-running.
//!
//! Repaints are not deduplicated: the same dialog painted five times is five
//! sets of firings. Line-level duplication is a property of this
//! configuration and shows up in the hit counts by design.

use std::collections::BTreeMap;

use crate::pacing::PacedInput;
use crate::patterns::{CompiledPatterns, GuardTrip};
use crate::strip::LineSegmenter;

/// Line counters of one fixture replay. `emissions` is the denominator of
/// the unrecognized ratio: total lines minus blank lines.
#[derive(Debug, Default, serde::Serialize)]
pub struct LineStats {
    pub total: u64,
    pub blank: u64,
    pub emissions: u64,
    pub matched: u64,
    pub unrecognized: u64,
    pub forced_segmentations: u64,
}

/// Everything one replay of one fixture produced.
pub struct ReplayOutcome {
    pub lines: LineStats,
    /// Line-level firings per pattern id (a pattern fires at most once per
    /// line, however often its needle occurs in it).
    pub pattern_hits: BTreeMap<&'static str, u64>,
    pub guard_trips: Vec<GuardTrip>,
    /// Distinct unmatched line texts with occurrence counts, for growing the
    /// pattern set from what the pipeline failed to classify.
    pub unmatched: BTreeMap<String, u64>,
}

/// Replay one fixture's byte stream through the text-matching pipeline.
/// The engine carries per-session state (the safety guard's disabled set),
/// so callers hand in a fresh one per fixture.
pub fn replay(input: &PacedInput, engine: &mut CompiledPatterns) -> ReplayOutcome {
    let mut segmenter = LineSegmenter::new();
    let mut completed = Vec::new();
    for (chunk, _monotonic_ns) in input.iter_chunks() {
        segmenter.feed(chunk, &mut completed);
    }
    segmenter.finish(&mut completed);

    let mut lines = LineStats::default();
    let mut pattern_hits: BTreeMap<&'static str, u64> = BTreeMap::new();
    for spec in engine.specs() {
        pattern_hits.insert(spec.id, 0);
    }
    let mut guard_trips = Vec::new();
    let mut unmatched: BTreeMap<String, u64> = BTreeMap::new();

    for line in &completed {
        lines.total += 1;
        if line.forced {
            lines.forced_segmentations += 1;
        }
        let trimmed = line.text.trim();
        if trimmed.is_empty() {
            lines.blank += 1;
            continue;
        }
        lines.emissions += 1;

        let fired = engine.evaluate(&line.text, lines.total, &mut guard_trips);
        if fired.is_empty() {
            lines.unrecognized += 1;
            *unmatched.entry(truncate_for_sample(trimmed)).or_insert(0) += 1;
        } else {
            lines.matched += 1;
            for index in fired {
                *pattern_hits.entry(engine.specs()[index].id).or_insert(0) += 1;
            }
        }
    }

    ReplayOutcome {
        lines,
        pattern_hits,
        guard_trips,
        unmatched,
    }
}

/// Sample keys are capped so one giant repaint line cannot bloat a report;
/// the count still records every occurrence.
fn truncate_for_sample(line: &str) -> String {
    const MAX_SAMPLE_CHARS: usize = 120;
    match line.char_indices().nth(MAX_SAMPLE_CHARS) {
        Some((byte_index, _)) => format!("{}…", &line[..byte_index]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patterns::Cli;

    fn paced(bytes: &[u8]) -> PacedInput {
        // One synthetic chunk per byte: the harshest boundary split the
        // stripper must be invariant to.
        let chunks = (0..bytes.len())
            .map(|offset| crate::pacing::ChunkBoundary {
                offset,
                len: 1,
                monotonic_ns: offset as u64,
            })
            .collect();
        PacedInput {
            bytes: bytes.to_vec(),
            chunks,
        }
    }

    #[test]
    fn emissions_exclude_blank_lines_and_count_both_shares() {
        let bytes = b"\x1b[2J\r\n\r\nDoyouwanttoproceed?\r\nfree text the set does not know\r\n";
        let mut engine = CompiledPatterns::for_cli(Cli::Claude).expect("compiles");
        let outcome = replay(&paced(bytes), &mut engine);

        assert_eq!(outcome.lines.total, 4);
        assert_eq!(outcome.lines.blank, 2);
        assert_eq!(outcome.lines.emissions, 2);
        assert_eq!(outcome.lines.matched, 1);
        assert_eq!(outcome.lines.unrecognized, 1);
        assert_eq!(
            outcome.pattern_hits["claude/permission-title-mashed"], 1,
            "hits: {:?}",
            outcome.pattern_hits
        );
        assert_eq!(
            outcome.unmatched.get("free text the set does not know"),
            Some(&1)
        );
    }

    #[test]
    fn one_line_can_fire_several_patterns() {
        let bytes = "❯ 1. Yes, and esc to interrupt\n".as_bytes();
        let mut engine = CompiledPatterns::for_cli(Cli::Claude).expect("compiles");
        let outcome = replay(&paced(bytes), &mut engine);

        assert_eq!(outcome.lines.matched, 1);
        assert_eq!(outcome.pattern_hits["claude/permission-option-yes"], 1);
        assert_eq!(outcome.pattern_hits["claude/status-esc-hint"], 1);
        assert_eq!(outcome.pattern_hits["claude/prompt-echo"], 1);
    }

    #[test]
    fn every_known_pattern_id_appears_in_the_hit_map_even_at_zero() {
        let mut engine = CompiledPatterns::for_cli(Cli::Codex).expect("compiles");
        let outcome = replay(&paced(b"nothing recognizable\n"), &mut engine);
        for spec in engine.specs() {
            assert!(
                outcome.pattern_hits.contains_key(spec.id),
                "{} missing from the hit map",
                spec.id
            );
        }
    }

    #[test]
    fn sample_lines_are_truncated_but_counted_in_full() {
        let long = "x".repeat(400);
        let bytes = format!("{long}\n{long}\n").into_bytes();
        let mut engine = CompiledPatterns::for_cli(Cli::Claude).expect("compiles");
        let outcome = replay(&paced(&bytes), &mut engine);

        assert_eq!(outcome.unmatched.len(), 1);
        let (sample, count) = outcome.unmatched.iter().next().expect("one sample");
        assert!(sample.chars().count() <= 121, "sample stays capped");
        assert!(sample.ends_with('…'));
        assert_eq!(*count, 2);
    }
}
