//! Full-corpus replay lanes for the structured-side-channel configuration.
//!
//! One lane per claude version — configuration (c) is claude-only because
//! no other CLI's corpus records hook payloads and transcripts. The lanes
//! keep the shape of the other configurations': every fixture replays twice
//! and must serialize to identical reports, and the tuned version
//! (claude 2.1.201 — the version the classifier table was read against,
//! beside the documented hook contract) additionally pins its results
//! exactly. The neighbouring versions replay through the same lanes but
//! pin nothing beyond determinism and the corpus's structural shape: their
//! shortfalls are the version-drift measurement.
//!
//! The tuned-version pins are this configuration's findings:
//!
//! - **Zero unrecognized emissions**: every hook event, notification type,
//!   transcript record type, and content block in the tuned capture is
//!   structurally known — the typed-channel contrast to the text
//!   configurations' unrecognized shares.
//! - **No anchored misses**: everything the step log says happened arrived
//!   on a primary channel or, for the two fallback surfaces with ground
//!   truth (the ask-degraded permission dialog, the interrupted notice),
//!   was detected on the screen.
//! - **The control misses exactly where it must**: the Ctrl+C interrupt
//!   fires no hook, so `claude/hook-interrupt-signal` is red in the
//!   interrupt scenarios and green-by-absence everywhere else — the mirror
//!   of the idle notification, which the byte-stream configurations carry
//!   as a control and this one classifies first-class.
//! - **Full correlation**: every tool call the hooks saw is also in the
//!   transcript under the same `tool_use_id`, and vice versa.
//!
//! Two shapes are structural for the committed corpus and assert on every
//! version: the clear fixtures replay two transcript files (the pre-clear
//! file first — the tailer's path-switch evidence), and no fixture ever
//! has more than one `PreToolUse` decision pending at once. The latter is
//! the re-scoped multi-pending finding, asserted per fixture in
//! [`config_c_multi_pending_approvals`]: the CLI serialises batched tool
//! calls when a synchronous hook is installed, so the recorded bracketing
//! is strictly `Pre(A) Post(A) Pre(B) Post(B)` — if a version ever breaks
//! that, the lane failure *is* the finding.

use std::collections::BTreeSet;
use std::path::PathBuf;

use agent_bridge_detection_spike::config_c;
use agent_bridge_detection_spike::corpus::{self, Fixture};
use agent_bridge_detection_spike::metrics::{self, ChannelFixtureReport};
use agent_bridge_detection_spike::pacing::PacedInput;

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

fn claude_fixtures(version: &str) -> Vec<Fixture> {
    let fixtures = corpus::discover(&corpus_root(), &["claude".to_string()])
        .expect("corpus discovery over the committed fixtures");
    let selected: Vec<Fixture> = fixtures
        .into_iter()
        .filter(|fixture| fixture.id.version == version)
        .collect();
    assert!(
        !selected.is_empty(),
        "no fixtures for claude/{version} — corpus layout changed?"
    );
    selected
}

fn replay_once(fixture: &Fixture) -> ChannelFixtureReport {
    let input = config_c::load(&fixture.dir).expect("fixture side-channel artifacts load");
    let paced = PacedInput::load(&fixture.dir).expect("fixture input pair loads");
    let steps = corpus::load_steps(&fixture.dir).expect("fixture step log loads");
    let outcome = config_c::replay(&input, &paced, fixture.id.cols, fixture.id.rows)
        .expect("committed fixture replays through the side channels");
    let expected = metrics::expected_channel_firings(&steps);
    metrics::channel_fixture_report(&fixture.id, outcome, &expected, 0)
}

/// Replay every fixture twice and assert byte-identical reports, plus the
/// structural shape every lane demands. Returns the first replay's reports.
fn replay_lane(version: &str) -> Vec<ChannelFixtureReport> {
    let mut reports = Vec::new();
    for fixture in claude_fixtures(version) {
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
            first.channel.hook_events > 0,
            "{}: no hook events",
            fixture.id
        );
        assert!(
            first.channel.transcript_blocks > 0,
            "{}: no transcript blocks",
            fixture.id
        );
        assert!(
            (0.0..=1.0).contains(&first.unrecognized_ratio),
            "{}: ratio {} out of range",
            fixture.id,
            first.unrecognized_ratio
        );
        // The tailer's path-switch evidence: `/clear` advertises a second
        // path and both files replay, pre-clear first.
        let expected_files = if first.scenario == "clear" { 2 } else { 1 };
        assert_eq!(
            first.channel.transcript_files, expected_files,
            "{}: transcript files diverge from the committed layout",
            fixture.id
        );
        assert!(
            first.channel.max_pending_approvals <= 1,
            "{}: {} PreToolUse decisions pending at once — the serial-bracketing \
             finding no longer holds for this version",
            fixture.id,
            first.channel.max_pending_approvals
        );
        reports.push(first);
    }
    reports
}

/// The control classifiers required to miss in one fixture of the tuned
/// version. A control that unexpectedly fires is as much a lane failure as
/// an anchored classifier that misses.
fn pinned_control_misses(scenario: &str) -> BTreeSet<&'static str> {
    let mut misses = BTreeSet::new();
    if scenario == "interrupt" {
        // The Ctrl+C byte stops the generation without a hook; only the
        // fallback screen carries the evidence.
        misses.insert("claude/hook-interrupt-signal");
    }
    misses
}

/// Tool pairs each scenario of the corpus records.
fn pinned_pair_count(scenario: &str) -> usize {
    match scenario {
        "approval-arrow-key" | "approval-number-key" | "tool-lifecycle" => 1,
        "parallel-tools" => 2,
        _ => 0,
    }
}

fn assert_tuned_version_pins(reports: &[ChannelFixtureReport]) {
    for report in reports {
        assert_eq!(
            report.channel.unrecognized, 0,
            "{}: the classifier table covers every shape the tuned version emits \
             (unmatched: {:?})",
            report.fixture, report.unmatched_top
        );

        let anchored_misses: BTreeSet<&str> = report
            .patterns
            .iter()
            .filter(|row| row.role == "anchored" && row.false_negatives.unwrap_or(0) > 0)
            .map(|row| row.id)
            .collect();
        assert!(
            anchored_misses.is_empty(),
            "{}: anchored classifiers missed: {anchored_misses:?}",
            report.fixture
        );

        let control_misses: BTreeSet<&str> = report
            .patterns
            .iter()
            .filter(|row| row.role == "control" && row.false_negatives.unwrap_or(0) > 0)
            .map(|row| row.id)
            .collect();
        let expected_controls = pinned_control_misses(&report.scenario);
        assert_eq!(
            control_misses, expected_controls,
            "{}: control misses diverge from the tuned-version pin",
            report.fixture
        );

        assert_eq!(
            report.tool_pairs.len(),
            pinned_pair_count(&report.scenario),
            "{}: tool-pair count diverges from the recorded scenario",
            report.fixture
        );
        for pair in &report.tool_pairs {
            assert!(
                pair.correlated(),
                "{}: {} not correlated across both channels: {pair:?}",
                report.fixture,
                pair.tool_use_id
            );
        }
    }
}

#[test]
fn replay_config_c_claude_2_1_200() {
    replay_lane("2.1.200");
}

#[test]
fn replay_config_c_claude_2_1_201() {
    let reports = replay_lane("2.1.201");
    assert_tuned_version_pins(&reports);
}

#[test]
fn replay_config_c_claude_2_1_202() {
    replay_lane("2.1.202");
}

/// The multi-pending assertion, re-scoped to what the captures show
/// (2026-07-13 sitting): the CLI serialises batched tool calls under a
/// synchronous hook — `Pre(A) Post(A) Pre(B) Post(B)`, never two pending
/// `PreToolUse` at once — stably across all three pinned versions at both
/// sizes. The parallel-tools fixtures are the evidence, and they stay the
/// `tool_use_id`-correlation input: both calls sit in one assistant turn,
/// so both must still resolve independently across both channels.
#[test]
fn config_c_multi_pending_approvals() {
    let fixtures: Vec<Fixture> = corpus::discover(&corpus_root(), &["claude".to_string()])
        .expect("corpus discovery over the committed fixtures")
        .into_iter()
        .filter(|fixture| fixture.id.scenario == "parallel-tools")
        .collect();
    assert_eq!(
        fixtures.len(),
        6,
        "three pinned versions at two sizes — corpus layout changed?"
    );

    for fixture in fixtures {
        let report = replay_once(&fixture);
        assert_eq!(
            report.channel.max_pending_approvals, 1,
            "{}: the serial-bracketing finding no longer holds",
            fixture.id
        );
        assert_eq!(report.tool_pairs.len(), 2, "{}", fixture.id);
        assert_ne!(
            report.tool_pairs[0].tool_use_id, report.tool_pairs[1].tool_use_id,
            "{}: batched calls must keep distinct tool_use_ids",
            fixture.id
        );
        for pair in &report.tool_pairs {
            assert!(
                pair.correlated(),
                "{}: {} not correlated across both channels: {pair:?}",
                fixture.id,
                pair.tool_use_id
            );
        }
    }
}
