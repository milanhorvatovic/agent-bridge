//! Detection-spike binary — replays captured fixtures through a prototype
//! detection pipeline and reports per-pattern hit/miss accounting.
//!
//! **Deprecated** — the measurement campaign has concluded; the binary
//! stays only to keep the corpus-guarding CI lanes running (see the
//! crate-level note in `lib.rs`).
//!
//! ```text
//! detection-spike replay  [--config a|b|c] [--cli claude|codex] [--version <v>]
//!                         [--all-versions] [--corpus <dir>] [--out <file>]
//!                         [--dump-unmatched <n>]
//! detection-spike metrics [--corpus <dir>] [--effort-log <file>] [--out <file>]
//! ```
//!
//! `replay` walks the corpus, replays every selected fixture through the
//! chosen pipeline configuration, prints one step line per fixture, and — if
//! `--out` is given — writes the full accounting as JSON. All versions
//! replay by default; `--version` narrows to one, `--all-versions` states
//! the default explicitly. Configuration `a` is the text-matching pipeline,
//! `b` the screen-state pipeline, `c` the structured-side-channel pipeline —
//! claude-only, because only the claude corpus records hook payloads and
//! transcripts.
//!
//! `metrics` replays the whole corpus through all three configurations in
//! one run and prints the collected measurements — per-configuration
//! summaries, measured version drift against the tuned versions, and the
//! logged effort — writing the full report as JSON via `--out`.

// This binary legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

use std::path::{Path, PathBuf};
use std::process::exit;

use agent_bridge_detection_spike::collect;
use agent_bridge_detection_spike::corpus::{self, Fixture};
use agent_bridge_detection_spike::metrics::{self, PatternRow, SummaryRow};
use agent_bridge_detection_spike::patterns::Cli;
use agent_bridge_detection_spike::{Failure, print_step};

const USAGE: &str = "usage: detection-spike replay [--config a|b|c] [--cli claude|codex] \
[--version <v>] [--all-versions] [--corpus <dir>] [--out <file>] [--dump-unmatched <n>] \
| metrics [--corpus <dir>] [--effort-log <file>] [--out <file>]";

struct ReplayConfig {
    config: String,
    clis: Vec<String>,
    version: Option<String>,
    corpus: PathBuf,
    out: Option<PathBuf>,
    dump_unmatched: usize,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            config: "a".to_string(),
            clis: vec!["claude".to_string(), "codex".to_string()],
            version: None,
            corpus: default_corpus_root(),
            out: None,
            dump_unmatched: 0,
        }
    }
}

/// The committed corpus, resolved from this crate's manifest directory so
/// the tool works from any working directory inside the checkout. The path
/// is baked at compile time, which is fine for a dev tool only ever run
/// from its own workspace; `--corpus` overrides it for anything else.
fn default_corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

/// The committed effort log, resolved the same way; `--effort-log`
/// overrides it.
fn default_effort_log() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("effort-log.json")
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        exit(2);
    };
    let result = match command.as_str() {
        "replay" => parse_replay(&args[1..]).and_then(run_replay),
        "metrics" => parse_metrics(&args[1..]).and_then(run_metrics),
        other => {
            eprintln!("unknown command '{other}'. {USAGE}");
            exit(2);
        }
    };
    match result {
        Ok(()) => println!("detection-spike mode={command} result=pass"),
        Err(failure) => {
            print_step(failure.step, "fail", &failure.detail);
            eprintln!(
                "detection-spike: step {} failed: {}",
                failure.step, failure.detail
            );
            exit(failure.code);
        }
    }
}

fn usage(detail: impl Into<String>) -> Failure {
    Failure::new("args", 2, detail)
}

fn parse_replay(args: &[String]) -> Result<ReplayConfig, Failure> {
    let mut config = ReplayConfig::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                let value = next_value(&mut iter, "--config")?;
                if value != "a" && value != "b" && value != "c" {
                    return Err(usage(format!(
                        "unknown configuration '{value}' — 'a' (text matching), \
                         'b' (screen state), and 'c' (structured side channels) exist"
                    )));
                }
                config.config = value;
            }
            "--cli" => {
                let value = next_value(&mut iter, "--cli")?;
                if Cli::parse(&value).is_none() {
                    return Err(usage(format!("unknown cli '{value}' (claude or codex)")));
                }
                config.clis = vec![value];
            }
            "--version" => config.version = Some(next_value(&mut iter, "--version")?),
            // The default; accepted so invocations can state it explicitly.
            "--all-versions" => config.version = None,
            "--corpus" => config.corpus = PathBuf::from(next_value(&mut iter, "--corpus")?),
            "--out" => config.out = Some(PathBuf::from(next_value(&mut iter, "--out")?)),
            "--dump-unmatched" => {
                let value = next_value(&mut iter, "--dump-unmatched")?;
                config.dump_unmatched = value.parse().map_err(|_| {
                    usage(format!("--dump-unmatched needs a number, got '{value}'"))
                })?;
            }
            other => return Err(usage(format!("unknown flag '{other}'. {USAGE}"))),
        }
    }
    Ok(config)
}

fn next_value(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> Result<String, Failure> {
    iter.next()
        .cloned()
        .ok_or_else(|| usage(format!("{flag} needs a value")))
}

struct MetricsConfig {
    corpus: PathBuf,
    effort_log: PathBuf,
    out: Option<PathBuf>,
}

fn parse_metrics(args: &[String]) -> Result<MetricsConfig, Failure> {
    let mut config = MetricsConfig {
        corpus: default_corpus_root(),
        effort_log: default_effort_log(),
        out: None,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--corpus" => config.corpus = PathBuf::from(next_value(&mut iter, "--corpus")?),
            "--effort-log" => {
                config.effort_log = PathBuf::from(next_value(&mut iter, "--effort-log")?);
            }
            "--out" => config.out = Some(PathBuf::from(next_value(&mut iter, "--out")?)),
            other => return Err(usage(format!("unknown flag '{other}'. {USAGE}"))),
        }
    }
    Ok(config)
}

fn run_metrics(config: MetricsConfig) -> Result<(), Failure> {
    let effort = collect::load_effort_log(&config.effort_log)
        .map_err(|err| Failure::new("effort-log", 94, err))?;
    let clis = vec!["claude".to_string(), "codex".to_string()];
    let fixtures =
        corpus::discover(&config.corpus, &clis).map_err(|err| Failure::new("discover", 90, err))?;
    let report =
        collect::collect(&fixtures, effort).map_err(|err| Failure::new("collect", 91, err))?;

    for configuration in &report.configurations {
        for row in &configuration.summary {
            print_step(
                "summary",
                "ok",
                &format!(
                    "config={} {}/{} fixtures={} emissions={} unrecognized={} ratio={:.3} \
                     anchored_fn={}",
                    configuration.config,
                    row.cli,
                    row.version,
                    row.fixtures,
                    row.emissions,
                    row.unrecognized,
                    row.unrecognized_ratio,
                    row.anchored_false_negatives,
                ),
            );
        }
        for drift in &configuration.drift {
            print_step(
                "drift",
                "ok",
                &format!(
                    "config={} {} {}->{} anchored_fn {}->{} ratio {:.3}->{:.3} regressed={}",
                    configuration.config,
                    drift.cli,
                    drift.tuned,
                    drift.version,
                    drift.tuned_anchored_false_negatives,
                    drift.anchored_false_negatives,
                    drift.tuned_unrecognized_ratio,
                    drift.unrecognized_ratio,
                    drift.regressions.len(),
                ),
            );
        }
    }
    for session in &report.effort.drift_regreen {
        print_step(
            "effort",
            "ok",
            &format!(
                "regreen config={} cli={} wall_clock_s={} patterns_touched={} committed={}",
                session.config,
                session.cli,
                session.wall_clock_seconds,
                session.fixes.len(),
                session.committed,
            ),
        );
    }
    for trial in &report.effort.add_pattern_trials {
        print_step(
            "effort",
            "ok",
            &format!(
                "trial cli={} wall_clock_s={} patterns_added={} committed={}",
                trial.cli,
                trial.wall_clock_seconds,
                trial.patterns_added.len(),
                trial.committed,
            ),
        );
    }

    let Some(out) = &config.out else {
        return Ok(());
    };
    let json = serde_json::to_string_pretty(&report)
        .map_err(|err| Failure::new("report", 93, format!("serialize: {err}")))?;
    std::fs::write(out, json)
        .map_err(|err| Failure::new("report", 93, format!("{}: {err}", out.display())))?;
    print_step("report", "ok", &format!("written to {}", out.display()));
    Ok(())
}

fn run_replay(mut config: ReplayConfig) -> Result<(), Failure> {
    // The side-channel configuration replays artifacts only the claude
    // corpus records: an explicit codex ask is an error, the default CLI
    // set narrows silently.
    if config.config == "c" {
        if config.clis.len() == 1 && config.clis[0] != "claude" {
            return Err(usage(format!(
                "configuration 'c' replays the structured side channels, which \
                 the {} corpus does not record",
                config.clis[0]
            )));
        }
        config.clis = vec!["claude".to_string()];
    }

    let fixtures = corpus::discover(&config.corpus, &config.clis)
        .map_err(|err| Failure::new("discover", 90, err))?;
    let selected: Vec<_> = fixtures
        .into_iter()
        .filter(|fixture| {
            config
                .version
                .as_ref()
                .is_none_or(|version| fixture.id.version == *version)
        })
        .collect();
    if selected.is_empty() {
        return Err(Failure::new(
            "discover",
            90,
            format!(
                "no fixtures under {} for {:?} version {:?}",
                config.corpus.display(),
                config.clis,
                config.version
            ),
        ));
    }

    match config.config.as_str() {
        "a" => replay_config_a(&config, &selected),
        "b" => replay_config_b(&config, &selected),
        "c" => replay_config_c(&config, &selected),
        // parse_replay admits nothing else.
        other => unreachable!("unvalidated configuration '{other}'"),
    }
}

fn replay_config_a(config: &ReplayConfig, selected: &[Fixture]) -> Result<(), Failure> {
    let mut reports = Vec::with_capacity(selected.len());
    for fixture in selected {
        let report = collect::fixture_report_a(fixture, config.dump_unmatched)
            .map_err(|err| Failure::new("replay", 91, err))?;

        print_step(
            "replay",
            "ok",
            &format!(
                "{} emissions={} unrecognized={} ratio={:.3} anchored_fn={} guard_trips={}",
                report.fixture,
                report.lines.emissions,
                report.lines.unrecognized,
                report.unrecognized_ratio,
                anchored_false_negatives(&report.patterns),
                report.guard_trips.len(),
            ),
        );
        reports.push(report);
    }

    let summary = metrics::summarize(&reports);
    print_summary(&summary);
    write_report(config, reports, summary)
}

fn replay_config_b(config: &ReplayConfig, selected: &[Fixture]) -> Result<(), Failure> {
    let mut reports = Vec::with_capacity(selected.len());
    for fixture in selected {
        let report = collect::fixture_report_b(fixture, config.dump_unmatched)
            .map_err(|err| Failure::new("replay", 91, err))?;

        print_step(
            "replay",
            "ok",
            &format!(
                "{} eval_points={} emissions={} unrecognized={} ratio={:.3} \
                 anchored_fn={} dialogs={} guard_trips={}",
                report.fixture,
                report.screen.eval_points,
                report.screen.emissions,
                report.screen.unrecognized,
                report.unrecognized_ratio,
                anchored_false_negatives(&report.patterns),
                report.dialogs.len(),
                report.guard_trips.len(),
            ),
        );
        reports.push(report);
    }

    let summary = metrics::summarize(&reports);
    print_summary(&summary);
    write_report(config, reports, summary)
}

fn replay_config_c(config: &ReplayConfig, selected: &[Fixture]) -> Result<(), Failure> {
    let mut reports = Vec::with_capacity(selected.len());
    for fixture in selected {
        let report = collect::fixture_report_c(fixture, config.dump_unmatched)
            .map_err(|err| Failure::new("replay", 91, err))?;

        print_step(
            "replay",
            "ok",
            &format!(
                "{} hook_events={} transcript_blocks={} fallback={} emissions={} \
                 unrecognized={} ratio={:.3} anchored_fn={} pairs={} max_pending={}",
                report.fixture,
                report.channel.hook_events,
                report.channel.transcript_blocks,
                report.channel.fallback_detections,
                report.channel.emissions,
                report.channel.unrecognized,
                report.unrecognized_ratio,
                anchored_false_negatives(&report.patterns),
                report.tool_pairs.len(),
                report.channel.max_pending_approvals,
            ),
        );
        reports.push(report);
    }

    let summary = metrics::summarize(&reports);
    print_summary(&summary);
    write_report(config, reports, summary)
}

fn anchored_false_negatives(patterns: &[PatternRow]) -> u64 {
    patterns
        .iter()
        .filter(|pattern| pattern.role == "anchored")
        .filter_map(|pattern| pattern.false_negatives)
        .sum()
}

fn print_summary(summary: &[SummaryRow]) {
    for row in summary {
        print_step(
            "summary",
            "ok",
            &format!(
                "{}/{} fixtures={} emissions={} unrecognized={} ratio={:.3} anchored_fn={}",
                row.cli,
                row.version,
                row.fixtures,
                row.emissions,
                row.unrecognized,
                row.unrecognized_ratio,
                row.anchored_false_negatives,
            ),
        );
    }
}

fn write_report<F: serde::Serialize>(
    config: &ReplayConfig,
    fixtures: Vec<F>,
    summary: Vec<SummaryRow>,
) -> Result<(), Failure> {
    let Some(out) = &config.out else {
        return Ok(());
    };
    let run = metrics::RunReport {
        config: config.config.clone(),
        fixtures,
        summary,
    };
    let json = serde_json::to_string_pretty(&run)
        .map_err(|err| Failure::new("report", 93, format!("serialize: {err}")))?;
    std::fs::write(out, json)
        .map_err(|err| Failure::new("report", 93, format!("{}: {err}", out.display())))?;
    print_step("report", "ok", &format!("written to {}", out.display()));
    Ok(())
}
