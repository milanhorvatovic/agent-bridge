//! replay-child — performs a compiled replay plan from the far side of the
//! terminal.
//!
//! It must be the child under the PTY, not code inside the probe: the whole
//! point of the replay lane is that the recorded traffic crosses the same
//! terminal boundary the synthetic traffic does, and only a process on the
//! child side of that boundary can put it there. It is deliberately dumb —
//! read the plan, write each chunk when it falls due, exit — because every
//! judgement about what arrived belongs to the probe, which is the side that
//! can see it.
//!
//! Pacing is against an absolute schedule on the shared monotonic clock: a
//! chunk is due at the sum of every gap before it, and an overdue chunk goes
//! out immediately rather than waiting its full gap again, so timer
//! coarseness makes delivery burstier without stretching the run.
//!
//! In recorded mode the child turns off output post-processing on its side
//! of the terminal before writing a byte. The captured stream already went
//! through a terminal's rewriting once, when it was recorded; letting this
//! terminal rewrite it again would hand the probe a stream that differs from
//! the recording on every line ending, and the byte-for-byte comparison the
//! mode exists for would be comparing the wrong thing.
//!
//! Usage: replay-child --plan <plan.ndjson> [--bytes <plan.bytes>]

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use agent_bridge_fake_cli::clock::monotonic_ns;
use agent_bridge_perf_probe::replay::{Mode, Plan};

fn main() {
    let (plan_path, bytes_path) = match parse_args(std::env::args().skip(1)) {
        Ok(paths) => paths,
        Err(message) => {
            eprintln!("replay-child: {message}");
            std::process::exit(2);
        }
    };
    let plan = match Plan::read(&plan_path, bytes_path.as_deref()) {
        Ok(plan) => plan,
        Err(message) => {
            eprintln!("replay-child: {message}");
            std::process::exit(3);
        }
    };
    if let Err(message) = perform(&plan) {
        eprintln!("replay-child: {message}");
        std::process::exit(4);
    }
}

fn parse_args<I: Iterator<Item = String>>(
    mut args: I,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let mut plan = None;
    let mut bytes = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--plan" => plan = Some(PathBuf::from(args.next().ok_or("--plan needs a value")?)),
            "--bytes" => bytes = Some(PathBuf::from(args.next().ok_or("--bytes needs a value")?)),
            other => {
                return Err(format!(
                    "unknown argument: {other}. usage: replay-child --plan <file> [--bytes <file>]"
                ));
            }
        }
    }
    Ok((plan.ok_or("--plan is required")?, bytes))
}

fn perform(plan: &Plan) -> Result<(), String> {
    if plan.mode == Mode::Recorded {
        raw_output()?;
    }
    // The exact stream the probe expects, derived from the same plan by the
    // same code — the symmetry the byte-for-byte and line verdicts rest on.
    let bytes = plan.expected_bytes();

    let mut stdout = std::io::stdout().lock();
    let start_ns = monotonic_ns();
    let mut due_ns = start_ns;
    for (gap_ns, range) in plan.chunk_ranges() {
        due_ns += gap_ns;
        sleep_until(due_ns);
        let chunk = &bytes[range];
        if chunk.is_empty() {
            continue; // a tail chunk the whole-line rounding emptied
        }
        // One write and one flush per chunk: the chunk boundary is part of
        // the recording, and it must hold on the wire where the probe
        // observes it.
        stdout
            .write_all(chunk)
            .and_then(|()| stdout.flush())
            .map_err(|err| format!("writing a {}-byte chunk failed: {err}", chunk.len()))?;
    }
    Ok(())
}

fn sleep_until(target_ns: u64) {
    let now = monotonic_ns();
    if target_ns > now {
        std::thread::sleep(Duration::from_nanos(target_ns - now));
    }
}

/// Turn off output post-processing on this process's terminal, so the bytes
/// written are the bytes the master reads.
#[cfg(unix)]
fn raw_output() -> Result<(), String> {
    // SAFETY: `termios` is a plain struct the two calls fill and read; fd 1
    // is this process's stdout for the process's whole life.
    unsafe {
        let mut termios = std::mem::zeroed::<libc::termios>();
        if libc::tcgetattr(1, &mut termios) != 0 {
            return Err(format!(
                "tcgetattr on stdout failed: {} — recorded mode needs a terminal",
                std::io::Error::last_os_error()
            ));
        }
        termios.c_oflag &= !(libc::OPOST as libc::tcflag_t);
        if libc::tcsetattr(1, libc::TCSANOW, &termios) != 0 {
            return Err(format!(
                "tcsetattr on stdout failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

/// Recorded mode never runs here: the terminal on this platform re-renders
/// its child's output, so byte identity is not a property even an
/// uncorrupted stream has. The probe refuses the combination before ever
/// spawning a child; this arm exists so the refusal has exactly one
/// authoritative wording there rather than two drifting ones.
#[cfg(windows)]
fn raw_output() -> Result<(), String> {
    Err("recorded-content replay is not meaningful under a re-rendering terminal".to_string())
}
