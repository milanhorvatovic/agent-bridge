//! Detection-spike binary — replays captured fixtures through a prototype
//! detection pipeline and reports per-pattern hit/miss accounting.
//!
//! ```text
//! detection-spike replay [--config a|b] [--cli claude|codex] [--version <v>]
//!                        [--all-versions] [--corpus <dir>] [--out <file>]
//!                        [--dump-unmatched <n>]
//! ```
//!
//! `replay` walks the corpus, replays every selected fixture through the
//! chosen pipeline configuration, prints one step line per fixture, and — if
//! `--out` is given — writes the full accounting as JSON. All versions
//! replay by default; `--version` narrows to one, `--all-versions` states
//! the default explicitly. Configuration `a` is the text-matching pipeline,
//! `b` the screen-state pipeline; the side-channel configuration lands as a
//! later step of the same spike.

// This binary legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

use std::path::{Path, PathBuf};
use std::process::exit;

use agent_bridge_detection_spike::corpus::{self, Fixture};
use agent_bridge_detection_spike::metrics::{self, PatternRow, SummaryRow};
use agent_bridge_detection_spike::pacing::PacedInput;
use agent_bridge_detection_spike::patterns::{Cli, CompiledPatterns};
use agent_bridge_detection_spike::{Failure, config_a, config_b, dialog, print_step};

const USAGE: &str = "usage: detection-spike replay [--config a|b] [--cli claude|codex] \
[--version <v>] [--all-versions] [--corpus <dir>] [--out <file>] [--dump-unmatched <n>]";

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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        eprintln!("{USAGE}");
        exit(2);
    };
    let result = match command.as_str() {
        "replay" => parse_replay(&args[1..]).and_then(run_replay),
        other => {
            eprintln!("unknown command '{other}'. {USAGE}");
            exit(2);
        }
    };
    match result {
        Ok(()) => println!("detection-spike mode=replay result=pass"),
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
                if value != "a" && value != "b" {
                    return Err(usage(format!(
                        "unknown configuration '{value}' — 'a' (text matching) and \
                         'b' (screen state) exist so far"
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

fn run_replay(config: ReplayConfig) -> Result<(), Failure> {
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
        // parse_replay admits nothing else.
        other => unreachable!("unvalidated configuration '{other}'"),
    }
}

/// Load the artifacts every configuration replays from.
fn load_fixture(fixture: &Fixture) -> Result<(Cli, PacedInput, Vec<corpus::StepRecord>), Failure> {
    let cli = Cli::parse(&fixture.id.cli).ok_or_else(|| {
        Failure::new(
            "fixture",
            91,
            format!("{}: no pattern set for this cli", fixture.id),
        )
    })?;
    let input = PacedInput::load(&fixture.dir).map_err(|err| Failure::new("fixture", 91, err))?;
    let steps = corpus::load_steps(&fixture.dir).map_err(|err| Failure::new("fixture", 91, err))?;
    Ok((cli, input, steps))
}

fn replay_config_a(config: &ReplayConfig, selected: &[Fixture]) -> Result<(), Failure> {
    let mut reports = Vec::with_capacity(selected.len());
    for fixture in selected {
        let (cli, input, steps) = load_fixture(fixture)?;

        // A fresh engine per fixture: the safety guard's disabled set is
        // per-session state and must not leak across replays.
        let mut engine =
            CompiledPatterns::for_cli(cli).map_err(|err| Failure::new("patterns", 92, err))?;
        let outcome = config_a::replay(&input, &mut engine);
        let expected = metrics::expected_firings(cli, &steps);
        let report =
            metrics::fixture_report(&fixture.id, cli, outcome, &expected, config.dump_unmatched);

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
        let (cli, input, steps) = load_fixture(fixture)?;

        let mut engine =
            CompiledPatterns::for_screen(cli).map_err(|err| Failure::new("patterns", 92, err))?;
        let dialogs = dialog::for_cli(cli);
        let outcome = config_b::replay(
            &input,
            fixture.id.cols,
            fixture.id.rows,
            &mut engine,
            &dialogs,
        )
        .map_err(|err| Failure::new("fixture", 91, format!("{}: {err}", fixture.id)))?;
        let expected = metrics::expected_screen_firings(cli, &steps);
        let report = metrics::screen_fixture_report(
            &fixture.id,
            cli,
            outcome,
            &expected,
            config.dump_unmatched,
        );

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
