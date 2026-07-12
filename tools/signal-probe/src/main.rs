//! Interrupt-delivery probe — proves, against the probe-child fixture under
//! a real PTY, that the two interrupt mechanisms are distinct:
//!
//! - **The byte path.** 0x03 (the Ctrl+C character) written to the PTY
//!   master is *input*, not an interrupt. A raw-mode child — `ISIG` off /
//!   `ENABLE_PROCESSED_INPUT` off, the mode full-screen interactive CLIs
//!   run in — reads it like any other byte and no handler fires.
//!   Interrupting such a CLI therefore means writing this byte and letting
//!   the CLI react, exactly as its own Ctrl+C keybinding would.
//! - **The signal path.** A `SIGINT` delivered with `kill(-pgid, SIGINT)`
//!   (POSIX) reaches the *same* raw-mode child's handler with no byte
//!   appearing on stdin. This is the separate shutdown-path primitive — a
//!   process-group signal also reaches any subshells the child spawned —
//!   and it never doubles as the interrupt keypress.
//!
//! The cooked-mode run shows where the two are commonly conflated: with
//! `ISIG` on, the terminal itself consumes the same 0x03 and synthesizes
//! `SIGINT` — the byte never reaches the child's read. On Windows the
//! console host plays the line discipline's role: with
//! `ENABLE_PROCESSED_INPUT` set it raises `CTRL_C_EVENT` for 0x03, with it
//! cleared ConPTY passes the byte through to the input stream. Both
//! Windows runs assert that contract and record the observed console-mode
//! bits, so a ConPTY behavior change fails a PR here instead of surfacing
//! as a runtime mystery later.
//!
//! One scenario per invocation: `signal-probe raw` / `signal-probe cooked`.
//! Same step contract as the sibling probes — one machine-readable
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
use agent_bridge_interactive_probe::platform_report;
use agent_bridge_interactive_probe::pty::{
    OutputTracker, SharedWriter, alloc_pty, spawn_reader, teardown, wait_child,
};
use agent_bridge_interactive_probe::reports::{count_reports, wait_for_report};
use agent_bridge_probe_child::{EVENT_INTERRUPT, EVENT_QUIT, EVENT_READY, QUIT_BYTE};
use portable_pty::{CommandBuilder, PtyPair};

/// The interrupt character under test, written to the PTY master exactly as
/// a terminal writes a Ctrl+C keypress.
const CTRL_C: u8 = 0x03;

/// Deliberately wide: ConPTY reflows output to the PTY width, and a report
/// line hard-wrapped mid-`key=value` would not parse. The fixture's longest
/// line is well under half of this.
const COLS: u16 = 200;
const ROWS: u16 = 50;

const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// How long "nothing further" gets to become observable before an absence
/// is asserted. Signal synthesis is immediate and the fixture's watcher
/// reports within 20ms, so half a second is generous, not tight.
const SETTLE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
enum Mode {
    Raw,
    Cooked,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Raw => "raw",
            Mode::Cooked => "cooked",
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
            eprintln!("signal-probe: {message}");
            std::process::exit(2);
        }
    };
    println!("signal-probe {}", platform_report());
    match run(mode, timeout) {
        Ok(()) => println!("signal-probe mode={} result=pass", mode.name()),
        Err(failure) => {
            print_step(failure.step, "fail", &failure.detail);
            eprintln!(
                "signal-probe: step {} failed: {}",
                failure.step, failure.detail
            );
            std::process::exit(failure.code);
        }
    }
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<(Mode, Duration), String> {
    const USAGE: &str = "usage: signal-probe <raw|cooked> [--timeout-secs N]";
    let mut mode: Option<Mode> = None;
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "raw" if mode.is_none() => mode = Some(Mode::Raw),
            "cooked" if mode.is_none() => mode = Some(Mode::Cooked),
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
    println!("signal-probe step={step} status={status} detail=\"{clean}\"");
}

/// The scenario, in lifecycle order. Exit codes are step-stable: 10 alloc,
/// 11 spawn, 12 ready, 13 the byte write's outcome, 14 the process-group
/// signal, 15 quit, 16 child exit, 17 teardown.
fn run(mode: Mode, timeout: Duration) -> Result<(), Failure> {
    let (pair, alloc_ms) =
        alloc_pty(COLS, ROWS, timeout).map_err(|detail| Failure::new("alloc", 10, detail))?;
    print_step(
        "alloc",
        "pass",
        &format!("pty allocated at {COLS}x{ROWS} in {alloc_ms}ms"),
    );
    let PtyPair { master, slave } = pair;

    let fixture = sibling_probe_child().map_err(|detail| Failure::new("spawn", 11, detail))?;
    let mut command = CommandBuilder::new(&fixture);
    command.arg(mode.name());
    let mut child = slave
        .spawn_command(command)
        .map_err(|err| Failure::new("spawn", 11, format!("child spawn failed: {err:#}")))?;
    // Release our copy of the child end: holding it open would keep the
    // master from ever seeing end-of-stream after the child exits.
    drop(slave);
    print_step(
        "spawn",
        "pass",
        &format!(
            "spawned `{} {}` pid={}",
            fixture.display(),
            mode.name(),
            child
                .process_id()
                .map_or_else(|| "unknown".to_string(), |pid| pid.to_string()),
        ),
    );

    let reader = master
        .try_clone_reader()
        .map_err(|err| Failure::new("ready", 12, format!("cloning the reader failed: {err:#}")))?;
    let writer =
        SharedWriter::new(master.take_writer().map_err(|err| {
            Failure::new("ready", 12, format!("taking the writer failed: {err:#}"))
        })?);
    let events = spawn_reader(reader, writer.clone(), Arc::new(AtomicU32::new(0)));
    let mut tracker = OutputTracker::new(events, FirstTokenClock::new(Instant::now()), None);

    // Nothing is written before the fixture's ready report: only input that
    // arrives after it is guaranteed to be observed under the requested
    // terminal mode.
    let ready = wait_for_report(
        &mut tracker,
        "the fixture's ready report",
        |report| report.event == EVENT_READY,
        timeout,
    )
    .map_err(|detail| Failure::new("ready", 12, detail))?;
    if ready.field("mode") != Some(mode.name()) {
        return Err(Failure::new(
            "ready",
            12,
            format!("the fixture came up in the wrong mode: {ready}"),
        ));
    }
    // The bit under test must have actually applied — the fixture reports
    // the verified state, not the request. A silently unapplied mode would
    // make the cooked run's expectations pass against a raw terminal.
    let signal_bit = if cfg!(windows) {
        "processed_input"
    } else {
        "isig"
    };
    let expected_bit = match mode {
        Mode::Raw => "off",
        Mode::Cooked => "on",
    };
    if ready.field(signal_bit) != Some(expected_bit) {
        return Err(Failure::new(
            "ready",
            12,
            format!("the terminal did not apply {signal_bit}={expected_bit}: {ready}"),
        ));
    }
    #[cfg(unix)]
    let pgid: i32 = ready
        .field("pgid")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            Failure::new(
                "ready",
                12,
                format!("the ready report carries no parseable pgid: {ready}"),
            )
        })?;
    print_step("ready", "pass", &format!("fixture reports: {ready}"));

    writer
        .send(&[CTRL_C])
        .map_err(|err| Failure::new("ctrl_c_byte", 13, format!("writing 0x03 failed: {err}")))?;
    let byte_detail = match mode {
        Mode::Raw => check_byte_is_data(&mut tracker, timeout),
        Mode::Cooked => check_byte_becomes_interrupt(&mut tracker, timeout),
    }
    .map_err(|detail| Failure::new("ctrl_c_byte", 13, detail))?;
    print_step("ctrl_c_byte", "pass", &byte_detail);

    if matches!(mode, Mode::Raw) {
        #[cfg(unix)]
        {
            let signal_detail = check_pgroup_signal(&mut tracker, pgid, timeout)
                .map_err(|detail| Failure::new("pgroup_signal", 14, detail))?;
            print_step("pgroup_signal", "pass", &signal_detail);
        }
        #[cfg(windows)]
        print_step(
            "pgroup_signal",
            "skip",
            "Windows has no process-group SIGINT; the console-control analogue (CTRL_C_EVENT) is what the cooked-mode run observes",
        );
    }

    writer
        .send(&[QUIT_BYTE])
        .map_err(|err| Failure::new("quit", 15, format!("writing the quit byte failed: {err}")))?;
    let quit = wait_for_report(
        &mut tracker,
        "the fixture's quit report",
        |report| report.event == EVENT_QUIT,
        timeout,
    )
    .map_err(|detail| Failure::new("quit", 15, detail))?;
    print_step("quit", "pass", &format!("fixture reports: {quit}"));

    let exit_detail = wait_child(child.as_mut(), timeout)
        .map_err(|detail| Failure::new("child_exit", 16, detail))?;
    print_step("child_exit", "pass", &exit_detail);

    let (events, _, end) = tracker.into_teardown_parts();
    let teardown_detail = teardown(master, &events, end, timeout, None)
        .map_err(|detail| Failure::new("teardown", 17, detail))?;
    print_step("teardown", "pass", &teardown_detail);
    Ok(())
}

/// Raw mode: 0x03 must arrive on stdin as plain data, and nothing may
/// synthesize an interrupt from it.
fn check_byte_is_data(tracker: &mut OutputTracker, timeout: Duration) -> Result<String, String> {
    wait_for_report(
        tracker,
        "the fixture's report of the 0x03 byte",
        |report| report.is_byte(CTRL_C),
        timeout,
    )?;
    tracker.pump(SETTLE).map_err(|detail| {
        format!("draining output while awaiting interrupt silence failed: {detail}")
    })?;
    let fired = count_reports(tracker, |report| report.event == EVENT_INTERRUPT);
    if fired > 0 {
        return Err(format!(
            "the interrupt handler fired {fired} time(s) from the byte write — to a raw-mode child 0x03 must be data, not a signal"
        ));
    }
    Ok(format!(
        "0x03 arrived on stdin as data and no handler fired within {}ms{}",
        SETTLE.as_millis(),
        if cfg!(windows) {
            " — ConPTY passed the byte through to the input stream rather than raising a console-control event"
        } else {
            ""
        }
    ))
}

/// Cooked mode: the same write must fire the interrupt handler, and the
/// byte itself must never reach the child's read — the terminal consumed
/// it to synthesize the signal.
fn check_byte_becomes_interrupt(
    tracker: &mut OutputTracker,
    timeout: Duration,
) -> Result<String, String> {
    let interrupt = wait_for_report(
        tracker,
        "the fixture's interrupt report",
        |report| report.event == EVENT_INTERRUPT,
        timeout,
    )?;
    tracker.pump(SETTLE).map_err(|detail| {
        format!("draining output while awaiting byte silence failed: {detail}")
    })?;
    let leaked = count_reports(tracker, |report| report.is_byte(CTRL_C));
    if leaked > 0 {
        return Err(format!(
            "0x03 also arrived on stdin as data ({leaked} report(s)) — the terminal both synthesized the signal and delivered the byte"
        ));
    }
    Ok(format!(
        "the terminal consumed 0x03 and {} instead ({interrupt}); the byte never reached the child's read",
        if cfg!(windows) {
            "raised CTRL_C_EVENT"
        } else {
            "synthesized SIGINT"
        }
    ))
}

/// Raw mode, POSIX: a real SIGINT to the process group must reach the same
/// child that just read 0x03 as data — and must not put a byte on stdin.
/// This is the pairing that proves the two paths are distinct mechanisms.
#[cfg(unix)]
fn check_pgroup_signal(
    tracker: &mut OutputTracker,
    pgid: i32,
    timeout: Duration,
) -> Result<String, String> {
    use agent_bridge_probe_child::EVENT_BYTE;

    let bytes_before = count_reports(tracker, |report| report.event == EVENT_BYTE);
    // SAFETY: kill takes only plain values; the negative pid addresses the
    // process group the child itself reported.
    if unsafe { libc::kill(-pgid, libc::SIGINT) } != 0 {
        return Err(format!(
            "kill(-{pgid}, SIGINT) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    wait_for_report(
        tracker,
        "the fixture's interrupt report after the delivered SIGINT",
        |report| report.event == EVENT_INTERRUPT,
        timeout,
    )?;
    tracker.pump(SETTLE).map_err(|detail| {
        format!("draining output while awaiting byte silence failed: {detail}")
    })?;
    let new_bytes = count_reports(tracker, |report| report.event == EVENT_BYTE) - bytes_before;
    if new_bytes > 0 {
        return Err(format!(
            "{new_bytes} byte report(s) appeared from the delivered SIGINT — the signal path must not put data on stdin"
        ));
    }
    Ok(format!(
        "kill(-{pgid}, SIGINT) reached the raw-mode child's handler with no byte on stdin — the signal path and the byte path are distinct"
    ))
}

/// The fixture binary sits next to this one — cargo builds every workspace
/// binary into the same directory.
fn sibling_probe_child() -> Result<std::path::PathBuf, String> {
    let me = std::env::current_exe().map_err(|err| format!("current_exe failed: {err}"))?;
    let dir = me
        .parent()
        .ok_or_else(|| "current_exe has no parent directory".to_string())?;
    let fixture = dir.join(format!("probe-child{}", std::env::consts::EXE_SUFFIX));
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
    use std::sync::mpsc;

    /// A tracker whose stream carries `chunks` and then ends — the shape a
    /// completed fixture run leaves behind, so settle-pumps return promptly
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

    #[test]
    fn args_select_mode_and_timeout() {
        let args = ["cooked", "--timeout-secs", "3"].map(String::from);
        let (mode, timeout) = parse_args(args.into_iter()).unwrap();
        assert!(matches!(mode, Mode::Cooked));
        assert_eq!(timeout, Duration::from_secs(3));
    }

    #[test]
    fn a_mode_is_required_and_unknown_arguments_are_rejected() {
        assert!(parse_args(std::iter::empty()).is_err());
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
        assert!(parse_args(["raw".to_string(), "cooked".to_string()].into_iter()).is_err());
    }

    #[test]
    fn raw_mode_check_rejects_a_fired_interrupt() {
        let mut tracker = tracker_with(&[
            "probe-child event=byte hex=0x03\r\n",
            "probe-child event=interrupt count=1 via=sigint-handler\r\n",
        ]);
        let err = check_byte_is_data(&mut tracker, Duration::from_secs(5)).unwrap_err();
        assert!(
            err.contains("must be data, not a signal"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn cooked_mode_check_rejects_a_leaked_byte() {
        let mut tracker = tracker_with(&[
            "probe-child event=interrupt count=1 via=sigint-handler\r\n",
            "probe-child event=byte hex=0x03\r\n",
        ]);
        let err = check_byte_becomes_interrupt(&mut tracker, Duration::from_secs(5)).unwrap_err();
        assert!(
            err.contains("also arrived on stdin"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn cooked_mode_check_accepts_a_clean_synthesis() {
        let mut tracker =
            tracker_with(&["probe-child event=interrupt count=1 via=sigint-handler\r\n"]);
        let detail = check_byte_becomes_interrupt(&mut tracker, Duration::from_secs(5))
            .expect("a clean synthesis must pass");
        assert!(
            detail.contains("never reached the child's read"),
            "unexpected detail: {detail}"
        );
    }
}
