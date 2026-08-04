//! What the terminal costs, in the two directions that have budgets.
//!
//! **First byte out.** The child stamps a marker line with its reading of
//! the shared monotonic clock as it writes it; the probe stamps the read
//! that delivered it. The difference is everything between a child's
//! `write()` and the hosting process seeing the bytes — the terminal
//! transit, the kernel buffering, the scheduler — and nothing else: the
//! child's startup, the scenario parse, the spawn all happen before any
//! marker exists, so they cannot leak into a sample.
//!
//! **Input forwarding.** The probe stamps its own clock, writes a line of
//! input, and the child answers the moment that input has matched — with a
//! marker stamped as the answer is written. The difference is the cost of
//! getting a keystroke *into* a hosted child plus the first write back out.
//! That is deliberately a round trip's worth of honesty: the child cannot
//! observe its own `read()` returning any earlier than the step that reacts
//! to it, so the number carries at most one scenario-step transition of
//! overhead — microseconds — and is measured, not modelled.
//!
//! Both lanes are one distribution each, judged at P99 over enough samples
//! that the P99 is a real rank and not an artifact of a handful of
//! stragglers. The first samples after spawn are discarded — a terminal
//! warms up (ConPTY answers its own startup queries, buffers get their
//! first faults), and a budget on steady-state delivery should not be spent
//! on the first hundred milliseconds of a session's life. The discard count
//! is in the report.
//!
//! Markers are framed (`M<digits>E`, `R<digits>E`) so a marker cut short by
//! a repainting terminal fails to parse instead of parsing as a smaller
//! number, and readings must strictly increase so a re-sent marker is
//! recognised as the repaint it is rather than sampled twice.

use std::time::Duration;

use crate::clock::{Anchor, monotonic_ns};
use crate::lines::LineSplitter;
use crate::report::{Budget, Measurement, Report};
use crate::session::{self, COLS, ROWS, ScenarioFile, Session};
use crate::stats::summarize;
use crate::{human_ns, print_step};

/// The published budgets under test.
pub const FIRST_BYTE_BUDGET_NS: u64 = 50_000_000;
pub const INPUT_FORWARDING_BUDGET_NS: u64 = 10_000_000;

/// Longest the lane waits with nothing arriving before calling the run
/// stalled.
const STALL: Duration = Duration::from_secs(30);

/// Per-round-trip timeout scripted into the input-forwarding scenario.
/// Generous: the probe writes the next ping immediately after the previous
/// answer, so this only fires when something is actually wrong.
const AWAIT_TIMEOUT_MS: u64 = 30_000;

pub struct Options {
    /// Measured samples per direction, after the discard.
    pub samples: usize,
    /// Spacing of the first-byte markers. Far enough apart that each marker
    /// measures a delivery rather than its place in a queue.
    pub marker_interval_us: u64,
    /// Samples thrown away at the start of each direction.
    pub discard: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            samples: 10_000,
            marker_interval_us: 1_000,
            discard: 50,
        }
    }
}

pub struct Outcome {
    pub first_byte_ns: Vec<u64>,
    pub forwarding_ns: Vec<u64>,
    pub repaints_skipped: u64,
}

/// Run both directions and fold the distributions into a report.
pub fn run(options: &Options) -> Result<(Report, Outcome), String> {
    let first_byte = first_byte(options)?;
    print_step(
        "first-byte",
        "pass",
        &format!("{} samples collected", first_byte.0.len()),
    );
    let forwarding = forwarding(options)?;
    print_step(
        "input-forwarding",
        "pass",
        &format!("{} samples collected", forwarding.0.len()),
    );

    let mut outcome = Outcome {
        first_byte_ns: first_byte.0,
        forwarding_ns: forwarding.0,
        repaints_skipped: first_byte.1 + forwarding.1,
    };

    let mut report = Report::new("bench-latency", "synthetic");
    report.note(format!(
        "{} samples per direction after discarding the first {} of each; first-byte markers \
         {} µs apart; input-forwarding round trips are serial",
        options.samples, options.discard, options.marker_interval_us,
    ));
    if outcome.repaints_skipped > 0 {
        report.note(format!(
            "{} re-sent or truncated markers were recognised and skipped, not sampled",
            outcome.repaints_skipped
        ));
    }

    let first_summary =
        summarize(&mut outcome.first_byte_ns).ok_or("the first-byte lane collected no samples")?;
    report.add(Measurement::from_p99(
        "first_byte_latency",
        "ns",
        first_summary,
        Some(Budget::AtMost(FIRST_BYTE_BUDGET_NS)),
    ));
    print_step(
        "first-byte",
        "pass",
        &format!(
            "p50 {} / p99 {} / max {}",
            human_ns(first_summary.p50),
            human_ns(first_summary.p99),
            human_ns(first_summary.max),
        ),
    );

    let forwarding_summary = summarize(&mut outcome.forwarding_ns)
        .ok_or("the input-forwarding lane collected no samples")?;
    report.add(
        Measurement::from_p99(
            "input_forwarding_latency",
            "ns",
            forwarding_summary,
            Some(Budget::AtMost(INPUT_FORWARDING_BUDGET_NS)),
        )
        .with_note(
            "measured as a write-to-answer round trip: the child stamps the marker as it \
             reacts to the matched input, so the number includes one scenario-step \
             transition on the child's side",
        ),
    );
    print_step(
        "input-forwarding",
        "pass",
        &format!(
            "p50 {} / p99 {} / max {}",
            human_ns(forwarding_summary.p50),
            human_ns(forwarding_summary.p99),
            human_ns(forwarding_summary.max),
        ),
    );

    Ok((report, outcome))
}

/// The first-byte direction: the child streams stamped markers on its own
/// schedule; the probe collects deliveries.
fn first_byte(options: &Options) -> Result<(Vec<u64>, u64), String> {
    let total = options.samples + options.discard;
    let scenario = ScenarioFile::write(
        "latency-first-byte",
        &format!(
            r#"{{"name":"perf-latency-first-byte","steps":[{{"emit":"M{{ts}}E\n","channel":"stdout","repeat":{total},"repeat_interval_us":{}}},{{"exit":0}}]}}"#,
            options.marker_interval_us,
        ),
    )?;

    let anchor = Anchor::take();
    let mut session = Session::spawn(&scenario.argv()?, COLS, ROWS)?;
    let mut splitter = LineSplitter::new();
    let mut samples = Vec::with_capacity(total);
    let mut repaints = 0u64;
    let mut last_stamp = 0u64;
    let mut watch = session::EndWatch::new();

    'read: loop {
        match session.pump(session::PUMP_TICK)? {
            session::Pump::Data { at, bytes } => {
                watch.data();
                let arrived_ns = anchor.ns_at(at);
                splitter.push(&bytes, |line| {
                    match accept_marker(line, b'M', &mut last_stamp) {
                        MarkerRead::Fresh(stamp) => {
                            samples.push(arrived_ns.saturating_sub(stamp));
                        }
                        MarkerRead::Repaint => repaints += 1,
                        MarkerRead::NotAMarker => {}
                    }
                });
                if samples.len() >= total {
                    break 'read;
                }
            }
            // The shortfall check below turns either form of the end — the
            // reader's, or an exited child gone quiet — into a diagnosis.
            session::Pump::Ended => break 'read,
            session::Pump::Quiet => {
                if watch.ended(&mut session) {
                    break 'read;
                }
                if watch.since_data() >= STALL {
                    return Err(format!(
                        "nothing arrived for {} s with {} of {total} markers collected",
                        STALL.as_secs(),
                        samples.len(),
                    ));
                }
            }
        }
    }
    session.finish()?;

    if samples.len() < total {
        return Err(format!(
            "the stream ended after {} of {total} markers",
            samples.len()
        ));
    }
    samples.drain(..options.discard);
    Ok((samples, repaints))
}

/// The input-forwarding direction: serial round trips, each one scripted as
/// an exact-match await followed by a stamped answer.
fn forwarding(options: &Options) -> Result<(Vec<u64>, u64), String> {
    let total = options.samples + options.discard;
    let mut steps = String::new();
    for _ in 0..total {
        steps.push_str(&format!(
            r#"{{"await_stdin":"ping\n","timeout_ms":{AWAIT_TIMEOUT_MS}}},{{"emit":"R{{ts}}E\n","channel":"stdout"}},"#
        ));
    }
    let scenario = ScenarioFile::write(
        "latency-forwarding",
        &format!(r#"{{"name":"perf-latency-forwarding","steps":[{steps}{{"exit":0}}]}}"#),
    )?;

    // No anchor here: both ends of a forwarding sample — the probe's write
    // instant and the child's embedded answer — are already readings of the
    // shared counter.
    let mut session = Session::spawn(&scenario.argv()?, COLS, ROWS)?;
    let mut splitter = LineSplitter::new();
    let mut samples = Vec::with_capacity(total);
    let mut repaints = 0u64;
    let mut last_stamp = 0u64;

    let mut watch = session::EndWatch::new();
    for round in 0..total {
        let wrote_ns = monotonic_ns();
        // Enter is a carriage return on a terminal; the terminal (or the
        // child's normalisation) turns it into the line ending the script
        // awaits.
        session
            .writer
            .send(b"ping\r")
            .map_err(|err| format!("round {round}: writing input failed: {err}"))?;

        let mut answer: Option<u64> = None;
        while answer.is_none() {
            match session.pump(session::PUMP_TICK)? {
                session::Pump::Data { at: _, bytes } => {
                    watch.data();
                    splitter.push(&bytes, |line| {
                        match accept_marker(line, b'R', &mut last_stamp) {
                            MarkerRead::Fresh(stamp) => answer = Some(stamp),
                            MarkerRead::Repaint => repaints += 1,
                            MarkerRead::NotAMarker => {}
                        }
                    });
                }
                session::Pump::Ended => {
                    return Err(format!(
                        "the child ended at round {round} of {total} — a scripted await \
                         failed or timed out"
                    ));
                }
                session::Pump::Quiet => {
                    if watch.ended(&mut session) {
                        return Err(format!(
                            "the child ended at round {round} of {total} — a scripted await \
                             failed or timed out"
                        ));
                    }
                    if watch.since_data() >= STALL {
                        return Err(format!(
                            "round {round}: no answer for {} s after the input was written",
                            STALL.as_secs()
                        ));
                    }
                }
            }
        }
        let stamp = answer.expect("the loop above only exits with an answer");
        // A stamp from before the write would mean the answer to a previous
        // round was mistaken for this one; the strictly-increasing guard
        // makes that impossible, so this is a pure sanity assertion.
        samples.push(stamp.saturating_sub(wrote_ns));
    }
    session.finish()?;
    samples.drain(..options.discard);
    Ok((samples, repaints))
}

enum MarkerRead {
    /// A well-formed marker with a reading newer than any seen before.
    Fresh(u64),
    /// A well-formed or truncated marker already accounted for — a
    /// re-rendering terminal saying something again.
    Repaint,
    NotAMarker,
}

/// Parse `<tag><digits>E` and enforce the strictly-increasing rule. The
/// trailing `E` is the frame: a marker cut short by a repaint loses it and
/// is dropped here rather than parsed as a smaller number.
fn accept_marker(line: &str, tag: u8, last_stamp: &mut u64) -> MarkerRead {
    let bytes = line.as_bytes();
    let Some(rest) = bytes.strip_prefix(&[tag]) else {
        return MarkerRead::NotAMarker;
    };
    let Some(digits) = rest.strip_suffix(b"E") else {
        // A prefix of a marker (the digits cut off mid-line) is what a
        // partial repaint looks like; count it as one only if it could be
        // one, i.e. it is all digits so far.
        return if !rest.is_empty() && rest.iter().all(u8::is_ascii_digit) {
            MarkerRead::Repaint
        } else {
            MarkerRead::NotAMarker
        };
    };
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return MarkerRead::NotAMarker;
    }
    let Ok(stamp) = std::str::from_utf8(digits)
        .expect("digits are ASCII")
        .parse::<u64>()
    else {
        return MarkerRead::NotAMarker;
    };
    if stamp <= *last_stamp {
        return MarkerRead::Repaint;
    }
    *last_stamp = stamp;
    MarkerRead::Fresh(stamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept(line: &str, last: &mut u64) -> MarkerRead {
        accept_marker(line, b'M', last)
    }

    #[test]
    fn a_fresh_marker_parses_and_advances_the_watermark() {
        let mut last = 0;
        assert!(matches!(
            accept("M12345E", &mut last),
            MarkerRead::Fresh(12_345)
        ));
        assert_eq!(last, 12_345);
    }

    #[test]
    fn a_resent_marker_is_a_repaint_not_a_second_sample() {
        let mut last = 0;
        assert!(matches!(accept("M100E", &mut last), MarkerRead::Fresh(_)));
        assert!(matches!(accept("M100E", &mut last), MarkerRead::Repaint));
        assert!(matches!(accept("M99E", &mut last), MarkerRead::Repaint));
    }

    #[test]
    fn a_truncated_marker_is_never_a_smaller_number() {
        // "M123456789E" cut short: without the frame this would parse as
        // 12345 and record a fantastic latency.
        let mut last = 0;
        assert!(matches!(accept("M12345", &mut last), MarkerRead::Repaint));
        assert_eq!(last, 0, "a truncated marker must not advance the watermark");
    }

    #[test]
    fn noise_is_not_a_marker() {
        let mut last = 0;
        for line in ["", "M", "ME", "MxE", "R100E", "ping", "M100Etrailing"] {
            assert!(
                matches!(accept(line, &mut last), MarkerRead::NotAMarker),
                "{line:?} must not be a marker"
            );
        }
    }

    #[test]
    fn the_first_byte_scenario_is_one_the_fake_cli_accepts() {
        let options = Options {
            samples: 100,
            marker_interval_us: 500,
            discard: 10,
        };
        let json = format!(
            r#"{{"name":"perf-latency-first-byte","steps":[{{"emit":"M{{ts}}E\n","channel":"stdout","repeat":{},"repeat_interval_us":{}}},{{"exit":0}}]}}"#,
            options.samples + options.discard,
            options.marker_interval_us,
        );
        let scenario = agent_bridge_fake_cli::scenario::parse(&json)
            .unwrap_or_else(|err| panic!("the first-byte scenario must parse: {err}"));
        assert_eq!(scenario.steps.len(), 2);
    }

    #[test]
    fn the_forwarding_scenario_is_one_the_fake_cli_accepts() {
        let mut steps = String::new();
        for _ in 0..3 {
            steps.push_str(&format!(
                r#"{{"await_stdin":"ping\n","timeout_ms":{AWAIT_TIMEOUT_MS}}},{{"emit":"R{{ts}}E\n","channel":"stdout"}},"#
            ));
        }
        let json = format!(r#"{{"name":"perf-latency-forwarding","steps":[{steps}{{"exit":0}}]}}"#);
        let scenario = agent_bridge_fake_cli::scenario::parse(&json)
            .unwrap_or_else(|err| panic!("the forwarding scenario must parse: {err}"));
        assert_eq!(scenario.steps.len(), 7, "3 round trips and an exit");
    }
}
