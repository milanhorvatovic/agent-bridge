//! PTY allocation probe — allocates a real PTY (ConPTY on Windows), spawns an
//! echo child under it with the runtime's child-env defaults, reads the
//! output back through the master, and tears down with the child reaped and
//! the handle closed. Each step prints a machine-readable result line; the
//! first failing step exits with that step's code, so CI asserts the exit
//! status and a human reads the step log.
//!
//! The probe is deliberately synchronous: blocking reads on a dedicated
//! thread are all a spawn-read-exit lifecycle needs. Known ConPTY rough
//! edges (documented by production users of the same PTY library, e.g.
//! can1357/oh-my-pi) are each guarded so a hang becomes a diagnosed failure,
//! not a stuck CI lane: allocation and master-close run on timeout-guarded
//! helper threads, the reader answers the cursor-position query ConPTY emits
//! at startup, and child exit is polled rather than blocking-waited. Whether
//! any of those hazards was actually observed is visible in the step details.
//!
//! Usage: pty-probe [--check-env] [--timeout-secs N]
//!
//! `--check-env` spawns an environment-dumping child instead of the echo
//! child and asserts every child-env default arrived intact.

// This crate legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

mod utf8;

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};

const COLS: u16 = 80;
const ROWS: u16 = 24;
const DEFAULT_TIMEOUT_SECS: u64 = 10;

#[derive(Clone, Copy)]
enum Mode {
    Echo,
    CheckEnv,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Echo => "echo",
            Mode::CheckEnv => "check-env",
        }
    }
}

#[derive(Clone, Copy)]
enum Step {
    Alloc,
    Spawn,
    Read,
    ChildExit,
    Teardown,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Alloc => "alloc",
            Step::Spawn => "spawn",
            Step::Read => "read",
            Step::ChildExit => "child_exit",
            Step::Teardown => "teardown",
        }
    }

    fn exit_code(self) -> i32 {
        match self {
            Step::Alloc => 10,
            Step::Spawn => 11,
            Step::Read => 12,
            Step::ChildExit => 13,
            Step::Teardown => 14,
        }
    }
}

struct Failure {
    step: Step,
    detail: String,
}

fn main() {
    let (mode, timeout) = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("pty-probe: {message}");
            std::process::exit(2);
        }
    };
    println!("pty-probe {}", platform_report());
    match run(mode, timeout) {
        Ok(()) => println!("pty-probe mode={} result=pass", mode.name()),
        Err(failure) => {
            print_step(failure.step, "fail", &failure.detail);
            eprintln!(
                "pty-probe: step {} failed: {}",
                failure.step.name(),
                failure.detail
            );
            std::process::exit(failure.step.exit_code());
        }
    }
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<(Mode, Duration), String> {
    let mut mode = Mode::Echo;
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--check-env" => mode = Mode::CheckEnv,
            "--timeout-secs" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--timeout-secs needs a value".to_string())?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid --timeout-secs value: {value}"))?;
                timeout = Duration::from_secs(secs);
            }
            other => {
                return Err(format!(
                    "unknown argument: {other}. usage: pty-probe [--check-env] [--timeout-secs N]"
                ));
            }
        }
    }
    Ok((mode, timeout))
}

fn platform_report() -> String {
    format!(
        "os={} arch={} family={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
    )
}

fn print_step(step: Step, status: &str, detail: &str) {
    // Keep every step line single-line and parseable: the detail field is
    // quoted, so newlines and double quotes inside it are normalized away.
    let clean = detail.replace(['\r', '\n'], " ").replace('"', "'");
    println!(
        "pty-probe step={} status={status} detail=\"{clean}\"",
        step.name()
    );
}

/// The probe steps in lifecycle order. Every step is a function returning
/// `Result` so sibling probes can copy the skeleton and swap the middle.
fn run(mode: Mode, timeout: Duration) -> Result<(), Failure> {
    let fail = |step: Step| move |detail: String| Failure { step, detail };

    let (pair, alloc_ms) = alloc_pty(timeout).map_err(fail(Step::Alloc))?;
    print_step(
        Step::Alloc,
        "pass",
        &format!("pty allocated at {COLS}x{ROWS} in {alloc_ms}ms"),
    );
    let PtyPair { master, slave } = pair;

    let argv = child_argv(mode);
    let mut command = CommandBuilder::new(argv[0]);
    command.args(&argv[1..]);
    for (key, value) in child_env(COLS, ROWS) {
        command.env(key, value);
    }
    let mut child = slave
        .spawn_command(command)
        .map_err(|err| fail(Step::Spawn)(format!("child spawn failed: {err:#}")))?;
    // Release our copy of the child end: holding it open would keep the
    // master from ever seeing end-of-stream after the child exits.
    drop(slave);
    print_step(
        Step::Spawn,
        "pass",
        &format!(
            "spawned `{}` pid={}",
            display_argv(&argv),
            child
                .process_id()
                .map_or_else(|| "unknown".to_string(), |pid| pid.to_string()),
        ),
    );

    let reader = master
        .try_clone_reader()
        .map_err(|err| fail(Step::Read)(format!("cloning the master reader failed: {err:#}")))?;
    let writer = master
        .take_writer()
        .map_err(|err| fail(Step::Read)(format!("taking the master writer failed: {err:#}")))?;
    let events = spawn_reader(reader, writer);
    let (read_detail, end) = read_expected(&events, mode, timeout).map_err(fail(Step::Read))?;
    print_step(Step::Read, "pass", &read_detail);

    let exit_detail = wait_child(child.as_mut(), timeout).map_err(fail(Step::ChildExit))?;
    print_step(Step::ChildExit, "pass", &exit_detail);

    let teardown_detail = teardown(master, &events, end, timeout).map_err(fail(Step::Teardown))?;
    print_step(Step::Teardown, "pass", &teardown_detail);
    Ok(())
}

/// Allocate the PTY on a helper thread: `openpty` can hang on Windows when
/// the console subsystem is not yet initialised, and a hang must surface as
/// a failed step, not a stuck probe. On timeout the helper thread is
/// deliberately leaked — the process is about to exit with a diagnostic.
fn alloc_pty(timeout: Duration) -> Result<(PtyPair, u128), String> {
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let _ = tx.send(native_pty_system().openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        }));
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(pair)) => Ok((pair, started.elapsed().as_millis())),
        Ok(Err(err)) => Err(format!("pty allocation failed: {err:#}")),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "pty allocation did not complete within {}s{}",
            timeout.as_secs(),
            if cfg!(windows) {
                " — matches the known ConPTY hang when the console subsystem is uninitialised"
            } else {
                ""
            }
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err("pty allocation thread died without a result".to_string())
        }
    }
}

/// The child command per mode and platform. The single `cfg!(windows)`
/// divergence in the probe: everything else is the PTY library's job —
/// that being true is precisely what the probe tests.
fn child_argv(mode: Mode) -> Vec<&'static str> {
    match (mode, cfg!(windows)) {
        (Mode::Echo, false) => vec!["bash", "-lc", "echo hi"],
        (Mode::Echo, true) => vec!["cmd.exe", "/c", "echo hi"],
        // A full environment dump, one KEY=VALUE per line: single short lines
        // survive the 80-column terminal intact, where one long echoed line
        // would wrap mid-value.
        (Mode::CheckEnv, false) => vec!["env"],
        (Mode::CheckEnv, true) => vec!["cmd.exe", "/c", "set"],
    }
}

/// Render an argv for the step log the way a shell user would type it, so
/// `bash -lc 'echo hi'` does not flatten into `bash -lc echo hi`.
fn display_argv(argv: &[&str]) -> String {
    argv.iter()
        .map(|arg| {
            if arg.contains(' ') {
                format!("'{arg}'")
            } else {
                (*arg).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The env defaults every child spawned under a PTY receives. These are
/// load-bearing for CLI behavior: without them, many CLIs degrade — "dumb"
/// terminal mode, disabled color, or broken UTF-8.
fn child_env(cols: u16, rows: u16) -> Vec<(&'static str, String)> {
    // macOS ships no C.UTF-8 locale; en_US.UTF-8 is its UTF-8-forcing
    // equivalent.
    let locale = if cfg!(target_os = "macos") {
        "en_US.UTF-8"
    } else {
        "C.UTF-8"
    };
    vec![
        ("TERM", "xterm-256color".to_string()),
        ("LC_ALL", locale.to_string()),
        ("LANG", locale.to_string()),
        ("COLUMNS", cols.to_string()),
        ("LINES", rows.to_string()),
        ("COLORTERM", "truecolor".to_string()),
    ]
}

enum ReaderEvent {
    Data(Vec<u8>),
    End(EndInfo),
}

#[derive(Debug)]
struct EndInfo {
    reason: String,
    cursor_queries_answered: u32,
    /// First failure writing the cursor-position reply, if any. A failed
    /// reply usually shows up later as a blocked child, so the root cause
    /// must survive into the diagnostics.
    cursor_reply_error: Option<String>,
}

/// Read the master on a dedicated thread, forwarding chunks over a channel.
/// The thread also answers ConPTY's `ESC[6n` cursor-position query — ConPTY
/// emits it at startup and blocks the child until a reply arrives — and it
/// keeps draining until end-of-stream so teardown never closes a master
/// whose buffered output has no reader.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    mut writer: Box<dyn Write + Send>,
) -> mpsc::Receiver<ReaderEvent> {
    const CURSOR_QUERY: &[u8] = b"\x1b[6n";
    const CURSOR_REPLY: &[u8] = b"\x1b[1;1R";
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut answered: u32 = 0;
        let mut reply_error: Option<String> = None;
        let mut scan_tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        let reason = loop {
            match reader.read(&mut buf) {
                Ok(0) => break "eof".to_string(),
                Ok(n) => {
                    let chunk = &buf[..n];
                    // Scan across chunk boundaries via the carried tail — the
                    // query is 4 bytes and can arrive split.
                    let mut scan = std::mem::take(&mut scan_tail);
                    scan.extend_from_slice(chunk);
                    for window in scan.windows(CURSOR_QUERY.len()) {
                        if window == CURSOR_QUERY {
                            // Count only replies that were actually delivered;
                            // a swallowed write failure plus an inflated count
                            // would point a hang diagnosis the wrong way.
                            match writer.write_all(CURSOR_REPLY).and_then(|()| writer.flush()) {
                                Ok(()) => answered += 1,
                                Err(err) => {
                                    reply_error.get_or_insert_with(|| err.to_string());
                                }
                            }
                        }
                    }
                    scan_tail = scan[scan.len().saturating_sub(CURSOR_QUERY.len() - 1)..].to_vec();
                    if tx.send(ReaderEvent::Data(chunk.to_vec())).is_err() {
                        return; // the probe gave up on this stream
                    }
                }
                // A signal can cut a blocking read short; that is not the end
                // of anything, so resume where it left off.
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                // A master read on a closed PTY surfaces as an error on some
                // platforms (EIO on Linux) rather than a 0-byte read; both
                // mean the stream ended.
                Err(err) => break format!("read error: {err}"),
            }
        };
        let _ = tx.send(ReaderEvent::End(EndInfo {
            reason,
            cursor_queries_answered: answered,
            cursor_reply_error: reply_error,
        }));
    });
    rx
}

/// Collect output until the mode's expectation is met, failing — never
/// hanging — on timeout, on end-of-stream before the expected output
/// arrives (a child that exited without producing it), and on invalid or
/// truncated UTF-8.
fn read_expected(
    events: &mpsc::Receiver<ReaderEvent>,
    mode: Mode,
    timeout: Duration,
) -> Result<(String, Option<EndInfo>), String> {
    let deadline = Instant::now() + timeout;
    let mut reassembler = utf8::Reassembler::new();
    let mut end: Option<EndInfo> = None;
    loop {
        if expectation_met(mode, reassembler.decoded()) {
            let detail = match mode {
                Mode::Echo => format!(
                    "read expected output back through the master ({} bytes decoded)",
                    reassembler.decoded().len()
                ),
                Mode::CheckEnv => format!(
                    "child environment carries all {} defaults",
                    child_env(COLS, ROWS).len()
                ),
            };
            return Ok((detail, end));
        }
        if let Some(info) = &end {
            return Err(format!(
                "stream ended ({}{}) before the expected output; {}{}",
                info.reason,
                info.cursor_reply_error
                    .as_ref()
                    .map_or_else(String::new, |err| format!(
                        "; cursor-reply write failed: {err}"
                    )),
                missing_summary(mode, reassembler.decoded()),
                excerpt_note(mode, reassembler.decoded()),
            ));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(format!(
                "expected output not observed within {}s; {}{}",
                timeout.as_secs(),
                missing_summary(mode, reassembler.decoded()),
                excerpt_note(mode, reassembler.decoded()),
            ));
        }
        match events.recv_timeout(deadline - now) {
            Ok(ReaderEvent::Data(chunk)) => {
                reassembler.push(&chunk).map_err(|_| {
                    "output contained bytes that can never be valid UTF-8".to_string()
                })?;
            }
            Ok(ReaderEvent::End(info)) => {
                if reassembler.pending() != 0 {
                    return Err(format!(
                        "stream ended mid-codepoint with {} undecodable trailing bytes",
                        reassembler.pending()
                    ));
                }
                end = Some(info);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {} // deadline re-checked at loop top
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("reader thread ended without reporting end-of-stream".to_string());
            }
        }
    }
}

fn expectation_met(mode: Mode, text: &str) -> bool {
    match mode {
        // A whole line, not a substring: incidental output like "this"
        // contains "hi" and must not satisfy the probe.
        Mode::Echo => strip_ansi(text).lines().any(|line| line.trim() == "hi"),
        Mode::CheckEnv => missing_env_lines(text).is_empty(),
    }
}

fn missing_summary(mode: Mode, text: &str) -> String {
    match mode {
        Mode::Echo => "expected a line reading 'hi'".to_string(),
        Mode::CheckEnv => format!("missing env lines: {}", missing_env_lines(text).join(", ")),
    }
}

/// The child-env defaults not yet visible in the child's environment dump.
/// Whole trimmed lines are compared so `COLUMNS=800` can never satisfy
/// `COLUMNS=80`.
fn missing_env_lines(text: &str) -> Vec<String> {
    let stripped = strip_ansi(text);
    let lines: std::collections::HashSet<&str> = stripped.lines().map(str::trim).collect();
    child_env(COLS, ROWS)
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .filter(|line| !lines.contains(line.as_str()))
        .collect()
}

/// Strip ANSI escape sequences so text assertions see what a human would
/// read — ConPTY brackets even trivial output with cursor, color, and title
/// controls. CSI sequences end at a final byte in `0x40..=0x7E`; OSC
/// sequences end at BEL or ESC-backslash; any other ESC pair is dropped
/// whole.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' || (c == '\x1b' && chars.next() == Some('\\')) {
                        break;
                    }
                }
            }
            _ => {} // two-character escape — both consumed
        }
    }
    out
}

/// A short sample of decoded output for failure diagnostics — but never in
/// check-env mode, where the decoded text is the child's entire environment
/// dump and the missing-lines summary already carries the signal.
fn excerpt_note(mode: Mode, text: &str) -> String {
    match mode {
        Mode::Echo => format!(
            "; decoded so far: '{}'",
            strip_ansi(text).chars().take(200).collect::<String>()
        ),
        Mode::CheckEnv => String::new(),
    }
}

/// Reap the child by polling `try_wait` against a deadline: a blocking
/// `wait()` is a known ConPTY hang, so the probe never calls it. On timeout
/// the child is killed so a failed step does not leave a live child behind.
/// The child stays on the calling thread, so no thread-safety bounds are
/// asked of it.
fn wait_child(child: &mut dyn Child, timeout: Duration) -> Result<String, String> {
    let started = Instant::now();
    let mut polls: u32 = 0;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return Ok(format!(
                    "child exited cleanly in {}ms (try_wait polls: {polls})",
                    started.elapsed().as_millis()
                ));
            }
            Ok(Some(status)) => {
                return Err(format!("child exited with code {}", status.exit_code()));
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    return Err(format!(
                        "child still running after {}s of try_wait polling; killed",
                        timeout.as_secs()
                    ));
                }
                polls += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(format!("child wait failed: {err}")),
        }
    }
}

/// Close the master and prove the reader observes end-of-stream instead of
/// hanging. The close runs on a helper thread because `ClosePseudoConsole`
/// can deadlock when buffered output has no reader draining it
/// (microsoft/terminal#1810); on timeout the thread is deliberately leaked —
/// the process is about to exit with a diagnostic.
fn teardown(
    master: Box<dyn MasterPty + Send>,
    events: &mpsc::Receiver<ReaderEvent>,
    mut end: Option<EndInfo>,
    timeout: Duration,
) -> Result<String, String> {
    let started = Instant::now();
    let (tx, closed) = mpsc::channel();
    std::thread::spawn(move || {
        drop(master);
        let _ = tx.send(());
    });
    if closed.recv_timeout(timeout).is_err() {
        return Err(format!(
            "closing the pty master did not complete within {}s{}",
            timeout.as_secs(),
            if cfg!(windows) {
                " — matches the known ClosePseudoConsole deadlock (microsoft/terminal#1810)"
            } else {
                ""
            }
        ));
    }
    let close_ms = started.elapsed().as_millis();

    let deadline = Instant::now() + timeout;
    let info = loop {
        if let Some(info) = end.take() {
            break info;
        }
        match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(ReaderEvent::Data(_)) => {} // draining output that arrived after the read step
            Ok(ReaderEvent::End(info)) => end = Some(info),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(format!(
                    "the reader did not reach end-of-stream within {}s of closing the master",
                    timeout.as_secs()
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("reader thread ended without reporting end-of-stream".to_string());
            }
        }
    };
    let mut detail = format!(
        "master closed in {close_ms}ms; reader end: {}; cursor-position queries answered: {}",
        info.reason, info.cursor_queries_answered
    );
    if let Some(err) = &info.cursor_reply_error {
        detail.push_str(&format!("; cursor-reply write failed: {err}"));
    }
    Ok(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[test]
    fn platform_report_names_this_os() {
        let report = platform_report();
        assert!(
            report.contains("linux") || report.contains("macos") || report.contains("windows"),
            "unexpected platform report: {report}"
        );
    }

    #[test]
    fn args_select_mode_and_timeout() {
        let args = ["--check-env", "--timeout-secs", "3"].map(String::from);
        let (mode, timeout) = parse_args(args.into_iter()).unwrap();
        assert!(matches!(mode, Mode::CheckEnv));
        assert_eq!(timeout, Duration::from_secs(3));
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
    }

    #[test]
    fn child_env_reflects_dims_and_forces_utf8() {
        let env = child_env(120, 40);
        let get = |key: &str| {
            env.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("missing env default: {key}"))
        };
        assert_eq!(get("TERM"), "xterm-256color");
        assert_eq!(get("COLUMNS"), "120");
        assert_eq!(get("LINES"), "40");
        assert_eq!(get("COLORTERM"), "truecolor");
        assert!(get("LC_ALL").ends_with("UTF-8"), "LC_ALL must force UTF-8");
        assert_eq!(get("LANG"), get("LC_ALL"));
    }

    #[test]
    fn spawning_a_missing_binary_is_a_typed_error_not_a_hang() {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: ROWS,
                cols: COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .expect("pty allocation must succeed");
        let result = pair
            .slave
            .spawn_command(CommandBuilder::new("agent-bridge-no-such-binary"));
        assert!(
            result.is_err(),
            "a missing binary must surface a spawn error"
        );
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc_sequences() {
        let decorated = "\x1b[2J\x1b[1;1H\x1b]0;title\x07hi\x1b[0m";
        assert_eq!(strip_ansi(decorated), "hi");
    }

    #[test]
    fn strip_ansi_keeps_plain_text() {
        assert_eq!(
            strip_ansi("TERM=xterm-256color\r\n"),
            "TERM=xterm-256color\r\n"
        );
    }

    #[test]
    fn env_lines_match_whole_lines_not_prefixes() {
        let mut report = String::from("SHELL=/bin/bash\r\n");
        for (key, value) in child_env(COLS, ROWS) {
            report.push_str(&format!("{key}={value}\r\n"));
        }
        assert_eq!(missing_env_lines(&report), Vec::<String>::new());
        assert!(
            missing_env_lines("COLUMNS=800\r\nLINES=240\r\n").contains(&"COLUMNS=80".to_string()),
            "a prefix collision must not satisfy an env default"
        );
    }

    /// Feeds fixed chunks, then end-of-stream — the shape a PTY reader sees.
    struct ChunkedReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.chunks.pop_front() {
                Some(mut chunk) => {
                    // A Read impl must not assume the caller's buffer fits a
                    // whole chunk: hand back a partial read and keep the rest.
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    if n < chunk.len() {
                        chunk.drain(..n);
                        self.chunks.push_front(chunk);
                    }
                    Ok(n)
                }
                None => Ok(0),
            }
        }
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn cursor_position_query_is_answered_even_when_split_across_reads() {
        let reader = ChunkedReader {
            chunks: VecDeque::from(vec![b"boot \x1b[".to_vec(), b"6n rest".to_vec()]),
        };
        let written = Arc::new(Mutex::new(Vec::new()));
        let events = spawn_reader(Box::new(reader), Box::new(SharedWriter(written.clone())));
        let mut data = Vec::new();
        let info = loop {
            match events
                .recv_timeout(Duration::from_secs(5))
                .expect("reader must reach end-of-stream")
            {
                ReaderEvent::Data(chunk) => data.extend(chunk),
                ReaderEvent::End(info) => break info,
            }
        };
        assert_eq!(info.cursor_queries_answered, 1);
        assert_eq!(written.lock().unwrap().as_slice(), b"\x1b[1;1R");
        assert_eq!(data, b"boot \x1b[6n rest");
        assert_eq!(info.reason, "eof");
        assert_eq!(info.cursor_reply_error, None);
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "input pipe closed",
            ))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn failed_cursor_reply_is_recorded_not_counted() {
        let reader = ChunkedReader {
            chunks: VecDeque::from(vec![b"\x1b[6n".to_vec()]),
        };
        let events = spawn_reader(Box::new(reader), Box::new(FailingWriter));
        let info = loop {
            match events
                .recv_timeout(Duration::from_secs(5))
                .expect("reader must reach end-of-stream")
            {
                ReaderEvent::Data(_) => {}
                ReaderEvent::End(info) => break info,
            }
        };
        assert_eq!(
            info.cursor_queries_answered, 0,
            "an undelivered reply must not count as answered"
        );
        assert!(
            info.cursor_reply_error
                .as_deref()
                .is_some_and(|e| e.contains("input pipe closed")),
            "the write failure must be recorded: {:?}",
            info.cursor_reply_error
        );
    }

    #[test]
    fn expected_output_split_across_chunks_is_found() {
        let (tx, events) = mpsc::channel();
        tx.send(ReaderEvent::Data(b"h".to_vec())).unwrap();
        tx.send(ReaderEvent::Data(b"i".to_vec())).unwrap();
        assert!(read_expected(&events, Mode::Echo, Duration::from_secs(5)).is_ok());
    }

    #[test]
    fn echo_expectation_needs_a_whole_line_not_a_substring() {
        assert!(!expectation_met(Mode::Echo, "this\r\n"));
        assert!(!expectation_met(Mode::Echo, "high\r\n"));
        assert!(expectation_met(Mode::Echo, "\x1b[2Jhi\x1b[0m\r\n"));
        assert!(expectation_met(Mode::Echo, "profile noise\r\nhi\r\n"));
    }

    #[test]
    fn stream_end_before_expected_output_fails_not_hangs() {
        let (tx, events) = mpsc::channel();
        tx.send(ReaderEvent::End(EndInfo {
            reason: "eof".to_string(),
            cursor_queries_answered: 0,
            cursor_reply_error: None,
        }))
        .unwrap();
        let err = read_expected(&events, Mode::Echo, Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("ended"), "unexpected error: {err}");
        assert!(
            err.contains("(eof)"),
            "the reader's end reason must survive into the failure: {err}"
        );
    }

    #[test]
    fn check_env_failure_diagnostics_do_not_dump_the_environment() {
        let (tx, events) = mpsc::channel();
        tx.send(ReaderEvent::Data(b"SECRET_TOKEN=hunter2\r\n".to_vec()))
            .unwrap();
        tx.send(ReaderEvent::End(EndInfo {
            reason: "eof".to_string(),
            cursor_queries_answered: 0,
            cursor_reply_error: None,
        }))
        .unwrap();
        let err = read_expected(&events, Mode::CheckEnv, Duration::from_secs(5)).unwrap_err();
        assert!(
            !err.contains("hunter2"),
            "diagnostic leaked the child environment: {err}"
        );
        assert!(
            err.contains("TERM="),
            "diagnostic should still name the missing defaults: {err}"
        );
    }

    #[test]
    fn argv_display_keeps_shell_style_quoting() {
        assert_eq!(
            display_argv(&["bash", "-lc", "echo hi"]),
            "bash -lc 'echo hi'"
        );
    }

    #[test]
    fn silent_stream_times_out_instead_of_hanging() {
        let (tx, events) = mpsc::channel::<ReaderEvent>();
        let _keep_stream_open = tx;
        let err = read_expected(&events, Mode::Echo, Duration::from_millis(20)).unwrap_err();
        assert!(err.contains("within"), "unexpected error: {err}");
    }
}
