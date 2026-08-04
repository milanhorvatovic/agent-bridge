//! Step executor: runs a validated scenario against the real process
//! surfaces — stdout for scripted bytes, stdin for control input, the exit
//! status for the scripted outcome.
//!
//! Determinism discipline: no randomness, no wall-clock content. The only
//! time-dependent behavior is pacing (`byte_delay_ms`, `line_interval_us`)
//! and the `await_stdin` deadline, all driven by the monotonic clock, and
//! none of them ever changes which bytes go out — the same scenario produces
//! the same stdout bytes on every run and every OS.
//!
//! The single sanctioned exception is the `{ts}` token in an `emit`, which
//! carries a monotonic-clock reading into the stream so a reader can measure
//! how long the terminal took to deliver it. It is a token rather than
//! ambient behaviour precisely so the exception is visible in the scenario
//! file: a scenario that does not write `{ts}` is byte-identical run to run,
//! and one that does says so.
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

use crate::clock::monotonic_ns;
use crate::generator::{Rolling, checksum_line, write_payload_line};
use crate::scenario::{Channel, Scenario, Step};

/// The template token an `emit` substitutes with a monotonic-clock reading.
const TS_TOKEN: &str = "{ts}";

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
                repeat,
                repeat_interval_us,
            } => {
                if let Err(err) = emit_repeated(
                    &mut stdout,
                    text,
                    *byte_delay_ms,
                    *repeat,
                    *repeat_interval_us,
                ) {
                    fail(
                        scenario,
                        index,
                        "emit",
                        EXIT_IO,
                        &format!("writing to stdout failed: {err}"),
                    );
                }
            }
            Step::Generate {
                lines,
                line_bytes,
                checksum_every,
                line_interval_us,
                channel: Channel::Stdout,
            } => {
                if let Err(err) = generate(
                    &mut stdout,
                    *lines,
                    *line_bytes,
                    *checksum_every,
                    *line_interval_us,
                ) {
                    fail(
                        scenario,
                        index,
                        "generate",
                        EXIT_IO,
                        &format!("writing the generated stream to stdout failed: {err}"),
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

/// Write the step's text, `repeat` times, onto a schedule `interval_us`
/// apart. The schedule is absolute for the same reason the generator's is:
/// a per-write sleep would accumulate the platform's timer error until the
/// spacing meant nothing.
fn emit_repeated(
    out: &mut impl Write,
    text: &str,
    byte_delay_ms: u64,
    repeat: u64,
    interval_us: u64,
) -> io::Result<()> {
    if repeat == 1 && interval_us == 0 {
        return emit(out, text, byte_delay_ms);
    }
    let start_ns = monotonic_ns();
    let interval_ns = interval_us.saturating_mul(1_000);
    for iteration in 0..repeat {
        if interval_ns > 0 {
            sleep_until(start_ns + iteration.saturating_mul(interval_ns));
        }
        emit(out, text, byte_delay_ms)?;
    }
    Ok(())
}

/// Write the scripted bytes. Rust's stdout handle buffers, so the paced path
/// flushes after every byte — "one byte per write" must hold on the wire,
/// where the consumer observes it, not just at this call site.
fn emit(out: &mut impl Write, text: &str, byte_delay_ms: u64) -> io::Result<()> {
    let expanded = expand_timestamps(text);
    let text = expanded.as_deref().unwrap_or(text);
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

/// Replace every `{ts}` with one reading of the monotonic clock, taken as
/// the step starts writing. `None` when the text has no token — the
/// overwhelming majority of steps, which must not pay an allocation for a
/// feature they do not use.
///
/// One reading per step, not per occurrence: a step's timestamps mark when
/// the step began, and two readings inside one line would invite reading
/// their difference as something meaningful. Paced emits are the case that
/// makes this a real choice — the last byte of a byte-delayed line can leave
/// seconds after the reading it carries — and the answer stays the same,
/// because the reader measures delivery of the marker, not of the step.
fn expand_timestamps(text: &str) -> Option<String> {
    if !text.contains(TS_TOKEN) {
        return None;
    }
    Some(text.replace(TS_TOKEN, &monotonic_ns().to_string()))
}

/// Emit the generated stream: payload lines derived from their line numbers,
/// a checksum line every `checksum_every` of them, each line one write.
///
/// One write and one flush per line on purpose. That is the shape a real CLI
/// streaming tokens produces, and it is the shape whose cost the reader is
/// measuring; a buffered writer would batch the lines into large writes and
/// report a throughput number no CLI could ever deliver.
///
/// Pacing is against an absolute schedule — line `n` is due at
/// `start + n × interval` — rather than a sleep between lines. Sleep
/// granularity varies by an order of magnitude across the platforms this
/// runs on (Windows rounds up to its timer tick), and a per-line sleep would
/// turn that into a per-line error that compounds: thirty minutes of
/// scheduled traffic would take hours. Against a schedule the same
/// coarseness makes delivery burstier while the total duration stays put,
/// because a line already overdue is written immediately instead of waiting
/// again.
fn generate(
    out: &mut impl Write,
    lines: u64,
    line_bytes: usize,
    checksum_every: u64,
    line_interval_us: u64,
) -> io::Result<()> {
    let start_ns = monotonic_ns();
    let interval_ns = line_interval_us.saturating_mul(1_000);
    let mut line = String::with_capacity(line_bytes + 32);
    let mut rolling = Rolling::new();
    for seq in 0..lines {
        if interval_ns > 0 {
            sleep_until(start_ns + seq.saturating_mul(interval_ns));
        }
        write_payload_line(seq, line_bytes, &mut line);
        rolling.feed(&line);
        line.push('\n');
        out.write_all(line.as_bytes())?;
        out.flush()?;
        let covered = seq + 1;
        if checksum_every > 0 && covered % checksum_every == 0 {
            line.clear();
            line.push_str(&checksum_line(covered, rolling.value()));
            line.push('\n');
            out.write_all(line.as_bytes())?;
            out.flush()?;
        }
    }
    Ok(())
}

/// Sleep until the monotonic clock reaches `target_ns`, or return at once if
/// it already has. The already-passed case is the schedule catching up after
/// an overshooting sleep, and it is the common case at intervals near the
/// platform's timer resolution.
fn sleep_until(target_ns: u64) {
    let now = monotonic_ns();
    if target_ns > now {
        thread::sleep(Duration::from_nanos(target_ns - now));
    }
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
    fn the_ts_token_carries_a_reading_of_the_shared_clock() {
        // The reading must sit inside the window the caller can observe from
        // its own side of the same clock — that bracketing is the entire
        // contract a reader relies on when it subtracts the two.
        let before = monotonic_ns();
        let expanded = expand_timestamps("mark {ts}\n").expect("the token must expand");
        let after = monotonic_ns();
        let stamped: u64 = expanded
            .trim_end()
            .rsplit(' ')
            .next()
            .expect("the expansion leaves a field")
            .parse()
            .unwrap_or_else(|_| panic!("the expansion must be a number: {expanded}"));
        assert!(
            (before..=after).contains(&stamped),
            "{stamped} is outside the window {before}..={after} it was taken in"
        );
    }

    #[test]
    fn every_occurrence_of_the_token_gets_the_same_reading() {
        let expanded = expand_timestamps("{ts} {ts}").expect("the token must expand");
        let (first, second) = expanded.split_once(' ').expect("two fields");
        assert_eq!(
            first, second,
            "one reading per step: two readings inside one line would invite \
             reading their difference as something meaningful"
        );
    }

    #[test]
    fn a_repeated_emit_re_reads_the_clock_every_time() {
        let mut out = Vec::new();
        emit_repeated(&mut out, "{ts}\n", 0, 4, 1_000).expect("writing to memory cannot fail");
        let text = String::from_utf8(out).expect("readings are ASCII");
        let readings: Vec<u64> = text
            .lines()
            .map(|line| line.parse().expect("each line is one reading"))
            .collect();
        assert_eq!(readings.len(), 4);
        assert!(
            readings.windows(2).all(|pair| pair[1] > pair[0]),
            "a repeated marker must carry a fresh reading each time: {readings:?}"
        );
    }

    #[test]
    fn text_without_the_token_is_left_alone() {
        assert_eq!(expand_timestamps("Writing file...\n"), None);
    }

    #[test]
    fn the_generated_stream_is_byte_identical_across_runs() {
        let run = || {
            let mut out = Vec::new();
            generate(&mut out, 200, 32, 25, 0).expect("generating into memory cannot fail");
            out
        };
        assert_eq!(
            run(),
            run(),
            "the generated stream is derived from line numbers, so it cannot vary"
        );
    }

    #[test]
    fn checksum_lines_cover_every_payload_line_before_them() {
        use crate::generator::{Line, parse_line};

        let mut out = Vec::new();
        generate(&mut out, 100, 16, 25, 0).expect("generating into memory cannot fail");
        let text = String::from_utf8(out).expect("the generated stream is ASCII");

        let mut rolling = Rolling::new();
        let mut payloads = 0u64;
        let mut checksums = 0;
        for line in text.lines() {
            match parse_line(line) {
                Some(Line::Payload { seq, .. }) => {
                    assert_eq!(seq, payloads, "payload lines must be numbered in order");
                    rolling.feed(line);
                    payloads += 1;
                }
                Some(Line::Checksum { covered, digest }) => {
                    assert_eq!(covered, payloads, "a checksum names how much it covers");
                    assert_eq!(
                        digest,
                        rolling.value(),
                        "the digest must match the payload lines before it"
                    );
                    checksums += 1;
                }
                None => panic!("unrecognized generated line: {line}"),
            }
        }
        assert_eq!(payloads, 100);
        assert_eq!(checksums, 4, "one checksum line per 25 payload lines");
    }

    #[test]
    fn paced_generation_keeps_to_its_schedule_rather_than_its_sleeps() {
        // 40 lines at 5 ms apart is 195 ms of schedule. The lower bound is
        // what a probe's duration assertion rests on; the upper bound is the
        // property a per-line sleep would lose — on a platform whose timer
        // ticks coarser than the interval, every sleep overshoots and only
        // an absolute schedule stops the error compounding.
        let started = Instant::now();
        let mut out = Vec::new();
        generate(&mut out, 40, 8, 0, 5_000).expect("generating into memory cannot fail");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(195),
            "pacing must not run ahead of its schedule (took {elapsed:?})"
        );
        assert!(
            elapsed < Duration::from_millis(600),
            "pacing must not compound its sleep error (took {elapsed:?})"
        );
    }

    #[test]
    fn expected_text_is_normalized_too() {
        // A scenario authored with CRLF terminators means the same lines.
        let mut feed = feed(b"a\nb\n");
        await_stdin(&mut feed, "a\r\nb\r\n", 1_000).expect("authored CRLF must match \\n input");
    }
}
