//! The endurance lane: stream continuously for a scripted duration and check
//! every line of it.
//!
//! Duration is the whole point. Five minutes of streaming proves the read
//! path works; it does not surface a descriptor leaked once a minute or a
//! buffer that grows a kilobyte an hour. Half an hour does, which is why the
//! lane is long enough to be inconvenient.
//!
//! The load is a generated stream rather than a scripted one, for a reason
//! that matters to the result: at a thousand lines a second for thirty
//! minutes the scenario would otherwise have to carry 1.8 million lines of
//! literal text. Deriving them from their line numbers keeps the load source
//! honest — the reader knows what every line should say, so "no corruption"
//! is checked rather than assumed — and keeps the scenario file four lines
//! long.
//!
//! Pacing is a target, not a promise. The child writes against an absolute
//! schedule, so the run takes about as long as it was asked to even where
//! the platform's timer is coarse; what varies is how evenly the lines are
//! spread inside it. The report carries the rate actually achieved, because
//! a soak that ran at a tenth of its requested rate soaked for a tenth as
//! long as it claims to have.

use std::path::PathBuf;
use std::time::Duration;

use crate::clock::{Anchor, monotonic_ns};
use crate::lines::LineSplitter;
use crate::monitor::{self, Monitor};
use crate::report::{Budget, Measurement, Report};
use crate::session::{self, COLS, ROWS, ScenarioFile, Session};
use crate::verify::{Findings, Verifier};
use crate::{human_bytes, human_ns, print_step};

/// Longest the lane waits with nothing arriving before calling the run
/// stalled. A paced stream delivers continuously; a gap this long is a
/// broken run, and waiting out a thirty-minute deadline to say so would
/// waste the run and the diagnosis.
const STALL: Duration = Duration::from_secs(30);

/// How long past its scheduled end the child gets to finish before the lane
/// stops waiting and reports the overrun.
const OVERRUN_GRACE: Duration = Duration::from_secs(60);

pub struct Options {
    pub duration: Duration,
    pub lines_per_second: u64,
    pub line_bytes: usize,
    pub checksum_every: u64,
    pub monitor_out: Option<PathBuf>,
    pub monitor_interval: Duration,
    pub warmup: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(30 * 60),
            // The per-session throughput budget, so the endurance lane soaks
            // at the rate the runtime promises rather than at an idle trickle.
            lines_per_second: 1000,
            line_bytes: agent_bridge_fake_cli::generator::DEFAULT_LINE_BYTES,
            // Roughly one integrity checkpoint a second at the default rate:
            // frequent enough to place a fault within the run, rare enough
            // to stay a rounding error in the traffic.
            checksum_every: 1000,
            monitor_out: None,
            monitor_interval: monitor::DEFAULT_INTERVAL,
            warmup: monitor::DEFAULT_WARMUP,
        }
    }
}

impl Options {
    /// How many payload lines the run asks for.
    pub fn lines(&self) -> u64 {
        (self.duration.as_secs() * self.lines_per_second).max(1)
    }

    fn scenario_json(&self) -> String {
        let interval_us = 1_000_000u64.checked_div(self.lines_per_second).unwrap_or(0);
        format!(
            r#"{{"name":"perf-soak","steps":[{{"generate":{},"channel":"stdout","line_bytes":{},"checksum_every":{},"line_interval_us":{}}},{{"exit":0}}]}}"#,
            self.lines(),
            self.line_bytes,
            self.checksum_every,
            interval_us,
        )
    }
}

pub struct Outcome {
    pub findings: Findings,
    pub bytes_read: u64,
    pub chunks_read: u64,
    pub elapsed_ns: u64,
    pub monitor: Option<monitor::Assessment>,
    pub teardown: String,
}

/// Run the lane and fold the result into a report. An `Err` means the run
/// could not be performed at all; a run that happened and found faults
/// reports them as exceeded budgets, because that is a result, not an error.
pub fn run(options: &Options) -> Result<(Report, Outcome), String> {
    let scenario = ScenarioFile::write("soak", &options.scenario_json())?;
    let expected_lines = options.lines();
    print_step(
        "plan",
        "pass",
        &format!(
            "{} lines of {} bytes at {}/s over {} s, checksum every {} lines",
            expected_lines,
            options.line_bytes,
            options.lines_per_second,
            options.duration.as_secs(),
            options.checksum_every,
        ),
    );

    let monitor = Monitor::start(options.monitor_interval, options.monitor_out.clone())?;
    let outcome = stream(options, &scenario, expected_lines);
    // Stop the monitor whatever happened: a failed run's resource curve is
    // the one someone will want to read.
    let samples = monitor.stop()?;
    let mut outcome = outcome?;
    outcome.monitor = monitor::assess(&samples, options.warmup);

    if let Some(path) = &options.monitor_out {
        print_step(
            "monitor",
            "pass",
            &format!("{} samples written to {}", samples.len(), path.display()),
        );
    }
    Ok((
        build_report("soak", "synthetic", options, &outcome),
        outcome,
    ))
}

fn stream(
    options: &Options,
    scenario: &ScenarioFile,
    expected_lines: u64,
) -> Result<Outcome, String> {
    let anchor = Anchor::take();
    let mut session = Session::spawn(&scenario.argv()?, COLS, ROWS)?;
    let started_ns = monotonic_ns();
    let deadline_ns =
        started_ns + options.duration.as_nanos() as u64 + OVERRUN_GRACE.as_nanos() as u64;

    let mut verifier = Verifier::for_this_platform(options.line_bytes);
    let mut splitter = LineSplitter::new();
    let mut bytes_read = 0u64;
    let mut chunks_read = 0u64;
    // The run is complete when everything the scenario promised has been
    // accounted for — every payload line and every checkpoint, whether it
    // verified or faulted. Waiting for end-of-stream instead would hang on
    // a re-rendering terminal, which reports no end until the master closes.
    let expected_checkpoints = expected_lines
        .checked_div(options.checksum_every)
        .unwrap_or(0);
    let mut watch = session::EndWatch::new();

    loop {
        match session.pump(session::PUMP_TICK)? {
            session::Pump::Data { at, bytes } => {
                watch.data();
                bytes_read += bytes.len() as u64;
                chunks_read += 1;
                let at_ns = anchor.ns_at(at).saturating_sub(started_ns);
                splitter.push(&bytes, |line| verifier.feed(line, at_ns));
                let findings = verifier.findings();
                if verifier.accounted() >= expected_lines
                    && findings.checksums_verified + findings.checksum_faults
                        >= expected_checkpoints
                {
                    break;
                }
            }
            session::Pump::Ended => break,
            session::Pump::Quiet => {
                if watch.ended(&mut session) {
                    break;
                }
                if watch.since_data() >= STALL {
                    return Err(format!(
                        "nothing arrived for {} s, {} s into the run — the stream stalled after \
                         {} lines and {}",
                        STALL.as_secs(),
                        (monotonic_ns() - started_ns) / 1_000_000_000,
                        verifier.accounted(),
                        human_bytes(bytes_read),
                    ));
                }
            }
        }
        if monotonic_ns() > deadline_ns {
            return Err(format!(
                "the child was still streaming {} s past its scheduled end ({} of {} lines) — \
                 pacing overran by more than the grace window",
                OVERRUN_GRACE.as_secs(),
                verifier.accounted(),
                expected_lines,
            ));
        }
    }
    splitter.finish(|line| verifier.feed(line, monotonic_ns() - started_ns));
    let elapsed_ns = monotonic_ns() - started_ns;
    let teardown = session.finish()?;

    Ok(Outcome {
        findings: verifier.finish(expected_lines, expected_checkpoints),
        bytes_read,
        chunks_read,
        elapsed_ns,
        monitor: None,
        teardown,
    })
}

/// Fold an outcome into the report shape every lane shares.
///
/// Integrity is expressed as budgets rather than as a pass/fail flag so it
/// travels the same road as the latency numbers: one file, one verdict per
/// claim, and a gate that can read all of them the same way.
pub fn build_report(lane: &str, workload: &str, options: &Options, outcome: &Outcome) -> Report {
    let mut report = Report::new(lane, workload);
    let seconds = (outcome.elapsed_ns as f64 / 1e9).max(f64::MIN_POSITIVE);
    let findings = &outcome.findings;

    report.note(format!(
        "{} over {} at a requested {} lines/s",
        findings.summary(),
        human_ns(outcome.elapsed_ns),
        options.lines_per_second,
    ));
    if cfg!(windows) {
        report.note(
            "the terminal re-renders its child's output on this platform, so repeated and \
             truncated lines are counted as repaints rather than faults; lines that never \
             arrive are counted as lost on every platform"
                .to_string(),
        );
    }

    report.add(Measurement::scalar(
        "lines_lost",
        "lines",
        findings.lines_lost,
        Some(Budget::AtMost(0)),
    ));
    report.add(Measurement::scalar(
        "content_faults",
        "lines",
        findings.content_faults,
        Some(Budget::AtMost(0)),
    ));
    report.add(Measurement::scalar(
        "checksum_faults",
        "checkpoints",
        findings.checksum_faults,
        Some(Budget::AtMost(0)),
    ));
    report.add(
        Measurement::scalar(
            "sustained_throughput",
            "lines_per_second",
            (findings.lines_verified as f64 / seconds) as u64,
            None,
        )
        .with_note(
            "the rate this run actually achieved, which is the requested pacing rather than \
             the path's capacity — the throughput lane measures capacity",
        ),
    );
    report.add(Measurement::scalar(
        "bytes_read",
        "bytes",
        outcome.bytes_read,
        None,
    ));
    report.add(Measurement::scalar(
        "elapsed",
        "ns",
        outcome.elapsed_ns,
        None,
    ));

    match &outcome.monitor {
        Some(assessment) => {
            report.add(
                Measurement::scalar(
                    "descriptor_growth",
                    "descriptors",
                    assessment.descriptor_delta.max(0) as u64,
                    Some(Budget::AtMost(0)),
                )
                .with_note(format!(
                    "{} went from {} to {} over {} samples, measured from the first sample past \
                     the warm-up window (net delta {})",
                    monitor::DESCRIPTOR_NOUN,
                    assessment.baseline_descriptors,
                    assessment.final_descriptors,
                    assessment.samples,
                    assessment.descriptor_delta,
                )),
            );
            report.add(
                Measurement::scalar(
                    "rss_growth",
                    "bytes",
                    assessment.rss_growth_bytes.max(0) as u64,
                    Some(Budget::AtMost(monitor::RSS_GROWTH_BUDGET_BYTES)),
                )
                .with_note(format!(
                    "resident memory went from {} to {} (net delta {} bytes), peaking at {}",
                    human_bytes(assessment.baseline_rss_bytes),
                    human_bytes(assessment.final_rss_bytes),
                    assessment.rss_growth_bytes,
                    human_bytes(assessment.peak_rss_bytes),
                )),
            );
        }
        None => report.note(
            "the run was too short to have a steady state, so no resource growth was assessed"
                .to_string(),
        ),
    }

    for detail in &findings.detail {
        report.note(detail.clone());
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scenario_asks_for_the_duration_it_was_given() {
        let options = Options {
            duration: Duration::from_secs(30 * 60),
            lines_per_second: 1000,
            ..Options::default()
        };
        assert_eq!(options.lines(), 1_800_000);
        let json = options.scenario_json();
        assert!(json.contains("\"generate\":1800000"), "{json}");
        assert!(
            json.contains("\"line_interval_us\":1000"),
            "1000 lines a second is a line every 1000 µs: {json}"
        );
    }

    #[test]
    fn the_assembled_scenario_is_one_the_fake_cli_accepts() {
        // The lane writes scenario text rather than serialising a structure,
        // so this is the test that catches the two drifting apart.
        let json = Options::default().scenario_json();
        let scenario = agent_bridge_fake_cli::scenario::parse(&json)
            .unwrap_or_else(|err| panic!("the soak scenario must parse: {err}"));
        assert_eq!(scenario.name, "perf-soak");
        assert_eq!(scenario.steps.len(), 2);
    }

    #[test]
    fn an_unpaced_run_asks_for_no_interval() {
        let options = Options {
            lines_per_second: 0,
            ..Options::default()
        };
        assert!(options.scenario_json().contains("\"line_interval_us\":0"));
        assert_eq!(options.lines(), 1, "a rate of zero still asks for a line");
    }

    #[test]
    fn integrity_findings_become_budgets_a_gate_can_read() {
        let outcome = Outcome {
            findings: Findings {
                lines_verified: 1000,
                lines_lost: 3,
                ..Findings::default()
            },
            bytes_read: 73_000,
            chunks_read: 500,
            elapsed_ns: 1_000_000_000,
            monitor: None,
            teardown: "torn down".to_string(),
        };
        let report = build_report("soak", "synthetic", &Options::default(), &outcome);
        let lost = report
            .measurements
            .iter()
            .find(|m| m.name == "lines_lost")
            .expect("the report must carry the loss count");
        assert_eq!(lost.value, 3);
        assert_eq!(lost.verdict, crate::report::Verdict::Exceeded);
        assert_eq!(report.exceeded().len(), 1);
    }
}
