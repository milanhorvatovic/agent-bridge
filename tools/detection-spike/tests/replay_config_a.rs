//! Full-corpus replay lanes for the text-matching pipeline configuration.
//!
//! One lane per (cli, version) in the committed corpus. Each lane replays
//! every fixture of its pair and asserts two things:
//!
//! - **Determinism**: two independent replays serialize to identical
//!   reports. This is the property that makes the numbers reviewable and
//!   the lane safe for the PR tier on all three OSes — a differential
//!   result across OSes is a finding, not flake.
//! - **Pinned misses on the tuned versions** (claude 2.1.201 and codex
//!   0.145.0, the versions the pattern set was read out of): every anchored
//!   pattern fires as expected except the Read-result fold's
//!   by-construction per-event shortfall, and every control pattern misses
//!   exactly as the captures say it must. The pins are exact, so a pattern
//!   that silently starts or stops firing fails the lane either way.
//!
//! The neighbouring versions replay through the same lanes but pin nothing
//! beyond determinism: their shortfalls are the version-drift measurement,
//! collected by the metrics step, and freezing them here would turn
//! measured data into a contract.

use std::collections::BTreeSet;
use std::path::PathBuf;

use agent_bridge_detection_spike::config_a;
use agent_bridge_detection_spike::corpus::{self, Fixture};
use agent_bridge_detection_spike::metrics::{self, FixtureReport};
use agent_bridge_detection_spike::pacing::PacedInput;
use agent_bridge_detection_spike::patterns::{Cli, CompiledPatterns};

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

fn fixtures_for(cli: &str, version: &str) -> Vec<Fixture> {
    let fixtures = corpus::discover(&corpus_root(), &[cli.to_string()])
        .expect("corpus discovery over the committed fixtures");
    let selected: Vec<Fixture> = fixtures
        .into_iter()
        .filter(|fixture| fixture.id.version == version)
        .collect();
    assert!(
        !selected.is_empty(),
        "no fixtures for {cli}/{version} — corpus layout changed?"
    );
    selected
}

fn replay_once(fixture: &Fixture) -> FixtureReport {
    let cli = Cli::parse(&fixture.id.cli).expect("corpus cli has a pattern set");
    let input = PacedInput::load(&fixture.dir).expect("fixture input pair loads");
    let steps = corpus::load_steps(&fixture.dir).expect("fixture step log loads");
    let mut engine = CompiledPatterns::for_cli(cli).expect("pattern set compiles");
    let outcome = config_a::replay(&input, &mut engine);
    let expected = metrics::expected_firings(cli, &steps);
    metrics::fixture_report(&fixture.id, cli, outcome, &expected, 0)
}

/// Replay every fixture twice and assert byte-identical reports, plus the
/// baseline shape every lane demands. Returns the first replay's reports.
fn replay_lane(cli: &str, version: &str) -> Vec<FixtureReport> {
    let mut reports = Vec::new();
    for fixture in fixtures_for(cli, version) {
        let first = replay_once(&fixture);
        let second = replay_once(&fixture);
        let first_json = serde_json::to_string(&first).expect("report serializes");
        let second_json = serde_json::to_string(&second).expect("report serializes");
        assert_eq!(
            first_json, second_json,
            "{}: two replays disagree — replay is not deterministic",
            fixture.id
        );

        assert!(first.lines.emissions > 0, "{}: empty replay", fixture.id);
        assert!(
            (0.0..=1.0).contains(&first.unrecognized_ratio),
            "{}: ratio {} out of range",
            fixture.id,
            first.unrecognized_ratio
        );
        assert!(
            first.guard_trips.is_empty(),
            "{}: safety guard tripped on the prototype set: {:?}",
            fixture.id,
            first.guard_trips
        );
        assert_eq!(
            first.lines.forced_segmentations, 0,
            "{}: the line cap fired on a real fixture",
            fixture.id
        );
        reports.push(first);
    }
    reports
}

/// The anchored patterns allowed (and required) to miss in one fixture of a
/// tuned version, by scenario and recorded width.
fn pinned_anchored_misses(cli: &str, scenario: &str, cols: u16) -> BTreeSet<&'static str> {
    let mut misses = BTreeSet::new();
    if cli == "claude" && scenario == "parallel-tools" && cols == 120 {
        // Two Read events fold into one `Read 2 files` paint. At 80×24 the
        // repaint churn duplicates the line often enough to cover both
        // expectations; at 120×40 it paints once, and the per-event
        // shortfall the add-a-pattern trial demonstrated remains.
        misses.insert("claude/tool-read-result");
    }
    misses
}

/// The control patterns required to miss in one fixture of a tuned version,
/// by scenario. A control that unexpectedly fires is as much a lane failure
/// as an anchored pattern that misses.
fn pinned_control_misses(cli: &str, scenario: &str) -> BTreeSet<&'static str> {
    let mut misses = BTreeSet::new();
    match cli {
        "claude" => {
            if scenario.starts_with("approval-") {
                // The TUI paints the dialog title cursor-mashed; the spaced
                // phrasing never survives.
                misses.insert("claude/permission-title-spaced");
            }
            if scenario == "idle-notification" {
                // The idle notification paints nothing at all.
                misses.insert("claude/idle-notice");
            }
        }
        // Every codex scenario opens on the trust prompt, and its startup
        // paint always mashes the spaced phrasing.
        "codex" => {
            misses.insert("codex/trust-title-spaced");
        }
        other => panic!("no pins defined for cli {other}"),
    }
    misses
}

fn assert_tuned_version_pins(reports: &[FixtureReport]) {
    for report in reports {
        let anchored_misses: BTreeSet<&str> = report
            .patterns
            .iter()
            .filter(|row| row.role == "anchored" && row.false_negatives.unwrap_or(0) > 0)
            .map(|row| row.id)
            .collect();
        let expected_anchored: BTreeSet<&str> =
            pinned_anchored_misses(&report.cli, &report.scenario, report.cols)
                .into_iter()
                .collect();
        assert_eq!(
            anchored_misses, expected_anchored,
            "{}: anchored misses diverge from the tuned-version pin",
            report.fixture
        );

        let control_misses: BTreeSet<&str> = report
            .patterns
            .iter()
            .filter(|row| row.role == "control" && row.false_negatives.unwrap_or(0) > 0)
            .map(|row| row.id)
            .collect();
        let expected_controls: BTreeSet<&str> =
            pinned_control_misses(&report.cli, &report.scenario)
                .into_iter()
                .collect();
        assert_eq!(
            control_misses, expected_controls,
            "{}: control misses diverge from the tuned-version pin",
            report.fixture
        );
    }
}

#[test]
fn replay_config_a_claude_2_1_200() {
    replay_lane("claude", "2.1.200");
}

#[test]
fn replay_config_a_claude_2_1_201() {
    let reports = replay_lane("claude", "2.1.201");
    assert_tuned_version_pins(&reports);
}

#[test]
fn replay_config_a_claude_2_1_202() {
    replay_lane("claude", "2.1.202");
}

#[test]
fn replay_config_a_codex_0_144_6() {
    replay_lane("codex", "0.144.6");
}

#[test]
fn replay_config_a_codex_0_145_0() {
    let reports = replay_lane("codex", "0.145.0");
    assert_tuned_version_pins(&reports);
}

#[test]
fn replay_config_a_codex_0_146_0() {
    replay_lane("codex", "0.146.0");
}

#[test]
fn recorded_pacing_reassembles_every_committed_stream() {
    // The chunker must reproduce each fixture's byte stream exactly from its
    // recorded read boundaries — a mis-sliced replay would silently change
    // what the pipeline sees at chunk edges.
    for cli in ["claude", "codex"] {
        let fixtures = corpus::discover(&corpus_root(), &[cli.to_string()])
            .expect("corpus discovery over the committed fixtures");
        for fixture in fixtures {
            let input = PacedInput::load(&fixture.dir).expect("fixture input pair loads");
            let rebuilt: Vec<u8> = input
                .iter_chunks()
                .flat_map(|(chunk, _)| chunk.to_vec())
                .collect();
            assert_eq!(
                rebuilt, input.bytes,
                "{}: chunks do not reassemble the recorded stream",
                fixture.id
            );
        }
    }
}
