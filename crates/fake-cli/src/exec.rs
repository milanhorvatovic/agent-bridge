//! Step executor: runs a validated scenario against the real process
//! surfaces — stdout for scripted bytes, stdin for control input, the exit
//! status for the scripted outcome.
//!
//! Determinism discipline: no randomness, no wall-clock content. The only
//! time-dependent behavior is pacing (`byte_delay_ms`) and the `await_stdin`
//! deadline, both driven by the monotonic clock, and neither ever changes
//! which bytes go out — the same scenario produces the same stdout bytes on
//! every run and every OS.
//!
//! Stdout is the scripted surface and is written exclusively through
//! `write_all` — never through formatting macros — because byte-exactness is
//! the contract. Diagnostics go to stderr, which no scenario scripts.

use std::io::{self, Read, Write};
use std::process;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::scenario::{Channel, Scenario, Step};

/// Failure-path exit codes, distinct per class so a red CI lane is
/// diagnosable from the status alone. Callers should assert "non-zero plus a
/// stderr diagnostic" — the class split is a courtesy, not a contract.
const EXIT_MISMATCH: i32 = 3;
const EXIT_TIMEOUT: i32 = 4;
const EXIT_IO: i32 = 5;

pub fn run(scenario: &Scenario) -> ! {
    let mut stdout = io::stdout().lock();
    // The stdin reader starts on first use: scenarios without control input
    // never touch stdin at all.
    let mut stdin_bytes: Option<mpsc::Receiver<u8>> = None;
    for (index, step) in scenario.steps.iter().enumerate() {
        match step {
            Step::Emit {
                text,
                channel: Channel::Stdout,
                byte_delay_ms,
            } => {
                if let Err(err) = emit(&mut stdout, text, *byte_delay_ms) {
                    fail(
                        scenario,
                        index,
                        "emit",
                        EXIT_IO,
                        &format!("writing to stdout failed: {err}"),
                    );
                }
            }
            Step::AwaitStdin {
                expected,
                timeout_ms,
            } => {
                let bytes = stdin_bytes.get_or_insert_with(spawn_stdin_reader);
                if let Err((code, message)) = await_stdin(bytes, expected, *timeout_ms) {
                    fail(scenario, index, "await_stdin", code, &message);
                }
            }
            Step::Exit { code } => {
                if let Err(err) = stdout.flush() {
                    fail(
                        scenario,
                        index,
                        "exit",
                        EXIT_IO,
                        &format!("flushing stdout failed: {err}"),
                    );
                }
                process::exit(*code);
            }
        }
    }
    unreachable!("scenario validation guarantees a trailing exit step");
}

fn fail(scenario: &Scenario, index: usize, kind: &str, code: i32, message: &str) -> ! {
    eprintln!(
        "fake-cli: scenario \"{}\": step {index} ({kind}): {message}",
        scenario.name
    );
    process::exit(code);
}

/// Write the scripted bytes. Rust's stdout handle buffers, so the paced path
/// flushes after every byte — "one byte per write" must hold on the wire,
/// where the consumer observes it, not just at this call site.
fn emit(out: &mut impl Write, text: &str, byte_delay_ms: u64) -> io::Result<()> {
    if byte_delay_ms == 0 {
        out.write_all(text.as_bytes())?;
        return out.flush();
    }
    let delay = Duration::from_millis(byte_delay_ms);
    for (position, byte) in text.as_bytes().iter().enumerate() {
        if position > 0 {
            thread::sleep(delay);
        }
        out.write_all(&[*byte])?;
        out.flush()?;
    }
    Ok(())
}

/// Feed stdin to the executor one byte at a time over a channel, so a step
/// can wait with a deadline (`recv_timeout`) on a surface that std can only
/// read blockingly. A closed channel means stdin reached end-of-input; the
/// thread itself never outlives the process's interest in it.
fn spawn_stdin_reader() -> mpsc::Receiver<u8> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 1];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => return,
                Ok(_) => {
                    if tx.send(buf[0]).is_err() {
                        return;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    });
    rx
}

/// Wait for exactly the expected bytes. The comparison is incremental — the
/// first diverging byte fails the step immediately rather than waiting out
/// the timeout — and exact: matching consumes exactly `expected.len()` bytes,
/// so scripted input beyond the match stays queued for the next await step.
fn await_stdin(
    bytes: &mpsc::Receiver<u8>,
    expected: &str,
    timeout_ms: u64,
) -> Result<(), (i32, String)> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let expected_bytes = expected.as_bytes();
    let mut got: Vec<u8> = Vec::with_capacity(expected_bytes.len());
    while got.len() < expected_bytes.len() {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err((EXIT_TIMEOUT, timeout_message(expected, &got, timeout_ms)));
        };
        match bytes.recv_timeout(remaining) {
            Ok(byte) => {
                got.push(byte);
                if !expected_bytes.starts_with(&got) {
                    return Err((
                        EXIT_MISMATCH,
                        format!(
                            "mismatch — expected input \"{}\", got \"{}\"",
                            expected.escape_debug(),
                            String::from_utf8_lossy(&got).escape_debug(),
                        ),
                    ));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err((EXIT_TIMEOUT, timeout_message(expected, &got, timeout_ms)));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err((
                    EXIT_MISMATCH,
                    format!(
                        "stdin closed after {} of {} expected bytes (got \"{}\")",
                        got.len(),
                        expected_bytes.len(),
                        String::from_utf8_lossy(&got).escape_debug(),
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn timeout_message(expected: &str, got: &[u8], timeout_ms: u64) -> String {
    format!(
        "timed out after {timeout_ms} ms waiting for \"{}\" (received \"{}\" so far)",
        expected.escape_debug(),
        String::from_utf8_lossy(got).escape_debug(),
    )
}
