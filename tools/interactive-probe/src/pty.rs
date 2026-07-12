//! PTY plumbing for the interactive probe, extending the spawn-read-teardown
//! skeleton of `pty-probe` with what an *interactive* child needs: chunk
//! timestamps (first-token latency), a writer the probe can type into while
//! the reader thread still answers terminal queries, and an output tracker
//! that keeps a bounded window of recent decoded text so lanes can wait for
//! on-screen markers without the buffer growing with the session.
//!
//! The known ConPTY rough edges are guarded the same way as in `pty-probe`,
//! so a hang becomes a diagnosed failure rather than a stuck CI lane:
//! allocation and master-close run on timeout-guarded helper threads, the
//! reader answers the cursor-position query ConPTY emits at startup (an
//! unanswered query stalls the child before its first paint — which would
//! masquerade as first-token latency), and child exit is polled, never
//! blocking-waited.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{Child, MasterPty, PtyPair, PtySize, native_pty_system};

use crate::capture::CaptureWriter;
use crate::firsttoken::FirstTokenClock;
use crate::utf8;

/// Allocate the PTY on a helper thread: `openpty` can hang on Windows when
/// the console subsystem is not yet initialised, and a hang must surface as
/// a failed step, not a stuck probe. On timeout the helper thread is
/// deliberately leaked — the process is about to exit with a diagnostic.
pub fn alloc_pty(cols: u16, rows: u16, timeout: Duration) -> Result<(PtyPair, u128), String> {
    let (tx, rx) = mpsc::channel();
    let started = Instant::now();
    std::thread::spawn(move || {
        let _ = tx.send(native_pty_system().openpty(PtySize {
            rows,
            cols,
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

/// The PTY master writer, shared between the probe (typing input) and the
/// reader thread (answering terminal queries). Writes are line-short and
/// rare on both sides, so one mutex is plenty.
#[derive(Clone)]
pub struct SharedWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl SharedWriter {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(writer)))
    }

    /// Write bytes to the child and flush, e.g. a control byte or a typed
    /// line. Poisoning is unreachable: no holder panics while writing.
    pub fn send(&self, bytes: &[u8]) -> std::io::Result<()> {
        let mut writer = self.0.lock().unwrap();
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// Type text the way a human does: the text, a settle pause so the
    /// TUI's input loop registers it, then Enter as a carriage return —
    /// terminals send `\r` for Enter; the TTY (or ConPTY) translates.
    /// Returns the instant Enter reached the child, which is the submit
    /// timestamp every turn measurement is relative to.
    pub fn type_line(&self, text: &str, settle: Duration) -> std::io::Result<Instant> {
        self.send(text.as_bytes())?;
        std::thread::sleep(settle);
        let submitted_at = Instant::now();
        self.send(b"\r")?;
        Ok(submitted_at)
    }
}

pub enum ReaderEvent {
    Data { at: Instant, bytes: Vec<u8> },
    End(EndInfo),
}

#[derive(Debug)]
pub struct EndInfo {
    pub reason: String,
    pub cursor_queries_answered: u32,
    /// First failure writing the cursor-position reply, if any. A failed
    /// reply usually shows up later as a blocked child, so the root cause
    /// must survive into the diagnostics.
    pub cursor_reply_error: Option<String>,
}

/// How many bytes one master read may return — the buffer size for every
/// probe that is not deliberately forcing tiny reads. 8 KiB comfortably
/// exceeds any burst the fixtures produce.
pub const DEFAULT_READ_BUFFER_BYTES: usize = 8192;

/// Read the master on a dedicated thread, forwarding timestamped chunks over
/// a channel. The thread also answers ConPTY's `ESC[6n` cursor-position
/// query — ConPTY emits it at startup and blocks the child until a reply
/// arrives — and it keeps draining until end-of-stream so teardown never
/// closes a master whose buffered output has no reader. `queries_answered`
/// is updated live so a first-token-timeout diagnostic can already report
/// whether the startup query was seen and answered.
pub fn spawn_reader(
    reader: Box<dyn Read + Send>,
    writer: SharedWriter,
    queries_answered: Arc<AtomicU32>,
) -> mpsc::Receiver<ReaderEvent> {
    spawn_reader_with_buffer(reader, writer, queries_answered, DEFAULT_READ_BUFFER_BYTES)
}

/// [`spawn_reader`] with a caller-chosen read-buffer size. The UTF-8 probe
/// sweeps this down to a single byte to force a chunk boundary at every
/// offset of its corpus; everything else — the cursor-query answering, the
/// drain-until-end contract — is deliberately identical, so tiny-buffer
/// runs exercise the same reader every other probe trusts at full size. A
/// zero size is nudged up to one byte: a reader that can never make
/// progress would leave the caller's timeout as the only diagnostic.
pub fn spawn_reader_with_buffer(
    mut reader: Box<dyn Read + Send>,
    writer: SharedWriter,
    queries_answered: Arc<AtomicU32>,
    buffer_bytes: usize,
) -> mpsc::Receiver<ReaderEvent> {
    const CURSOR_QUERY: &[u8] = b"\x1b[6n";
    const CURSOR_REPLY: &[u8] = b"\x1b[1;1R";
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reply_error: Option<String> = None;
        let mut scan_tail: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; buffer_bytes.max(1)];
        let reason = loop {
            match reader.read(&mut buf) {
                Ok(0) => break "eof".to_string(),
                Ok(n) => {
                    let at = Instant::now();
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
                            match writer.send(CURSOR_REPLY) {
                                Ok(()) => {
                                    queries_answered.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(err) => {
                                    reply_error.get_or_insert_with(|| err.to_string());
                                }
                            }
                        }
                    }
                    scan_tail = scan[scan.len().saturating_sub(CURSOR_QUERY.len() - 1)..].to_vec();
                    if tx
                        .send(ReaderEvent::Data {
                            at,
                            bytes: chunk.to_vec(),
                        })
                        .is_err()
                    {
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
            cursor_queries_answered: queries_answered.load(Ordering::Relaxed),
            cursor_reply_error: reply_error,
        }));
    });
    rx
}

/// Accumulates the child's output — decoded text for marker waits, chunk
/// timestamps into the first-token clock, raw bytes into an optional
/// capture file — while remembering end-of-stream when it arrives.
/// How much recently-decoded output the tracker keeps. Generous next to any
/// screen (80×24 is under 2 KiB) and next to any single read (8 KiB), so a
/// marker cannot be evicted between arriving and being looked for; small
/// enough that a half-hour of streaming costs neither memory nor time.
const RECENT_WINDOW_BYTES: usize = 64 * 1024;

pub struct OutputTracker {
    events: mpsc::Receiver<ReaderEvent>,
    reassembler: utf8::Reassembler,
    /// A rolling window of the child's decoded output, oldest text dropped.
    /// Raw, not ANSI-stripped: an escape sequence can straddle two reads, so
    /// stripping must happen over a contiguous run and therefore on read.
    recent: String,
    pub clock: FirstTokenClock,
    capture: Option<CaptureWriter>,
    end: Option<EndInfo>,
    chunks: u64,
}

impl OutputTracker {
    pub fn new(
        events: mpsc::Receiver<ReaderEvent>,
        clock: FirstTokenClock,
        capture: Option<CaptureWriter>,
    ) -> Self {
        Self {
            events,
            reassembler: utf8::Reassembler::new(),
            recent: String::new(),
            clock,
            capture,
            end: None,
            chunks: 0,
        }
    }

    /// How many output chunks have arrived. Comparing this across a pause is
    /// how a caller tells "the child went quiet" from "the child is still
    /// streaming" without matching on what it says.
    pub fn chunks_seen(&self) -> u64 {
        self.chunks
    }

    fn absorb(&mut self, event: ReaderEvent) -> Result<(), String> {
        match event {
            ReaderEvent::Data { at, bytes } => {
                self.chunks += 1;
                self.clock.note_chunk(at);
                if let Some(capture) = &mut self.capture {
                    capture
                        .record(at, &bytes)
                        .map_err(|err| format!("capture write failed: {err}"))?;
                }
                self.reassembler.push(&bytes).map_err(|_| {
                    "output contained bytes that can never be valid UTF-8".to_string()
                })?;
                self.recent.push_str(&self.reassembler.take_decoded());
                self.trim_recent();
                Ok(())
            }
            ReaderEvent::End(info) => {
                self.end = Some(info);
                Ok(())
            }
        }
    }

    /// Drop the oldest text once the window is over budget, on a character
    /// boundary. Every consumer of this buffer wants "what is on screen now":
    /// marker waits look at output that just arrived, and the failure tails
    /// quote the last few hundred characters. Keeping the whole session
    /// instead would grow memory without bound and make each of those reads
    /// cost the length of the session so far.
    fn trim_recent(&mut self) {
        if self.recent.len() <= RECENT_WINDOW_BYTES {
            return;
        }
        let excess = self.recent.len() - RECENT_WINDOW_BYTES;
        let cut = (excess..self.recent.len())
            .find(|at| self.recent.is_char_boundary(*at))
            .unwrap_or(self.recent.len());
        self.recent.drain(..cut);
    }

    /// Drain whatever output arrives within `slice`. End-of-stream is noted,
    /// not an error — the caller decides whether an ended stream fails its
    /// step. Once the stream has ended there is nothing left to wait for, so
    /// this returns immediately; loops that poll it must check
    /// [`Self::stream_ended`] or they will spin.
    pub fn pump(&mut self, slice: Duration) -> Result<(), String> {
        let deadline = Instant::now() + slice;
        loop {
            if self.end.is_some() {
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(());
            }
            match self.events.recv_timeout(deadline - now) {
                Ok(event) => self.absorb(event)?,
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("reader thread ended without reporting end-of-stream".to_string());
                }
            }
        }
    }

    /// Why the child's output stream ended, if it has. A waiting loop that
    /// ignores this both spins hot against a dead child and, worse, mistakes
    /// "the process died" for whatever silence it was waiting for.
    pub fn stream_ended(&self) -> Option<&str> {
        self.end.as_ref().map(|info| info.reason.as_str())
    }

    /// `Err` as soon as the stream has ended, naming `waiting_for` so the
    /// diagnostic says what was being awaited when the child went away.
    pub fn ensure_live(&self, waiting_for: &str) -> Result<(), String> {
        match self.stream_ended() {
            Some(reason) => Err(format!(
                "the child's output stream ended ({reason}) while waiting for {waiting_for} — the process is gone, not merely quiet"
            )),
            None => Ok(()),
        }
    }

    /// Wait for the very first output chunk and return the launch latency.
    /// Chunk arrival, not decoded text, is the trigger: the first token is
    /// defined as the first byte readable from the master, whatever it is.
    pub fn wait_for_first_chunk(&mut self, timeout: Duration) -> Result<Duration, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(latency) = self.clock.launch_latency() {
                return Ok(latency);
            }
            if let Some(info) = &self.end {
                return Err(format!(
                    "stream ended ({}) before any output byte",
                    info.reason
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "no output byte within {}ms of spawn",
                    timeout.as_millis()
                ));
            }
            match self.events.recv_timeout(deadline - now) {
                Ok(event) => self.absorb(event)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {} // deadline re-checked at loop top
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("reader thread ended without reporting end-of-stream".to_string());
                }
            }
        }
    }

    /// Wait until the decoded output satisfies `pred`, failing — never
    /// hanging — on timeout or on end-of-stream before the marker arrives.
    /// `what` names the awaited marker in failure diagnostics.
    pub fn wait_for_text(
        &mut self,
        what: &str,
        pred: impl Fn(&str) -> bool,
        timeout: Duration,
    ) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            if pred(&self.recent) {
                return Ok(());
            }
            if let Some(info) = &self.end {
                return Err(format!(
                    "stream ended ({}) before {what}; tail: '{}'",
                    info.reason,
                    self.screen_tail(200),
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!(
                    "{what} not observed within {}ms; tail: '{}'",
                    timeout.as_millis(),
                    self.screen_tail(200),
                ));
            }
            match self.events.recv_timeout(deadline - now) {
                Ok(event) => self.absorb(event)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {} // deadline re-checked at loop top
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("reader thread ended without reporting end-of-stream".to_string());
                }
            }
        }
    }

    /// The recent output, ANSI-stripped — what a human would have seen scroll
    /// past lately, not a reconstructed screen and not the whole session.
    pub fn visible_text(&self) -> String {
        strip_ansi(&self.recent)
    }

    /// The last `chars` characters of the visible text — where a dialog or
    /// prompt currently sits.
    pub fn screen_tail(&self, chars: usize) -> String {
        let text = self.visible_text();
        let start = text
            .char_indices()
            .rev()
            .nth(chars.saturating_sub(1))
            .map_or(0, |(i, _)| i);
        text[start..].to_string()
    }

    pub fn end_info(&self) -> Option<&EndInfo> {
        self.end.as_ref()
    }

    /// Hand back the pieces teardown needs: the raw event stream (to keep
    /// draining), the capture writer (to finalize), and any end-of-stream
    /// already absorbed. That last one is load-bearing — if the child exits
    /// while a step is still pumping, the reader's `End` lands here and its
    /// thread exits, leaving the channel empty *and disconnected*. Teardown
    /// would then wait for an end-of-stream that has already happened and
    /// report a clean session as a failure.
    pub fn into_teardown_parts(
        self,
    ) -> (
        mpsc::Receiver<ReaderEvent>,
        Option<CaptureWriter>,
        Option<EndInfo>,
    ) {
        (self.events, self.capture, self.end)
    }
}

/// Reap the child by polling `try_wait` against a deadline: a blocking
/// `wait()` is a known ConPTY hang, so the probe never calls it. On timeout
/// the child is killed and the kill is confirmed by reaping, so a failed
/// step neither leaves a live child behind nor claims a cleanup it did not
/// verify.
pub fn wait_child(child: &mut dyn Child, timeout: Duration) -> Result<String, String> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return Ok(format!(
                    "child exited cleanly in {}ms",
                    started.elapsed().as_millis()
                ));
            }
            Ok(Some(status)) => {
                return Err(format!("child exited with code {}", status.exit_code()));
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    return Err(kill_and_reap(child, timeout));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(format!("child wait failed: {err}")),
        }
    }
}

/// How long a killed child gets to disappear before the probe reports the
/// kill as unconfirmed.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Kill a child and confirm it by reaping — `kill` only signals, and a probe
/// reports what actually happened rather than assuming the signal worked.
/// Returns what happened, in every case; the caller decides whether that is
/// a failure. A child that had already exited is reaped, not killed; a child
/// whose state cannot even be read is killed anyway — unreadable proves
/// neither alive nor exited, and only a confirmed exit earns trust.
pub fn force_kill(child: &mut dyn Child) -> String {
    let unreadable = match child.try_wait() {
        Ok(Some(status)) => {
            return format!("child had already exited (code {})", status.exit_code());
        }
        Ok(None) => None,
        Err(err) => Some(err.to_string()),
    };
    if let Err(err) = child.kill() {
        return match unreadable {
            Some(check) => format!(
                "checking whether the child was alive failed ({check}) and the precautionary kill also failed: {err}"
            ),
            None => format!("kill failed: {err}"),
        };
    }
    let grace_started = Instant::now();
    while grace_started.elapsed() < KILL_GRACE {
        match child.try_wait() {
            Ok(Some(status)) => {
                return format!("killed and reaped (exit code {})", status.exit_code());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(err) => return format!("killed, but the reap failed: {err}"),
        }
    }
    format!(
        "kill issued, but the child was not reaped within {}s",
        KILL_GRACE.as_secs()
    )
}

/// The timeout path of `wait_child`. Always returns the failure detail.
fn kill_and_reap(child: &mut dyn Child, timeout: Duration) -> String {
    format!(
        "child still running after {}s of try_wait polling; {}",
        timeout.as_secs(),
        force_kill(child)
    )
}

/// Close the master and prove the reader observes end-of-stream instead of
/// hanging. The close runs on a helper thread because `ClosePseudoConsole`
/// can deadlock when buffered output has no reader draining it
/// (microsoft/terminal#1810); on timeout the thread is deliberately leaked —
/// the process is about to exit with a diagnostic.
pub fn teardown(
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
            Ok(ReaderEvent::Data { .. }) => {} // draining output that arrived after the last step
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

/// Strip ANSI escape sequences so text assertions see what a human would
/// read — ConPTY brackets even trivial output with cursor, color, and title
/// controls. CSI sequences end at a final byte in `0x40..=0x7E`; OSC
/// sequences end at BEL or ESC-backslash; any other ESC pair is dropped
/// whole.
pub fn strip_ansi(text: &str) -> String {
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
                    if c == '\x07' {
                        break;
                    }
                    // Peek before consuming: an ESC inside the payload that
                    // is not the ST terminator must not eat the next char —
                    // it could be the real terminator.
                    if c == '\x1b' && chars.clone().next() == Some('\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {} // two-character escape — both consumed
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(at: Instant, bytes: &[u8]) -> ReaderEvent {
        ReaderEvent::Data {
            at,
            bytes: bytes.to_vec(),
        }
    }

    fn tracker(events: mpsc::Receiver<ReaderEvent>) -> OutputTracker {
        OutputTracker::new(events, FirstTokenClock::new(Instant::now()), None)
    }

    #[test]
    fn strip_ansi_removes_csi_and_osc_sequences() {
        let decorated = "\x1b[2J\x1b[1;1H\x1b]0;title\x07hi\x1b[0m";
        assert_eq!(strip_ansi(decorated), "hi");
    }

    #[test]
    fn strip_ansi_survives_a_stray_esc_inside_an_osc_payload() {
        // The ESC before BEL is payload, not the ST terminator: probing for
        // the backslash must not consume the BEL that actually ends the
        // sequence, or the following text would be swallowed.
        assert_eq!(strip_ansi("\x1b]0;t\x1b\x07hi"), "hi");
        // The two-char ST terminator still ends the sequence.
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\hi"), "hi");
    }

    #[test]
    fn wait_for_text_finds_marker_split_across_chunks() {
        let (tx, events) = mpsc::channel();
        tx.send(data(Instant::now(), b"BAN")).unwrap();
        tx.send(data(Instant::now(), b"NER")).unwrap();
        let mut tracker = tracker(events);
        tracker
            .wait_for_text(
                "banner",
                |text| text.contains("BANNER"),
                Duration::from_secs(5),
            )
            .expect("split marker must be found");
    }

    #[test]
    fn wait_for_text_reports_stream_end_not_a_hang() {
        let (tx, events) = mpsc::channel();
        tx.send(ReaderEvent::End(EndInfo {
            reason: "eof".to_string(),
            cursor_queries_answered: 0,
            cursor_reply_error: None,
        }))
        .unwrap();
        let mut tracker = tracker(events);
        let err = tracker
            .wait_for_text(
                "banner",
                |text| text.contains("BANNER"),
                Duration::from_secs(5),
            )
            .unwrap_err();
        assert!(err.contains("ended (eof)"), "unexpected error: {err}");
    }

    #[test]
    fn wait_for_text_times_out_on_a_silent_stream() {
        let (tx, events) = mpsc::channel::<ReaderEvent>();
        let _keep_stream_open = tx;
        let mut tracker = tracker(events);
        let err = tracker
            .wait_for_text(
                "banner",
                |text| text.contains("BANNER"),
                Duration::from_millis(20),
            )
            .unwrap_err();
        assert!(err.contains("within"), "unexpected error: {err}");
    }

    #[test]
    fn an_absorbed_end_of_stream_survives_into_teardown() {
        // The child exits while a step is still pumping: the reader sends
        // End, its thread exits, and the channel becomes disconnected. If
        // teardown does not inherit that End it waits for an end-of-stream
        // that already happened and fails a clean session.
        let (tx, events) = mpsc::channel();
        tx.send(ReaderEvent::End(EndInfo {
            reason: "eof".to_string(),
            cursor_queries_answered: 0,
            cursor_reply_error: None,
        }))
        .unwrap();
        drop(tx); // the reader thread is gone; the channel is disconnected

        let mut tracker = tracker(events);
        tracker.pump(Duration::from_millis(50)).unwrap();
        assert_eq!(tracker.stream_ended(), Some("eof"));

        let (events, _, end) = tracker.into_teardown_parts();
        assert!(end.is_some(), "teardown must inherit the absorbed end");

        // Teardown must close the master and accept the end it was handed,
        // rather than going looking for it down a disconnected channel.
        let (pair, _) = alloc_pty(80, 24, Duration::from_secs(5)).expect("pty must allocate");
        drop(pair.slave);
        let detail = teardown(pair.master, &events, end, Duration::from_secs(5))
            .expect("teardown must succeed on an already-ended stream");
        assert!(detail.contains("eof"), "unexpected detail: {detail}");
    }

    #[test]
    fn an_ended_stream_is_reported_not_mistaken_for_silence() {
        let (tx, events) = mpsc::channel();
        tx.send(ReaderEvent::End(EndInfo {
            reason: "read error: EIO".to_string(),
            cursor_queries_answered: 0,
            cursor_reply_error: None,
        }))
        .unwrap();
        let mut tracker = tracker(events);
        tracker.pump(Duration::from_millis(50)).unwrap();
        let err = tracker
            .ensure_live("the interrupt to take effect")
            .unwrap_err();
        assert!(err.contains("EIO"), "the end reason must survive: {err}");
        assert!(
            err.contains("the process is gone"),
            "a dead child must not read as quiet: {err}"
        );
    }

    #[test]
    fn the_output_window_stays_bounded_across_a_long_stream() {
        // A streaming turn must not grow the tracker with the session. The
        // recent window is what marker waits and failure tails read, so it is
        // bounded and the oldest text falls off the front.
        let (tx, events) = mpsc::channel();
        let chunk = "x".repeat(8 * 1024);
        for _ in 0..40 {
            tx.send(data(Instant::now(), chunk.as_bytes())).unwrap();
        }
        tx.send(data(Instant::now(), b"THE-LATEST-MARKER")).unwrap();
        let mut tracker = tracker(events);
        tracker.pump(Duration::from_millis(200)).unwrap();

        assert_eq!(tracker.chunks_seen(), 41, "every chunk must still be seen");
        assert!(
            tracker.recent.len() <= RECENT_WINDOW_BYTES + chunk.len(),
            "the window grew to {} bytes",
            tracker.recent.len()
        );
        assert!(
            tracker.screen_tail(64).contains("THE-LATEST-MARKER"),
            "the newest output must survive trimming"
        );
    }

    #[test]
    fn trimming_the_window_never_splits_a_codepoint() {
        let (tx, events) = mpsc::channel();
        // Multi-byte throughout, so a naive byte cut would land mid-codepoint.
        let chunk = "é".repeat(8 * 1024);
        for _ in 0..12 {
            tx.send(data(Instant::now(), chunk.as_bytes())).unwrap();
        }
        let mut tracker = tracker(events);
        // Decoding `recent` at all is the assertion: a split codepoint would
        // have panicked on the char-boundary drain inside absorb.
        tracker.pump(Duration::from_millis(200)).unwrap();
        assert!(tracker.recent.chars().all(|c| c == 'é'));
    }

    #[test]
    fn screen_tail_is_ansi_stripped_and_char_bounded() {
        let (tx, events) = mpsc::channel();
        tx.send(data(Instant::now(), b"\x1b[2Jhello world"))
            .unwrap();
        let mut tracker = tracker(events);
        tracker.pump(Duration::from_millis(50)).unwrap();
        assert_eq!(tracker.screen_tail(5), "world");
        assert_eq!(tracker.screen_tail(500), "hello world");
    }

    #[test]
    fn cursor_position_query_is_answered_even_when_split_across_reads() {
        struct ChunkedReader {
            chunks: std::collections::VecDeque<Vec<u8>>,
        }
        impl Read for ChunkedReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                match self.chunks.pop_front() {
                    Some(mut chunk) => {
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
        struct SinkWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for SinkWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let reader = ChunkedReader {
            chunks: std::collections::VecDeque::from(vec![
                b"boot \x1b[".to_vec(),
                b"6n rest".to_vec(),
            ]),
        };
        let written = Arc::new(Mutex::new(Vec::new()));
        let answered = Arc::new(AtomicU32::new(0));
        let events = spawn_reader(
            Box::new(reader),
            SharedWriter::new(Box::new(SinkWriter(written.clone()))),
            answered.clone(),
        );
        let info = loop {
            match events
                .recv_timeout(Duration::from_secs(5))
                .expect("reader must reach end-of-stream")
            {
                ReaderEvent::Data { .. } => {}
                ReaderEvent::End(info) => break info,
            }
        };
        assert_eq!(info.cursor_queries_answered, 1);
        assert_eq!(
            answered.load(Ordering::Relaxed),
            1,
            "live counter must agree"
        );
        assert_eq!(written.lock().unwrap().as_slice(), b"\x1b[1;1R");
        assert_eq!(info.reason, "eof");
        assert_eq!(info.cursor_reply_error, None);
    }

    #[test]
    fn a_one_byte_buffer_still_delivers_everything_and_answers_the_query() {
        // The UTF-8 probe's harshest sweep setting: every read returns one
        // byte, so the 4-byte cursor query arrives maximally split and the
        // data reaches the channel one chunk per byte.
        struct SinkWriter(Arc<Mutex<Vec<u8>>>);
        impl Write for SinkWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let payload = b"pre \x1b[6n post".to_vec();
        let written = Arc::new(Mutex::new(Vec::new()));
        let events = spawn_reader_with_buffer(
            Box::new(std::io::Cursor::new(payload.clone())),
            SharedWriter::new(Box::new(SinkWriter(written.clone()))),
            Arc::new(AtomicU32::new(0)),
            1,
        );
        let mut data = Vec::new();
        let mut chunks = 0u64;
        let info = loop {
            match events
                .recv_timeout(Duration::from_secs(5))
                .expect("reader must reach end-of-stream")
            {
                ReaderEvent::Data { bytes, .. } => {
                    chunks += 1;
                    data.extend(bytes);
                }
                ReaderEvent::End(info) => break info,
            }
        };
        assert_eq!(data, payload, "no byte may be lost to the tiny buffer");
        assert_eq!(chunks, payload.len() as u64, "one chunk per byte");
        assert_eq!(info.cursor_queries_answered, 1);
        assert_eq!(written.lock().unwrap().as_slice(), b"\x1b[1;1R");
    }
}
