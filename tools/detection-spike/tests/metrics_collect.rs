//! Full-corpus collection lane for the `metrics` subcommand.
//!
//! The collection replays every committed fixture through all three
//! configurations, so this lane is the cross-configuration counterpart of
//! the per-configuration replay lanes. It asserts:
//!
//! - **Determinism**: two independent collections serialize to identical
//!   reports — the property that makes the metrics reviewable and the lane
//!   safe for the PR tier on all three OSes.
//! - **Shape**: all three configurations report, each over its own stated
//!   denominator, with the side-channel block claude-only.
//! - **Log-against-corpus consistency**: the committed effort log's
//!   re-green entries must agree with the drift the corpus actually
//!   measures — every logged fix names a measured regression, and each
//!   bump's touched-pattern set equals the measured regressed set for that
//!   version, empty sets included. A corpus re-record that changes the
//!   drift forces the log to be re-measured rather than silently rotting.
//! - **The trial's corpus witness**: the logged after-counts match the
//!   measured shortfalls, and the trial pattern's tuned-version row shows
//!   the fold's by-construction under-fire.

use std::collections::BTreeSet;
use std::path::PathBuf;

use agent_bridge_detection_spike::collect::{self, MetricsReport};
use agent_bridge_detection_spike::corpus;

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

fn effort_log_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("effort-log.json")
}

fn collect_once() -> MetricsReport {
    let effort = collect::load_effort_log(&effort_log_path()).expect("committed effort log loads");
    let fixtures = corpus::discover(&corpus_root(), &["claude".to_string(), "codex".to_string()])
        .expect("corpus discovery over the committed fixtures");
    collect::collect(&fixtures, effort).expect("full-corpus collection")
}

#[test]
fn collection_is_deterministic() {
    let first = serde_json::to_string(&collect_once()).expect("report serializes");
    let second = serde_json::to_string(&collect_once()).expect("report serializes");
    assert_eq!(
        first, second,
        "two collections disagree — the metrics are not deterministic"
    );
}

#[test]
fn every_configuration_reports_over_its_own_denominator() {
    let report = collect_once();
    let configs: Vec<&str> = report
        .configurations
        .iter()
        .map(|configuration| configuration.config.as_str())
        .collect();
    assert_eq!(configs, ["a", "b", "c"]);

    let denominators: BTreeSet<&str> = report
        .configurations
        .iter()
        .map(|configuration| configuration.denominator)
        .collect();
    assert_eq!(denominators.len(), 3, "denominators must stay distinct");

    for configuration in &report.configurations {
        assert!(
            !configuration.summary.is_empty(),
            "config {} reports no summary rows",
            configuration.config
        );
        for row in &configuration.summary {
            assert!(
                (0.0..=1.0).contains(&row.unrecognized_ratio),
                "config {} {}/{}: ratio {} out of range",
                configuration.config,
                row.cli,
                row.version,
                row.unrecognized_ratio
            );
        }
        assert!(
            !configuration.patterns.is_empty(),
            "config {} reports no pattern rows",
            configuration.config
        );
    }

    let side_channel = &report.configurations[2];
    assert!(
        side_channel.summary.iter().all(|row| row.cli == "claude"),
        "the side-channel block replays artifacts only the claude corpus records"
    );
}

#[test]
fn measured_drift_agrees_with_the_logged_regreen_entries() {
    let report = collect_once();
    for session in &report.effort.drift_regreen {
        let configuration = report
            .configurations
            .iter()
            .find(|configuration| configuration.config == session.config)
            .expect("logged session names a known configuration");

        for fix in &session.fixes {
            assert!(
                configuration.drift.iter().any(|row| {
                    row.cli == session.cli
                        && row
                            .regressions
                            .iter()
                            .any(|regression| regression.id == fix.id)
                }),
                "logged fix {} has no measured regression in config {}",
                fix.id,
                session.config
            );
        }

        for bump in &session.bumps {
            let row = configuration
                .drift
                .iter()
                .find(|row| row.cli == session.cli && row.version == bump.version)
                .expect("logged bump names a measured version");
            let measured: BTreeSet<&str> = row
                .regressions
                .iter()
                .map(|regression| regression.id)
                .collect();
            let logged: BTreeSet<&str> = bump.patterns_touched.iter().map(String::as_str).collect();
            assert_eq!(
                measured, logged,
                "config {} {}/{}: measured drift diverges from the logged bump",
                session.config, session.cli, bump.version
            );
        }
    }
}

#[test]
fn trial_after_counts_match_the_measured_corpus() {
    let report = collect_once();
    for trial in &report.effort.add_pattern_trials {
        for (config_name, versions) in &trial.anchored_false_negatives_after {
            let configuration = report
                .configurations
                .iter()
                .find(|configuration| &configuration.config == config_name)
                .expect("logged trial names a known configuration");
            for (version, logged) in versions {
                let row = configuration
                    .summary
                    .iter()
                    .find(|row| row.cli == trial.cli && &row.version == version)
                    .expect("logged trial names a measured version");
                assert_eq!(
                    row.anchored_false_negatives, *logged,
                    "config {config_name} {}/{version}: logged after-count diverges from \
                     the measured corpus",
                    trial.cli
                );
            }
        }
    }
}

#[test]
fn trial_pattern_shows_the_fold_shortfall_on_the_tuned_version() {
    let report = collect_once();
    let text_matching = &report.configurations[0];
    let row = text_matching
        .patterns
        .iter()
        .find(|row| {
            row.cli == "claude" && row.version == "2.1.201" && row.id == "claude/tool-read-result"
        })
        .expect("the trial pattern has a tuned-version row");

    // Two parallel-tools fixtures × two Read events each; the 80×24 stream
    // repaints the fold enough to cover both expectations, the 120×40
    // stream paints it once — the by-construction per-event under-fire.
    assert_eq!(row.expected, Some(4));
    assert_eq!(row.hits, 4);
    assert_eq!(row.false_negatives, Some(1));
    assert!((row.false_negative_rate.unwrap() - 0.25).abs() < 1e-9);
}
