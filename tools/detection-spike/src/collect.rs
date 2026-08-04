//! Cross-configuration metrics collection: everything the `metrics`
//! subcommand adds on top of the per-fixture accounting in [`crate::metrics`].
//!
//! One collection run replays the whole committed corpus through all three
//! pipeline configurations and reduces the per-fixture reports to the four
//! measurements the spike exists to produce:
//!
//! - **Unrecognized ratios** per configuration per (cli, version), each
//!   over its own emission population — the denominators differ by
//!   construction and every configuration block states its own, so the
//!   three ratios sit side by side without ever being summed.
//! - **Per-pattern false-negative rates**: corpus-wide hits, expectations,
//!   and shortfalls per pattern per (cli, version).
//! - **Version drift, measured**: each matcher set was read out of one
//!   tuned version and left untouched for the neighbours, so an anchored
//!   pattern whose shortfall grows on a neighbour *is* the drift. The
//!   collection computes those regressions from the replay, down to the
//!   fixtures they occur in.
//! - **Effort, logged**: what re-greening the drifted patterns and adding
//!   a new one actually cost. Wall-clock cannot be replayed out of the
//!   corpus, so those measurements live in a committed log file this
//!   module loads, validates against the known matcher ids, and embeds in
//!   the report beside the computed numbers they price.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::channel::CHANNEL_CLASSIFIERS;
use crate::config_a;
use crate::config_b;
use crate::config_c;
use crate::corpus::{self, Fixture};
use crate::dialog::{self, DIALOGS};
use crate::metrics::{
    self, ChannelFixtureReport, FixtureReport, PatternRow, ScreenFixtureReport, SummaryFixture,
    SummaryRow,
};
use crate::pacing::PacedInput;
use crate::patterns::{Cli, CompiledPatterns, PATTERNS, SCREEN_PATTERNS};

/// One fixture's shared replay inputs — the byte stream and the driver
/// step log — loaded once and replayed by any number of configurations,
/// so a multi-configuration collection reads and parses each artifact a
/// single time instead of once per configuration.
pub struct LoadedFixture<'corpus> {
    fixture: &'corpus Fixture,
    cli: Cli,
    input: PacedInput,
    steps: Vec<corpus::StepRecord>,
}

impl<'corpus> LoadedFixture<'corpus> {
    /// Load the artifacts every configuration replays from. Errors carry
    /// the fixture identity so a caller can report them without more
    /// context.
    pub fn load(fixture: &'corpus Fixture) -> Result<Self, String> {
        let cli = Cli::parse(&fixture.id.cli)
            .ok_or_else(|| format!("{}: no pattern set for this cli", fixture.id))?;
        let input =
            PacedInput::load(&fixture.dir).map_err(|err| format!("{}: {err}", fixture.id))?;
        let steps =
            corpus::load_steps(&fixture.dir).map_err(|err| format!("{}: {err}", fixture.id))?;
        Ok(Self {
            fixture,
            cli,
            input,
            steps,
        })
    }

    /// Replay through the text-matching pipeline and account it. A fresh
    /// engine per replay: the safety guard's disabled set is per-session
    /// state and must not leak across replays.
    pub fn report_a(&self, dump_unmatched: usize) -> Result<FixtureReport, String> {
        let mut engine = CompiledPatterns::for_cli(self.cli)?;
        let outcome = config_a::replay(&self.input, &mut engine);
        let expected = metrics::expected_firings(self.cli, &self.steps);
        Ok(metrics::fixture_report(
            &self.fixture.id,
            self.cli,
            outcome,
            &expected,
            dump_unmatched,
        ))
    }

    /// Replay through the screen-state pipeline and account it.
    pub fn report_b(&self, dump_unmatched: usize) -> Result<ScreenFixtureReport, String> {
        let mut engine = CompiledPatterns::for_screen(self.cli)?;
        let dialogs = dialog::for_cli(self.cli);
        let outcome = config_b::replay(
            &self.input,
            self.fixture.id.cols,
            self.fixture.id.rows,
            &mut engine,
            &dialogs,
        )
        .map_err(|err| format!("{}: {err}", self.fixture.id))?;
        let expected = metrics::expected_screen_firings(self.cli, &self.steps);
        Ok(metrics::screen_fixture_report(
            &self.fixture.id,
            self.cli,
            outcome,
            &expected,
            dump_unmatched,
        ))
    }

    /// Replay through the structured-side-channel pipeline and account
    /// it. Claude-only, like the channels themselves; the channel
    /// artifacts belong to this configuration alone, so they load here
    /// rather than in [`LoadedFixture::load`].
    pub fn report_c(&self, dump_unmatched: usize) -> Result<ChannelFixtureReport, String> {
        let channels = config_c::load(&self.fixture.dir)
            .map_err(|err| format!("{}: {err}", self.fixture.id))?;
        let outcome = config_c::replay(
            &channels,
            &self.input,
            self.fixture.id.cols,
            self.fixture.id.rows,
        )
        .map_err(|err| format!("{}: {err}", self.fixture.id))?;
        let expected = metrics::expected_channel_firings(&self.steps);
        Ok(metrics::channel_fixture_report(
            &self.fixture.id,
            outcome,
            &expected,
            dump_unmatched,
        ))
    }
}

/// Replay one fixture through the text-matching pipeline and account it.
pub fn fixture_report_a(fixture: &Fixture, dump_unmatched: usize) -> Result<FixtureReport, String> {
    LoadedFixture::load(fixture)?.report_a(dump_unmatched)
}

/// Replay one fixture through the screen-state pipeline and account it.
pub fn fixture_report_b(
    fixture: &Fixture,
    dump_unmatched: usize,
) -> Result<ScreenFixtureReport, String> {
    LoadedFixture::load(fixture)?.report_b(dump_unmatched)
}

/// Replay one fixture through the structured-side-channel pipeline and
/// account it.
pub fn fixture_report_c(
    fixture: &Fixture,
    dump_unmatched: usize,
) -> Result<ChannelFixtureReport, String> {
    LoadedFixture::load(fixture)?.report_c(dump_unmatched)
}

/// One pattern's corpus-wide accounting over every fixture of one
/// (cli, version) pair.
#[derive(Debug, Serialize)]
pub struct AggregatedPatternRow {
    pub cli: String,
    pub version: String,
    pub id: &'static str,
    pub class: &'static str,
    pub role: &'static str,
    pub hits: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub false_negatives: Option<u64>,
    /// Shortfall ÷ expectation. Absent for ambient rows (no ground truth)
    /// and for rows the corpus never expects (a rate needs a denominator).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub false_negative_rate: Option<f64>,
}

/// One anchored pattern whose shortfall grew on a neighbouring version —
/// a measured drift datapoint, located to the fixtures it occurs in.
#[derive(Debug, Serialize)]
pub struct PatternRegression {
    pub id: &'static str,
    pub tuned_false_negatives: u64,
    pub false_negatives: u64,
    /// Neighbour fixtures whose shortfall exceeds the tuned version's
    /// same-scenario, same-dims fixture.
    pub fixtures: Vec<String>,
}

/// The measured drift of one neighbouring version against the tuned one,
/// for one CLI under one configuration.
#[derive(Debug, Serialize)]
pub struct DriftRow {
    pub cli: String,
    pub tuned: String,
    pub version: String,
    pub tuned_anchored_false_negatives: u64,
    pub anchored_false_negatives: u64,
    pub tuned_unrecognized_ratio: f64,
    pub unrecognized_ratio: f64,
    pub regressions: Vec<PatternRegression>,
}

/// Everything one configuration contributes to the metrics report.
#[derive(Debug, Serialize)]
pub struct ConfigurationMetrics {
    pub config: String,
    /// What one emission is in this configuration — the ratio denominators
    /// differ by construction and must never be mixed across blocks.
    pub denominator: &'static str,
    pub summary: Vec<SummaryRow>,
    pub patterns: Vec<AggregatedPatternRow>,
    pub drift: Vec<DriftRow>,
}

/// The whole collection: the three configuration blocks plus the logged
/// effort measurements, as written to `--out`.
#[derive(Debug, Serialize)]
pub struct MetricsReport {
    pub configurations: Vec<ConfigurationMetrics>,
    pub effort: EffortLog,
}

/// Aggregate per-fixture pattern rows into corpus-wide rows per
/// (cli, version). Fixtures arrive version-grouped from discovery, and the
/// aggregation preserves that order plus each set's own pattern order:
/// the rows live in a Vec in first-seen order, and a keyed index into it
/// carries the lookups so no row is ever found by scanning the
/// accumulated vector.
pub fn aggregate_patterns<F: SummaryFixture>(fixtures: &[F]) -> Vec<AggregatedPatternRow> {
    let mut rows: Vec<AggregatedPatternRow> = Vec::new();
    let mut index: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
    for fixture in fixtures {
        for pattern in fixture.patterns() {
            let position = *index
                .entry((fixture.cli(), fixture.version(), pattern.id))
                .or_insert_with(|| {
                    rows.push(AggregatedPatternRow {
                        cli: fixture.cli().to_string(),
                        version: fixture.version().to_string(),
                        id: pattern.id,
                        class: pattern.class,
                        role: pattern.role,
                        hits: 0,
                        expected: None,
                        false_negatives: None,
                        false_negative_rate: None,
                    });
                    rows.len() - 1
                });
            let row = &mut rows[position];
            row.hits += pattern.hits;
            if let Some(expected) = pattern.expected {
                *row.expected.get_or_insert(0) += expected;
            }
            if let Some(false_negatives) = pattern.false_negatives {
                *row.false_negatives.get_or_insert(0) += false_negatives;
            }
        }
    }
    for row in &mut rows {
        row.false_negative_rate = match (row.false_negatives, row.expected) {
            (Some(shortfall), Some(expected)) if expected > 0 => {
                Some(shortfall as f64 / expected as f64)
            }
            _ => None,
        };
    }
    rows
}

/// Compute the measured version drift from per-fixture reports and their
/// summary roll-up: for each CLI, every non-tuned version gets a row
/// comparing it against the tuned one, with the anchored patterns whose
/// shortfall grew listed as regressions.
///
/// The comparison is only meaningful over identical fixture sets — a
/// scenario recorded for one version but not the other would let the
/// shortfall totals compare different populations and misattribute the
/// delta as drift — so a neighbour whose (scenario, dims) set diverges
/// from the tuned version's is an error, not a guess.
pub fn version_drift<F: SummaryFixture>(
    fixtures: &[F],
    summary: &[SummaryRow],
) -> Result<Vec<DriftRow>, String> {
    // Per-fixture anchored shortfalls of the tuned versions, keyed by
    // (cli, scenario, dims, pattern) — the baseline a neighbour fixture's
    // shortfall is compared against.
    type FixtureKey = (String, String, (u16, u16), &'static str);
    let mut tuned_by_fixture: BTreeMap<FixtureKey, u64> = BTreeMap::new();
    for fixture in fixtures {
        let Some(cli) = Cli::parse(fixture.cli()) else {
            continue;
        };
        if fixture.version() != cli.tuned_version() {
            continue;
        }
        for pattern in anchored(fixture.patterns()) {
            tuned_by_fixture.insert(
                (
                    fixture.cli().to_string(),
                    fixture.scenario().to_string(),
                    fixture.dims(),
                    pattern.id,
                ),
                pattern.false_negatives.unwrap_or(0),
            );
        }
    }

    // The (scenario, dims) set of every (cli, version), for the parity
    // check above the comparison.
    type ScenarioDims = BTreeSet<(String, (u16, u16))>;
    let mut fixture_sets: BTreeMap<(String, String), ScenarioDims> = BTreeMap::new();
    for fixture in fixtures {
        fixture_sets
            .entry((fixture.cli().to_string(), fixture.version().to_string()))
            .or_default()
            .insert((fixture.scenario().to_string(), fixture.dims()));
    }

    // Corpus-wide anchored shortfalls per (cli, version, pattern).
    let mut totals: BTreeMap<(String, String, &str), u64> = BTreeMap::new();
    for fixture in fixtures {
        for pattern in anchored(fixture.patterns()) {
            *totals
                .entry((
                    fixture.cli().to_string(),
                    fixture.version().to_string(),
                    pattern.id,
                ))
                .or_insert(0) += pattern.false_negatives.unwrap_or(0);
        }
    }

    let mut rows = Vec::new();
    for row in summary {
        let Some(cli) = Cli::parse(&row.cli) else {
            continue;
        };
        let tuned = cli.tuned_version();
        if row.version == tuned {
            continue;
        }
        let tuned_summary = summary
            .iter()
            .find(|candidate| candidate.cli == row.cli && candidate.version == tuned);
        let Some(tuned_summary) = tuned_summary else {
            // A corpus without the tuned version has no baseline to
            // measure drift against; the neighbour's absolute numbers are
            // still in the summary block.
            continue;
        };

        let tuned_set = fixture_sets
            .get(&(row.cli.clone(), tuned.to_string()))
            .expect("summary rows derive from fixtures");
        let version_set = fixture_sets
            .get(&(row.cli.clone(), row.version.clone()))
            .expect("summary rows derive from fixtures");
        if tuned_set != version_set {
            let (scenario, (cols, dim_rows)) = tuned_set
                .symmetric_difference(version_set)
                .next()
                .expect("differing sets have a differing member");
            let missing_from = if version_set.contains(&(scenario.clone(), (*cols, *dim_rows))) {
                tuned
            } else {
                row.version.as_str()
            };
            return Err(format!(
                "cannot measure {} drift {} -> {}: the fixture sets differ — \
                 {scenario}-{cols}x{dim_rows} is missing from {missing_from}",
                row.cli, tuned, row.version
            ));
        }

        let mut regressions = Vec::new();
        for ((total_cli, version, id), &false_negatives) in &totals {
            if *total_cli != row.cli || *version != row.version {
                continue;
            }
            let tuned_false_negatives = totals
                .get(&(row.cli.clone(), tuned.to_string(), *id))
                .copied()
                .unwrap_or(0);
            if false_negatives <= tuned_false_negatives {
                continue;
            }
            let fixtures: Vec<String> = fixtures
                .iter()
                .filter(|fixture| fixture.cli() == row.cli && fixture.version() == row.version)
                .filter(|fixture| {
                    let shortfall = fixture
                        .patterns()
                        .iter()
                        .find(|pattern| pattern.id == *id)
                        .and_then(|pattern| pattern.false_negatives)
                        .unwrap_or(0);
                    let baseline = tuned_by_fixture
                        .get(&(
                            fixture.cli().to_string(),
                            fixture.scenario().to_string(),
                            fixture.dims(),
                            *id,
                        ))
                        .copied()
                        .expect("the parity check guarantees a tuned counterpart");
                    shortfall > baseline
                })
                .map(|fixture| fixture.fixture().to_string())
                .collect();
            regressions.push(PatternRegression {
                id,
                tuned_false_negatives,
                false_negatives,
                fixtures,
            });
        }

        rows.push(DriftRow {
            cli: row.cli.clone(),
            tuned: tuned.to_string(),
            version: row.version.clone(),
            tuned_anchored_false_negatives: tuned_summary.anchored_false_negatives,
            anchored_false_negatives: row.anchored_false_negatives,
            tuned_unrecognized_ratio: tuned_summary.unrecognized_ratio,
            unrecognized_ratio: row.unrecognized_ratio,
            regressions,
        });
    }
    Ok(rows)
}

fn anchored(patterns: &[PatternRow]) -> impl Iterator<Item = &PatternRow> {
    patterns.iter().filter(|pattern| pattern.role == "anchored")
}

/// One pattern fix applied during a re-green session: what changed and why.
#[derive(Debug, Deserialize, Serialize)]
pub struct PatternFix {
    pub id: String,
    pub fix: String,
}

/// The patterns one version bump required touching.
#[derive(Debug, Deserialize, Serialize)]
pub struct RegreenBump {
    pub version: String,
    pub patterns_touched: Vec<String>,
}

/// One measured re-green session: the pipeline green on the tuned version,
/// the neighbours replayed untouched, the failures fixed, the effort
/// logged. `committed: false` records that the fixes were reverted so the
/// committed sets stay tuned-version-pure and the drift stays measurable.
#[derive(Debug, Deserialize, Serialize)]
pub struct RegreenSession {
    pub config: String,
    pub cli: String,
    pub tuned: String,
    pub date: String,
    pub wall_clock_seconds: u64,
    pub committed: bool,
    pub bumps: Vec<RegreenBump>,
    pub fixes: Vec<PatternFix>,
    pub notes: String,
}

/// One measured add-a-pattern trial: an uncovered known surface, a pattern
/// authored for it, wall-clock from first artifact inspection to a green
/// suite. Shortfall counts before and after are keyed by configuration,
/// then version.
#[derive(Debug, Deserialize, Serialize)]
pub struct AddPatternTrial {
    pub cli: String,
    pub target: String,
    pub date: String,
    pub wall_clock_seconds: u64,
    pub committed: bool,
    pub patterns_added: Vec<String>,
    pub steps: Vec<String>,
    pub anchored_false_negatives_before: BTreeMap<String, BTreeMap<String, u64>>,
    pub anchored_false_negatives_after: BTreeMap<String, BTreeMap<String, u64>>,
    pub notes: String,
}

/// The committed effort log: the two metrics that price maintenance work
/// rather than classification coverage. Hand-logged because wall-clock
/// cannot be replayed out of the corpus; loading validates everything the
/// code itself pins — matcher ids, configuration names, CLI names, and
/// each session's claimed tuned version — so a typo cannot silently
/// detach a log entry from what it prices. What only the corpus can
/// confirm (bump versions, regression sets, after-counts) is held by the
/// collection lane instead.
#[derive(Debug, Deserialize, Serialize)]
pub struct EffortLog {
    pub methodology: String,
    pub drift_regreen: Vec<RegreenSession>,
    pub add_pattern_trials: Vec<AddPatternTrial>,
}

/// The configuration names the pipeline supports, in report order.
const CONFIGURATIONS: [&str; 3] = ["a", "b", "c"];

impl EffortLog {
    /// Every matcher id the log references, for validation.
    fn pattern_ids(&self) -> impl Iterator<Item = &str> {
        let session_ids = self.drift_regreen.iter().flat_map(|session| {
            session
                .bumps
                .iter()
                .flat_map(|bump| bump.patterns_touched.iter())
                .chain(session.fixes.iter().map(|fix| &fix.id))
        });
        let trial_ids = self
            .add_pattern_trials
            .iter()
            .flat_map(|trial| trial.patterns_added.iter());
        session_ids.chain(trial_ids).map(String::as_str)
    }
}

/// Load and validate the committed effort log.
pub fn load_effort_log(path: &Path) -> Result<EffortLog, String> {
    let raw = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let log: EffortLog =
        serde_json::from_str(&raw).map_err(|err| format!("{}: {err}", path.display()))?;

    let known: BTreeSet<&str> = PATTERNS
        .iter()
        .map(|spec| spec.id)
        .chain(SCREEN_PATTERNS.iter().map(|spec| spec.id))
        .chain(DIALOGS.iter().map(|spec| spec.id))
        .chain(CHANNEL_CLASSIFIERS.iter().map(|spec| spec.id))
        .collect();
    for id in log.pattern_ids() {
        if !known.contains(id) {
            return Err(format!(
                "{}: log references unknown matcher id {id}",
                path.display()
            ));
        }
    }

    for session in &log.drift_regreen {
        if !CONFIGURATIONS.contains(&session.config.as_str()) {
            return Err(format!(
                "{}: regreen session names unknown configuration '{}'",
                path.display(),
                session.config
            ));
        }
        let cli = Cli::parse(&session.cli).ok_or_else(|| {
            format!(
                "{}: regreen session names unknown cli '{}'",
                path.display(),
                session.cli
            )
        })?;
        if session.tuned != cli.tuned_version() {
            return Err(format!(
                "{}: regreen session claims {} was tuned at {}, but the {} sets are \
                 tuned at {}",
                path.display(),
                session.cli,
                session.tuned,
                session.cli,
                cli.tuned_version()
            ));
        }
    }
    for trial in &log.add_pattern_trials {
        if Cli::parse(&trial.cli).is_none() {
            return Err(format!(
                "{}: trial names unknown cli '{}'",
                path.display(),
                trial.cli
            ));
        }
        let counted_configs = trial
            .anchored_false_negatives_before
            .keys()
            .chain(trial.anchored_false_negatives_after.keys());
        for config in counted_configs {
            if !CONFIGURATIONS.contains(&config.as_str()) {
                return Err(format!(
                    "{}: trial counts shortfalls for unknown configuration '{config}'",
                    path.display()
                ));
            }
        }
    }
    Ok(log)
}

/// Replay every configuration over the discovered fixtures and assemble
/// the full metrics report around the logged effort. Each fixture's
/// shared artifacts are loaded once and replayed by all of its
/// configurations.
pub fn collect(fixtures: &[Fixture], effort: EffortLog) -> Result<MetricsReport, String> {
    let mut reports_a = Vec::with_capacity(fixtures.len());
    let mut reports_b = Vec::with_capacity(fixtures.len());
    let mut reports_c = Vec::new();
    for fixture in fixtures {
        let loaded = LoadedFixture::load(fixture)?;
        reports_a.push(loaded.report_a(0)?);
        reports_b.push(loaded.report_b(0)?);
        // The side channels exist only in the claude corpus.
        if fixture.id.cli == "claude" {
            reports_c.push(loaded.report_c(0)?);
        }
    }

    Ok(MetricsReport {
        configurations: vec![
            configuration_metrics(CONFIGURATIONS[0], "non-blank stripped lines", &reports_a)?,
            configuration_metrics(
                CONFIGURATIONS[1],
                "deduplicated screen rows at evaluation points",
                &reports_b,
            )?,
            configuration_metrics(
                CONFIGURATIONS[2],
                "hook events + transcript blocks + fallback-surface detections",
                &reports_c,
            )?,
        ],
        effort,
    })
}

fn configuration_metrics<F: SummaryFixture>(
    config: &str,
    denominator: &'static str,
    reports: &[F],
) -> Result<ConfigurationMetrics, String> {
    let summary = metrics::summarize(reports);
    let patterns = aggregate_patterns(reports);
    let drift = version_drift(reports, &summary)?;
    Ok(ConfigurationMetrics {
        config: config.to_string(),
        denominator,
        summary,
        patterns,
        drift,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::FixtureId;

    fn report(
        version: &str,
        scenario: &str,
        cols: u16,
        patterns: Vec<PatternRow>,
    ) -> FixtureReport {
        let id = FixtureId {
            cli: "claude".to_string(),
            version: version.to_string(),
            scenario: scenario.to_string(),
            cols,
            rows: 24,
        };
        FixtureReport {
            fixture: id.to_string(),
            cli: id.cli.clone(),
            version: id.version.clone(),
            scenario: id.scenario.clone(),
            cols: id.cols,
            rows: id.rows,
            lines: config_a::LineStats {
                total: 10,
                blank: 0,
                emissions: 10,
                matched: 8,
                unrecognized: 2,
                forced_segmentations: 0,
            },
            unrecognized_ratio: 0.2,
            patterns,
            guard_trips: Vec::new(),
            unmatched_top: Vec::new(),
        }
    }

    fn pattern(
        id: &'static str,
        role: &'static str,
        hits: u64,
        expected: Option<u64>,
    ) -> PatternRow {
        PatternRow {
            id,
            class: "test.class",
            role,
            hits,
            expected,
            false_negatives: expected.map(|count| count.saturating_sub(hits)),
        }
    }

    #[test]
    fn aggregation_sums_fixtures_and_computes_rates_per_version() {
        let reports = [
            report(
                "2.1.201",
                "one",
                80,
                vec![
                    pattern("claude/anchored", "anchored", 1, Some(2)),
                    pattern("claude/ambient", "ambient", 5, None),
                ],
            ),
            report(
                "2.1.201",
                "two",
                80,
                vec![
                    pattern("claude/anchored", "anchored", 2, Some(2)),
                    pattern("claude/ambient", "ambient", 3, None),
                ],
            ),
        ];
        let rows = aggregate_patterns(&reports);

        assert_eq!(rows.len(), 2);
        let anchored = &rows[0];
        assert_eq!(anchored.id, "claude/anchored");
        assert_eq!(anchored.hits, 3);
        assert_eq!(anchored.expected, Some(4));
        assert_eq!(anchored.false_negatives, Some(1));
        assert!((anchored.false_negative_rate.unwrap() - 0.25).abs() < 1e-9);
        let ambient = &rows[1];
        assert_eq!(ambient.hits, 8);
        assert_eq!(ambient.expected, None);
        assert_eq!(ambient.false_negative_rate, None);
    }

    #[test]
    fn aggregation_keeps_versions_apart_and_skips_rate_without_expectations() {
        let reports = [
            report(
                "2.1.201",
                "one",
                80,
                vec![pattern("claude/anchored", "anchored", 1, Some(0))],
            ),
            report(
                "2.1.202",
                "one",
                80,
                vec![pattern("claude/anchored", "anchored", 0, Some(1))],
            ),
        ];
        let rows = aggregate_patterns(&reports);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].version, "2.1.201");
        assert_eq!(
            rows[0].false_negative_rate, None,
            "nothing expected, so no rate"
        );
        assert_eq!(rows[1].version, "2.1.202");
        assert!((rows[1].false_negative_rate.unwrap() - 1.0).abs() < 1e-9);
    }

    #[test]
    fn drift_reports_regressed_patterns_with_their_fixtures() {
        // Green on the tuned version, red on the neighbour in one fixture.
        let reports = [
            report(
                "2.1.201",
                "compact",
                80,
                vec![pattern("claude/compact-result", "anchored", 1, Some(1))],
            ),
            report(
                "2.1.202",
                "compact",
                80,
                vec![pattern("claude/compact-result", "anchored", 0, Some(1))],
            ),
        ];
        let summary = metrics::summarize(&reports);
        let drift = version_drift(&reports, &summary).expect("fixture sets match");

        assert_eq!(drift.len(), 1);
        let row = &drift[0];
        assert_eq!(row.version, "2.1.202");
        assert_eq!(row.tuned, "2.1.201");
        assert_eq!(row.tuned_anchored_false_negatives, 0);
        assert_eq!(row.anchored_false_negatives, 1);
        assert_eq!(row.regressions.len(), 1);
        assert_eq!(row.regressions[0].id, "claude/compact-result");
        assert_eq!(
            row.regressions[0].fixtures,
            ["claude/2.1.202/compact-80x24"]
        );
    }

    #[test]
    fn drift_ignores_shortfalls_the_tuned_version_shares() {
        // A by-construction shortfall present at every version is not
        // drift: the neighbour is no worse than the tuned baseline.
        let reports = [
            report(
                "2.1.201",
                "parallel-tools",
                120,
                vec![pattern("claude/tool-read-result", "anchored", 1, Some(2))],
            ),
            report(
                "2.1.202",
                "parallel-tools",
                120,
                vec![pattern("claude/tool-read-result", "anchored", 1, Some(2))],
            ),
        ];
        let summary = metrics::summarize(&reports);
        let drift = version_drift(&reports, &summary).expect("fixture sets match");

        assert_eq!(drift.len(), 1);
        assert!(
            drift[0].regressions.is_empty(),
            "shared shortfall reported as a regression: {:?}",
            drift[0].regressions
        );
    }

    #[test]
    fn drift_refuses_fixture_sets_that_do_not_match_the_tuned_version() {
        // The neighbour records a scenario the tuned version does not:
        // the totals would compare different populations, so the
        // measurement refuses instead of guessing a zero baseline.
        let reports = [
            report(
                "2.1.201",
                "compact",
                80,
                vec![pattern("claude/compact-result", "anchored", 1, Some(1))],
            ),
            report(
                "2.1.202",
                "compact",
                80,
                vec![pattern("claude/compact-result", "anchored", 1, Some(1))],
            ),
            report(
                "2.1.202",
                "clear",
                80,
                vec![pattern("claude/compact-result", "anchored", 0, Some(1))],
            ),
        ];
        let summary = metrics::summarize(&reports);
        let err = version_drift(&reports, &summary).unwrap_err();
        assert!(
            err.contains("clear-80x24") && err.contains("missing from 2.1.201"),
            "error names the asymmetric fixture: {err}"
        );
    }

    fn trial(cli: &str) -> AddPatternTrial {
        AddPatternTrial {
            cli: cli.to_string(),
            target: "test".to_string(),
            date: "2026-08-04".to_string(),
            wall_clock_seconds: 1,
            committed: true,
            patterns_added: vec!["claude/tool-read-result".to_string()],
            steps: Vec::new(),
            anchored_false_negatives_before: BTreeMap::new(),
            anchored_false_negatives_after: BTreeMap::new(),
            notes: String::new(),
        }
    }

    fn session(config: &str, cli: &str, tuned: &str) -> RegreenSession {
        RegreenSession {
            config: config.to_string(),
            cli: cli.to_string(),
            tuned: tuned.to_string(),
            date: "2026-08-04".to_string(),
            wall_clock_seconds: 1,
            committed: false,
            bumps: Vec::new(),
            fixes: Vec::new(),
            notes: String::new(),
        }
    }

    fn empty_log() -> EffortLog {
        EffortLog {
            methodology: "test".to_string(),
            drift_regreen: Vec::new(),
            add_pattern_trials: Vec::new(),
        }
    }

    /// Round one log through a temp file and return what loading says.
    fn load(log: &EffortLog, name: &str) -> Result<EffortLog, String> {
        let dir = std::env::temp_dir().join(format!(
            "detection-spike-effort-log-{name}-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        let path = dir.join("effort-log.json");
        fs::write(&path, serde_json::to_string(log).expect("serializes")).expect("write log");
        let result = load_effort_log(&path);
        fs::remove_dir_all(&dir).expect("cleanup");
        result
    }

    #[test]
    fn effort_log_rejects_unknown_matcher_ids() {
        let mut bad = trial("claude");
        bad.patterns_added = vec!["claude/no-such-pattern".to_string()];
        let log = EffortLog {
            add_pattern_trials: vec![bad],
            ..empty_log()
        };
        let err = load(&log, "unknown-id").unwrap_err();
        assert!(
            err.contains("claude/no-such-pattern"),
            "error names the unknown id: {err}"
        );
    }

    #[test]
    fn effort_log_rejects_metadata_the_code_does_not_pin() {
        // An unknown configuration, an unknown CLI, and a tuned-version
        // claim diverging from the code's pin each fail loading — the
        // report must not embed metadata the collection contradicts.
        let cases: [(EffortLog, &str, &str); 5] = [
            (
                EffortLog {
                    drift_regreen: vec![session("z", "claude", "2.1.201")],
                    ..empty_log()
                },
                "unknown-config",
                "unknown configuration 'z'",
            ),
            (
                EffortLog {
                    drift_regreen: vec![session("a", "goose", "2.1.201")],
                    ..empty_log()
                },
                "unknown-cli",
                "unknown cli 'goose'",
            ),
            (
                EffortLog {
                    drift_regreen: vec![session("a", "claude", "2.1.200")],
                    ..empty_log()
                },
                "stale-tuned",
                "tuned at 2.1.201",
            ),
            (
                EffortLog {
                    add_pattern_trials: vec![trial("goose")],
                    ..empty_log()
                },
                "trial-cli",
                "unknown cli 'goose'",
            ),
            (
                EffortLog {
                    add_pattern_trials: vec![{
                        let mut bad = trial("claude");
                        bad.anchored_false_negatives_after
                            .insert("z".to_string(), BTreeMap::new());
                        bad
                    }],
                    ..empty_log()
                },
                "trial-config",
                "unknown configuration 'z'",
            ),
        ];
        for (log, name, needle) in &cases {
            let err = load(log, name).unwrap_err();
            assert!(
                err.contains(needle),
                "{name}: error carries {needle:?}: {err}"
            );
        }
    }

    #[test]
    fn effort_log_accepts_metadata_matching_the_pins() {
        let log = EffortLog {
            drift_regreen: vec![
                session("a", "claude", "2.1.201"),
                session("c", "codex", "0.145.0"),
            ],
            add_pattern_trials: vec![trial("claude")],
            ..empty_log()
        };
        load(&log, "valid").expect("pinned metadata loads");
    }
}
