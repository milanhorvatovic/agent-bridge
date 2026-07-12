//! UTF-8 integrity probe — proves, against the utf8-child fixture under a
//! real PTY (ConPTY on Windows), that multi-byte UTF-8 crosses the
//! terminal byte-exact even when nothing about the transport cooperates:
//! the fixture places write boundaries mid-codepoint on its side, and the
//! probe forces its reads down to a single byte so a chunk boundary
//! actually lands at every offset of the stream.
//!
//! Two scenarios:
//!
//! - **sweep**: spawns the fixture once per read-buffer size (1, 2, 3, 7,
//!   64 bytes, then the default) and holds the reassembled corpus — ZWJ
//!   emoji, flag pairs, CJK, combining diacritics — to the fixture's
//!   trailer: every item byte-exact, totals matching, FNV-1a 64 checksum
//!   matching, zero invalid spans. A terminal that transcodes, normalizes,
//!   or drops so much as one byte fails here.
//! - **invalid**: the fixture embeds bytes that can never be UTF-8 between
//!   valid neighbors. On POSIX a PTY is a transparent byte pipe, so the
//!   junk must arrive verbatim and the decode layer must report it as
//!   exactly-located `InvalidSpan`s while both neighbors survive
//!   byte-exact. On Windows, what ConPTY does with the junk is recorded as
//!   a typed outcome — passed through verbatim, or substituted with
//!   U+FFFD — and the one prohibited result is the junk vanishing without
//!   a trace.
//!
//! The decode layer under test is [`carry::Utf8Carry`], carried here with
//! its exhaustive split-position unit suite as the seed the production
//! stream reader will adopt.
//!
//! Same step contract as the sibling probes — one machine-readable
//! `step=… status=… detail="…"` line per step, exit non-zero with a
//! step-identifying code on the first failure — so CI asserts the exit
//! status while a human reads the log.

// This crate legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

mod carry;

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_bridge_interactive_probe::platform_report;
use agent_bridge_interactive_probe::pty::{
    DEFAULT_READ_BUFFER_BYTES, EndInfo, ReaderEvent, SharedWriter, alloc_pty,
    spawn_reader_with_buffer, strip_ansi, teardown, wait_child,
};
use agent_bridge_probe_child::corpus::{
    CorpusSummary, EVENT_UTF8_END, JUNK_BYTES, UTF8_CORPUS, UTF8_MODE_INVALID, UTF8_MODE_VALID,
    corpus_summary, junk_decoded, junk_seq, parse_corpus_line,
};
use agent_bridge_probe_child::{EVENT_READY, Report, reports_in};
use carry::{InvalidSpan, Utf8Carry};
use portable_pty::{CommandBuilder, PtyPair};

/// Deliberately wide: ConPTY reflows output to the PTY width, and a corpus
/// line hard-wrapped mid-payload would never reassemble. The longest line
/// the fixture emits is well under half of this.
const COLS: u16 = 200;
const ROWS: u16 = 50;

const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// The read-buffer sizes the sweep forces. One byte makes every offset a
/// chunk boundary; 2, 3, and 7 keep boundaries sliding across sequence
/// starts (7 is coprime to every UTF-8 sequence length); 64 is a small
/// realistic buffer; the default is what every other probe reads with.
const SWEEP_BUFFERS: &[usize] = &[1, 2, 3, 7, 64, DEFAULT_READ_BUFFER_BYTES];

/// The invalid lane needs the junk split across reads once (one byte) and
/// delivered whole once (default); the full sweep adds nothing — span
/// reports are chunking-independent, which the unit suite already pins.
const INVALID_BUFFERS: &[usize] = &[1, DEFAULT_READ_BUFFER_BYTES];

#[derive(Clone, Copy)]
enum Scenario {
    Sweep,
    Invalid,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::Sweep => "sweep",
            Scenario::Invalid => "invalid",
        }
    }

    /// The mode argument the fixture is spawned with.
    fn fixture_mode(self) -> &'static str {
        match self {
            Scenario::Sweep => UTF8_MODE_VALID,
            Scenario::Invalid => UTF8_MODE_INVALID,
        }
    }

    fn include_junk(self) -> bool {
        matches!(self, Scenario::Invalid)
    }

    fn buffers(self) -> &'static [usize] {
        match self {
            Scenario::Sweep => SWEEP_BUFFERS,
            Scenario::Invalid => INVALID_BUFFERS,
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
    let (scenario, timeout) = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("utf8-probe: {message}");
            std::process::exit(2);
        }
    };
    println!("utf8-probe {}", platform_report());
    match run(scenario, timeout) {
        Ok(()) => println!("utf8-probe scenario={} result=pass", scenario.name()),
        Err(failure) => {
            print_step(failure.step, "fail", &failure.detail);
            eprintln!(
                "utf8-probe: step {} failed: {}",
                failure.step, failure.detail
            );
            std::process::exit(failure.code);
        }
    }
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<(Scenario, Duration), String> {
    const USAGE: &str = "usage: utf8-probe <sweep|invalid> [--timeout-secs N]";
    let mut scenario: Option<Scenario> = None;
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "sweep" if scenario.is_none() => scenario = Some(Scenario::Sweep),
            "invalid" if scenario.is_none() => scenario = Some(Scenario::Invalid),
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
    scenario
        .map(|scenario| (scenario, timeout))
        .ok_or_else(|| format!("a scenario is required. {USAGE}"))
}

fn print_step(step: &str, status: &str, detail: &str) {
    // Keep every step line single-line and parseable: the detail field is
    // quoted, so newlines and double quotes inside it are normalized away.
    let clean = detail.replace(['\r', '\n'], " ").replace('"', "'");
    println!("utf8-probe step={step} status={status} detail=\"{clean}\"");
}

fn run(scenario: Scenario, timeout: Duration) -> Result<(), Failure> {
    for &buffer_bytes in scenario.buffers() {
        run_one(scenario, buffer_bytes, timeout)?;
    }
    Ok(())
}

/// One full fixture lifecycle at one read-buffer size. Exit codes are
/// step-stable across the sweep: 10 alloc, 11 spawn, 12 ready, 13 corpus,
/// 14 the invalid-byte contract, 15 child exit, 16 teardown; the buffer
/// size rides in every detail.
fn run_one(scenario: Scenario, buffer_bytes: usize, timeout: Duration) -> Result<(), Failure> {
    println!(
        "utf8-probe scenario={} buffer_bytes={buffer_bytes}",
        scenario.name()
    );
    let with_buf = |detail: String| format!("buf={buffer_bytes}: {detail}");

    let (pair, alloc_ms) = alloc_pty(COLS, ROWS, timeout)
        .map_err(|detail| Failure::new("alloc", 10, with_buf(detail)))?;
    print_step(
        "alloc",
        "pass",
        &with_buf(format!("pty allocated at {COLS}x{ROWS} in {alloc_ms}ms")),
    );
    let PtyPair { master, slave } = pair;

    let fixture =
        sibling_utf8_child().map_err(|detail| Failure::new("spawn", 11, with_buf(detail)))?;
    let mut command = CommandBuilder::new(&fixture);
    command.arg(scenario.fixture_mode());
    let mut child = slave.spawn_command(command).map_err(|err| {
        Failure::new(
            "spawn",
            11,
            with_buf(format!("child spawn failed: {err:#}")),
        )
    })?;
    // Release our copy of the child end: holding it open would keep the
    // master from ever seeing end-of-stream after the child exits.
    drop(slave);
    print_step(
        "spawn",
        "pass",
        &with_buf(format!(
            "spawned `{} {}` pid={}",
            fixture.display(),
            scenario.fixture_mode(),
            child
                .process_id()
                .map_or_else(|| "unknown".to_string(), |pid| pid.to_string()),
        )),
    );

    let reader = master.try_clone_reader().map_err(|err| {
        Failure::new(
            "ready",
            12,
            with_buf(format!("cloning the reader failed: {err:#}")),
        )
    })?;
    let writer = SharedWriter::new(master.take_writer().map_err(|err| {
        Failure::new(
            "ready",
            12,
            with_buf(format!("taking the writer failed: {err:#}")),
        )
    })?);
    let events =
        spawn_reader_with_buffer(reader, writer, Arc::new(AtomicU32::new(0)), buffer_bytes);
    let mut stream = Stream::new(events);

    let ready = stream
        .wait_for_report(
            "the fixture's ready report",
            |report| report.event == EVENT_READY,
            timeout,
        )
        .map_err(|detail| Failure::new("ready", 12, with_buf(detail)))?;
    check_ready(&ready, scenario, cfg!(windows))
        .map_err(|detail| Failure::new("ready", 12, with_buf(detail)))?;
    print_step(
        "ready",
        "pass",
        &with_buf(format!("fixture reports: {ready}")),
    );

    let trailer = stream
        .wait_for_report(
            "the fixture's corpus trailer",
            |report| report.event == EVENT_UTF8_END,
            timeout,
        )
        .map_err(|detail| Failure::new("corpus", 13, with_buf(detail)))?;
    let corpus_detail = check_corpus(&stream, &trailer, scenario)
        .map_err(|detail| Failure::new("corpus", 13, with_buf(detail)))?;
    print_step("corpus", "pass", &with_buf(corpus_detail));

    if scenario.include_junk() {
        let junk_detail = check_junk(
            &strip_ansi(&stream.text),
            &stream.raw,
            &stream.spans,
            cfg!(windows),
        )
        .map_err(|detail| Failure::new("junk", 14, with_buf(detail)))?;
        print_step("junk", "pass", &with_buf(junk_detail));
    }

    let exit_detail = wait_child(child.as_mut(), timeout)
        .map_err(|detail| Failure::new("child_exit", 15, with_buf(detail)))?;
    print_step("child_exit", "pass", &with_buf(exit_detail));

    let (events, end) = stream.into_teardown_parts();
    let teardown_detail = teardown(master, &events, end, timeout)
        .map_err(|detail| Failure::new("teardown", 16, with_buf(detail)))?;
    print_step("teardown", "pass", &with_buf(teardown_detail));
    Ok(())
}

/// The child's whole output run, reassembled through the decode layer
/// under test. Everything is kept — the raw bytes (the coordinate space
/// invalid spans point into), the decoded text, the spans — because a
/// corpus run is a few hundred payload bytes plus decoration, and the
/// verification wants the complete transcript.
struct Stream {
    events: mpsc::Receiver<ReaderEvent>,
    carry: Utf8Carry,
    text: String,
    raw: Vec<u8>,
    spans: Vec<InvalidSpan>,
    end: Option<EndInfo>,
    reads: u64,
}

impl Stream {
    fn new(events: mpsc::Receiver<ReaderEvent>) -> Self {
        Self {
            events,
            carry: Utf8Carry::new(),
            text: String::new(),
            raw: Vec::new(),
            spans: Vec::new(),
            end: None,
            reads: 0,
        }
    }

    fn absorb(&mut self, event: ReaderEvent) {
        match event {
            ReaderEvent::Data { bytes, .. } => {
                self.reads += 1;
                self.raw.extend_from_slice(&bytes);
                let decoded = self.carry.push(&bytes);
                self.text.push_str(&decoded.text);
                self.spans.extend(decoded.invalid);
            }
            ReaderEvent::End(info) => {
                // The stream is over: a still-carried partial codepoint can
                // never complete now, so it surfaces as a span rather than
                // disappearing with the stream.
                self.spans.extend(self.carry.finish());
                self.end = Some(info);
            }
        }
    }

    /// Wait until a report matching `matches` has arrived, failing — never
    /// hanging — on timeout or on end-of-stream first. Reports are parsed
    /// from the ANSI-stripped view of the decoded text; re-parsing the
    /// whole (small) transcript per event buys simplicity at no real cost.
    ///
    /// Only lines the stream has finished — terminated by a newline — are
    /// parsed. With one-byte reads a report arrives one character at a
    /// time, and `probe-child event=ready` is a well-formed report the
    /// instant before its fields stream in; matching a half-arrived line
    /// would assert against fields that are still in flight.
    fn wait_for_report(
        &mut self,
        what: &str,
        matches: impl Fn(&Report) -> bool,
        timeout: Duration,
    ) -> Result<Report, String> {
        let deadline = Instant::now() + timeout;
        loop {
            let stripped = strip_ansi(&self.text);
            let complete = &stripped[..stripped.rfind('\n').map_or(0, |at| at + 1)];
            if let Some(report) = reports_in(complete).into_iter().find(&matches) {
                return Ok(report);
            }
            if let Some(info) = &self.end {
                return Err(format!(
                    "stream ended ({}) before {what}; tail: '{}'",
                    info.reason,
                    self.tail(200),
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "{what} not observed within {}ms ({} byte(s) held mid-codepoint); tail: '{}'",
                    timeout.as_millis(),
                    self.carry.pending(),
                    self.tail(200),
                ));
            }
            match self.events.recv_timeout(deadline - now) {
                Ok(event) => self.absorb(event),
                Err(mpsc::RecvTimeoutError::Timeout) => {} // deadline re-checked at loop top
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("reader thread ended without reporting end-of-stream".to_string());
                }
            }
        }
    }

    /// The last `chars` characters of the ANSI-stripped transcript, for
    /// failure diagnostics.
    fn tail(&self, chars: usize) -> String {
        let text = strip_ansi(&self.text);
        let start = text
            .char_indices()
            .rev()
            .nth(chars.saturating_sub(1))
            .map_or(0, |(i, _)| i);
        text[start..].to_string()
    }

    fn into_teardown_parts(self) -> (mpsc::Receiver<ReaderEvent>, Option<EndInfo>) {
        (self.events, self.end)
    }
}

/// The fixture must have come up in the requested mode, and under ConPTY
/// the console must actually hold UTF-8 code pages — the fixture reports
/// the verified values, and a run against a legacy code page would measure
/// the wrong thing no matter how green it looked. The platform contract is
/// a parameter (the live call passes `cfg!(windows)`) so unit tests
/// exercise both variants on any host.
fn check_ready(ready: &Report, scenario: Scenario, conpty: bool) -> Result<(), String> {
    if ready.field("mode") != Some(scenario.fixture_mode()) {
        return Err(format!("the fixture came up in the wrong mode: {ready}"));
    }
    if conpty {
        for key in ["output_cp", "input_cp"] {
            match ready.field(key) {
                Some(value) if value.ends_with("->65001") => {}
                other => {
                    return Err(format!(
                        "the console is not on CP_UTF8 ({key}={}): {ready}",
                        other.unwrap_or("missing")
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Everything the corpus step asserts once the trailer has arrived: the
/// trailer states what this probe expects the fixture to have written, the
/// reassembled lines are byte-exact, and — outside the invalid lane —
/// nothing in the stream produced an invalid span.
fn check_corpus(stream: &Stream, trailer: &Report, scenario: Scenario) -> Result<String, String> {
    let summary = corpus_summary(scenario.include_junk());
    check_trailer(trailer, &summary)?;
    let stripped = strip_ansi(&stream.text);
    let expected: Vec<(usize, &str)> = UTF8_CORPUS
        .iter()
        .enumerate()
        .map(|(seq, item)| (seq, *item))
        .collect();
    let ignored = scenario.include_junk().then(junk_seq);
    check_corpus_lines(&stripped, &expected, ignored)?;
    if !scenario.include_junk() && !stream.spans.is_empty() {
        return Err(format!(
            "{} invalid span(s) in a corpus that is entirely valid UTF-8; first: {:?}",
            stream.spans.len(),
            stream.spans[0],
        ));
    }
    Ok(format!(
        "{} items, {} payload bytes, {} codepoints reassembled byte-exact across {} reads ({} raw bytes); trailer fnv={:016x} matches",
        summary.items,
        summary.bytes,
        summary.chars,
        stream.reads,
        stream.raw.len(),
        summary.fnv,
    ))
}

/// The trailer is the fixture's statement of what it actually wrote; every
/// field must match what this probe independently expects from the shared
/// corpus, or the two sides are not talking about the same bytes.
fn check_trailer(trailer: &Report, summary: &CorpusSummary) -> Result<(), String> {
    let expected = [
        ("items", summary.items.to_string()),
        ("bytes", summary.bytes.to_string()),
        ("chars", summary.chars.to_string()),
        ("fnv", format!("{:016x}", summary.fnv)),
    ];
    for (key, want) in &expected {
        match trailer.field(key) {
            Some(got) if got == want => {}
            got => {
                return Err(format!(
                    "trailer {key} is {}, expected {want} — the fixture did not write (or the terminal did not deliver) the corpus this probe verifies: {trailer}",
                    got.unwrap_or("missing"),
                ));
            }
        }
    }
    Ok(())
}

/// Every expected corpus line must appear byte-exact at least once in the
/// (ANSI-stripped) transcript. A payload that is a strict prefix of its
/// item is tolerated alongside the complete copy — a terminal repaint can
/// cut a line short and re-emit it — but a mismatch that is not a prefix
/// is corruption, and an unknown `seq` means the framing itself was
/// damaged. `ignored`, if set, exempts one seq (the junk line, which has
/// its own contract).
fn check_corpus_lines(
    stripped: &str,
    expected: &[(usize, &str)],
    ignored: Option<usize>,
) -> Result<(), String> {
    let mut exact = vec![false; expected.len()];
    for line in stripped.lines() {
        let Some((seq, payload)) = parse_corpus_line(line) else {
            continue;
        };
        if ignored == Some(seq) {
            continue;
        }
        let Some(slot) = expected.iter().position(|(want, _)| *want == seq) else {
            return Err(format!(
                "a corpus line carries unknown seq {seq}: '{}'",
                line.escape_debug()
            ));
        };
        let want = expected[slot].1;
        if payload == want {
            exact[slot] = true;
        } else if !want.starts_with(payload) {
            return Err(format!(
                "corpus item {seq} arrived corrupted: expected '{}', got '{}'",
                want.escape_debug(),
                payload.escape_debug(),
            ));
        }
    }
    let missing: Vec<String> = expected
        .iter()
        .zip(&exact)
        .filter(|(_, found)| !**found)
        .map(|((seq, _), _)| seq.to_string())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "no byte-exact copy arrived for corpus item(s) {}",
            missing.join(", ")
        ))
    }
}

/// The invalid-byte contract, as a typed outcome. On POSIX (`conpty` =
/// false) a PTY is a byte pipe: the junk must be in the raw stream
/// verbatim, the decode layer must report it as exactly one 1-byte span
/// per junk byte at the junk's raw offset, and the decoded junk line must
/// be both neighbors with nothing between. On Windows ConPTY re-renders
/// output, so two outcomes are legitimate and recorded: the same verbatim
/// pass-through, or U+FFFD substituted for the junk with both neighbors
/// intact. What no platform may do is make the junk vanish without a
/// trace — bytes that were written must be decoded, reported, or visibly
/// replaced, never silently dropped.
fn check_junk(
    stripped: &str,
    raw: &[u8],
    spans: &[InvalidSpan],
    conpty: bool,
) -> Result<String, String> {
    // The junk line's candidates, under the same repaint tolerance as the
    // valid items: a strict prefix of a legitimate shape is a truncated
    // repaint; anything else that parses as the junk seq must BE a
    // legitimate shape.
    let dropped_shape = junk_decoded();
    let mut spans_shape_seen = false;
    let mut substituted: Option<usize> = None;
    for line in stripped.lines() {
        let Some((seq, payload)) = parse_corpus_line(line) else {
            continue;
        };
        if seq != junk_seq() {
            continue;
        }
        if payload == dropped_shape {
            spans_shape_seen = true;
            continue;
        }
        let replacements = payload.chars().filter(|c| *c == '\u{fffd}').count();
        let without: String = payload.chars().filter(|c| *c != '\u{fffd}').collect();
        if replacements > 0 && without == dropped_shape {
            substituted = Some(substituted.map_or(replacements, |n| n.max(replacements)));
            continue;
        }
        if dropped_shape.starts_with(payload) {
            continue; // a repaint cut this copy short; a full copy must also appear
        }
        return Err(format!(
            "the junk line arrived in an unrecognized shape: '{}' — neighbors damaged",
            payload.escape_debug()
        ));
    }

    if let Some(junk_at) = find_junk(raw) {
        // The junk bytes crossed the terminal verbatim. Now the decode
        // layer is on the hook for the exact report: one 1-byte span per
        // junk byte (none of them could ever start a valid sequence), at
        // the junk's position in the stream.
        let expected: Vec<InvalidSpan> = (0..JUNK_BYTES.len())
            .map(|i| InvalidSpan {
                offset: junk_at + i as u64,
                len: 1,
            })
            .collect();
        if spans != expected {
            return Err(format!(
                "the junk bytes are in the stream at offset {junk_at}, but the reported spans are wrong: expected {expected:?}, got {spans:?}"
            ));
        }
        if !spans_shape_seen {
            return Err(
                "the junk was reported as spans, but no intact copy of its neighbors arrived"
                    .to_string(),
            );
        }
        return Ok(format!(
            "the {} junk bytes crossed the pty verbatim and the decode layer reported them as one span each at stream offset {junk_at}; both neighbors byte-exact",
            JUNK_BYTES.len()
        ));
    }

    // The junk did not arrive verbatim.
    if !conpty {
        return Err(
            "the junk bytes never reached the master — a POSIX pty must pass them through verbatim"
                .to_string(),
        );
    }
    if !spans.is_empty() {
        return Err(format!(
            "invalid spans without the junk bytes present in the stream: {spans:?}"
        ));
    }
    match substituted {
        Some(replacements) => Ok(format!(
            "conpty substituted {replacements} U+FFFD replacement(s) for the {} junk bytes; both neighbors byte-exact — visibly replaced, not silently dropped",
            JUNK_BYTES.len()
        )),
        None => Err(
            "the junk bytes left no trace — neither verbatim spans nor U+FFFD replacements; they were silently dropped"
                .to_string(),
        ),
    }
}

/// Where the junk bytes sit in the raw stream, if they arrived verbatim.
/// The sequence cannot occur anywhere else: terminal decoration is ASCII
/// escape sequences and the corpus is valid UTF-8, neither of which can
/// contain these bytes.
fn find_junk(raw: &[u8]) -> Option<u64> {
    raw.windows(JUNK_BYTES.len())
        .position(|window| window == JUNK_BYTES)
        .and_then(|at| u64::try_from(at).ok())
}

/// The fixture binary sits next to this one — cargo builds every workspace
/// binary into the same directory.
fn sibling_utf8_child() -> Result<std::path::PathBuf, String> {
    let me = std::env::current_exe().map_err(|err| format!("current_exe failed: {err}"))?;
    let dir = me
        .parent()
        .ok_or_else(|| "current_exe has no parent directory".to_string())?;
    let fixture = dir.join(format!("utf8-child{}", std::env::consts::EXE_SUFFIX));
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
    use agent_bridge_probe_child::corpus::corpus_line_lead;

    /// A raw child transcript: every corpus line in wire shape, optionally
    /// with the junk line, exactly as a byte-transparent PTY would deliver
    /// it. Runs the real decode layer over it so tests exercise the same
    /// text/raw/span triple the live probe verifies.
    fn transcript(
        include_junk: bool,
        mutate: impl FnOnce(&mut Vec<u8>),
    ) -> (String, Vec<u8>, Vec<InvalidSpan>) {
        let mut raw = Vec::new();
        for line in agent_bridge_probe_child::corpus::corpus_lines(include_junk) {
            raw.extend_from_slice(corpus_line_lead(line.seq).as_bytes());
            raw.extend_from_slice(&line.payload);
            raw.extend_from_slice(b"\r\n");
        }
        mutate(&mut raw);
        let mut carry = Utf8Carry::new();
        let decoded = carry.push(&raw);
        let mut spans = decoded.invalid;
        spans.extend(carry.finish());
        (decoded.text, raw, spans)
    }

    fn expected_items() -> Vec<(usize, &'static str)> {
        UTF8_CORPUS
            .iter()
            .enumerate()
            .map(|(seq, item)| (seq, *item))
            .collect()
    }

    #[test]
    fn args_select_scenario_and_timeout() {
        let args = ["invalid", "--timeout-secs", "3"].map(String::from);
        let (scenario, timeout) = parse_args(args.into_iter()).unwrap();
        assert!(matches!(scenario, Scenario::Invalid));
        assert_eq!(timeout, Duration::from_secs(3));
    }

    #[test]
    fn a_scenario_is_required_and_unknown_arguments_are_rejected() {
        assert!(parse_args(std::iter::empty()).is_err());
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
        assert!(parse_args(["sweep".to_string(), "invalid".to_string()].into_iter()).is_err());
    }

    #[test]
    fn a_clean_transcript_passes_the_line_check() {
        let (text, _, spans) = transcript(false, |_| {});
        assert!(spans.is_empty());
        check_corpus_lines(&strip_ansi(&text), &expected_items(), None)
            .expect("a byte-exact transcript must pass");
    }

    #[test]
    fn ansi_decoration_does_not_hide_a_corpus_line() {
        let (text, _, _) = transcript(false, |raw| {
            // ConPTY-style bracketing: clear-screen and color around, and a
            // cursor query mid-stream.
            let mut decorated = b"\x1b[2J\x1b[1;1H".to_vec();
            decorated.extend_from_slice(raw);
            decorated.extend_from_slice(b"\x1b[0m");
            *raw = decorated;
        });
        check_corpus_lines(&strip_ansi(&text), &expected_items(), None)
            .expect("decoration must strip away, not corrupt");
    }

    #[test]
    fn a_single_flipped_byte_fails_the_line_check() {
        // Flip one continuation byte inside the first item's é.
        let (text, _, _) = transcript(false, |raw| {
            let at = raw
                .windows(2)
                .position(|w| w == "é".as_bytes())
                .expect("é must be in the transcript");
            raw[at + 1] = 0xA8; // é -> è: still valid UTF-8, wrong bytes
        });
        let err = check_corpus_lines(&strip_ansi(&text), &expected_items(), None).unwrap_err();
        assert!(err.contains("corrupted"), "unexpected error: {err}");
    }

    #[test]
    fn a_truncated_repaint_is_tolerated_only_next_to_a_complete_copy() {
        // The complete line plus a prefix-truncated repaint of it: pass.
        let (text, _, _) = transcript(false, |raw| {
            let extra = format!("{}héllo\r\n", corpus_line_lead(0));
            raw.extend_from_slice(extra.as_bytes());
        });
        check_corpus_lines(&strip_ansi(&text), &expected_items(), None)
            .expect("a truncated extra copy must not fail the exact one");

        // Only the truncated copy, no complete line: fail.
        let alone = format!("{}héllo\r\n", corpus_line_lead(0));
        let err = check_corpus_lines(&alone, &[(0, "héllo 🌍")], None).unwrap_err();
        assert!(
            err.contains("no byte-exact copy"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn a_missing_item_names_its_seq() {
        let err = check_corpus_lines("nothing here", &expected_items(), None).unwrap_err();
        assert!(
            err.contains("no byte-exact copy"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn an_unknown_seq_is_framing_damage() {
        let line = format!("{}whatever\r\n", corpus_line_lead(99));
        let err = check_corpus_lines(&line, &expected_items(), None).unwrap_err();
        assert!(err.contains("unknown seq 99"), "unexpected error: {err}");
    }

    #[test]
    fn the_trailer_check_holds_every_field() {
        let summary = corpus_summary(false);
        let line = format!(
            "probe-child event=utf8-end items={} bytes={} chars={} fnv={:016x}",
            summary.items, summary.bytes, summary.chars, summary.fnv
        );
        let trailer = Report::parse(&line).unwrap();
        check_trailer(&trailer, &summary).expect("a faithful trailer must pass");

        let wrong = Report::parse(&line.replace("items=", "items=1")).unwrap();
        let err = check_trailer(&wrong, &summary).unwrap_err();
        assert!(err.contains("trailer items"), "unexpected error: {err}");
    }

    #[test]
    fn verbatim_junk_passes_on_both_platform_contracts() {
        let (text, raw, spans) = transcript(true, |_| {});
        let stripped = strip_ansi(&text);
        for conpty in [false, true] {
            let detail = check_junk(&stripped, &raw, &spans, conpty)
                .expect("verbatim junk with exact spans must pass");
            assert!(detail.contains("verbatim"), "unexpected detail: {detail}");
        }
    }

    #[test]
    fn wrong_spans_fail_even_when_the_junk_arrived() {
        let (text, raw, _) = transcript(true, |_| {});
        let err = check_junk(&strip_ansi(&text), &raw, &[], cfg!(windows)).unwrap_err();
        assert!(err.contains("spans are wrong"), "unexpected error: {err}");
    }

    #[test]
    fn substitution_is_a_recorded_outcome_on_conpty_and_a_failure_elsewhere() {
        // What a substituting terminal delivers: junk replaced by U+FFFD
        // before the bytes ever reach the probe.
        let (text, raw, spans) = transcript(true, |raw| {
            let at = raw
                .windows(JUNK_BYTES.len())
                .position(|w| w == JUNK_BYTES)
                .expect("junk must be present before substitution");
            raw.splice(
                at..at + JUNK_BYTES.len(),
                "\u{fffd}".as_bytes().iter().copied(),
            );
        });
        assert!(spans.is_empty(), "U+FFFD is valid UTF-8; no spans expected");
        let stripped = strip_ansi(&text);
        let detail = check_junk(&stripped, &raw, &spans, true)
            .expect("substitution is a legitimate conpty outcome");
        assert!(
            detail.contains("substituted"),
            "unexpected detail: {detail}"
        );
        let err = check_junk(&stripped, &raw, &spans, false).unwrap_err();
        assert!(
            err.contains("must pass them through verbatim"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn silently_dropped_junk_fails_everywhere() {
        let (text, raw, spans) = transcript(true, |raw| {
            let at = raw
                .windows(JUNK_BYTES.len())
                .position(|w| w == JUNK_BYTES)
                .expect("junk must be present before the drop");
            raw.drain(at..at + JUNK_BYTES.len());
        });
        assert!(spans.is_empty());
        let stripped = strip_ansi(&text);
        for conpty in [false, true] {
            let err = check_junk(&stripped, &raw, &spans, conpty).unwrap_err();
            assert!(
                err.contains("verbatim") || err.contains("silently dropped"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn damaged_junk_neighbors_fail_in_any_shape() {
        // The junk becomes spans but the neighbor text is mangled: the
        // candidate parses, matches no legitimate shape, and must fail.
        let neighbor = "b🌍".as_bytes();
        let (text, raw, spans) = transcript(true, |raw| {
            let at = raw
                .windows(neighbor.len())
                .position(|w| w == neighbor)
                .expect("the after-neighbor must be present");
            raw[at] = b'x'; // b -> x
        });
        let err = check_junk(&strip_ansi(&text), &raw, &spans, cfg!(windows)).unwrap_err();
        assert!(
            err.contains("unrecognized shape") || err.contains("neighbors"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn the_junk_finder_needs_the_exact_bytes() {
        let (_, raw, _) = transcript(true, |_| {});
        assert!(find_junk(&raw).is_some());
        let (_, clean, _) = transcript(false, |_| {});
        assert_eq!(find_junk(&clean), None);
    }

    #[test]
    fn a_half_arrived_report_is_not_matched_early() {
        // With one-byte reads, `probe-child event=ready` is on screen
        // before its fields are; the wait must hold out for the line's
        // newline, or it asserts against fields still in flight.
        let (tx, events) = mpsc::channel();
        tx.send(ReaderEvent::Data {
            at: Instant::now(),
            bytes: b"probe-child event=ready".to_vec(),
        })
        .unwrap();
        tx.send(ReaderEvent::Data {
            at: Instant::now(),
            bytes: b" mode=valid os=linux pid=1\r\n".to_vec(),
        })
        .unwrap();
        let mut stream = Stream::new(events);
        let ready = stream
            .wait_for_report(
                "the ready report",
                |report| report.event == EVENT_READY,
                Duration::from_secs(5),
            )
            .expect("the completed line must match");
        assert_eq!(
            ready.field("mode"),
            Some("valid"),
            "the match ran against a half-arrived line"
        );
    }

    #[test]
    fn ready_checks_the_mode_field() {
        let ready = Report::parse("probe-child event=ready mode=valid os=linux pid=1").unwrap();
        check_ready(&ready, Scenario::Sweep, false).expect("matching mode must pass");
        assert!(check_ready(&ready, Scenario::Invalid, false).is_err());
    }

    #[test]
    fn ready_on_conpty_requires_verified_utf8_code_pages() {
        // Without the code-page fields (or with a legacy value in them), a
        // ConPTY run would measure the wrong thing however green it looked
        // — the ready gate must refuse it.
        let bare = Report::parse("probe-child event=ready mode=valid os=windows pid=1").unwrap();
        let err = check_ready(&bare, Scenario::Sweep, true).unwrap_err();
        assert!(err.contains("CP_UTF8"), "unexpected error: {err}");

        let good = Report::parse(
            "probe-child event=ready mode=valid os=windows pid=1 \
             output_cp=437->65001 input_cp=437->65001",
        )
        .unwrap();
        check_ready(&good, Scenario::Sweep, true).expect("verified UTF-8 code pages must pass");

        let legacy = Report::parse(
            "probe-child event=ready mode=valid os=windows pid=1 \
             output_cp=437->437 input_cp=437->65001",
        )
        .unwrap();
        assert!(check_ready(&legacy, Scenario::Sweep, true).is_err());
    }
}
