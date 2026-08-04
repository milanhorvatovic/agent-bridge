//! Full-corpus replay lanes for the screen-state pipeline configuration.
//!
//! One lane per (cli, version) in the committed corpus, the same shape as
//! the text-matching lanes: every fixture replays twice and must serialize
//! to identical reports (the property that makes the numbers reviewable and
//! the lane safe for the PR tier on all three OSes), and the tuned versions
//! (claude 2.1.201, codex 0.145.0 — the versions the screen needles and
//! dialog anchors were read out of) additionally pin their misses exactly.
//!
//! The pinned misses are this configuration's own findings, not carried
//! over from the text lanes:
//!
//! - `/clear` wipes the first turn before any quiet period samples it, and
//!   at 80×24 the compact scenario's first turn scrolls out of the
//!   viewport — evaluation-point sampling cannot see content that never
//!   survives to a settled screen.
//! - The screen folds the parallel Read calls into one `Read 2 files` line,
//!   so the Read-result pattern (added as the metrics step's add-a-pattern
//!   trial) fires once for two events and one shortfall remains.
//! - The mashed dialog titles and the busy-status hint never appear on a
//!   rendered screen, and the idle notification still paints nothing.
//!
//! The dialog detector's tuned-version behavior is pinned too: the trust
//! dialog opens once in every fixture, the permission/approval dialog once
//! in the approval scenarios, each with its full option set extracted. The
//! neighbouring versions replay through the same lanes but pin nothing
//! beyond determinism: their shortfalls are the version-drift measurement.

use std::collections::BTreeSet;
use std::path::PathBuf;

use agent_bridge_detection_spike::config_b;
use agent_bridge_detection_spike::corpus::{self, Fixture};
use agent_bridge_detection_spike::dialog;
use agent_bridge_detection_spike::metrics::{self, ScreenFixtureReport};
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

fn replay_once(fixture: &Fixture) -> ScreenFixtureReport {
    let cli = Cli::parse(&fixture.id.cli).expect("corpus cli has a pattern set");
    let input = PacedInput::load(&fixture.dir).expect("fixture input pair loads");
    let steps = corpus::load_steps(&fixture.dir).expect("fixture step log loads");
    let mut engine = CompiledPatterns::for_screen(cli).expect("screen set compiles");
    let outcome = config_b::replay(
        &input,
        fixture.id.cols,
        fixture.id.rows,
        &mut engine,
        &dialog::for_cli(cli),
    )
    .expect("committed fixture replays through the virtual terminal");
    let expected = metrics::expected_screen_firings(cli, &steps);
    metrics::screen_fixture_report(&fixture.id, cli, outcome, &expected, 0)
}

/// Replay every fixture twice and assert byte-identical reports, plus the
/// baseline shape every lane demands. Returns the first replay's reports.
fn replay_lane(cli: &str, version: &str) -> Vec<ScreenFixtureReport> {
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

        assert!(
            first.screen.eval_points > 0,
            "{}: no evaluation points",
            fixture.id
        );
        assert!(first.screen.emissions > 0, "{}: empty replay", fixture.id);
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
        reports.push(first);
    }
    reports
}

/// The anchored patterns allowed (and required) to miss in one fixture of a
/// tuned version, by scenario and recorded width.
fn pinned_anchored_misses(cli: &str, scenario: &str, cols: u16) -> BTreeSet<&'static str> {
    let mut misses = BTreeSet::new();
    if cli == "claude" {
        // `/clear` wipes the first turn before any quiet period samples it,
        // at either width.
        if scenario == "clear" {
            misses.insert("claude/screen-response-bullet");
        }
        // At 80×24 the compact scenario's first turn scrolls out of the
        // viewport between evaluation points; at 120×40 it stays visible.
        if scenario == "compact" && cols == 80 {
            misses.insert("claude/screen-response-bullet");
        }
        // The two parallel Read calls fold into one `Read 2 files` line:
        // the covering pattern fires once for two expected events, so one
        // per-event shortfall remains by construction.
        if scenario == "parallel-tools" {
            misses.insert("claude/screen-tool-result-read");
        }
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
                // The screen shows the spaced title; the stream's mashed
                // artefact structurally cannot appear on it.
                misses.insert("claude/screen-permission-title-mashed");
            }
            if scenario == "idle-notification" {
                // The idle notification paints nothing on any surface.
                misses.insert("claude/screen-idle-notice");
            }
        }
        // Every codex scenario opens on the trust prompt, spaced on the
        // screen — the mashed control misses everywhere.
        "codex" => {
            misses.insert("codex/screen-trust-title-mashed");
        }
        other => panic!("no pins defined for cli {other}"),
    }
    misses
}

/// The dialog appearances required in one fixture of a tuned version:
/// (dialog id, extracted option count).
fn pinned_dialogs(cli: &str, scenario: &str) -> Vec<(&'static str, usize)> {
    let mut dialogs = Vec::new();
    match cli {
        "claude" => {
            // The startup trust dialog opens once in every fixture.
            dialogs.push(("claude/screen-dialog-trust", 2));
            if scenario.starts_with("approval-") {
                dialogs.push(("claude/screen-dialog-permission", 2));
            }
        }
        "codex" => {
            dialogs.push(("codex/screen-dialog-trust", 2));
            if scenario.starts_with("approval-") {
                dialogs.push(("codex/screen-dialog-approval", 3));
            }
        }
        other => panic!("no pins defined for cli {other}"),
    }
    dialogs
}

fn assert_tuned_version_pins(reports: &[ScreenFixtureReport]) {
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

        let sightings: Vec<(&str, usize)> = report
            .dialogs
            .iter()
            .map(|sighting| (sighting.id, sighting.options.len()))
            .collect();
        let expected_dialogs = pinned_dialogs(&report.cli, &report.scenario);
        assert_eq!(
            sightings, expected_dialogs,
            "{}: dialog appearances diverge from the tuned-version pin",
            report.fixture
        );
    }
}

#[test]
fn replay_config_b_claude_2_1_200() {
    replay_lane("claude", "2.1.200");
}

#[test]
fn replay_config_b_claude_2_1_201() {
    let reports = replay_lane("claude", "2.1.201");
    assert_tuned_version_pins(&reports);
}

#[test]
fn replay_config_b_claude_2_1_202() {
    replay_lane("claude", "2.1.202");
}

#[test]
fn replay_config_b_codex_0_144_6() {
    replay_lane("codex", "0.144.6");
}

#[test]
fn replay_config_b_codex_0_145_0() {
    let reports = replay_lane("codex", "0.145.0");
    assert_tuned_version_pins(&reports);
}

#[test]
fn replay_config_b_codex_0_146_0() {
    replay_lane("codex", "0.146.0");
}
