//! The child's output, on its own thread, cut where a reader can use it.
//!
//! A terminal read is a blocking call that is not cancel-safe and cannot be
//! polled, so it lives on a dedicated thread and reaches the rest of the
//! runtime over a bounded channel. The bound is the whole flow-control
//! story: when the consumer stops draining, the channel fills, the thread
//! stops reading, the kernel's terminal buffer fills, and the child's own
//! writes block. Backpressure ends up where it belongs — on the process
//! producing the bytes — instead of as unbounded memory here.
//!
//! Two things happen to the bytes on the way past, and both are terminal
//! behavior rather than interpretation:
//!
//! - **Chunks are cut on character boundaries.** A multi-byte character
//!   split across two reads is held until it completes, so no consumer ever
//!   receives half of one. Bytes that can never become valid are reported
//!   where they occurred and carried through — this layer never silently
//!   drops content, and never substitutes for it either, because the
//!   replacement policy and the event that records it belong upstream.
//! - **A cursor-position query is answered.** A terminal that receives
//!   `ESC[6n` is expected to reply, and a child that asked can block until
//!   it does. We are that terminal, so the reader answers.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvError, RecvTimeoutError, SyncSender};
use std::time::Duration;

use crate::backend::{InputPort, write_control};

/// How many bytes one read may return.
///
/// Larger buffers do not make a terminal deliver more: a read returns
/// whatever has arrived, and under interactive streaming that is usually one
/// token. Eight kibibytes comfortably exceeds a full-screen repaint, which
/// is the largest burst a CLI produces in normal operation.
const READ_BUFFER_BYTES: usize = 8 * 1024;

/// How many chunks may be in flight between the reader thread and its
/// consumer.
///
/// Eight buffers' worth is 64 KiB of unread output — the figure the runtime
/// budgets for a session's terminal read buffer. Past it the thread stops
/// reading rather than growing, which is the intended outcome: a CLI stalled
/// on its own output is recoverable, and a runtime that ran out of memory
/// holding it is not.
const CHANNEL_CAPACITY: usize = 8;

/// The query a child sends to ask where the cursor is.
const CURSOR_POSITION_QUERY: &[u8] = b"\x1b[6n";

/// What the reader answers with: row 1, column 1.
///
/// A fixed answer, and deliberately so. This layer holds no screen state —
/// reconstructing what a terminal would display is somebody else's job by
/// design — so the honest choice is between a constant and leaving a child
/// blocked on a query nobody will answer. Every CLI observed so far sends
/// the query to discover whether it is talking to a terminal at all and
/// discards the coordinates.
const CURSOR_POSITION_REPLY: &[u8] = b"\x1b[1;1R";

/// One piece of the child's output, in the order it was produced.
#[derive(Debug, PartialEq, Eq)]
pub enum ReadChunk {
    /// Output bytes. Never begins or ends part-way through a character, so a
    /// consumer may decode a chunk on its own.
    Output(Vec<u8>),
    /// A run of bytes that no continuation could ever make valid UTF-8.
    ///
    /// Carried rather than dropped, and located rather than merely counted,
    /// so the layer above can substitute a replacement character and record
    /// where the substitution happened. Those bytes are undecodable output
    /// of unknown provenance: they belong in a diagnosis, never in an event
    /// payload or a log line.
    Invalid {
        /// Position of the first byte, counted from the first byte the child
        /// ever wrote — so it locates the run in a recording of the session
        /// however the reads happened to fall.
        offset: u64,
        /// The bytes themselves.
        bytes: Vec<u8>,
    },
    /// The last chunk of every stream: there will be no more output.
    ///
    /// **What ends the stream is the terminal closing, not the child
    /// exiting**, and the two coincide on only one platform. A POSIX
    /// terminal reports the end as soon as the last process lets go of it,
    /// so a child that exits ends the stream. A pseudo-console on Windows
    /// keeps its output pipe open until the console itself is closed, which
    /// happens when the handle is dropped — so a caller that terminates a
    /// session and then waits for `End` without dropping the handle waits
    /// forever there.
    End(EndOfStream),
}

/// Why the child's output stopped.
#[derive(Debug)]
pub enum EndOfStream {
    /// The child closed the terminal, normally by exiting.
    Eof,
    /// The read itself failed. The stream is over either way — there is no
    /// resuming a terminal that cannot be read — but the cause survives so a
    /// session can tell a crash from a clean close.
    Failed(std::io::Error),
}

impl PartialEq for EndOfStream {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (EndOfStream::Eof, EndOfStream::Eof) => true,
            // Two failures are the same failure when the operating system
            // gives them the same classification; the message is a rendering
            // detail that varies by platform.
            (EndOfStream::Failed(left), EndOfStream::Failed(right)) => left.kind() == right.kind(),
            _ => false,
        }
    }
}

impl Eq for EndOfStream {}

/// The receiving half of the child's output.
///
/// Handed out once, at spawn, so "who reads this terminal" is answered by
/// the type rather than by a runtime check: there is exactly one of these
/// and no way to ask for a second.
///
/// It carries the child's standard output and standard error together,
/// because a terminal gives a process one device for both and the bytes are
/// interleaved by the time anything here can see them.
pub struct ReadStream {
    chunks: Receiver<ReadChunk>,
}

impl ReadStream {
    /// Block until the next chunk arrives.
    ///
    /// Fails only once the reader thread is gone, which is always *after*
    /// [`ReadChunk::End`] has been delivered — so a consumer that stops at
    /// `End` never sees this error.
    pub fn recv(&self) -> Result<ReadChunk, RecvError> {
        self.chunks.recv()
    }

    /// [`ReadStream::recv`] with a deadline, distinguishing "nothing yet"
    /// from "nothing ever again".
    pub fn recv_timeout(&self, timeout: Duration) -> Result<ReadChunk, RecvTimeoutError> {
        self.chunks.recv_timeout(timeout)
    }
}

/// Start reading `source` on its own thread.
///
/// `control` is how the reader answers a cursor-position query; it writes to
/// the same terminal the caller writes input to, which is why it is shared
/// rather than owned here.
///
/// Fails only if the thread cannot be started, which leaves a terminal
/// nobody will ever read — an allocated session that cannot work, so the
/// caller reports it as a failure to stand the terminal up rather than
/// handing back a handle that will never produce a byte.
pub(crate) fn spawn(
    source: Box<dyn Read + Send>,
    control: Arc<dyn InputPort>,
    spoken: Arc<AtomicBool>,
    thread_name: String,
) -> std::io::Result<ReadStream> {
    let (sender, chunks) = std::sync::mpsc::sync_channel(CHANNEL_CAPACITY);
    // A named thread so a stack dump or a profiler says which session's
    // output it is stuck on, rather than "thread <unnamed>".
    std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || pump(source, control.as_ref(), &spoken, &sender))?;
    Ok(ReadStream { chunks })
}

/// Read until the stream ends, forwarding chunks and answering queries.
///
/// `spoken` is raised by the first byte that arrives. It is the only
/// evidence this layer has that the child took possession of its terminal,
/// and a resize issued before it means the child may never have seen the
/// geometry.
fn pump(
    mut source: Box<dyn Read + Send>,
    control: &dyn InputPort,
    spoken: &AtomicBool,
    sender: &SyncSender<ReadChunk>,
) {
    let mut boundary = Utf8Boundary::default();
    let mut queries = CursorQueryScanner::default();
    let mut buffer = vec![0u8; READ_BUFFER_BYTES];
    let end = loop {
        match source.read(&mut buffer) {
            Ok(0) => break EndOfStream::Eof,
            Ok(read) => {
                spoken.store(true, Ordering::Relaxed);
                let bytes = &buffer[..read];
                if queries.scan(bytes) {
                    // A failed reply is not worth ending the stream over,
                    // and there is nobody here to report it to: the child
                    // either proceeds without its answer or stalls, and the
                    // stall surfaces as the caller's own timeout.
                    let _ = write_control(control, CURSOR_POSITION_REPLY);
                }
                for chunk in boundary.push(bytes) {
                    if send(sender, chunk).is_err() {
                        return; // the consumer is gone
                    }
                }
            }
            // A signal can cut a blocking read short. Nothing ended; resume.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            // Anything else is a real failure. A platform that spells the
            // end of a terminal as an error rather than as zero bytes has
            // already translated it by the time the bytes reach here — that
            // belongs with the descriptor, not with the thread reading it.
            Err(err) => break EndOfStream::Failed(err),
        }
    };
    // A character left half-finished when the stream ended can never be
    // completed now, so it is reported rather than quietly discarded.
    if let Some(truncated) = boundary.finish()
        && send(sender, truncated).is_err()
    {
        return;
    }
    let _ = send(sender, ReadChunk::End(end));
}

/// Hand one chunk to the consumer, blocking while the channel is full.
///
/// Blocking is the point: it is what stops this thread reading, which is
/// what lets the kernel apply backpressure to the child. `Err` means the
/// consumer is gone for good.
fn send(sender: &SyncSender<ReadChunk>, chunk: ReadChunk) -> Result<(), ()> {
    sender.send(chunk).map_err(|_| ())
}

/// Holds back a character split across two reads.
///
/// The state is at most three bytes — the longest incomplete prefix UTF-8
/// permits — so "buffering" here costs a fixed handful of bytes rather than
/// anything that grows with the session.
#[derive(Default)]
struct Utf8Boundary {
    /// Bytes still waiting for the rest of their character.
    carry: Vec<u8>,
    /// Stream position of `carry[0]`. Everything before it has been handed
    /// on.
    offset: u64,
}

impl Utf8Boundary {
    /// Split `chunk` into what can be handed on now, holding back an
    /// unfinished trailing character.
    fn push(&mut self, chunk: &[u8]) -> Vec<ReadChunk> {
        self.carry.extend_from_slice(chunk);
        let mut chunks = Vec::new();
        // Index into the carry of the first byte not yet accounted for.
        let mut at = 0;
        loop {
            let rest = &self.carry[at..];
            let (valid, invalid) = match std::str::from_utf8(rest) {
                Ok(_) => (rest.len(), None),
                // `error_len` distinguishes the two cases that matter:
                // `Some` is a sequence no continuation could repair, `None`
                // is one that simply has not finished arriving.
                Err(err) => (err.valid_up_to(), err.error_len()),
            };
            if valid > 0 {
                chunks.push(ReadChunk::Output(rest[..valid].to_vec()));
            }
            let Some(length) = invalid else {
                at += valid;
                break;
            };
            chunks.push(ReadChunk::Invalid {
                offset: self.offset + (at + valid) as u64,
                bytes: rest[valid..valid + length].to_vec(),
            });
            at += valid + length;
        }
        self.carry.drain(..at);
        self.offset += at as u64;
        chunks
    }

    /// End of stream: whatever is still held back will never complete.
    fn finish(&mut self) -> Option<ReadChunk> {
        if self.carry.is_empty() {
            return None;
        }
        let truncated = ReadChunk::Invalid {
            offset: self.offset,
            bytes: std::mem::take(&mut self.carry),
        };
        Some(truncated)
    }
}

/// Finds `ESC[6n` in a byte stream, including when it arrives split.
#[derive(Default)]
struct CursorQueryScanner {
    /// The tail of the previous chunk, short enough to complete a query that
    /// straddles the boundary and no longer.
    tail: Vec<u8>,
}

impl CursorQueryScanner {
    /// Whether this chunk completed at least one query.
    fn scan(&mut self, chunk: &[u8]) -> bool {
        self.tail.extend_from_slice(chunk);
        let found = self
            .tail
            .windows(CURSOR_POSITION_QUERY.len())
            .any(|window| window == CURSOR_POSITION_QUERY);
        let keep = CURSOR_POSITION_QUERY.len() - 1;
        if self.tail.len() > keep {
            self.tail.drain(..self.tail.len() - keep);
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(bytes: &[u8]) -> ReadChunk {
        ReadChunk::Output(bytes.to_vec())
    }

    /// Feed `chunks` in order and finish, returning the decoded text and
    /// every invalid run reported along the way.
    fn feed(chunks: &[&[u8]]) -> (Vec<u8>, Vec<(u64, Vec<u8>)>) {
        let mut boundary = Utf8Boundary::default();
        let mut text = Vec::new();
        let mut invalid = Vec::new();
        let mut absorb = |chunk: ReadChunk| match chunk {
            ReadChunk::Output(bytes) => text.extend_from_slice(&bytes),
            ReadChunk::Invalid { offset, bytes } => invalid.push((offset, bytes)),
            ReadChunk::End(_) => unreachable!("the boundary never ends a stream"),
        };
        for chunk in chunks {
            for produced in boundary.push(chunk) {
                absorb(produced);
            }
        }
        if let Some(truncated) = boundary.finish() {
            absorb(truncated);
        }
        (text, invalid)
    }

    #[test]
    fn suffix_carry_roundtrips_split_emoji() {
        // The acceptance bar: no offset in a mixed-width string may exist at
        // which a chunk boundary changes what the consumer receives. This is
        // every offset, not a sample.
        let full = "héllo 🌍".as_bytes();
        for at in 0..=full.len() {
            let (text, invalid) = feed(&[&full[..at], &full[at..]]);
            assert!(invalid.is_empty(), "a split at {at} invented a bad run");
            assert_eq!(text, full, "a split at {at} corrupted the output");
        }
    }

    #[test]
    fn a_chunk_never_ends_part_way_through_a_character() {
        // The promise a consumer relies on to decode a chunk on its own: the
        // two bytes of 'é' are split across reads, and neither is handed on
        // until both have arrived.
        let mut boundary = Utf8Boundary::default();
        let full = "aé".as_bytes();
        assert_eq!(boundary.push(&full[..2]), vec![output(b"a")]);
        assert_eq!(boundary.push(&full[2..]), vec![output("é".as_bytes())]);
    }

    #[test]
    fn genuinely_invalid_bytes_are_located_never_dropped() {
        // 0xFF can never appear in UTF-8 and no continuation repairs it, so
        // it is reported where it happened while its neighbours survive.
        let (text, invalid) = feed(&[b"ok", &[0xFF], b"go"]);
        assert_eq!(text, b"okgo");
        assert_eq!(invalid, vec![(2, vec![0xFF])]);
    }

    #[test]
    fn a_bad_run_is_reported_the_same_wherever_the_chunk_boundary_falls() {
        // The report is a property of the stream, not of how the reads fell.
        let payload = b"caf\xC3\xA9\xF0\x9F\xFF!";
        let expected = feed(&[payload]);
        for cut in 0..=payload.len() {
            assert_eq!(
                feed(&[&payload[..cut], &payload[cut..]]),
                expected,
                "a split at {cut} moved the report"
            );
        }
    }

    #[test]
    fn a_stream_ending_mid_character_reports_the_truncation() {
        // Held-back bytes are not an error while more may arrive, and are
        // one the moment nothing will.
        let emoji = "🌍".as_bytes();
        let mut boundary = Utf8Boundary::default();
        assert!(boundary.push(&emoji[..2]).is_empty());
        assert_eq!(
            boundary.finish(),
            Some(ReadChunk::Invalid {
                offset: 0,
                bytes: emoji[..2].to_vec(),
            })
        );
        assert_eq!(boundary.finish(), None, "there is nothing left to report");
    }

    #[test]
    fn offsets_count_the_whole_stream_not_the_current_chunk() {
        let (_, invalid) = feed(&[b"12345", &[0x80]]);
        assert_eq!(invalid, vec![(5, vec![0x80])]);
    }

    #[test]
    fn a_cursor_query_split_across_reads_is_still_found() {
        // The query is four bytes and a read can end anywhere, including
        // inside it — which is exactly when a child is left waiting.
        let mut scanner = CursorQueryScanner::default();
        assert!(!scanner.scan(b"boot \x1b["));
        assert!(scanner.scan(b"6n rest"));
    }

    #[test]
    fn output_without_a_query_is_not_answered() {
        let mut scanner = CursorQueryScanner::default();
        assert!(!scanner.scan(b"\x1b[2J\x1b[1;1Hplain output"));
    }

    #[test]
    fn the_query_scanner_holds_a_fixed_amount_of_history() {
        // It runs for the length of a session, so it must not accumulate:
        // three bytes is all a straddling query needs.
        let mut scanner = CursorQueryScanner::default();
        for _ in 0..1000 {
            scanner.scan(&[b'x'; 512]);
        }
        assert_eq!(scanner.tail.len(), CURSOR_POSITION_QUERY.len() - 1);
    }
}
