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

//! Input discipline: `await_stdin` matches line terminators as an
//! equivalence class — `\n`, `\r`, and `\r\n` are one logical newline on
//! both sides of the comparison. What Enter delivers to this process is the
//! hosting platform's choice, not the scenario's: a POSIX PTY in cooked
//! mode rewrites the terminal's CR to NL before stdin sees it, while ConPTY
//! forwards the CR as-is. Byte-exact terminators would therefore make every
//! stdin-awaiting scenario silently POSIX-only the moment it runs under a
//! real terminal surface. Every non-terminator byte stays exact.

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
    let mut stdin_feed: Option<StdinFeed> = None;
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
                let feed = stdin_feed.get_or_insert_with(StdinFeed::start);
                if let Err((code, message)) = await_stdin(feed, expected, *timeout_ms) {
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

/// Stdin as the executor consumes it: the raw byte channel plus the one bit
/// of line-terminator state that must outlive a single await step — whether
/// the last logical newline arrived as a CR whose paired LF may still be in
/// flight. A CRLF can straddle two await steps; without carried state the
/// stray LF would open the next await as a mismatch.
struct StdinFeed {
    bytes: mpsc::Receiver<u8>,
    swallow_lf: bool,
}

impl StdinFeed {
    fn start() -> Self {
        Self {
            bytes: spawn_stdin_reader(),
            swallow_lf: false,
        }
    }
}

/// One logical newline, whatever the platform delivered. `\r\n` and a bare
/// `\r` both become `\n`; everything else is untouched.
fn normalize_terminators(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// Wait for exactly the expected text, with line terminators matched as an
/// equivalence class (see the module note). The comparison is incremental —
/// the first diverging logical byte fails the step immediately rather than
/// waiting out the timeout — and exact in logical terms: matching consumes
/// exactly the expected logical bytes (plus the LF half of a CRLF), so
/// scripted input beyond the match stays queued for the next await step.
fn await_stdin(feed: &mut StdinFeed, expected: &str, timeout_ms: u64) -> Result<(), (i32, String)> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let expected_norm = normalize_terminators(expected);
    let expected_bytes = expected_norm.as_bytes();
    // Raw bytes for diagnostics, logical bytes for matching: a mismatch
    // message that showed the normalized form would hide exactly the
    // terminator detail someone debugging a platform difference needs.
    let mut got_raw: Vec<u8> = Vec::with_capacity(expected_bytes.len());
    let mut logical: Vec<u8> = Vec::with_capacity(expected_bytes.len());
    while logical.len() < expected_bytes.len() {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err((
                EXIT_TIMEOUT,
                timeout_message(expected, &got_raw, timeout_ms),
            ));
        };
        match feed.bytes.recv_timeout(remaining) {
            Ok(byte) => {
                got_raw.push(byte);
                if feed.swallow_lf && byte == b'\n' {
                    feed.swallow_lf = false;
                    continue;
                }
                feed.swallow_lf = byte == b'\r';
                logical.push(if byte == b'\r' { b'\n' } else { byte });
                if !expected_bytes.starts_with(&logical) {
                    return Err((
                        EXIT_MISMATCH,
                        format!(
                            "mismatch — expected input \"{}\", got \"{}\"",
                            expected.escape_debug(),
                            String::from_utf8_lossy(&got_raw).escape_debug(),
                        ),
                    ));
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err((
                    EXIT_TIMEOUT,
                    timeout_message(expected, &got_raw, timeout_ms),
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err((
                    EXIT_MISMATCH,
                    format!(
                        "stdin closed after {} of {} expected bytes (got \"{}\")",
                        logical.len(),
                        expected_bytes.len(),
                        String::from_utf8_lossy(&got_raw).escape_debug(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(input: &[u8]) -> StdinFeed {
        let (tx, rx) = mpsc::channel();
        for byte in input {
            tx.send(*byte).unwrap();
        }
        // Dropping the sender closes the channel: end-of-input, like a
        // closed stdin.
        StdinFeed {
            bytes: rx,
            swallow_lf: false,
        }
    }

    #[test]
    fn every_terminator_form_satisfies_an_lf_expectation() {
        // POSIX cooked PTYs deliver Enter as \n, ConPTY as \r, and a pipe
        // driver writes whatever the script says — all three are the same
        // logical line.
        for input in [b"y\n".as_slice(), b"y\r", b"y\r\n"] {
            let mut feed = feed(input);
            await_stdin(&mut feed, "y\n", 1_000)
                .unwrap_or_else(|(_, msg)| panic!("{input:?} must match: {msg}"));
        }
    }

    #[test]
    fn a_crlf_straddling_two_awaits_does_not_poison_the_second() {
        // The LF half of a CRLF arrives after the first await already
        // matched on the CR; without carried state it would open the next
        // await as a mismatch.
        let mut feed = feed(b"y\r\nquit\r");
        await_stdin(&mut feed, "y\n", 1_000).expect("first line must match");
        await_stdin(&mut feed, "quit\n", 1_000).expect("second line must match");
    }

    #[test]
    fn non_terminator_bytes_stay_exact() {
        let mut feed = feed(b"x");
        let (code, message) =
            await_stdin(&mut feed, "y\n", 1_000).expect_err("diverging input must fail");
        assert_eq!(code, EXIT_MISMATCH);
        assert!(
            message.contains("got \"x\""),
            "raw byte in message: {message}"
        );
    }

    #[test]
    fn mismatch_messages_show_the_raw_bytes_not_the_normalized_form() {
        // Someone debugging a platform difference needs to see the CR that
        // actually arrived.
        let mut feed = feed(b"y\rz");
        await_stdin(&mut feed, "y\n", 1_000).expect("the CR line matches");
        let (_, message) =
            await_stdin(&mut feed, "quit\n", 1_000).expect_err("z diverges from quit");
        assert!(
            message.contains("got \"z\""),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn expected_text_is_normalized_too() {
        // A scenario authored with CRLF terminators means the same lines.
        let mut feed = feed(b"a\nb\n");
        await_stdin(&mut feed, "a\r\nb\r\n", 1_000).expect("authored CRLF must match \\n input");
    }
}
