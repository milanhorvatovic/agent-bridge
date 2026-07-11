//! Resize-propagation probe — proves, against the resize-child fixture
//! under a real PTY, that a resize issued on the master reaches the
//! application as a delivered notification, repeatably, and that the
//! dimension env values do not move with it:
//!
//! - **steady**: spawn at the runtime's default 80×24, grow to 120×40,
//!   shrink back to 80×24. Each transition must surface in the fixture as
//!   a winch report — the `SIGWINCH` handler fired on POSIX, a
//!   window-buffer-size event arrived on the console input queue on
//!   Windows; never polling — and an on-demand dims sample after each
//!   transition must show the live size moved while `COLUMNS`/`LINES`
//!   still carry the spawn-time values. Env is set once at spawn; only the
//!   kernel/console dimensions change.
//! - **early**: issue the grow *before* the fixture's ready line — the
//!   resize-before-launch race. The outcome is characterized, not assumed:
//!   the resize call itself must succeed, the fixture must still come up
//!   (no hang), the console must settle at one of the two known
//!   geometries — the resize applied and held, or was silently dropped in
//!   the attach window; the first Windows runs produced one of each, with
//!   the call succeeding either way — and a follow-up resize away from
//!   wherever it settled must still be observed: the race must not corrupt
//!   the channel. Which arm occurred, and whether the early notification
//!   arrived, is recorded in the step details per platform.
//!
//! One scenario per invocation: `resize-probe steady` / `resize-probe
//! early`. Same step contract as the sibling probes — one machine-readable
//! `step=… status=… detail="…"` line per step, exit non-zero with a
//! step-identifying code on the first failure, so CI asserts the exit
//! status while a human reads the log.

// This crate legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use agent_bridge_interactive_probe::firsttoken::FirstTokenClock;
use agent_bridge_interactive_probe::pty::{
    OutputTracker, SharedWriter, alloc_pty, spawn_reader, teardown, wait_child,
};
use agent_bridge_interactive_probe::reports::wait_for_report;
use agent_bridge_interactive_probe::{COLS, ROWS, platform_report};
use agent_bridge_probe_child::{
    DIMS_BYTE, EVENT_ARMED, EVENT_DIMS, EVENT_QUIT, EVENT_READY, EVENT_WINCH, QUIT_BYTE, Report,
    WINCH_VIA, reports_in,
};
use portable_pty::{CommandBuilder, MasterPty, PtyPair, PtySize};

/// One terminal geometry, as the fixture reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Dims {
    cols: u16,
    rows: u16,
}

impl std::fmt::Display for Dims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.cols, self.rows)
    }
}

/// The two geometries under test: spawn at the runtime's documented 80×24
/// default (the probe library's constants), grow to the 120×40 counterpart
/// the resize criterion names.
const SPAWN: Dims = Dims {
    cols: COLS,
    rows: ROWS,
};
const GROWN: Dims = Dims {
    cols: 120,
    rows: 40,
};

const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// How long a possibly-in-flight winch report gets to land before the early
/// scenario records how many notifications the early resize produced. The
/// fixture's watcher polls every 20ms, so this is generous, not tight.
const WINCH_DRAIN: Duration = Duration::from_millis(200);

#[derive(Clone, Copy)]
enum Mode {
    Steady,
    Early,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Steady => "steady",
            Mode::Early => "early",
        }
    }
}

struct Failure {
    step: &'static str,
    code: i32,
    detail: String,
}

impl Failure {
    fn new(step: &'static str, code: i32, detail: impl Into<String>) -> Self {
        Self {
            step,
            code,
            detail: detail.into(),
        }
    }
}

fn main() {
    let (mode, timeout) = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("resize-probe: {message}");
            std::process::exit(2);
        }
    };
    println!("resize-probe {}", platform_report());
    match run(mode, timeout) {
        Ok(()) => println!("resize-probe mode={} result=pass", mode.name()),
        Err(failure) => {
            print_step(failure.step, "fail", &failure.detail);
            eprintln!(
                "resize-probe: step {} failed: {}",
                failure.step, failure.detail
            );
            std::process::exit(failure.code);
        }
    }
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<(Mode, Duration), String> {
    const USAGE: &str = "usage: resize-probe <steady|early> [--timeout-secs N]";
    let mut mode: Option<Mode> = None;
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "steady" if mode.is_none() => mode = Some(Mode::Steady),
            "early" if mode.is_none() => mode = Some(Mode::Early),
            "--timeout-secs" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--timeout-secs needs a value. {USAGE}"))?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid --timeout-secs value: {value}"))?;
                timeout = Duration::from_secs(secs);
            }
            other => return Err(format!("unexpected argument: {other}. {USAGE}")),
        }
    }
    mode.map(|mode| (mode, timeout))
        .ok_or_else(|| format!("a mode is required. {USAGE}"))
}

fn print_step(step: &str, status: &str, detail: &str) {
    // Keep every step line single-line and parseable: the detail field is
    // quoted, so newlines and double quotes inside it are normalized away.
    let clean = detail.replace(['\r', '\n'], " ").replace('"', "'");
    println!("resize-probe step={step} status={status} detail=\"{clean}\"");
}

/// The scenario, in lifecycle order. Exit codes are step-stable across both
/// modes: 10 alloc, 11 spawn, 12 early_resize (early only), 13 ready, 14
/// baseline (steady) / settle (early), 15 grow (steady) / recover (early),
/// 16 shrink (steady only), 17 quit, 18 child_exit, 19 teardown.
fn run(mode: Mode, timeout: Duration) -> Result<(), Failure> {
    let (pair, alloc_ms) = alloc_pty(SPAWN.cols, SPAWN.rows, timeout)
        .map_err(|detail| Failure::new("alloc", 10, detail))?;
    print_step(
        "alloc",
        "pass",
        &format!("pty allocated at {SPAWN} in {alloc_ms}ms"),
    );
    let PtyPair { master, slave } = pair;

    let fixture = sibling_resize_child().map_err(|detail| Failure::new("spawn", 11, detail))?;
    let mut command = CommandBuilder::new(&fixture);
    // The runtime's contract under test: the dimension env matches the
    // spawn size and is set exactly once. Every env assertion later in the
    // scenario hangs off these two values.
    command.env("COLUMNS", SPAWN.cols.to_string());
    command.env("LINES", SPAWN.rows.to_string());
    let mut child = slave
        .spawn_command(command)
        .map_err(|err| Failure::new("spawn", 11, format!("child spawn failed: {err:#}")))?;
    let spawned_at = Instant::now();
    // Release our copy of the child end: holding it open would keep the
    // master from ever seeing end-of-stream after the child exits.
    drop(slave);
    print_step(
        "spawn",
        "pass",
        &format!(
            "spawned `{}` pid={} with COLUMNS={} LINES={}",
            fixture.display(),
            child
                .process_id()
                .map_or_else(|| "unknown".to_string(), |pid| pid.to_string()),
            SPAWN.cols,
            SPAWN.rows,
        ),
    );

    let reader = master
        .try_clone_reader()
        .map_err(|err| Failure::new("ready", 13, format!("cloning the reader failed: {err:#}")))?;
    let writer =
        SharedWriter::new(master.take_writer().map_err(|err| {
            Failure::new("ready", 13, format!("taking the writer failed: {err:#}"))
        })?);
    let events = spawn_reader(reader, writer.clone(), Arc::new(AtomicU32::new(0)));
    let mut tracker = OutputTracker::new(events, FirstTokenClock::new(Instant::now()), None);

    // Early mode: the resize goes in now, before the fixture's ready line —
    // the launch race under characterization. The reader thread is already
    // draining, so the repaint the resize triggers cannot back up.
    if matches!(mode, Mode::Early) {
        resize(&*master, GROWN).map_err(|detail| Failure::new("early_resize", 12, detail))?;
        print_step(
            "early_resize",
            "pass",
            &format!(
                "resize to {GROWN} issued {}µs after spawn returned, ahead of the ready line",
                spawned_at.elapsed().as_micros()
            ),
        );
    }

    let ready = wait_for_report(
        &mut tracker,
        "the fixture's ready report",
        |report| report.event == EVENT_READY,
        timeout,
    )
    .map_err(|detail| Failure::new("ready", 13, detail))?;
    let ready_dims = report_dims(&ready).map_err(|detail| Failure::new("ready", 13, detail))?;
    // The armed report precedes ready, so it is already visible: assert the
    // fixture installed this build's notification channel, not merely came
    // up.
    let armed = find_report(&tracker, |report| report.event == EVENT_ARMED).ok_or_else(|| {
        Failure::new(
            "ready",
            13,
            "the fixture's armed report never arrived ahead of ready",
        )
    })?;
    if armed.field("via") != Some(WINCH_VIA) {
        return Err(Failure::new(
            "ready",
            13,
            format!("the fixture armed the wrong channel: {armed} — expected via={WINCH_VIA}"),
        ));
    }
    match mode {
        Mode::Steady => {
            if ready_dims != SPAWN {
                return Err(Failure::new(
                    "ready",
                    13,
                    format!(
                        "the fixture came up at {ready_dims}, not the spawn size {SPAWN}: {ready}"
                    ),
                ));
            }
            print_step("ready", "pass", &format!("fixture up at {SPAWN} ({armed})"));
        }
        Mode::Early => {
            let outcome = early_ready_outcome(ready_dims)
                .map_err(|detail| Failure::new("ready", 13, detail))?;
            print_step(
                "ready",
                "pass",
                &format!("fixture up at {ready_dims} — {outcome} ({armed})"),
            );
        }
    }

    match mode {
        Mode::Steady => steady_steps(&*master, &writer, &mut tracker, timeout)?,
        Mode::Early => early_steps(&*master, &writer, &mut tracker, timeout)?,
    }

    writer
        .send(&[QUIT_BYTE])
        .map_err(|err| Failure::new("quit", 17, format!("writing the quit byte failed: {err}")))?;
    let quit = wait_for_report(
        &mut tracker,
        "the fixture's quit report",
        |report| report.event == EVENT_QUIT,
        timeout,
    )
    .map_err(|detail| Failure::new("quit", 17, detail))?;
    print_step("quit", "pass", &format!("fixture reports: {quit}"));

    let exit_detail = wait_child(child.as_mut(), timeout)
        .map_err(|detail| Failure::new("child_exit", 18, detail))?;
    print_step("child_exit", "pass", &exit_detail);

    let (events, _, end) = tracker.into_teardown_parts();
    let teardown_detail = teardown(master, &events, end, timeout)
        .map_err(|detail| Failure::new("teardown", 19, detail))?;
    print_step("teardown", "pass", &teardown_detail);
    Ok(())
}

/// The steady scenario's middle: a baseline sample, then grow and shrink —
/// each transition observed as a delivered notification, each sample
/// showing the env values pinned at their spawn-time numbers. The shrink
/// proves propagation is repeatable, not one-shot; its sample is the
/// after-both-resizes env assertion.
fn steady_steps(
    master: &dyn MasterPty,
    writer: &SharedWriter,
    tracker: &mut OutputTracker,
    timeout: Duration,
) -> Result<(), Failure> {
    let baseline = sample_dims(writer, tracker, SPAWN, timeout)
        .map_err(|detail| Failure::new("baseline", 14, detail))?;
    print_step(
        "baseline",
        "pass",
        &format!("before any resize: {baseline}"),
    );

    let grow = observe_resize(master, tracker, GROWN, timeout)
        .map_err(|detail| Failure::new("grow", 15, detail))?;
    let grown_sample = sample_dims(writer, tracker, GROWN, timeout)
        .map_err(|detail| Failure::new("grow", 15, detail))?;
    print_step(
        "grow",
        "pass",
        &format!("{SPAWN} -> {GROWN} observed via {WINCH_VIA} ({grow}); sample: {grown_sample}"),
    );

    let shrink = observe_resize(master, tracker, SPAWN, timeout)
        .map_err(|detail| Failure::new("shrink", 16, detail))?;
    let shrunk_sample = sample_dims(writer, tracker, SPAWN, timeout)
        .map_err(|detail| Failure::new("shrink", 16, detail))?;
    print_step(
        "shrink",
        "pass",
        &format!(
            "{GROWN} -> {SPAWN} observed via {WINCH_VIA} ({shrink}); sample after both resizes: {shrunk_sample}"
        ),
    );
    Ok(())
}

/// The early scenario's middle. The deterministic part: the console must
/// settle at one of the two known geometries with env pinned — anything
/// else is corruption — and a follow-up resize away from wherever it
/// settled must still be observed, proving the launch race did not corrupt
/// the channel. Which arm of the race occurred is the record, not an
/// assertion: the early resize can apply and hold, or be silently dropped —
/// the first Windows runs produced both, one per run, with the resize call
/// itself succeeding either way. POSIX has no dropped arm (the size is
/// kernel state the moment the ioctl returns), so a drop there would
/// surface as a new platform finding in this step's detail.
fn early_steps(
    master: &dyn MasterPty,
    writer: &SharedWriter,
    tracker: &mut OutputTracker,
    timeout: Duration,
) -> Result<(), Failure> {
    // Drain before sampling: a slow-applying early resize gets its window
    // to land (and to deliver any notification) before the verdict on
    // where the console settled is read.
    tracker
        .pump(WINCH_DRAIN)
        .map_err(|detail| Failure::new("settle", 14, detail))?;
    let sample = request_dims(writer, tracker, timeout)
        .map_err(|detail| Failure::new("settle", 14, detail))?;
    check_env_pinned(&sample).map_err(|detail| Failure::new("settle", 14, detail))?;
    let settled = report_dims(&sample).map_err(|detail| Failure::new("settle", 14, detail))?;
    let outcome =
        early_settle_outcome(settled).map_err(|detail| Failure::new("settle", 14, detail))?;
    let early_winches = max_seq(tracker, EVENT_WINCH);
    print_step(
        "settle",
        "pass",
        &format!(
            "settled at {settled} with env pinned ({sample}); {outcome}; notifications delivered so far: {early_winches}"
        ),
    );

    // Resize away from wherever the console settled: recovering toward the
    // settled size would be a no-op that no platform notifies about.
    let target = if settled == GROWN { SPAWN } else { GROWN };
    let recover = observe_resize(master, tracker, target, timeout)
        .map_err(|detail| Failure::new("recover", 15, detail))?;
    let recovered_sample = sample_dims(writer, tracker, target, timeout)
        .map_err(|detail| Failure::new("recover", 15, detail))?;
    print_step(
        "recover",
        "pass",
        &format!(
            "follow-up {settled} -> {target} observed via {WINCH_VIA} ({recover}); sample: {recovered_sample}"
        ),
    );
    Ok(())
}

/// Resize the PTY master. On failure the OS error is the probe's finding —
/// a typed failure, never a hang.
fn resize(master: &dyn MasterPty, to: Dims) -> Result<(), String> {
    master
        .resize(PtySize {
            rows: to.rows,
            cols: to.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|err| format!("resize to {to} failed: {err:#}"))
}

/// Issue one resize and wait for its delivered notification.
fn observe_resize(
    master: &dyn MasterPty,
    tracker: &mut OutputTracker,
    to: Dims,
    timeout: Duration,
) -> Result<Report, String> {
    let watermark = max_seq(tracker, EVENT_WINCH);
    resize(master, to)?;
    await_winch(tracker, watermark, to, timeout)
}

/// Wait for a winch report newer than `watermark` that carries exactly
/// `want` — the notification for the transition just issued. A resize may
/// deliver transitional notifications before the geometry settles (Windows
/// conhost adjusts the buffer in stages, and the first CI run showed a
/// shrink notifying at new-width/old-height first), so notifications with
/// other geometries are let pass rather than failed on sight; if the
/// requested size never arrives, the timeout failure names every fresh
/// notification that did.
fn await_winch(
    tracker: &mut OutputTracker,
    watermark: u32,
    want: Dims,
    timeout: Duration,
) -> Result<Report, String> {
    wait_for_report(
        tracker,
        "the fixture's winch report carrying the requested size",
        |report| {
            report.event == EVENT_WINCH
                && seq_of(report).is_some_and(|seq| seq > watermark)
                && report_dims(report).is_ok_and(|dims| dims == want)
        },
        timeout,
    )
    .map_err(|err| {
        let strays: Vec<String> = reports_in(&tracker.visible_text())
            .iter()
            .filter(|report| {
                report.event == EVENT_WINCH && seq_of(report).is_some_and(|seq| seq > watermark)
            })
            .map(|report| format!("{report}"))
            .collect();
        if strays.is_empty() {
            err
        } else {
            format!(
                "{err}; fresh notifications that did not carry {want}: {}",
                strays.join(", ")
            )
        }
    })
}

/// Ask the fixture for an on-demand dims sample and assert both channels:
/// the live size must be `want`, and `COLUMNS`/`LINES` must still carry the
/// spawn-time values.
fn sample_dims(
    writer: &SharedWriter,
    tracker: &mut OutputTracker,
    want: Dims,
    timeout: Duration,
) -> Result<Report, String> {
    let dims = request_dims(writer, tracker, timeout)?;
    check_dims_sample(&dims, want)?;
    Ok(dims)
}

/// Ask the fixture for a fresh on-demand dims sample, with no expectation
/// about what it carries — the early scenario's settle step reads the
/// verdict from it rather than asserting one.
fn request_dims(
    writer: &SharedWriter,
    tracker: &mut OutputTracker,
    timeout: Duration,
) -> Result<Report, String> {
    let watermark = max_seq(tracker, EVENT_DIMS);
    writer
        .send(&[DIMS_BYTE])
        .map_err(|err| format!("writing the dims request failed: {err}"))?;
    wait_for_report(
        tracker,
        "the fixture's dims report",
        |report| report.event == EVENT_DIMS && seq_of(report).is_some_and(|seq| seq > watermark),
        timeout,
    )
}

/// The assertion half of [`sample_dims`], separated so it is testable
/// without a live PTY.
fn check_dims_sample(dims: &Report, want: Dims) -> Result<(), String> {
    let got = report_dims(dims)?;
    if got != want {
        return Err(format!(
            "the live size is {got}, not the expected {want}: {dims}"
        ));
    }
    check_env_pinned(dims)
}

/// The env half of the sample assertion, usable on its own where the live
/// size is a verdict to read rather than a value to expect: whatever the
/// terminal did, `COLUMNS`/`LINES` must still carry the spawn-time values.
fn check_env_pinned(dims: &Report) -> Result<(), String> {
    let (Some(env_columns), Some(env_lines)) = (dims.field("env_columns"), dims.field("env_lines"))
    else {
        return Err(format!("the dims report carries no env fields: {dims}"));
    };
    if (env_columns, env_lines)
        != (
            SPAWN.cols.to_string().as_str(),
            SPAWN.rows.to_string().as_str(),
        )
    {
        return Err(format!(
            "COLUMNS/LINES moved off their spawn-time values {SPAWN}: {dims} — env is set once at spawn and must not track resizes"
        ));
    }
    Ok(())
}

/// Classify the size the fixture sampled at startup under an early resize.
/// The race has exactly two legitimate outcomes; anything else means the
/// early resize corrupted the geometry.
fn early_ready_outcome(dims: Dims) -> Result<&'static str, String> {
    if dims == GROWN {
        Ok("the early resize had already applied when the fixture sampled")
    } else if dims == SPAWN {
        Ok(
            "the fixture sampled the spawn size — the early resize had not applied yet, or never will; the settle step reads the verdict",
        )
    } else {
        Err(format!(
            "startup size {dims} is neither the spawn size {SPAWN} nor the requested {GROWN} — the early resize corrupted the geometry"
        ))
    }
}

/// Classify where the console settled after the early resize was given its
/// window to land. Both known geometries are legitimate race outcomes —
/// the first Windows runs produced one of each — and anything else is
/// corruption.
fn early_settle_outcome(dims: Dims) -> Result<&'static str, String> {
    if dims == GROWN {
        Ok("the early resize applied and held")
    } else if dims == SPAWN {
        Ok(
            "the early resize was silently dropped in the launch race — the resize call succeeded, the console stayed at the spawn size",
        )
    } else {
        Err(format!(
            "settled size {dims} is neither the spawn size {SPAWN} nor the requested {GROWN} — the early resize corrupted the geometry"
        ))
    }
}

/// The `cols`/`rows` fields of a report, parsed. Every geometry assertion
/// goes through this, so a missing or garbled field is named, not
/// unwrapped.
fn report_dims(report: &Report) -> Result<Dims, String> {
    let field = |key: &str| -> Result<u16, String> {
        report
            .field(key)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| format!("no parseable {key} in: {report}"))
    };
    Ok(Dims {
        cols: field("cols")?,
        rows: field("rows")?,
    })
}

/// The first currently-visible report matching `matches`, if any.
fn find_report(tracker: &OutputTracker, matches: impl Fn(&Report) -> bool) -> Option<Report> {
    reports_in(&tracker.visible_text())
        .into_iter()
        .find(|report| matches(report))
}

/// The highest `seq` among currently-visible reports of `event` — the
/// watermark a later wait must exceed. Matching on presence alone would be
/// wrong twice over: ConPTY repaints old report lines after a resize, and a
/// notification from an earlier transition could satisfy a later wait.
fn max_seq(tracker: &OutputTracker, event: &str) -> u32 {
    reports_in(&tracker.visible_text())
        .iter()
        .filter(|report| report.event == event)
        .filter_map(seq_of)
        .max()
        .unwrap_or(0)
}

fn seq_of(report: &Report) -> Option<u32> {
    report.field("seq").and_then(|value| value.parse().ok())
}

/// The fixture binary sits next to this one — cargo builds every workspace
/// binary into the same directory.
fn sibling_resize_child() -> Result<std::path::PathBuf, String> {
    let me = std::env::current_exe().map_err(|err| format!("current_exe failed: {err}"))?;
    let dir = me
        .parent()
        .ok_or_else(|| "current_exe has no parent directory".to_string())?;
    let fixture = dir.join(format!("resize-child{}", std::env::consts::EXE_SUFFIX));
    if fixture.exists() {
        Ok(fixture)
    } else {
        Err(format!(
            "fixture binary not found at {} — build it first: \
             cargo build --package agent-bridge-probe-child",
            fixture.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridge_interactive_probe::pty::{EndInfo, ReaderEvent};
    use std::sync::{Mutex, mpsc};

    /// A tracker whose stream carries `chunks` and then ends — the shape a
    /// completed fixture run leaves behind, so pumps return promptly
    /// instead of erroring on a disconnected channel.
    fn tracker_with(chunks: &[&str]) -> OutputTracker {
        let (tx, events) = mpsc::channel();
        for chunk in chunks {
            tx.send(ReaderEvent::Data {
                at: Instant::now(),
                bytes: chunk.as_bytes().to_vec(),
            })
            .unwrap();
        }
        tx.send(ReaderEvent::End(EndInfo {
            reason: "eof".to_string(),
            cursor_queries_answered: 0,
            cursor_reply_error: None,
        }))
        .unwrap();
        OutputTracker::new(events, FirstTokenClock::new(Instant::now()), None)
    }

    fn parsed(line: &str) -> Report {
        Report::parse(line).expect("test line must parse as a report")
    }

    #[test]
    fn args_select_mode_and_timeout() {
        let args = ["early", "--timeout-secs", "3"].map(String::from);
        let (mode, timeout) = parse_args(args.into_iter()).unwrap();
        assert!(matches!(mode, Mode::Early));
        assert_eq!(timeout, Duration::from_secs(3));
    }

    #[test]
    fn a_mode_is_required_and_unknown_arguments_are_rejected() {
        assert!(parse_args(std::iter::empty()).is_err());
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
        assert!(parse_args(["steady".to_string(), "early".to_string()].into_iter()).is_err());
    }

    #[test]
    fn a_stale_winch_below_the_watermark_never_satisfies_a_wait() {
        // ConPTY repaints the screen after a resize, so an already-observed
        // winch line reappears in the stream; only a seq above the
        // watermark may count, even when the stale line carries exactly the
        // dimensions being waited for.
        let mut tracker = tracker_with(&["probe-child event=winch seq=1 cols=80 rows=24\r\n"]);
        let err = await_winch(&mut tracker, 1, SPAWN, Duration::from_millis(20)).unwrap_err();
        assert!(err.contains("winch report"), "unexpected error: {err}");
    }

    #[test]
    fn a_fresh_winch_with_the_requested_dims_passes() {
        let mut tracker = tracker_with(&[
            "probe-child event=winch seq=1 cols=120 rows=40\r\n",
            "probe-child event=winch seq=2 cols=80 rows=24\r\n",
        ]);
        let winch = await_winch(&mut tracker, 1, SPAWN, Duration::from_secs(5))
            .expect("the fresh notification must be matched");
        assert_eq!(seq_of(&winch), Some(2));
    }

    #[test]
    fn a_winch_that_never_carries_the_requested_size_fails_naming_the_strays() {
        let mut tracker = tracker_with(&["probe-child event=winch seq=2 cols=200 rows=50\r\n"]);
        let err = await_winch(&mut tracker, 1, GROWN, Duration::from_secs(5)).unwrap_err();
        assert!(
            err.contains("cols=200 rows=50") && err.contains("did not carry 120x40"),
            "the stray notification and the wanted size must both be named: {err}"
        );
    }

    #[test]
    fn a_staged_resize_is_tolerated_until_the_requested_size_arrives() {
        // The first Windows CI run: a 120x40 -> 80x24 shrink notified at
        // new-width/old-height first. Transitional notifications must not
        // fail the wait; the one carrying the requested size satisfies it.
        let mut tracker = tracker_with(&[
            "probe-child event=winch seq=2 cols=80 rows=40 buf=80x40\r\n",
            "probe-child event=winch seq=3 cols=80 rows=24 buf=80x40\r\n",
        ]);
        let winch = await_winch(&mut tracker, 1, SPAWN, Duration::from_secs(5))
            .expect("the settled notification must be matched");
        assert_eq!(seq_of(&winch), Some(3));
    }

    #[test]
    fn a_dims_sample_with_pinned_env_passes() {
        let report =
            parsed("probe-child event=dims seq=1 cols=120 rows=40 env_columns=80 env_lines=24");
        check_dims_sample(&report, GROWN).expect("a pinned sample must pass");
    }

    #[test]
    fn env_that_tracked_the_resize_is_a_failure() {
        let report =
            parsed("probe-child event=dims seq=1 cols=120 rows=40 env_columns=120 env_lines=40");
        let err = check_dims_sample(&report, GROWN).unwrap_err();
        assert!(
            err.contains("set once at spawn"),
            "the env contract must be named: {err}"
        );
    }

    #[test]
    fn a_sample_at_the_wrong_live_size_is_a_failure() {
        let report =
            parsed("probe-child event=dims seq=1 cols=80 rows=24 env_columns=80 env_lines=24");
        let err = check_dims_sample(&report, GROWN).unwrap_err();
        assert!(
            err.contains("80x24") && err.contains("120x40"),
            "both geometries must be named: {err}"
        );
    }

    #[test]
    fn a_sample_without_env_fields_is_a_failure() {
        let report = parsed("probe-child event=dims seq=1 cols=120 rows=40");
        let err = check_dims_sample(&report, GROWN).unwrap_err();
        assert!(err.contains("no env fields"), "unexpected error: {err}");
    }

    #[test]
    fn the_early_ready_race_has_exactly_two_legitimate_outcomes() {
        assert!(early_ready_outcome(SPAWN).is_ok());
        assert!(early_ready_outcome(GROWN).is_ok());
        let err = early_ready_outcome(Dims {
            cols: 120,
            rows: 24,
        })
        .unwrap_err();
        assert!(
            err.contains("corrupted"),
            "a mixed geometry must be typed as corruption: {err}"
        );
    }

    #[test]
    fn the_early_settle_verdict_names_the_race_arm_and_types_corruption() {
        assert!(early_settle_outcome(GROWN).unwrap().contains("held"));
        assert!(
            early_settle_outcome(SPAWN)
                .unwrap()
                .contains("silently dropped"),
            "the dropped arm is a finding and must say so"
        );
        let err = early_settle_outcome(Dims { cols: 80, rows: 40 }).unwrap_err();
        assert!(
            err.contains("corrupted"),
            "a mixed geometry must be typed as corruption: {err}"
        );
    }

    #[test]
    fn repainted_duplicates_do_not_move_the_watermark() {
        let mut tracker = tracker_with(&[
            "probe-child event=winch seq=1 cols=120 rows=40\r\n",
            "probe-child event=winch seq=2 cols=80 rows=24\r\n",
            // The repaint: both lines appear again, verbatim.
            "probe-child event=winch seq=1 cols=120 rows=40\r\n",
            "probe-child event=winch seq=2 cols=80 rows=24\r\n",
        ]);
        tracker.pump(Duration::from_millis(100)).unwrap();
        assert_eq!(max_seq(&tracker, EVENT_WINCH), 2);
    }

    #[test]
    fn sample_dims_sends_the_request_byte_and_matches_only_a_fresh_report() {
        struct SinkWriter(std::sync::Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for SinkWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let written = std::sync::Arc::new(Mutex::new(Vec::new()));
        let writer = SharedWriter::new(Box::new(SinkWriter(written.clone())));
        let (tx, events) = mpsc::channel();
        let send = |line: &str| {
            tx.send(ReaderEvent::Data {
                at: Instant::now(),
                bytes: line.as_bytes().to_vec(),
            })
            .unwrap();
        };
        // An earlier sample is already on screen when the request goes out;
        // it must become the watermark, not the answer.
        send("probe-child event=dims seq=1 cols=80 rows=24 env_columns=80 env_lines=24\r\n");
        let mut tracker = OutputTracker::new(events, FirstTokenClock::new(Instant::now()), None);
        tracker.pump(Duration::from_millis(100)).unwrap();
        // The fresh answer arrives only once the request is in flight.
        send("probe-child event=dims seq=2 cols=120 rows=40 env_columns=80 env_lines=24\r\n");
        let report = sample_dims(&writer, &mut tracker, GROWN, Duration::from_secs(5))
            .expect("the fresh sample must be matched");
        assert_eq!(seq_of(&report), Some(2));
        assert_eq!(written.lock().unwrap().as_slice(), &[DIMS_BYTE]);
    }
}
