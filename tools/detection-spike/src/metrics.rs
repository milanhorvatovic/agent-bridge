//! Hit/miss accounting: expected firings from the driver step log, false
//! negatives per pattern, and the report shapes the replay run serializes.
//!
//! The driver's `steps.ndjson` is the ground truth. Its labeled steps mark
//! the instants an event demonstrably happened in the live session (a hook
//! arrived, a waited-for text painted, a key was sent), so expectations are
//! keyed on those records — never on what the pipeline itself managed to
//! see. Expected counts are per event occurrence; a pattern's hits are
//! line-level and repaints duplicate them, so `hits >= expected` is the
//! recognized case and the shortfall `expected - hits` is the false-negative
//! count. Only anchored and control patterns carry expectations; ambient
//! chrome has no per-event ground truth to expect against.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::config_a::ReplayOutcome;
use crate::corpus::{FixtureId, StepRecord};
use crate::patterns::{Cli, GuardTrip, PATTERNS, Role};

/// Expected firing count per pattern id for one fixture, derived from its
/// step log.
pub fn expected_firings(cli: Cli, steps: &[StepRecord]) -> BTreeMap<&'static str, u64> {
    let mut expected: BTreeMap<&'static str, u64> = BTreeMap::new();
    for spec in PATTERNS.iter().filter(|spec| spec.cli == cli) {
        if spec.role != Role::Ambient {
            expected.insert(spec.id, 0);
        }
    }

    let mut bump = |ids: &[&'static str]| {
        for id in ids {
            *expected.get_mut(id).unwrap_or_else(|| {
                panic!("expectation rule names unknown or ambient pattern {id}")
            }) += 1;
        }
    };

    for step in steps {
        match cli {
            Cli::Claude => match (step.step.as_str(), step.hook.as_deref()) {
                // A permission decision was requested and the dialog painted.
                ("wait_hook", Some("Notification")) => {
                    if step.kind.as_deref() == Some("permission") {
                        bump(&[
                            "claude/permission-title-mashed",
                            "claude/permission-title-spaced",
                            "claude/permission-option-yes",
                            "claude/permission-option-no",
                        ]);
                    } else {
                        // The idle notification: fires on the CLI's idle
                        // timer and paints nothing — the control pattern
                        // records the structural miss.
                        bump(&["claude/idle-notice"]);
                    }
                }
                // A tool ran to completion; its result block painted.
                ("wait_hook", Some("PostToolUse")) => bump(&["claude/tool-command-echo"]),
                // A turn completed; the response block bullet is durable.
                ("wait_hook", Some("Stop")) => bump(&["claude/response-bullet"]),
                ("wait_hook", Some("PreCompact")) => bump(&["claude/compact-result"]),
                ("press", _) if step.key.as_deref() == Some("ctrl-c") => {
                    bump(&["claude/interrupted-notice"]);
                }
                _ => {}
            },
            Cli::Codex => match (step.step.as_str(), step.marker.as_deref()) {
                ("wait_text", Some("trust")) => {
                    bump(&["codex/trust-title-mashed", "codex/trust-title-spaced"]);
                }
                ("wait_text", Some("proceed")) => bump(&[
                    "codex/approval-title",
                    "codex/approval-option-proceed",
                    "codex/approval-confirm-hint",
                ]),
                ("wait_text", Some("Explored")) => bump(&["codex/tool-explored"]),
                ("wait_text", Some("interrupted")) => bump(&["codex/interrupted-notice"]),
                ("wait_text", Some("compacted")) => bump(&["codex/compacted-notice"]),
                ("press", _)
                    if matches!(
                        step.label.as_deref(),
                        Some("approve-selection" | "approve-via-number")
                    ) =>
                {
                    bump(&["codex/approved-notice"]);
                }
                _ => {}
            },
        }
    }
    expected
}

/// One pattern's row in a fixture report.
#[derive(Debug, Serialize)]
pub struct PatternRow {
    pub id: &'static str,
    pub class: &'static str,
    pub role: &'static str,
    pub hits: u64,
    /// Absent for ambient patterns — they have no ground truth to expect
    /// against, which is different from expecting zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub false_negatives: Option<u64>,
}

/// The full accounting of one fixture replay.
#[derive(Debug, Serialize)]
pub struct FixtureReport {
    pub fixture: String,
    pub cli: String,
    pub version: String,
    pub scenario: String,
    pub cols: u16,
    pub rows: u16,
    pub lines: crate::config_a::LineStats,
    pub unrecognized_ratio: f64,
    pub patterns: Vec<PatternRow>,
    pub guard_trips: Vec<GuardTrip>,
    /// Most frequent unmatched lines, largest first; length capped by the
    /// caller's `--dump-unmatched`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unmatched_top: Vec<UnmatchedRow>,
}

#[derive(Debug, Serialize)]
pub struct UnmatchedRow {
    pub line: String,
    pub count: u64,
}

/// Aggregate over every fixture of one (cli, version) pair.
#[derive(Debug, Serialize)]
pub struct SummaryRow {
    pub cli: String,
    pub version: String,
    pub fixtures: u64,
    pub emissions: u64,
    pub unrecognized: u64,
    pub unrecognized_ratio: f64,
    /// Total false negatives across anchored patterns only — the controls
    /// exist to be red and would drown the signal.
    pub anchored_false_negatives: u64,
}

/// The whole replay run, as written to `--out`.
#[derive(Debug, Serialize)]
pub struct RunReport {
    pub config: String,
    pub fixtures: Vec<FixtureReport>,
    pub summary: Vec<SummaryRow>,
}

/// Assemble one fixture's report from its replay outcome and ground truth.
pub fn fixture_report(
    id: &FixtureId,
    cli: Cli,
    outcome: ReplayOutcome,
    expected: &BTreeMap<&'static str, u64>,
    unmatched_top: usize,
) -> FixtureReport {
    let mut patterns = Vec::new();
    for spec in PATTERNS.iter().filter(|spec| spec.cli == cli) {
        let hits = outcome.pattern_hits.get(spec.id).copied().unwrap_or(0);
        let expected_count = expected.get(spec.id).copied();
        patterns.push(PatternRow {
            id: spec.id,
            class: spec.class,
            role: spec.role.name(),
            hits,
            expected: expected_count,
            false_negatives: expected_count.map(|count| count.saturating_sub(hits)),
        });
    }

    let mut unmatched: Vec<UnmatchedRow> = outcome
        .unmatched
        .into_iter()
        .map(|(line, count)| UnmatchedRow { line, count })
        .collect();
    // Most frequent first; ties in the BTreeMap's text order so the report
    // is deterministic.
    unmatched.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.line.cmp(&b.line)));
    unmatched.truncate(unmatched_top);

    let ratio = if outcome.lines.emissions == 0 {
        0.0
    } else {
        outcome.lines.unrecognized as f64 / outcome.lines.emissions as f64
    };

    FixtureReport {
        fixture: id.to_string(),
        cli: id.cli.clone(),
        version: id.version.clone(),
        scenario: id.scenario.clone(),
        cols: id.cols,
        rows: id.rows,
        lines: outcome.lines,
        unrecognized_ratio: ratio,
        patterns,
        guard_trips: outcome.guard_trips,
        unmatched_top: unmatched,
    }
}

/// Roll fixture reports up into per-(cli, version) summary rows, in report
/// order.
pub fn summarize(fixtures: &[FixtureReport]) -> Vec<SummaryRow> {
    let mut rows: Vec<SummaryRow> = Vec::new();
    for fixture in fixtures {
        let row = match rows
            .iter_mut()
            .find(|row| row.cli == fixture.cli && row.version == fixture.version)
        {
            Some(row) => row,
            None => {
                rows.push(SummaryRow {
                    cli: fixture.cli.clone(),
                    version: fixture.version.clone(),
                    fixtures: 0,
                    emissions: 0,
                    unrecognized: 0,
                    unrecognized_ratio: 0.0,
                    anchored_false_negatives: 0,
                });
                rows.last_mut().expect("row just pushed")
            }
        };
        row.fixtures += 1;
        row.emissions += fixture.lines.emissions;
        row.unrecognized += fixture.lines.unrecognized;
        row.anchored_false_negatives += fixture
            .patterns
            .iter()
            .filter(|pattern| pattern.role == "anchored")
            .filter_map(|pattern| pattern.false_negatives)
            .sum::<u64>();
    }
    for row in &mut rows {
        if row.emissions > 0 {
            row.unrecognized_ratio = row.unrecognized as f64 / row.emissions as f64;
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(kind: &str) -> StepRecord {
        StepRecord {
            step: kind.to_string(),
            ..StepRecord::default()
        }
    }

    fn wait_hook(hook: &str, kind: Option<&str>) -> StepRecord {
        StepRecord {
            hook: Some(hook.to_string()),
            kind: kind.map(str::to_string),
            ..step("wait_hook")
        }
    }

    fn wait_text(marker: &str) -> StepRecord {
        StepRecord {
            marker: Some(marker.to_string()),
            ..step("wait_text")
        }
    }

    #[test]
    fn claude_permission_dialog_expects_title_and_options() {
        let steps = [wait_hook("Notification", Some("permission"))];
        let expected = expected_firings(Cli::Claude, &steps);
        assert_eq!(expected["claude/permission-title-mashed"], 1);
        assert_eq!(expected["claude/permission-title-spaced"], 1);
        assert_eq!(expected["claude/permission-option-yes"], 1);
        assert_eq!(expected["claude/permission-option-no"], 1);
        assert_eq!(expected["claude/compact-result"], 0);
    }

    #[test]
    fn claude_idle_notification_expects_the_control_pattern() {
        let steps = [wait_hook("Notification", None)];
        let expected = expected_firings(Cli::Claude, &steps);
        assert_eq!(expected["claude/idle-notice"], 1);
        assert_eq!(expected["claude/permission-title-mashed"], 0);
    }

    #[test]
    fn two_tool_runs_expect_two_result_firings() {
        // The parallel-tools shape: two PostToolUse hooks in one turn.
        let steps = [
            wait_hook("PostToolUse", None),
            wait_hook("PostToolUse", None),
            wait_hook("Stop", None),
        ];
        let expected = expected_firings(Cli::Claude, &steps);
        assert_eq!(expected["claude/tool-command-echo"], 2);
        assert_eq!(expected["claude/response-bullet"], 1);
    }

    #[test]
    fn codex_approval_flow_expects_dialog_and_confirmation() {
        let approve = StepRecord {
            label: Some("approve-via-number".to_string()),
            key: Some("1".to_string()),
            ..step("press")
        };
        let steps = [wait_text("trust"), wait_text("proceed"), approve];
        let expected = expected_firings(Cli::Codex, &steps);
        assert_eq!(expected["codex/trust-title-mashed"], 1);
        assert_eq!(expected["codex/approval-title"], 1);
        assert_eq!(expected["codex/approval-confirm-hint"], 1);
        assert_eq!(expected["codex/approved-notice"], 1);
        assert_eq!(expected["codex/interrupted-notice"], 0);
    }

    #[test]
    fn ambient_patterns_carry_no_expectation() {
        let expected = expected_firings(Cli::Claude, &[]);
        assert!(!expected.contains_key("claude/status-esc-hint"));
        assert!(!expected.contains_key("claude/box-border"));
    }

    #[test]
    fn summary_rolls_up_anchored_false_negatives_only() {
        let id = FixtureId {
            cli: "claude".to_string(),
            version: "2.1.201".to_string(),
            scenario: "token-streaming".to_string(),
            cols: 80,
            rows: 24,
        };
        let outcome = ReplayOutcome {
            lines: crate::config_a::LineStats {
                total: 10,
                blank: 2,
                emissions: 8,
                matched: 6,
                unrecognized: 2,
                forced_segmentations: 0,
            },
            pattern_hits: BTreeMap::new(),
            guard_trips: Vec::new(),
            unmatched: BTreeMap::new(),
        };
        let steps = [wait_hook("Stop", None), wait_hook("Notification", None)];
        let expected = expected_firings(Cli::Claude, &steps);
        let report = fixture_report(&id, Cli::Claude, outcome, &expected, 0);
        let summary = summarize(std::slice::from_ref(&report));

        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].fixtures, 1);
        assert_eq!(summary[0].emissions, 8);
        assert!((summary[0].unrecognized_ratio - 0.25).abs() < 1e-9);
        // The Stop expectation is anchored and unmet (1 FN); the idle-notice
        // expectation is a control and must not count here.
        assert_eq!(summary[0].anchored_false_negatives, 1);
    }
}
