//! The per-session reader: terminal bytes in, decoded text out, under a
//! bound that stops the drain instead of growing.
//!
//! One of these runs per session. It consumes the terminal's read stream and
//! hands on three things at once: decoded text to the strip-and-match
//! pipeline, the raw bytes to the reconstructed screen when the session
//! keeps one, and typed encoding incidents to whoever publishes `pty.error`
//! events. Text moves the moment it decodes — never held for a newline or a
//! complete message, because a consumer watching a CLI think token by token
//! perceives a held token as a hang.
//!
//! The bound is the flow-control story, and its first stage lives here.
//! Decoded text waiting for a slow consumer accumulates in a buffer with a
//! hard cap, and the cap is checked **before** each read from the source: a
//! full buffer stops the drain, the terminal's own bounded channel fills,
//! the thread reading the terminal blocks, and the kernel's pipe finally
//! backpressures the child. A CLI stalled on its own output is recoverable;
//! a runtime that ran out of memory holding that output is not — so the
//! child stalling is the correct outcome, and the deliberate one.
//!
//! Nothing here is dropped on the floor, ever: every byte in is accounted
//! for as either text handed downstream or a replacement reported as an
//! incident, and [`ReaderStats`] carries the equation so tests and the SLO
//! harness can check it held.

use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use agent_bridge_pty::{EndOfStream, ReadChunk, ReadStream};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use crate::decode::{BurstCoalescer, DecodeItem, EncodingIncident, decode};

/// Default for [`ReaderConfig::buffer_bytes`] — the `[stream] buffer_bytes`
/// configuration default, and the figure the per-session memory budget
/// reserves for this buffer.
pub const DEFAULT_BUFFER_BYTES: usize = 64 * 1024;

/// How many chunks the bridge from the terminal's blocking channel to the
/// async reader may hold.
///
/// Small on purpose: the terminal's own channel already holds the 64 KiB the
/// runtime budgets for unread output, and every chunk sitting here is slack
/// *added* to that budget. Two is enough to keep the bridge thread and the
/// reader task from lock-stepping on every chunk.
const BRIDGE_CAPACITY: usize = 2;

/// How the reader is sized. Populated from configuration by the wiring
/// layer; this crate only knows the default.
#[derive(Debug, Clone)]
pub struct ReaderConfig {
    /// The buffer cap, in bytes of decoded text waiting for the consumer.
    ///
    /// A soft ceiling with a hard consequence: a chunk already read is
    /// buffered whole, so occupancy can exceed the cap by at most one
    /// chunk — and the moment it does, the reader stops reading until the
    /// buffer is empty again.
    pub buffer_bytes: usize,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            buffer_bytes: DEFAULT_BUFFER_BYTES,
        }
    }
}

/// Decoded text on its way to the strip-and-match pipeline. Chunk boundaries
/// carry no meaning — segmentation happens downstream.
pub type TextSink = mpsc::Sender<String>;

/// Raw bytes on their way to the reconstructed screen — escape sequences
/// included, because the screen needs exactly what the strip path removes.
pub type RawSink = mpsc::Sender<Vec<u8>>;

/// Typed encoding incidents on their way to becoming `pty.error` events.
/// [`EncodingIncident::to_payload`] is the mapping; publication and `seq`
/// assignment belong to the core.
pub type IncidentSink = mpsc::Sender<EncodingIncident>;

/// Where the reader's three outputs go.
///
/// The channels are the caller's, and their capacities are part of the
/// session's memory budget: the cap in [`ReaderConfig`] bounds only the
/// reader's own buffer, and under backlog each slot of the text channel can
/// carry up to a buffer's worth of coalesced text — so what the channel can
/// hold is its capacity times the cap, on top of the cap itself. Keep the
/// text channel's capacity small; slack belongs in the reader's buffer,
/// where the cap governs it.
pub struct ReaderOutputs {
    /// The decoded-text feed. The one output the reader cannot live without:
    /// when this closes, the run ends, because a session whose consumer is
    /// gone has nobody left to read for.
    pub text: TextSink,
    /// The raw tee to the screen. `None` when the session keeps no screen
    /// (effective `tui_aware` off) — the tee then costs nothing at all.
    pub vt: Option<RawSink>,
    /// The incident feed. Losing it degrades reporting, not the session.
    pub incidents: IncidentSink,
}

/// Where the reader's chunks come from.
///
/// The seam that makes the reader testable: production wraps the terminal's
/// blocking receiver ([`PtyChunkSource`]); component tests feed scripted
/// chunks. Either way the values are the terminal layer's [`ReadChunk`]s —
/// this crate consumes that contract, it does not restate it.
pub trait ChunkSource: Send {
    /// The next chunk, or `None` once the stream is over.
    ///
    /// **Must be cancel-safe.** The reader races this future against its own
    /// downstream and drops it when the other side wins; an implementation
    /// must not lose a chunk to that drop. Receiving from a channel is safe;
    /// taking a chunk out of anything before the first poll is not.
    fn next(&mut self) -> impl Future<Output = Option<ReadChunk>> + Send;
}

/// Production [`ChunkSource`]: the terminal's blocking channel, bridged onto
/// a thread so the async reader never blocks on it.
///
/// The terminal crate takes no runtime dependency — its reader thread hands
/// chunks to a `std` bounded channel — so the hop into async happens here: a
/// named thread does the blocking receive and forwards over a small bounded
/// async channel. The bound is what carries backpressure *through* the
/// bridge: reader stalls, this channel fills, the bridge thread blocks, the
/// terminal's channel fills, and the chain continues down to the child.
pub struct PtyChunkSource {
    chunks: mpsc::Receiver<ReadChunk>,
}

impl PtyChunkSource {
    /// Start the bridge thread over `output`.
    ///
    /// `thread_name` should say which session's output this is, for the same
    /// reason the terminal names its reader thread: a stack dump that says
    /// `thread <unnamed>` helps nobody.
    ///
    /// The thread lives as long as the stream: dropping this source alone
    /// does not unblock a receive from a terminal that is simply quiet, so a
    /// teardown that wants both this thread and the terminal's own reader
    /// gone closes the terminal — dropping the `Pty` handle ends the stream,
    /// and the end releases them.
    ///
    /// Fails only if the thread cannot be started — a stream nobody will
    /// ever forward, which the caller reports as a failure to stand the
    /// session up.
    pub fn spawn(output: ReadStream, thread_name: String) -> std::io::Result<Self> {
        let (sender, chunks) = mpsc::channel(BRIDGE_CAPACITY);
        std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                loop {
                    // The stream errors only after `End` has been delivered, so
                    // plain return covers both endings.
                    let Ok(chunk) = output.recv() else { return };
                    let last = matches!(chunk, ReadChunk::End(_));
                    if sender.blocking_send(chunk).is_err() || last {
                        return;
                    }
                }
            })?;
        Ok(Self { chunks })
    }
}

impl ChunkSource for PtyChunkSource {
    fn next(&mut self) -> impl Future<Output = Option<ReadChunk>> + Send {
        self.chunks.recv()
    }
}

/// What one run counted, for the performance harness and the adversarial
/// stall validation to assert against.
///
/// The load-bearing row is the equation: `bytes_in` **always** equals
/// `text_bytes_out + bytes_replaced` once a run has finished cleanly. A
/// silent drop anywhere in the path breaks it, which is the point of
/// carrying it — and it is why delivery is counted at the sink, not at the
/// decoder: a run ended by a vanished consumer reports the undelivered
/// remainder as exactly the shortfall between the equation's sides, instead
/// of claiming bytes nobody received.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReaderStats {
    /// Bytes received from the source, output and invalid runs alike.
    pub bytes_in: u64,
    /// Input bytes whose text was handed to the consumer — counted when the
    /// send succeeds, so the name means what it says.
    pub text_bytes_out: u64,
    /// Input bytes replaced with U+FFFD. The replacement characters are the
    /// markers of these bytes, so they are deliberately not counted as text
    /// out — each input byte lands in exactly one column.
    pub bytes_replaced: u64,
    /// Deliveries into the text sink. Under backlog waiting text coalesces,
    /// so this counts what the consumer saw, not what the decoder produced.
    pub chunks_out: u64,
    /// How often the full buffer stopped the drain.
    pub stall_count: u64,
    /// Total time the drain was stopped, in nanoseconds.
    pub stall_ns_total: u64,
    /// The most the buffer ever held — bounded by the cap plus one chunk.
    pub peak_buffered_bytes: u64,
}

/// Why a run ended.
#[derive(Debug, PartialEq, Eq)]
pub enum ReaderEnd {
    /// The stream ended: the terminal closed, cleanly or not. The reason is
    /// carried so a session can tell a crash from a clean close.
    Stream(EndOfStream),
    /// The text sink closed under the reader: the consumer is gone for good,
    /// and with it any reason to keep reading.
    ConsumerGone,
}

/// Everything a finished run has to say.
#[derive(Debug)]
pub struct ReaderReport {
    pub stats: ReaderStats,
    pub end: ReaderEnd,
}

/// The per-session reader task. One per session; construct with the sinks,
/// then [`run`](StreamReader::run) it over a source until the stream ends.
pub struct StreamReader {
    cfg: ReaderConfig,
    outputs: ReaderOutputs,
}

/// What one pass through the reader's loop decided.
enum Step {
    /// A waiting chunk went downstream.
    Forwarded,
    /// The text sink closed.
    ConsumerGone,
    /// The burst window's deadline passed.
    WindowClosed,
    /// The source produced something, or ended.
    Chunk(Option<ReadChunk>),
}

/// A sleep target for when there is nothing to wake up for. Never polled —
/// the branch that would is disabled — but `select!` evaluates it anyway.
const NO_DEADLINE: Duration = Duration::from_secs(60 * 60);

impl StreamReader {
    pub fn new(cfg: ReaderConfig, outputs: ReaderOutputs) -> Self {
        Self { cfg, outputs }
    }

    /// Run until the stream ends or the consumer goes away.
    ///
    /// A plain async fn over the source — the runtime only has to drive the
    /// channels and, when a burst is pending, one timer.
    pub async fn run<S: ChunkSource>(self, mut source: S) -> ReaderReport {
        let StreamReader { cfg, outputs } = self;
        let ReaderOutputs {
            text,
            vt,
            incidents,
        } = outputs;
        let mut pump = Pump {
            cfg,
            vt,
            incidents: Some(incidents),
            pending: VecDeque::new(),
            pending_bytes: 0,
            offset: 0,
            coalescer: BurstCoalescer::new(),
            stats: ReaderStats::default(),
        };

        loop {
            // Forward whatever the consumer can take right now, so a chunk
            // decoded a moment ago is already on its way — sub-line content
            // propagates immediately.
            while !pump.pending.is_empty() {
                match text.try_reserve() {
                    Ok(permit) => pump.send_front(permit),
                    Err(TrySendError::Full(())) => break,
                    Err(TrySendError::Closed(())) => return pump.report(ReaderEnd::ConsumerGone),
                }
            }

            // The stage-1 stall, checked before each receive so the stall
            // point is deterministic: over the cap, the reader forwards and
            // only forwards — the source is not read again until the buffer
            // has drained to empty.
            if pump.pending_bytes >= pump.cfg.buffer_bytes && !pump.pending.is_empty() {
                pump.stats.stall_count += 1;
                let stalled_at = tokio::time::Instant::now();
                tracing::debug!(
                    buffered = pump.pending_bytes,
                    cap = pump.cfg.buffer_bytes,
                    "stream buffer full; PTY drain stops until the consumer catches up"
                );
                while !pump.pending.is_empty() {
                    // The stall stops the *drain*, not the reader's clock: a
                    // burst window that comes due while the consumer is away
                    // still closes on time, on its own channel.
                    let deadline = pump.coalescer.deadline();
                    let wake = deadline.map_or_else(
                        || tokio::time::Instant::now() + NO_DEADLINE,
                        tokio::time::Instant::from_std,
                    );
                    tokio::select! {
                        biased;
                        permit = text.reserve() => match permit {
                            Ok(permit) => pump.send_front(permit),
                            Err(_) => return pump.report(ReaderEnd::ConsumerGone),
                        },
                        () = tokio::time::sleep_until(wake), if deadline.is_some() => {
                            let now = tokio::time::Instant::now().into_std();
                            if let Some(burst) = pump.coalescer.poll(now) {
                                pump.emit(burst).await;
                            }
                        }
                    }
                }
                let stalled_for = stalled_at.elapsed();
                pump.stats.stall_ns_total += stalled_for.as_nanos() as u64;
                tracing::debug!(
                    stalled_us = stalled_for.as_micros() as u64,
                    "PTY drain resumes"
                );
            }

            let deadline = pump.coalescer.deadline();
            let wake = deadline.map_or_else(
                || tokio::time::Instant::now() + NO_DEADLINE,
                tokio::time::Instant::from_std,
            );
            let step = tokio::select! {
                // Forwarding first: text already decoded is older than
                // anything the source still holds.
                biased;
                permit = text.reserve(), if !pump.pending.is_empty() => match permit {
                    Ok(permit) => {
                        pump.send_front(permit);
                        Step::Forwarded
                    }
                    Err(_) => Step::ConsumerGone,
                },
                () = tokio::time::sleep_until(wake), if deadline.is_some() => Step::WindowClosed,
                chunk = source.next() => Step::Chunk(chunk),
            };
            match step {
                Step::Forwarded => {}
                Step::ConsumerGone => return pump.report(ReaderEnd::ConsumerGone),
                Step::WindowClosed => {
                    let now = tokio::time::Instant::now().into_std();
                    if let Some(burst) = pump.coalescer.poll(now) {
                        pump.emit(burst).await;
                    }
                }
                Step::Chunk(Some(ReadChunk::Output(bytes))) => pump.output(bytes).await,
                Step::Chunk(Some(ReadChunk::Invalid { offset, bytes })) => {
                    pump.invalid(offset, bytes).await;
                }
                Step::Chunk(Some(ReadChunk::End(end))) => return pump.finish(&text, end).await,
                Step::Chunk(None) => return pump.finish(&text, EndOfStream::Eof).await,
            }
        }
    }
}

/// The reader's working state, separate from the sinks the loop's `select!`
/// borrows — which is what lets a branch handler mutate this while the
/// losing branches still hold those borrows.
struct Pump {
    cfg: ReaderConfig,
    vt: Option<RawSink>,
    /// `None` once the incident channel has gone away — reporting degrades,
    /// the session does not.
    incidents: Option<IncidentSink>,
    /// Decoded text waiting for the consumer, oldest first, each entry
    /// paired with how many input bytes it represents — replacement
    /// characters stand for their span, so they carry text without carrying
    /// input bytes. Consecutive arrivals coalesce into the newest entry, so
    /// a backlog of many small tokens is a few large strings rather than
    /// thousands of allocations.
    pending: VecDeque<(String, u64)>,
    pending_bytes: usize,
    /// Absolute position in the child's output, for locating invalid spans
    /// the decoder finds itself.
    offset: u64,
    coalescer: BurstCoalescer,
    stats: ReaderStats,
}

impl Pump {
    /// One output chunk: tee the raw bytes to the screen first — it needs
    /// the escape bytes the strip path will remove — then decode.
    async fn output(&mut self, bytes: Vec<u8>) {
        self.stats.bytes_in += bytes.len() as u64;
        self.tee(&bytes).await;
        for item in decode(&bytes, self.offset) {
            match item {
                DecodeItem::Text(part) => self.push_text(part, part.len() as u64),
                // The terminal layer promises its output chunks decode
                // whole, so this branch should be dead — but the decision
                // about undecodable bytes is this layer's, and it holds
                // whatever the source turns out to do.
                DecodeItem::Invalid { offset, len } => self.replace(offset, len).await,
            }
        }
        self.offset += bytes.len() as u64;
    }

    /// One pre-located invalid run: same tee, straight to the replacement
    /// policy — both detection sites converge here.
    async fn invalid(&mut self, offset: u64, bytes: Vec<u8>) {
        self.stats.bytes_in += bytes.len() as u64;
        self.tee(&bytes).await;
        self.replace(offset, bytes.len() as u32).await;
        // The source's coordinates are authoritative for the runs it
        // locates; follow them so both numbering schemes stay one scheme.
        self.offset = offset + bytes.len() as u64;
    }

    /// U+FFFD into the text feed, an incident through the coalescer.
    async fn replace(&mut self, offset: u64, len: u32) {
        self.stats.bytes_replaced += u64::from(len);
        // The replaced input bytes are accounted under `bytes_replaced` the
        // moment the decision is made; the marker character carries none.
        self.push_text("\u{FFFD}", 0);
        let now = tokio::time::Instant::now().into_std();
        for incident in self.coalescer.on_replacement(now, offset, len) {
            self.emit(incident).await;
        }
    }

    fn push_text(&mut self, part: &str, input_bytes: u64) {
        match self.pending.back_mut() {
            Some((waiting, represents)) => {
                waiting.push_str(part);
                *represents += input_bytes;
            }
            None => self.pending.push_back((part.to_owned(), input_bytes)),
        }
        self.pending_bytes += part.len();
        self.stats.peak_buffered_bytes = self
            .stats
            .peak_buffered_bytes
            .max(self.pending_bytes as u64);
    }

    fn send_front(&mut self, permit: mpsc::Permit<'_, String>) {
        let Some((front, represents)) = self.pending.pop_front() else {
            return;
        };
        self.pending_bytes -= front.len();
        // Delivery is what this counts, at the reader's honest boundary: a
        // byte's text is "out" when it has been handed to the consumer's
        // queue, not when the decoder produced it. What the consumer does
        // with its queue after that is its own account.
        self.stats.text_bytes_out += represents;
        self.stats.chunks_out += 1;
        permit.send(front);
    }

    async fn tee(&mut self, bytes: &[u8]) {
        let Some(sink) = &self.vt else { return };
        if sink.send(bytes.to_vec()).await.is_err() {
            // The screen went away mid-session. The text path is the
            // product; it keeps flowing, and the loss is on the record.
            tracing::warn!("screen feed closed; raw tee disabled for the rest of this session");
            self.vt = None;
        }
    }

    async fn emit(&mut self, incident: EncodingIncident) {
        let Some(sink) = &self.incidents else { return };
        if sink.send(incident).await.is_err() {
            tracing::warn!(
                "incident channel closed; further encoding incidents are counted but undelivered"
            );
            self.incidents = None;
        }
    }

    /// The stream is over: close out the burst window first — it cannot
    /// grow further, and its report must not wait on a slow text consumer —
    /// then hand the consumer what is still waiting, and report.
    async fn finish(mut self, text: &TextSink, end: EndOfStream) -> ReaderReport {
        let now = tokio::time::Instant::now().into_std();
        if let Some(burst) = self.coalescer.finish(now) {
            self.emit(burst).await;
        }
        while !self.pending.is_empty() {
            match text.reserve().await {
                Ok(permit) => self.send_front(permit),
                Err(_) => return self.report(ReaderEnd::ConsumerGone),
            }
        }
        self.report(ReaderEnd::Stream(end))
    }

    fn report(self, end: ReaderEnd) -> ReaderReport {
        ReaderReport {
            stats: self.stats,
            end,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted [`ChunkSource`]: the queued chunks, then either the end of
    /// the stream or a stream that stays open and silent.
    struct Feed {
        chunks: VecDeque<ReadChunk>,
        hold_open: bool,
    }

    impl Feed {
        fn ending(chunks: Vec<ReadChunk>) -> Self {
            Self {
                chunks: chunks.into(),
                hold_open: false,
            }
        }

        fn open(chunks: Vec<ReadChunk>) -> Self {
            Self {
                chunks: chunks.into(),
                hold_open: true,
            }
        }
    }

    impl ChunkSource for Feed {
        async fn next(&mut self) -> Option<ReadChunk> {
            match self.chunks.pop_front() {
                Some(chunk) => Some(chunk),
                None if self.hold_open => std::future::pending().await,
                None => None,
            }
        }
    }

    fn out(text: &str) -> ReadChunk {
        ReadChunk::Output(text.as_bytes().to_vec())
    }

    fn bad(offset: u64, bytes: &[u8]) -> ReadChunk {
        ReadChunk::Invalid {
            offset,
            bytes: bytes.to_vec(),
        }
    }

    /// A running reader and the receiving ends of its three outputs.
    struct Rig {
        text: mpsc::Receiver<String>,
        vt: mpsc::Receiver<Vec<u8>>,
        incidents: mpsc::Receiver<EncodingIncident>,
        task: tokio::task::JoinHandle<ReaderReport>,
    }

    fn rig(cfg: ReaderConfig, text_capacity: usize, with_vt: bool, feed: Feed) -> Rig {
        let (text_tx, text) = mpsc::channel(text_capacity);
        let (vt_tx, vt) = mpsc::channel(64);
        let (incident_tx, incidents) = mpsc::channel(64);
        let reader = StreamReader::new(
            cfg,
            ReaderOutputs {
                text: text_tx,
                vt: with_vt.then_some(vt_tx),
                incidents: incident_tx,
            },
        );
        Rig {
            text,
            vt,
            incidents,
            task: tokio::spawn(reader.run(feed)),
        }
    }

    /// The never-silent equation, asserted at the end of every clean run.
    fn assert_accounts(stats: &ReaderStats) {
        assert_eq!(
            stats.bytes_in,
            stats.text_bytes_out + stats.bytes_replaced,
            "bytes went missing: in != text out + replaced"
        );
    }

    async fn drain_text(rig: &mut Rig) -> String {
        let mut all = String::new();
        while let Some(chunk) = rig.text.recv().await {
            all.push_str(&chunk);
        }
        all
    }

    async fn drain_incidents(rig: &mut Rig) -> Vec<EncodingIncident> {
        let mut all = Vec::new();
        while let Some(incident) = rig.incidents.recv().await {
            all.push(incident);
        }
        all
    }

    #[tokio::test]
    async fn decoded_text_flows_and_the_accounts_balance() {
        let mut rig = rig(
            ReaderConfig::default(),
            8,
            false,
            Feed::ending(vec![out("héllo "), out("🌍")]),
        );
        assert_eq!(drain_text(&mut rig).await, "héllo 🌍");
        let report = rig.task.await.expect("the reader must not panic");
        assert_eq!(report.end, ReaderEnd::Stream(EndOfStream::Eof));
        assert_eq!(report.stats.bytes_in, "héllo 🌍".len() as u64);
        assert_accounts(&report.stats);
    }

    #[tokio::test]
    async fn a_subline_chunk_reaches_the_consumer_while_the_stream_is_still_open() {
        // No newline anywhere and the source deliberately never ends: the
        // token must arrive anyway, because waiting for completion is
        // exactly the perceived hang this reader exists to prevent.
        let mut rig = rig(
            ReaderConfig::default(),
            8,
            false,
            Feed::open(vec![out("token")]),
        );
        let first = tokio::time::timeout(Duration::from_secs(5), rig.text.recv())
            .await
            .expect("a sub-line chunk must not wait for more input");
        assert_eq!(first.as_deref(), Some("token"));
        rig.task.abort();
    }

    #[tokio::test]
    async fn an_invalid_run_becomes_fffd_and_exactly_one_located_replacement() {
        let mut rig = rig(
            ReaderConfig::default(),
            8,
            false,
            Feed::ending(vec![out("ok"), bad(2, &[0xFF]), out("go")]),
        );
        assert_eq!(drain_text(&mut rig).await, "ok\u{FFFD}go");
        assert_eq!(
            drain_incidents(&mut rig).await,
            vec![EncodingIncident::Replacement { offset: 2, len: 1 }]
        );
        let report = rig.task.await.expect("the reader must not panic");
        assert_eq!(report.stats.bytes_replaced, 1);
        assert_eq!(report.stats.text_bytes_out, 4);
        assert_accounts(&report.stats);
    }

    #[tokio::test(start_paused = true)]
    async fn four_invalid_runs_in_a_second_coalesce_into_one_burst() {
        // The stream stays open: the burst must come out when the window
        // closes, on the reader's own timer, not only when the stream ends.
        let mut rig = rig(
            ReaderConfig::default(),
            8,
            false,
            Feed::open(vec![
                bad(0, &[0xFF]),
                bad(1, &[0xFE]),
                bad(2, &[0xFD]),
                bad(3, &[0xFC]),
            ]),
        );
        let mut incidents = Vec::new();
        for _ in 0..3 {
            let incident = tokio::time::timeout(Duration::from_secs(30), rig.incidents.recv())
                .await
                .expect("the burst must fire at the window close, not at end of stream")
                .expect("the incident channel must stay open");
            incidents.push(incident);
        }
        assert_eq!(
            incidents,
            vec![
                EncodingIncident::Replacement { offset: 0, len: 1 },
                EncodingIncident::Replacement { offset: 1, len: 1 },
                EncodingIncident::Burst {
                    count: 2,
                    window_ms: 1000,
                },
            ],
            "four invalid runs are two individual reports and one burst — never four events"
        );
        rig.task.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_consumer_stops_the_drain_and_loses_nothing() {
        // A consumer that sleeps while the source keeps producing. The cap
        // is small so the corpus overruns it decisively; the reader must
        // stop draining (that is the stall), hold no more than cap plus one
        // chunk, and deliver every byte in order once the consumer wakes.
        let corpus: Vec<String> = (0..200).map(|line| format!("line-{line:03} ")).collect();
        let chunks = corpus.iter().map(|line| out(line)).collect();
        let mut rig = rig(
            ReaderConfig { buffer_bytes: 1024 },
            1,
            false,
            Feed::ending(chunks),
        );
        let collected = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            drain_text(&mut rig).await
        }
        .await;
        let report = rig.task.await.expect("the reader must not panic");
        assert_eq!(
            collected,
            corpus.concat(),
            "everything, in order, nothing lost"
        );
        assert!(
            report.stats.stall_count > 0,
            "the drain must provably have stopped: {:?}",
            report.stats
        );
        assert!(
            report.stats.peak_buffered_bytes <= 1024 + 9,
            "the buffer must stay at the cap plus at most one chunk, held {}",
            report.stats.peak_buffered_bytes
        );
        assert!(report.stats.stall_ns_total > 0);
        assert_accounts(&report.stats);
    }

    #[tokio::test]
    async fn the_raw_tee_carries_escape_bytes_and_invalid_runs_alike() {
        // The screen needs exactly what the strip path will remove — and
        // what the decoder will replace. The tee is the bytes as they came.
        let mut rig = rig(
            ReaderConfig::default(),
            8,
            true,
            Feed::ending(vec![out("a\x1b[31mred"), bad(9, &[0xFF]), out("z")]),
        );
        assert_eq!(drain_text(&mut rig).await, "a\x1b[31mred\u{FFFD}z");
        let mut raw = Vec::new();
        while let Some(bytes) = rig.vt.recv().await {
            raw.extend(bytes);
        }
        assert_eq!(raw, b"a\x1b[31mred\xFFz".to_vec());
        let report = rig.task.await.expect("the reader must not panic");
        assert_eq!(
            report.stats.bytes_in,
            raw.len() as u64,
            "the tee saw every byte"
        );
        assert_accounts(&report.stats);
    }

    #[tokio::test]
    async fn without_a_screen_the_tee_costs_nothing_and_text_is_unaffected() {
        let mut rig = rig(
            ReaderConfig::default(),
            8,
            false,
            Feed::ending(vec![out("a\x1b[31mred"), out("z")]),
        );
        assert_eq!(drain_text(&mut rig).await, "a\x1b[31mredz");
        assert!(rig.vt.recv().await.is_none(), "no screen, no tee");
        assert_accounts(&rig.task.await.expect("the reader must not panic").stats);
    }

    #[tokio::test]
    async fn a_vanished_consumer_ends_the_run() {
        let mut rig = rig(
            ReaderConfig::default(),
            1,
            false,
            Feed::open(vec![out("a"), out("b"), out("c")]),
        );
        rig.text.close();
        drop(rig.text);
        let report = rig.task.await.expect("the reader must not panic");
        assert_eq!(report.end, ReaderEnd::ConsumerGone);
        // The undelivered text is a visible shortfall, not claimed output:
        // the equation's sides no longer meet, which is the honest report
        // for a run whose consumer vanished under it.
        assert_eq!(report.stats.text_bytes_out, 0, "nobody received anything");
        assert!(
            report.stats.bytes_in > report.stats.text_bytes_out + report.stats.bytes_replaced,
            "what was read but never delivered must show as the gap"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pending_burst_fires_on_time_even_while_the_drain_is_stalled() {
        // Four invalid runs open a pending burst; the flood behind them
        // fills the buffer past its cap while the consumer sleeps five
        // seconds. The burst window closes after one second: the incident
        // must arrive then, on the reader's own timer and its own channel —
        // not whenever the unrelated text consumer happens to return.
        let started = tokio::time::Instant::now();
        let mut chunks = vec![
            bad(0, &[0xFF]),
            bad(1, &[0xFE]),
            bad(2, &[0xFD]),
            bad(3, &[0xFC]),
        ];
        let flood = "x".repeat(64);
        chunks.extend((0..8).map(|_| out(&flood)));
        let rig = rig(
            ReaderConfig { buffer_bytes: 256 },
            1,
            false,
            Feed::ending(chunks),
        );
        let Rig {
            mut text,
            mut incidents,
            task,
            vt: _,
        } = rig;
        let collector = async {
            let mut seen = Vec::new();
            while let Some(incident) = incidents.recv().await {
                seen.push((incident, started.elapsed()));
            }
            seen
        };
        let consumer = async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let mut all = String::new();
            while let Some(chunk) = text.recv().await {
                all.push_str(&chunk);
            }
            all
        };
        let (seen, collected) = tokio::join!(collector, consumer);
        let report = task.await.expect("the reader must not panic");

        let bursts: Vec<(EncodingIncident, Duration)> = seen
            .iter()
            .filter(|(incident, _)| matches!(incident, EncodingIncident::Burst { .. }))
            .cloned()
            .collect();
        let [(burst, at)] = bursts.as_slice() else {
            panic!("expected exactly one burst, got {seen:?}");
        };
        assert_eq!(
            *burst,
            EncodingIncident::Burst {
                count: 2,
                window_ms: 1000,
            }
        );
        assert!(
            *at >= Duration::from_secs(1) && *at < Duration::from_secs(5),
            "the burst must fire at the window close, mid-stall — it arrived at {at:?}"
        );
        assert!(
            report.stats.stall_count > 0,
            "the premise is a stalled drain"
        );
        assert_eq!(collected.matches('x').count(), 512);
        assert_accounts(&report.stats);
    }

    #[tokio::test(start_paused = true)]
    async fn end_of_stream_flushes_a_pending_burst_before_the_text_drain() {
        // The stream ends with a burst pending, undelivered text waiting,
        // and the text channel full under a consumer that sleeps five
        // seconds. A window that cannot grow further must be reported at the
        // end of the stream, not after whatever the text drain has to wait
        // out.
        let started = tokio::time::Instant::now();
        let rig = rig(
            ReaderConfig::default(),
            1,
            false,
            Feed::ending(vec![
                bad(0, &[0xFF]),
                bad(1, &[0xFE]),
                bad(2, &[0xFD]),
                bad(3, &[0xFC]),
                out("tail"),
            ]),
        );
        let Rig {
            mut text,
            mut incidents,
            task,
            vt: _,
        } = rig;
        let collector = async {
            let mut seen = Vec::new();
            while let Some(incident) = incidents.recv().await {
                seen.push((incident, started.elapsed()));
            }
            seen
        };
        let consumer = async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let mut all = String::new();
            while let Some(chunk) = text.recv().await {
                all.push_str(&chunk);
            }
            all
        };
        let (seen, collected) = tokio::join!(collector, consumer);
        let report = task.await.expect("the reader must not panic");

        let Some((EncodingIncident::Burst { count, .. }, at)) = seen.last() else {
            panic!("the burst must be the last incident, got {seen:?}");
        };
        assert_eq!(*count, 2);
        assert!(
            *at < Duration::from_secs(5),
            "the burst must not wait out the text drain — it arrived at {at:?}"
        );
        assert_eq!(collected, "\u{FFFD}\u{FFFD}\u{FFFD}\u{FFFD}tail");
        assert_eq!(report.end, ReaderEnd::Stream(EndOfStream::Eof));
        assert_accounts(&report.stats);
    }

    #[tokio::test]
    async fn the_end_reason_survives_to_the_report() {
        let failed = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "terminal detached");
        let mut rig = rig(
            ReaderConfig::default(),
            8,
            false,
            Feed::ending(vec![
                out("tail"),
                ReadChunk::End(EndOfStream::Failed(failed)),
            ]),
        );
        assert_eq!(drain_text(&mut rig).await, "tail");
        let report = rig.task.await.expect("the reader must not panic");
        assert_eq!(
            report.end,
            ReaderEnd::Stream(EndOfStream::Failed(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "same kind, any message"
            )))
        );
    }

    #[tokio::test]
    async fn whole_character_splits_decode_intact_at_every_cut() {
        // The contract chunks arrive under: cut anywhere, but only at
        // character boundaries. Every such cut of a mixed-width corpus must
        // decode identically — and never invent an incident. Byte-level
        // splits live below this layer and are exercised against a real
        // terminal in the integration suite.
        let corpus = "héllo 🌍 — ascii, 2-byte é, 3-byte —, 4-byte 🌍";
        for (at, _) in corpus.char_indices().skip(1) {
            let (left, right) = corpus.split_at(at);
            let mut rig = rig(
                ReaderConfig::default(),
                8,
                false,
                Feed::ending(vec![out(left), out(right)]),
            );
            assert_eq!(
                drain_text(&mut rig).await,
                corpus,
                "a cut at {at} corrupted the text"
            );
            assert!(
                drain_incidents(&mut rig).await.is_empty(),
                "a cut at {at} invented an incident"
            );
            assert_accounts(&rig.task.await.expect("the reader must not panic").stats);
        }
    }
}
