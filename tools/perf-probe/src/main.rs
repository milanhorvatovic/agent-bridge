//! perf-probe — the command-line face of the measurement lanes.
//!
//! Subcommands map one-to-one onto the lanes (see the library root for what
//! each one measures and why):
//!
//!   perf-probe soak     [--seconds N | --minutes N] [--rate L] [--line-bytes B]
//!                       [--checksum-every K] [--monitor-out FILE]
//!                       [--monitor-interval-secs S] [--warmup-secs S] [--out FILE]
//!   perf-probe replay    --fixture DIR [--fixture DIR ...]
//!                       [--seconds N | --minutes N] [--content generated|recorded]
//!                       [--idle-threshold-ms T] [--idle-divisor D]
//!                       [--monitor-out FILE] [--monitor-interval-secs S]
//!                       [--warmup-secs S] [--out FILE]
//!   perf-probe bench-latency    [--samples N] [--interval-us I] [--discard N]
//!                               [--load DIR ...] [--out FILE]
//!   perf-probe bench-throughput [--lines N] [--sessions K] [--line-bytes B]
//!                               [--load DIR ...] [--out FILE]
//!   perf-probe compare   --baseline FILE --current FILE
//!
//! Exit discipline, sized for CI wiring:
//!
//! - The soak and replay lanes exit non-zero when the run found what it
//!   exists to find — corruption or a resource-budget miss. Those lanes run
//!   where an absolute verdict is meaningful, and a red lane is the alarm.
//! - The bench lanes exit zero whenever the measurement *completed*; their
//!   verdicts live in the report. On shared runners an absolute latency
//!   verdict is noise, and the gate that holds changes to account is
//!   `compare`, which exits non-zero on a regression past its threshold.
//!
//! Every lane writes the same report JSON (`--out`), which is the artifact
//! everything downstream — the gate, the write-up, the keep-or-downgrade
//! decision per budget — reads.

// This crate legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

use std::path::PathBuf;
use std::time::Duration;

use agent_bridge_perf_probe::{
    compare, latency, platform_report, print_step, replay, soak, throughput,
};

/// Exit codes, distinct per class so a red CI lane is diagnosable from the
/// status alone.
const EXIT_USAGE: i32 = 2;
const EXIT_RUN_FAILED: i32 = 10;
const EXIT_INTEGRITY: i32 = 11;
const EXIT_RESOURCES: i32 = 12;
const EXIT_REGRESSION: i32 = 13;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((command, rest)) = args.split_first() else {
        eprintln!(
            "perf-probe: usage: perf-probe <soak|replay|bench-latency|bench-throughput|compare> [options]"
        );
        std::process::exit(EXIT_USAGE);
    };
    println!("perf-probe {}", platform_report());
    let outcome = match command.as_str() {
        "soak" => run_soak(rest),
        "replay" => run_replay(rest),
        "bench-latency" => run_bench_latency(rest),
        "bench-throughput" => run_bench_throughput(rest),
        "compare" => run_compare(rest),
        other => Err(UsageError(format!("unknown subcommand: {other}")).into()),
    };
    match outcome {
        Ok(()) => {}
        Err(failure) => {
            eprintln!("perf-probe: {}", failure.detail);
            std::process::exit(failure.code);
        }
    }
}

struct Failure {
    code: i32,
    detail: String,
}

struct UsageError(String);

impl From<UsageError> for Failure {
    fn from(err: UsageError) -> Self {
        Failure {
            code: EXIT_USAGE,
            detail: err.0,
        }
    }
}

fn run_failed(detail: String) -> Failure {
    Failure {
        code: EXIT_RUN_FAILED,
        detail,
    }
}

/// A tiny flag walker: every lane's options are `--name value` pairs, and a
/// misspelled flag must be an error rather than a silently defaulted knob.
struct Args<'a> {
    rest: &'a [String],
    index: usize,
}

impl<'a> Args<'a> {
    fn new(rest: &'a [String]) -> Self {
        Self { rest, index: 0 }
    }

    fn next_flag(&mut self) -> Option<&'a str> {
        let flag = self.rest.get(self.index)?;
        self.index += 1;
        Some(flag)
    }

    fn value(&mut self, flag: &str) -> Result<&'a str, UsageError> {
        let value = self
            .rest
            .get(self.index)
            .ok_or_else(|| UsageError(format!("{flag} needs a value")))?;
        self.index += 1;
        Ok(value)
    }

    fn parsed<T: std::str::FromStr>(&mut self, flag: &str) -> Result<T, UsageError> {
        let raw = self.value(flag)?;
        raw.parse()
            .map_err(|_| UsageError(format!("invalid {flag} value: {raw}")))
    }
}

/// The shared duration pair: `--minutes` and `--seconds` compose, so
/// `--minutes 30` and `--seconds 90` are both natural spellings.
fn add_duration(total: &mut Option<Duration>, add: Duration) {
    *total = Some(total.unwrap_or_default() + add);
}

fn run_soak(rest: &[String]) -> Result<(), Failure> {
    let mut options = soak::Options::default();
    let mut duration = None;
    let mut out = None;
    let mut args = Args::new(rest);
    while let Some(flag) = args.next_flag() {
        match flag {
            "--minutes" => add_duration(
                &mut duration,
                Duration::from_secs(args.parsed::<u64>(flag)? * 60),
            ),
            "--seconds" => add_duration(&mut duration, Duration::from_secs(args.parsed(flag)?)),
            "--rate" => options.lines_per_second = args.parsed(flag)?,
            "--line-bytes" => options.line_bytes = args.parsed(flag)?,
            "--checksum-every" => options.checksum_every = args.parsed(flag)?,
            "--monitor-out" => options.monitor_out = Some(PathBuf::from(args.value(flag)?)),
            "--monitor-interval-secs" => {
                options.monitor_interval = Duration::from_secs(args.parsed(flag)?);
            }
            "--warmup-secs" => options.warmup = Duration::from_secs(args.parsed(flag)?),
            "--out" => out = Some(PathBuf::from(args.value(flag)?)),
            other => return Err(UsageError(format!("soak: unknown flag {other}")).into()),
        }
    }
    if let Some(duration) = duration {
        options.duration = duration;
    }

    let (report, outcome) = soak::run(&options).map_err(run_failed)?;
    write_report_out(&report, out.as_deref())?;
    judge_endurance(
        outcome.findings.clean(),
        &outcome.findings.summary(),
        outcome.monitor.as_ref(),
    )
}

fn run_replay(rest: &[String]) -> Result<(), Failure> {
    let mut options = replay::Options {
        fixture_dirs: Vec::new(),
        build: replay::BuildOptions::default(),
        monitor_out: None,
        monitor_interval: agent_bridge_perf_probe::monitor::DEFAULT_INTERVAL,
        warmup: agent_bridge_perf_probe::monitor::DEFAULT_WARMUP,
    };
    let mut duration = None;
    let mut out = None;
    let mut args = Args::new(rest);
    while let Some(flag) = args.next_flag() {
        match flag {
            "--fixture" => options.fixture_dirs.push(PathBuf::from(args.value(flag)?)),
            "--minutes" => add_duration(
                &mut duration,
                Duration::from_secs(args.parsed::<u64>(flag)? * 60),
            ),
            "--seconds" => add_duration(&mut duration, Duration::from_secs(args.parsed(flag)?)),
            "--content" => {
                options.build.mode = match args.value(flag)? {
                    "generated" => replay::Mode::Generated,
                    "recorded" => replay::Mode::Recorded,
                    other => {
                        return Err(UsageError(format!(
                            "--content is generated or recorded, not {other}"
                        ))
                        .into());
                    }
                };
            }
            "--idle-threshold-ms" => {
                options.build.idle_threshold = Duration::from_millis(args.parsed(flag)?);
            }
            "--idle-divisor" => options.build.idle_divisor = args.parsed(flag)?,
            "--monitor-out" => options.monitor_out = Some(PathBuf::from(args.value(flag)?)),
            "--monitor-interval-secs" => {
                options.monitor_interval = Duration::from_secs(args.parsed(flag)?);
            }
            "--warmup-secs" => options.warmup = Duration::from_secs(args.parsed(flag)?),
            "--out" => out = Some(PathBuf::from(args.value(flag)?)),
            other => return Err(UsageError(format!("replay: unknown flag {other}")).into()),
        }
    }
    if options.fixture_dirs.is_empty() {
        return Err(UsageError("replay needs at least one --fixture".to_string()).into());
    }
    if let Some(duration) = duration {
        options.build.duration = duration;
    }

    let (report, outcome) = replay::run(&options).map_err(run_failed)?;
    write_report_out(&report, out.as_deref())?;
    judge_endurance(
        outcome.faults == 0,
        &format!("{} integrity fault(s)", outcome.faults),
        outcome.monitor.as_ref(),
    )
}

/// The shared verdict of the two endurance lanes: red on corruption, red on
/// a resource budget, in that order of blame.
fn judge_endurance(
    intact: bool,
    integrity_summary: &str,
    monitor: Option<&agent_bridge_perf_probe::monitor::Assessment>,
) -> Result<(), Failure> {
    if !intact {
        print_step("verdict", "fail", integrity_summary);
        return Err(Failure {
            code: EXIT_INTEGRITY,
            detail: format!("the stream did not survive intact: {integrity_summary}"),
        });
    }
    if let Some(assessment) = monitor
        && !assessment.within_budget()
    {
        let detail = format!(
            "resource budgets missed: descriptor delta {}, resident growth {} bytes",
            assessment.descriptor_delta, assessment.rss_growth_bytes,
        );
        print_step("verdict", "fail", &detail);
        return Err(Failure {
            code: EXIT_RESOURCES,
            detail,
        });
    }
    print_step("verdict", "pass", integrity_summary);
    Ok(())
}

fn run_bench_latency(rest: &[String]) -> Result<(), Failure> {
    let mut options = latency::Options::default();
    let mut load = Vec::new();
    let mut out = None;
    let mut args = Args::new(rest);
    while let Some(flag) = args.next_flag() {
        match flag {
            "--samples" => options.samples = args.parsed(flag)?,
            "--interval-us" => options.marker_interval_us = args.parsed(flag)?,
            "--discard" => options.discard = args.parsed(flag)?,
            "--load" => load.push(PathBuf::from(args.value(flag)?)),
            "--out" => out = Some(PathBuf::from(args.value(flag)?)),
            other => return Err(UsageError(format!("bench-latency: unknown flag {other}")).into()),
        }
    }
    run_bench(out.as_deref(), &load, || {
        latency::run(&options).map(|(report, _)| report)
    })
}

fn run_bench_throughput(rest: &[String]) -> Result<(), Failure> {
    let mut options = throughput::Options::default();
    let mut load = Vec::new();
    let mut out = None;
    let mut args = Args::new(rest);
    while let Some(flag) = args.next_flag() {
        match flag {
            "--lines" => options.lines = args.parsed(flag)?,
            "--sessions" => options.sessions = args.parsed(flag)?,
            "--line-bytes" => options.line_bytes = args.parsed(flag)?,
            "--load" => load.push(PathBuf::from(args.value(flag)?)),
            "--out" => out = Some(PathBuf::from(args.value(flag)?)),
            other => {
                return Err(UsageError(format!("bench-throughput: unknown flag {other}")).into());
            }
        }
    }
    run_bench(out.as_deref(), &load, || {
        throughput::run(&options).map(|(report, _)| report)
    })
}

/// The shared bench frame: optional bimodal background load around the
/// measurement, the report annotated with what was running, and a zero exit
/// whenever the measurement completed — the verdicts live in the report and
/// the regression gate, not the exit status (see the module note).
fn run_bench(
    out: Option<&std::path::Path>,
    load: &[PathBuf],
    lane: impl FnOnce() -> Result<agent_bridge_perf_probe::report::Report, String>,
) -> Result<(), Failure> {
    let background = if load.is_empty() {
        None
    } else {
        // Built to outlast any plausible measurement; the stop() below ends
        // it the moment the lane is done.
        Some(
            replay::BackgroundLoad::start(load, Duration::from_secs(60 * 60))
                .map_err(run_failed)?,
        )
    };
    let mut result = lane();
    if let Some(background) = background {
        let stopped = background.stop().map_err(run_failed)?;
        print_step("background-load", "pass", &stopped);
        if let Ok(report) = &mut result {
            report.workload = format!("{}+bimodal-load", report.workload);
            report.note(format!(
                "measured while a generated-content replay of {load:?} streamed in a second session"
            ));
        }
    }
    let report = result.map_err(run_failed)?;
    write_report_out(&report, out)?;
    for measurement in report.exceeded() {
        print_step(
            "budget",
            "fail",
            &format!(
                "{} exceeded its budget on this machine — the report carries the verdict",
                measurement.name
            ),
        );
    }
    Ok(())
}

fn run_compare(rest: &[String]) -> Result<(), Failure> {
    let mut baseline = None;
    let mut current = None;
    let mut args = Args::new(rest);
    while let Some(flag) = args.next_flag() {
        match flag {
            "--baseline" => baseline = Some(PathBuf::from(args.value(flag)?)),
            "--current" => current = Some(PathBuf::from(args.value(flag)?)),
            other => return Err(UsageError(format!("compare: unknown flag {other}")).into()),
        }
    }
    let baseline = baseline.ok_or_else(|| UsageError("compare needs --baseline".to_string()))?;
    let current = current.ok_or_else(|| UsageError("compare needs --current".to_string()))?;
    match compare::run(&baseline, &current) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Failure {
            code: EXIT_REGRESSION,
            detail: "regression past the threshold — see the compare steps above".to_string(),
        }),
        Err(detail) => Err(run_failed(detail)),
    }
}

fn write_report_out(
    report: &agent_bridge_perf_probe::report::Report,
    out: Option<&std::path::Path>,
) -> Result<(), Failure> {
    if let Some(path) = out {
        report.write(path).map_err(run_failed)?;
        print_step("report", "pass", &format!("written to {}", path.display()));
    }
    Ok(())
}
