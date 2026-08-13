//! The per-session bounded ring: how much history backfill can still
//! replay.
//!
//! Bounded twice — an event count and a wall-clock age, whichever limit
//! hits first triggering FIFO eviction by `seq` — because each bound fails
//! alone: a count alone lets a quiet session pin arbitrarily old history,
//! and an age alone lets a bursty one hold an unbounded number of events
//! for five minutes. Eviction never signals anyone. A subscriber discovers
//! evicted history the honest way, through the gap shape of its own
//! backfill request, which is the [`replay`](super::replay) module's side
//! of the contract; gap-free `seq` is a promise about *generation*, never
//! about availability.
//!
//! What is deliberately *not* bounded here is bytes. The ~2.5 MiB
//! per-session budget row is instrumentation ([`RingStats`]) feeding the
//! soak assertions, not an eviction policy — so a single oversized event
//! (the transport's frame cap is ~6× this ring's whole budget) sits in the
//! ring and blows the budget visibly instead of silently truncating
//! history. That is the accepted frame-cap-vs-ring-budget tension,
//! encoded rather than fought.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_bridge_events::{Event, EventKind};

/// Bounds for each session's replay ring.
#[derive(Debug, Clone)]
pub struct RingConfig {
    /// Most events one session's ring holds — `stream.ring_max_events` in
    /// the deployment config. 0 disables retention entirely: every backfill
    /// request older than head reports a gap.
    pub max_events: usize,
    /// Longest an event stays replayable — `stream.ring_max_seconds` in the
    /// deployment config. Ages are read off the monotonic clock, not the
    /// wall clock the contract prose names: a wall clock stepped backward
    /// by NTP would keep stale entries alive (or evict fresh ones), and the
    /// contract's observable behavior — "at most the last five minutes" —
    /// is identical either way.
    pub max_age: Duration,
}

impl Default for RingConfig {
    fn default() -> Self {
        // The documented `[stream]` deployment defaults. Fixed here so the
        // binary's TOML loader becomes a field-for-field mapping, not a
        // second place deciding values.
        Self {
            max_events: 10_000,
            max_age: Duration::from_secs(300),
        }
    }
}

/// A point-in-time account of one session's ring, from
/// [`EventBus::ring_stats`](super::EventBus::ring_stats).
///
/// Instrumentation, not policy: nothing evicts on any of these numbers.
/// `approx_bytes` exists so the ~2.5 MiB per-session ring budget is a
/// measurement the soak harness can assert, and so an oversized event
/// blowing that budget is visible while it is resident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RingStats {
    /// Events currently held.
    pub events: usize,
    /// Estimated resident cost of the held events: struct size plus the
    /// text they carry. An estimate — container capacity, allocator slack,
    /// and the free-form detail maps on error and notice payloads are not
    /// walked — but one whose error is bounded and boring, which is all a
    /// budget assertion needs.
    pub approx_bytes: usize,
    /// The oldest `seq` still replayable; `None` when the ring is empty.
    pub earliest_seq: Option<u64>,
    /// The session's next `seq` to be stamped — one past the newest event
    /// that exists anywhere, in or out of the ring.
    pub head_seq: u64,
}

/// One session's bounded history, owned by its channel and touched only
/// inside the channel's critical sections.
#[derive(Debug)]
pub(crate) struct Ring {
    config: RingConfig,
    entries: VecDeque<RingEntry>,
    approx_bytes: usize,
}

/// Crate-visible only so [`Ring::clear`] can hand the storage across the
/// module boundary for an out-of-lock drop; nothing outside reads it.
#[derive(Debug)]
pub(crate) struct RingEntry {
    event: Arc<Event>,
    /// When this entry was published, on the monotonic clock — the reading
    /// the age bound is evaluated against.
    inserted_at: Instant,
    /// Remembered rather than recomputed at eviction, so the running total
    /// never drifts from what was added.
    approx_bytes: usize,
}

impl Ring {
    pub(crate) fn new(config: RingConfig) -> Self {
        Self {
            config,
            entries: VecDeque::new(),
            approx_bytes: 0,
        }
    }

    /// Admit one just-stamped event and evict whatever the bounds no longer
    /// cover — FIFO by `seq`, whichever bound hits first.
    ///
    /// `now` is passed in rather than read here so the age bound is
    /// evaluated against the same instant the publish path stamped into
    /// `monotonic_ns` — and so tests can age the ring without sleeping.
    /// Eviction runs only on insert: a session that stops publishing keeps
    /// entries past `max_age` until the next publish or seal. That idle
    /// tail is bounded by what the count bound already admitted, and a
    /// reader handed a stale-but-held event got *more* history than the
    /// contract promised, not less.
    pub(crate) fn push(&mut self, event: Arc<Event>, now: Instant) {
        let approx_bytes = approx_event_bytes(&event);
        self.approx_bytes += approx_bytes;
        self.entries.push_back(RingEntry {
            event,
            inserted_at: now,
            approx_bytes,
        });
        while self.entries.len() > self.config.max_events
            || self
                .entries
                .front()
                .is_some_and(|oldest| now.duration_since(oldest.inserted_at) > self.config.max_age)
        {
            let evicted = self
                .entries
                .pop_front()
                .expect("the eviction conditions hold only for a non-empty ring");
            self.approx_bytes -= evicted.approx_bytes;
        }
    }

    /// The held events from `from_seq` to the newest, oldest first — or
    /// `None` when `from_seq` has already been evicted.
    ///
    /// The empty ring answers `None` for every request: with nothing held
    /// it cannot vouch for any position, and whether that means a gap or
    /// simply "nothing missed" is the caller's to decide against head —
    /// only the caller knows whether anything was ever published.
    pub(crate) fn entries_from(&self, from_seq: u64) -> Option<impl Iterator<Item = &Arc<Event>>> {
        let earliest = self.earliest_seq()?;
        if from_seq < earliest {
            return None;
        }
        // Ring seqs are contiguous — every stamped event is pushed in the
        // same critical section that stamped it, and eviction only takes
        // from the front — so the position of `from_seq` is index
        // arithmetic, not a search. In bounds because the one caller
        // holds `from_seq ≤ head` and `head − earliest` is exactly the
        // ring's length.
        let skip = usize::try_from(from_seq - earliest).expect("a ring offset fits usize");
        Some(self.entries.range(skip..).map(|entry| &entry.event))
    }

    /// The oldest `seq` still held; `None` when the ring is empty.
    pub(crate) fn earliest_seq(&self) -> Option<u64> {
        self.entries.front().map(|entry| entry.event.seq)
    }

    /// Drop every entry, handing the storage back so the caller can release
    /// it outside the critical section — eviction of last resort for the
    /// seal path, where history has no reader left to serve.
    pub(crate) fn clear(&mut self) -> VecDeque<RingEntry> {
        self.approx_bytes = 0;
        std::mem::take(&mut self.entries)
    }

    /// The instrumentation snapshot; `head_seq` belongs to the channel's
    /// counter, so the caller supplies it.
    pub(crate) fn stats(&self, head_seq: u64) -> RingStats {
        RingStats {
            events: self.entries.len(),
            approx_bytes: self.approx_bytes,
            earliest_seq: self.earliest_seq(),
            head_seq,
        }
    }
}

/// Estimate one event's resident cost: the struct itself plus the heap the
/// text-carrying payloads own.
///
/// A heuristic, chosen over serializing for its length because that would
/// put a full payload walk on every publish to feed a number nothing
/// enforces on. The text fields dominate what varies — an event is a fixed
/// struct plus its strings — so counting them keeps the estimate honest for
/// exactly the payloads that can grow. Left uncounted: the free-form JSON
/// maps (error/notice `detail`, unknown payloads), whose walk *is* the
/// serialization cost this heuristic exists to avoid, and the two
/// subscription-scoped `session.reconnect*` payloads, which the emit
/// manifest keeps off the publish path entirely.
fn approx_event_bytes(event: &Event) -> usize {
    fn opt_len(text: Option<&String>) -> usize {
        text.map_or(0, String::len)
    }
    let envelope_text = opt_len(event.session_id.as_ref())
        + event.ts.len()
        + opt_len(event.approval_id.as_ref())
        + opt_len(event.correlation_id.as_ref());
    // Exhaustive on purpose: a new taxonomy variant must decide its heap
    // account here rather than inherit an invisible zero from a wildcard.
    let payload_text = match &event.kind {
        EventKind::StreamToken(payload) => opt_len(payload.source.as_ref()) + payload.content.len(),
        EventKind::StreamStderr(payload) => payload.content.len(),
        EventKind::StreamUnrecognizedOutput(payload) => payload.content.len(),
        EventKind::PromptApprovalRequired(payload) => {
            payload.prompt.len()
                + opt_len(payload.tool.as_ref())
                + payload
                    .options
                    .iter()
                    .flatten()
                    .map(String::len)
                    .sum::<usize>()
        }
        EventKind::ToolCallStarted(payload) => {
            payload.call_id.len() + payload.tool.len() + opt_len(payload.command.as_ref())
        }
        EventKind::ToolCallCompleted(payload) => payload.call_id.len(),
        EventKind::ToolCallFailed(payload) => payload.call_id.len() + payload.reason.len(),
        EventKind::ToolResult(payload) => payload.call_id.len() + payload.content.len(),
        EventKind::RuntimeError(payload) => payload.message.len(),
        EventKind::TransportError(payload) => payload.message.len(),
        EventKind::PtyError(payload) => payload.message.len(),
        EventKind::AdapterError(payload) => payload.message.len(),
        EventKind::RuntimeNotice(payload) => {
            payload.notification_type.len() + opt_len(payload.message.as_ref())
        }
        EventKind::RuntimeHealthChanged(payload) => opt_len(payload.reason.as_ref()),
        EventKind::AdapterVersionWarning(payload) => {
            opt_len(payload.adapter.as_ref())
                + opt_len(payload.detected_version.as_ref())
                + opt_len(payload.supported_range.as_ref())
        }
        EventKind::LifecycleSessionCreated(payload) => opt_len(payload.adapter.as_ref()),
        EventKind::SessionReconnecting(payload) => payload.subscriber.len(),
        EventKind::SessionWriterChanged(payload) => {
            opt_len(payload.writer.as_ref()) + opt_len(payload.previous_writer.as_ref())
        }
        EventKind::Unknown(payload) => payload.event_type.len(),
        EventKind::SessionReconnected(_)
        | EventKind::LifecycleSessionLaunching(_)
        | EventKind::LifecycleSessionConnecting(_)
        | EventKind::LifecycleSessionRunning(_)
        | EventKind::LifecycleSessionAwaitingApproval(_)
        | EventKind::LifecycleSessionInterrupted(_)
        | EventKind::LifecycleSessionClosing(_)
        | EventKind::LifecycleSessionClosed(_)
        | EventKind::LifecycleSessionCompacting(_)
        | EventKind::LifecycleTurnStarted(_)
        | EventKind::LifecycleTurnCompleted(_)
        | EventKind::RuntimeIdleTooLong(_) => 0,
    };
    size_of::<Event>() + envelope_text + payload_text
}

#[cfg(test)]
mod tests {
    use agent_bridge_events::{EventBody, SCHEMA_VERSION, StreamToken};

    use super::*;

    /// A stamped event the way the publish path would build it, without a
    /// bus: these tests drive the ring directly because the age bound needs
    /// instants no real clock will produce without sleeping.
    fn event(seq: u64) -> Arc<Event> {
        let body = EventBody::new(agent_bridge_events::EventKind::StreamToken(StreamToken {
            source: None,
            content: "x".into(),
        }));
        Arc::new(Event {
            schema_version: SCHEMA_VERSION,
            session_id: Some("s".into()),
            seq,
            monotonic_ns: None,
            ts: "2026-08-13T00:00:00.000Z".into(),
            approval_id: body.approval_id,
            correlation_id: body.correlation_id,
            kind: body.kind,
        })
    }

    fn config(max_events: usize, max_age_secs: u64) -> RingConfig {
        RingConfig {
            max_events,
            max_age: Duration::from_secs(max_age_secs),
        }
    }

    #[test]
    fn age_bound_evicts_with_mock_clock() {
        let mut ring = Ring::new(config(10_000, 300));
        let epoch = Instant::now();
        // Sparse events, far below the count bound: one per 200 s. Each
        // push must age out exactly the entries older than 300 s at that
        // push's instant — the age bound acting alone.
        for (seq, at_secs) in [(0, 0), (1, 200), (2, 400), (3, 600)] {
            ring.push(event(seq), epoch + Duration::from_secs(at_secs));
        }
        // At t=600: seq 0 (age 600) and seq 1 (age 400) are out; seq 2
        // (age 200) and seq 3 (age 0) survive.
        assert_eq!(ring.earliest_seq(), Some(2));
        assert_eq!(ring.stats(4).events, 2);
    }

    #[test]
    fn age_bound_is_inclusive_at_exactly_max_age() {
        let mut ring = Ring::new(config(10_000, 300));
        let epoch = Instant::now();
        ring.push(event(0), epoch);
        // "At most the last 300 s" keeps an entry exactly 300 s old; one
        // second past that, the next push evicts it.
        ring.push(event(1), epoch + Duration::from_secs(300));
        assert_eq!(ring.earliest_seq(), Some(0));
        ring.push(event(2), epoch + Duration::from_secs(301));
        assert_eq!(ring.earliest_seq(), Some(1));
    }

    #[test]
    fn count_bound_evicts_independently_of_age() {
        let mut ring = Ring::new(config(3, 300));
        let now = Instant::now();
        for seq in 0..5 {
            ring.push(event(seq), now);
        }
        assert_eq!(ring.earliest_seq(), Some(2));
        assert_eq!(ring.stats(5).events, 3);
    }

    #[test]
    fn zero_max_events_retains_nothing() {
        let mut ring = Ring::new(config(0, 300));
        ring.push(event(0), Instant::now());
        assert_eq!(ring.earliest_seq(), None);
        assert!(ring.entries_from(0).is_none());
    }

    #[test]
    fn entries_from_walks_to_the_newest_or_reports_eviction() {
        let mut ring = Ring::new(config(3, 300));
        let now = Instant::now();
        for seq in 0..5 {
            ring.push(event(seq), now);
        }
        // Held: 2, 3, 4. A request inside the ring gets the tail from its
        // position; a request at an evicted position gets None, never a
        // silently shortened slice.
        let seqs: Vec<u64> = ring
            .entries_from(3)
            .expect("seq 3 is held")
            .map(|event| event.seq)
            .collect();
        assert_eq!(seqs, [3, 4]);
        assert!(ring.entries_from(1).is_none());
    }

    #[test]
    fn byte_accounting_tracks_evictions_and_clear() {
        let mut ring = Ring::new(config(2, 300));
        let now = Instant::now();
        ring.push(event(0), now);
        let one = ring.stats(1).approx_bytes;
        assert!(one > 0, "an event has a nonzero estimated cost");
        ring.push(event(1), now);
        ring.push(event(2), now);
        // Same-shaped events: after evicting down to two, the total is
        // exactly two of them — eviction gave back what insertion added.
        assert_eq!(ring.stats(3).approx_bytes, one * 2);
        drop(ring.clear());
        assert_eq!(ring.stats(3).approx_bytes, 0);
        assert_eq!(ring.stats(3).events, 0);
    }
}
