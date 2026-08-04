//! How much the read path can carry — and what one session's number is
//! worth when it is not alone.
//!
//! The single-session lane is the simple half: the child streams generated
//! lines as fast as the terminal accepts them, every line is verified, and
//! the rate is lines over the time they took to arrive. The clock starts at
//! the first delivery, not at the spawn, so the number is a rate and not a
//! rate diluted by startup.
//!
//! The concurrent half exists because per-session numbers measured alone
//! quietly assume the machine to themselves. A runtime hosts sessions in the
//! plural, and the question that decides its architecture is not "what can
//! one session do" but "what happens to each session's share as sessions are
//! added" — does the aggregate grow, plateau, or come apart? One run at one
//! concurrency level is one point on that curve; the lane takes the level as
//! a parameter so a schedule of runs can trace the shape.
//!
//! Every session's stream is verified while it is measured. A throughput
//! number over a stream nobody checked would reward the failure mode this
//! probe exists to catch — a path that goes fast by dropping things.
//!
//! The verifier rides in the measured loop, so its cost is part of the
//! number. That is the honest side of the proxy: the real runtime also reads
//! *and processes* every chunk, and a rate measured into a null sink would
//! flatter the path. The report says so, so nobody mistakes the figure for
//! a kernel copy benchmark.

use std::time::Duration;

use crate::clock::Anchor;
use crate::lines::LineSplitter;
use crate::report::{Budget, Measurement, Report};
use crate::session::{self, COLS, ROWS, ScenarioFile, Session};
use crate::verify::{Findings, Verifier};
use crate::{human_bytes, print_step};

/// The published floor: what one session must sustain.
pub const PER_SESSION_FLOOR_LINES_PER_SEC: u64 = 1000;

/// Longest a session waits with nothing arriving before the run is called
/// stalled.
const STALL: Duration = Duration::from_secs(30);

pub struct Options {
    /// Payload lines per session.
    pub lines: u64,
    pub line_bytes: usize,
    pub checksum_every: u64,
    /// Concurrent sessions. One is the solo measurement; several is one
    /// point on the aggregate-versus-per-session curve.
    pub sessions: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            lines: 1_000_000,
            line_bytes: agent_bridge_fake_cli::generator::DEFAULT_LINE_BYTES,
            checksum_every: 1000,
            sessions: 1,
        }
    }
}

impl Options {
    fn scenario_json(&self) -> String {
        format!(
            r#"{{"name":"perf-throughput","steps":[{{"generate":{},"channel":"stdout","line_bytes":{},"checksum_every":{},"line_interval_us":0}},{{"exit":0}}]}}"#,
            self.lines, self.line_bytes, self.checksum_every,
        )
    }
}

/// One session's measured run.
pub struct SessionOutcome {
    pub findings: Findings,
    pub bytes_read: u64,
    /// First and last delivery, on the shared clock.
    pub first_ns: u64,
    pub last_ns: u64,
}

impl SessionOutcome {
    pub fn elapsed_ns(&self) -> u64 {
        self.last_ns.saturating_sub(self.first_ns).max(1)
    }

    pub fn lines_per_sec(&self) -> u64 {
        (self.findings.lines_verified as f64 / (self.elapsed_ns() as f64 / 1e9)) as u64
    }

    pub fn bytes_per_sec(&self) -> u64 {
        (self.bytes_read as f64 / (self.elapsed_ns() as f64 / 1e9)) as u64
    }
}

pub struct Outcome {
    pub sessions: Vec<SessionOutcome>,
}

impl Outcome {
    pub fn faults(&self) -> u64 {
        self.sessions.iter().map(|s| s.findings.faults()).sum()
    }

    /// The worst session's sustained rate — the number the per-session floor
    /// is held against. Concurrency that starves one session while the
    /// others fly is a finding, and an average would hide it.
    pub fn slowest_lines_per_sec(&self) -> u64 {
        self.sessions
            .iter()
            .map(SessionOutcome::lines_per_sec)
            .min()
            .unwrap_or(0)
    }

    /// Total verified lines over the wall-clock span from the first
    /// session's first delivery to the last session's last.
    pub fn aggregate_lines_per_sec(&self) -> u64 {
        let first = self.sessions.iter().map(|s| s.first_ns).min().unwrap_or(0);
        let last = self.sessions.iter().map(|s| s.last_ns).max().unwrap_or(0);
        let total: u64 = self
            .sessions
            .iter()
            .map(|s| s.findings.lines_verified)
            .sum();
        (total as f64 / (last.saturating_sub(first).max(1) as f64 / 1e9)) as u64
    }
}

pub fn run(options: &Options) -> Result<(Report, Outcome), String> {
    if options.sessions == 0 {
        return Err("a run needs at least one session".to_string());
    }
    let scenario = ScenarioFile::write("throughput", &options.scenario_json())?;
    print_step(
        "plan",
        "pass",
        &format!(
            "{} session(s) × {} lines of {} bytes, unpaced",
            options.sessions, options.lines, options.line_bytes,
        ),
    );

    // One anchor for every session, so their first/last instants land on one
    // time base and the aggregate span means something.
    let anchor = Anchor::take();
    let outcomes: Vec<Result<SessionOutcome, String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..options.sessions)
            .map(|index| {
                let scenario = &scenario;
                scope.spawn(move || {
                    stream_one(scenario, options, anchor)
                        .map_err(|err| format!("session {index}: {err}"))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("no session panics"))
            .collect()
    });
    let sessions = outcomes.into_iter().collect::<Result<Vec<_>, _>>()?;
    let outcome = Outcome { sessions };

    for (index, session) in outcome.sessions.iter().enumerate() {
        print_step(
            "session",
            if session.findings.clean() {
                "pass"
            } else {
                "fail"
            },
            &format!(
                "session {index}: {} lines/s, {}/s, {}",
                session.lines_per_sec(),
                human_bytes(session.bytes_per_sec()),
                session.findings.summary(),
            ),
        );
    }

    Ok((build_report(options, &outcome), outcome))
}

fn stream_one(
    scenario: &ScenarioFile,
    options: &Options,
    anchor: Anchor,
) -> Result<SessionOutcome, String> {
    let mut session = Session::spawn(&scenario.argv()?, COLS, ROWS)?;
    let mut verifier = Verifier::for_this_platform(options.line_bytes);
    let mut splitter = LineSplitter::new();
    let mut bytes_read = 0u64;
    let mut first_ns: Option<u64> = None;
    let mut last_ns = 0u64;
    // Complete on the session's own expectation — every promised line and
    // checkpoint accounted — never on end-of-stream, which a re-rendering
    // terminal only reports once the master closes.
    let expected_checkpoints = options
        .lines
        .checked_div(options.checksum_every)
        .unwrap_or(0);
    let mut watch = session::EndWatch::new();

    loop {
        match session.pump(session::PUMP_TICK)? {
            session::Pump::Data { at, bytes } => {
                watch.data();
                let at_ns = anchor.ns_at(at);
                let run_ns = at_ns.saturating_sub(*first_ns.get_or_insert(at_ns));
                last_ns = at_ns;
                bytes_read += bytes.len() as u64;
                splitter.push(&bytes, |line| verifier.feed(line, run_ns));
                let findings = verifier.findings();
                if verifier.accounted() >= options.lines
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
                        "nothing arrived for {} s after {} lines and {}",
                        STALL.as_secs(),
                        verifier.accounted(),
                        human_bytes(bytes_read),
                    ));
                }
            }
        }
    }
    let final_ns = last_ns.saturating_sub(first_ns.unwrap_or(0));
    splitter.finish(|line| verifier.feed(line, final_ns));
    session.finish()?;

    Ok(SessionOutcome {
        findings: verifier.finish(options.lines, expected_checkpoints),
        bytes_read,
        first_ns: first_ns.unwrap_or(0),
        last_ns,
    })
}

fn build_report(options: &Options, outcome: &Outcome) -> Report {
    let workload = if options.sessions == 1 {
        "synthetic".to_string()
    } else {
        format!("synthetic×{}", options.sessions)
    };
    let mut report = Report::new("bench-throughput", &workload);
    report.note(format!(
        "{} session(s) × {} lines of {} bytes, unpaced; rates include line verification, \
         which is deliberate — the path under test processes what it reads",
        options.sessions, options.lines, options.line_bytes,
    ));

    report.add(Measurement::scalar(
        "sessions",
        "sessions",
        options.sessions as u64,
        None,
    ));
    report.add(
        Measurement::scalar(
            "per_session_throughput",
            "lines_per_second",
            outcome.slowest_lines_per_sec(),
            Some(Budget::AtLeast(PER_SESSION_FLOOR_LINES_PER_SEC)),
        )
        .with_note(
            "the slowest session's sustained rate — an average would hide a starved session",
        ),
    );
    report.add(Measurement::scalar(
        "aggregate_throughput",
        "lines_per_second",
        outcome.aggregate_lines_per_sec(),
        None,
    ));
    let bytes_per_sec: u64 = outcome
        .sessions
        .iter()
        .map(SessionOutcome::bytes_per_sec)
        .sum();
    report.add(Measurement::scalar(
        "aggregate_bytes",
        "bytes_per_second",
        bytes_per_sec,
        None,
    ));
    report.add(Measurement::scalar(
        "integrity_faults",
        "faults",
        outcome.faults(),
        Some(Budget::AtMost(0)),
    ));
    for session in &outcome.sessions {
        for detail in &session.findings.detail {
            report.note(detail.clone());
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(rates: &[(u64, u64, u64)]) -> Outcome {
        // (lines_verified, first_ns, last_ns)
        Outcome {
            sessions: rates
                .iter()
                .map(|(lines, first, last)| SessionOutcome {
                    findings: Findings {
                        lines_verified: *lines,
                        ..Findings::default()
                    },
                    bytes_read: lines * 70,
                    first_ns: *first,
                    last_ns: *last,
                })
                .collect(),
        }
    }

    #[test]
    fn rates_are_lines_over_delivery_time_not_over_process_life() {
        let outcome = outcome(&[(10_000, 5_000_000_000, 6_000_000_000)]);
        // 10 000 lines in the one second between first and last delivery —
        // the five seconds before the first must not dilute it.
        assert_eq!(outcome.sessions[0].lines_per_sec(), 10_000);
    }

    #[test]
    fn the_floor_is_held_against_the_slowest_session() {
        let outcome = outcome(&[
            (10_000, 0, 1_000_000_000),
            (500, 0, 1_000_000_000), // starved
        ]);
        assert_eq!(outcome.slowest_lines_per_sec(), 500);
    }

    #[test]
    fn the_aggregate_spans_first_delivery_to_last() {
        let outcome = outcome(&[
            (10_000, 0, 1_000_000_000),
            (10_000, 500_000_000, 1_500_000_000),
        ]);
        // 20 000 lines across a 1.5 s span.
        assert_eq!(outcome.aggregate_lines_per_sec(), 13_333);
    }

    #[test]
    fn the_scenario_is_one_the_fake_cli_accepts() {
        let json = Options::default().scenario_json();
        let scenario = agent_bridge_fake_cli::scenario::parse(&json)
            .unwrap_or_else(|err| panic!("the throughput scenario must parse: {err}"));
        assert_eq!(scenario.name, "perf-throughput");
    }

    #[test]
    fn a_faulty_session_fails_the_integrity_budget() {
        let mut faulty = outcome(&[(10_000, 0, 1_000_000_000)]);
        faulty.sessions[0].findings.lines_lost = 2;
        let report = build_report(&Options::default(), &faulty);
        let integrity = report
            .measurements
            .iter()
            .find(|m| m.name == "integrity_faults")
            .expect("the report carries integrity");
        assert_eq!(integrity.verdict, crate::report::Verdict::Exceeded);
    }
}
