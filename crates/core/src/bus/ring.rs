//! The per-session bounded ring: how much history backfill can still
//! replay.
//!
//! Bounded twice — an event count and an age, whichever limit hits first
//! triggering FIFO eviction by `seq` — because each bound fails alone: a
//! count alone lets a quiet session pin arbitrarily old history, and an
//! age alone lets a bursty one hold an unbounded number of events for five
//! minutes. Eviction never signals anyone. A subscriber discovers evicted
//! history the honest way, through the gap shape of its own backfill
//! request, which is the [`replay`](super::replay) module's side of the
//! contract; gap-free `seq` is a promise about *generation*, never about
//! availability.
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
    /// How old an entry may grow before an eviction pass removes it —
    /// `stream.ring_max_seconds` in the deployment config. Ages are read
    /// off the monotonic clock, not the wall clock the contract prose
    /// names: a wall clock stepped backward by NTP would keep stale
    /// entries alive (or evict fresh ones), and the contract's observable
    /// behavior — "at most the last five minutes" — is identical either
    /// way. Passes are push-driven, so an idle session's tail can outlive
    /// this bound until the next publish or seal.
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

impl RingConfig {
    /// The retention-free configuration the global channel carries: it has
    /// no backfill surface, so its ring exists only to keep every channel
    /// the same shape, at the cost of one branch per publish.
    pub(crate) fn disabled() -> Self {
        Self {
            max_events: 0,
            max_age: Duration::ZERO,
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
    /// text and free-form JSON they carry. An estimate — container
    /// capacity and allocator slack are not modeled — but one whose error
    /// is bounded and boring, which is all a budget assertion needs.
    pub approx_bytes: usize,
    /// The oldest `seq` still replayable; `None` when the ring is empty.
    pub earliest_seq: Option<u64>,
    /// The session's next `seq` to be stamped — one past the newest event
    /// that exists anywhere, in or out of the ring.
    pub head_seq: u64,
}

/// What one push displaced, handed back for release outside the caller's
/// critical section: the count bound's single entry and the age bound's
/// batch. The common case is both empty; the count case is one moved
/// `Arc`; only an age batch allocates.
#[derive(Debug, Default)]
#[must_use = "drop the evicted events outside the critical section"]
pub(crate) struct Evicted {
    // Owned for dropping, never read outside tests — the fields exist to
    // carry the displaced events until the caller releases them beyond
    // its critical section. `allow` rather than `expect`, because the
    // unit tests do read them and would leave an `expect` unfulfilled.
    #[allow(
        dead_code,
        reason = "owned so the caller's drop frees it outside the lock"
    )]
    by_count: Option<Arc<Event>>,
    #[allow(
        dead_code,
        reason = "owned so the caller's drop frees it outside the lock"
    )]
    by_age: Vec<Arc<Event>>,
}

#[cfg(test)]
impl Evicted {
    fn is_empty(&self) -> bool {
        self.by_count.is_none() && self.by_age.is_empty()
    }
}

/// One session's bounded history, owned by its channel and touched only
/// inside the channel's critical sections.
#[derive(Debug)]
pub(crate) struct Ring {
    config: RingConfig,
    entries: VecDeque<RingEntry>,
    approx_bytes: usize,
}

#[derive(Debug)]
struct RingEntry {
    event: Arc<Event>,
    /// When this entry was published, on the monotonic clock — the reading
    /// the age bound is evaluated against.
    inserted_at: Instant,
    /// Priced once at insert and remembered, which makes the running total
    /// equal the sum of the held memos by construction — robust to any
    /// future change of the estimator — and spares eviction the re-walk.
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
    ///
    /// Everything a push displaces comes back to the caller instead of
    /// dropping here — the same discipline the seal path applies. The age
    /// batch because the first publish after an idle spell can age out
    /// most of the ring at once; the count bound's single entry because
    /// even one destructor is unbounded in principle — an event may own a
    /// frame-sized payload or a detail map of many allocations — and a
    /// free of unknowable size belongs outside the caller's critical
    /// section. The return is allocation-free unless something aged out.
    pub(crate) fn push(&mut self, event: &Arc<Event>, now: Instant) -> Evicted {
        if self.config.max_events == 0 {
            // Disabled retention holds nothing, so it prices nothing: the
            // estimator walk, the clone, and the push/evict round trip
            // would be pure per-publish waste.
            return Evicted::default();
        }
        let approx_bytes = approx_event_bytes(event);
        self.approx_bytes += approx_bytes;
        self.entries.push_back(RingEntry {
            event: Arc::clone(event),
            inserted_at: now,
            approx_bytes,
        });
        let by_count = if self.entries.len() > self.config.max_events {
            let evicted = self
                .entries
                .pop_front()
                .expect("the ring is non-empty: an entry was just pushed");
            self.approx_bytes -= evicted.approx_bytes;
            Some(evicted.event)
        } else {
            None
        };
        let stale = self
            .entries
            .iter()
            .take_while(|entry| now.duration_since(entry.inserted_at) > self.config.max_age)
            .count();
        let by_age = if stale == 0 {
            Vec::new()
        } else {
            let freed: usize = self
                .entries
                .iter()
                .take(stale)
                .map(|entry| entry.approx_bytes)
                .sum();
            self.approx_bytes -= freed;
            self.entries
                .drain(..stale)
                .map(|entry| entry.event)
                .collect()
        };
        Evicted { by_count, by_age }
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

    /// Hand back everything held as a value to drop outside the critical
    /// section, leaving this ring empty — eviction of last resort for the
    /// seal path, where history has no reader left to serve.
    #[must_use = "drop the drained ring outside the critical section"]
    pub(crate) fn drain(&mut self) -> Self {
        Self {
            config: self.config.clone(),
            entries: std::mem::take(&mut self.entries),
            approx_bytes: std::mem::take(&mut self.approx_bytes),
        }
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
/// struct plus its strings — so counting them keeps the estimate honest
/// for exactly the payloads that can grow. The free-form JSON maps
/// (error/notice `detail`, unknown payloads) are walked too: they are the
/// only unbounded non-text containers an event can own, and they ride
/// only rare variants, so pricing them costs the token hot path nothing.
/// Left uncounted: `session.reconnected`'s embedded replay payload — the
/// emit manifest keeps the reconnect notifications off the publish path,
/// and that convention is trusted rather than enforced here, so a
/// mistaken publish of one would sit in the ring underpriced by whatever
/// its screen snapshot weighs.
fn approx_event_bytes(event: &Event) -> usize {
    fn opt_len(text: Option<&String>) -> usize {
        text.map_or(0, String::len)
    }
    let envelope_text = opt_len(event.session_id.as_ref())
        + event.ts.len()
        + opt_len(event.approval_id.as_ref())
        + opt_len(event.correlation_id.as_ref());
    use agent_bridge_events::{
        AdapterErrorPayload, AdapterVersionWarning, LifecycleSessionAwaitingApproval,
        LifecycleSessionClosed, LifecycleSessionClosing, LifecycleSessionCompacting,
        LifecycleSessionConnecting, LifecycleSessionCreated, LifecycleSessionInterrupted,
        LifecycleSessionLaunching, LifecycleSessionRunning, LifecycleTurnCompleted,
        LifecycleTurnStarted, PromptApprovalRequired, PtyErrorPayload, RuntimeErrorPayload,
        RuntimeHealthChanged, RuntimeIdleTooLong, RuntimeNotice, SessionReconnected,
        SessionReconnecting, SessionWriterChanged, StreamStderr, StreamToken,
        StreamUnrecognizedOutput, ToolCallCompleted, ToolCallFailed, ToolCallStarted, ToolResult,
        TransportErrorPayload, UnknownEvent,
    };
    // Exhaustive down to the fields on purpose: a new taxonomy variant AND
    // a new field on an existing payload must both decide their heap
    // account here, rather than inherit an invisible zero from a wildcard
    // or a whole-payload binding. The one exception is forced:
    // `PromptApprovalRequired` is `#[non_exhaustive]`, so `..` is
    // required — but its construction is sealed to one function in the
    // events crate, which is where a new field would land in review.
    let payload_text = match &event.kind {
        EventKind::StreamToken(StreamToken { source, content }) => {
            opt_len(source.as_ref()) + content.len()
        }
        EventKind::StreamStderr(StreamStderr { content }) => content.len(),
        EventKind::StreamUnrecognizedOutput(StreamUnrecognizedOutput { content }) => content.len(),
        EventKind::PromptApprovalRequired(PromptApprovalRequired {
            prompt,
            tool,
            options,
            ..
        }) => {
            prompt.len()
                + opt_len(tool.as_ref())
                + options.iter().flatten().map(String::len).sum::<usize>()
        }
        EventKind::ToolCallStarted(ToolCallStarted {
            call_id,
            tool,
            command,
        }) => call_id.len() + tool.len() + opt_len(command.as_ref()),
        EventKind::ToolCallCompleted(ToolCallCompleted {
            call_id,
            exit_code: _,
            duration_ms: _,
        }) => call_id.len(),
        EventKind::ToolCallFailed(ToolCallFailed { call_id, reason }) => {
            call_id.len() + reason.len()
        }
        EventKind::ToolResult(ToolResult { call_id, content }) => call_id.len() + content.len(),
        EventKind::RuntimeError(RuntimeErrorPayload {
            code: _,
            message,
            detail,
        }) => message.len() + approx_json_map_bytes(detail),
        EventKind::TransportError(TransportErrorPayload {
            code: _,
            message,
            detail,
        }) => message.len() + approx_json_map_bytes(detail),
        EventKind::PtyError(PtyErrorPayload {
            code: _,
            message,
            detail,
        }) => message.len() + approx_json_map_bytes(detail),
        EventKind::AdapterError(AdapterErrorPayload {
            code: _,
            message,
            detail,
        }) => message.len() + approx_json_map_bytes(detail),
        EventKind::RuntimeNotice(RuntimeNotice {
            notification_type,
            message,
            detail,
        }) => notification_type.len() + opt_len(message.as_ref()) + approx_json_map_bytes(detail),
        EventKind::RuntimeHealthChanged(RuntimeHealthChanged {
            status: _,
            previous: _,
            reason,
        }) => opt_len(reason.as_ref()),
        EventKind::AdapterVersionWarning(AdapterVersionWarning {
            adapter,
            detected_version,
            supported_range,
        }) => {
            opt_len(adapter.as_ref())
                + opt_len(detected_version.as_ref())
                + opt_len(supported_range.as_ref())
        }
        EventKind::LifecycleSessionCreated(LifecycleSessionCreated { adapter }) => {
            opt_len(adapter.as_ref())
        }
        EventKind::LifecycleSessionClosed(LifecycleSessionClosed {
            exit_code: _,
            duration_ms: _,
            bytes_read: _,
            bytes_written: _,
            drained: _,
        }) => 0,
        EventKind::RuntimeIdleTooLong(RuntimeIdleTooLong {
            idle_ms: _,
            threshold_ms: _,
        }) => 0,
        EventKind::SessionReconnecting(SessionReconnecting {
            from_seq: _,
            subscriber,
        }) => subscriber.len(),
        EventKind::SessionReconnected(SessionReconnected { replay: _ }) => 0,
        EventKind::SessionWriterChanged(SessionWriterChanged {
            writer,
            previous_writer,
            reason: _,
        }) => opt_len(writer.as_ref()) + opt_len(previous_writer.as_ref()),
        EventKind::Unknown(UnknownEvent {
            event_type,
            payload,
        }) => event_type.len() + approx_json_map_bytes(payload),
        EventKind::LifecycleSessionLaunching(LifecycleSessionLaunching {})
        | EventKind::LifecycleSessionConnecting(LifecycleSessionConnecting {})
        | EventKind::LifecycleSessionRunning(LifecycleSessionRunning {})
        | EventKind::LifecycleSessionAwaitingApproval(LifecycleSessionAwaitingApproval {})
        | EventKind::LifecycleSessionInterrupted(LifecycleSessionInterrupted {})
        | EventKind::LifecycleSessionClosing(LifecycleSessionClosing {})
        | EventKind::LifecycleSessionCompacting(LifecycleSessionCompacting {})
        | EventKind::LifecycleTurnStarted(LifecycleTurnStarted {})
        | EventKind::LifecycleTurnCompleted(LifecycleTurnCompleted {}) => 0,
    };
    size_of::<Event>() + envelope_text + payload_text
}

/// Approximate heap owned by a free-form JSON map: key and string bytes
/// plus a per-node overhead for the containers. Runs only on the rare
/// detail-bearing and unknown payloads — never on the token hot path —
/// which is what keeps the no-payload-walk-per-publish argument true
/// where it matters. Recursion is bounded by the value's own nesting,
/// which in-process producers control.
fn approx_json_map_bytes(map: &serde_json::Map<String, serde_json::Value>) -> usize {
    map.iter()
        .map(|(key, value)| key.len() + size_of::<serde_json::Value>() + approx_json_bytes(value))
        .sum()
}

fn approx_json_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
        serde_json::Value::String(text) => text.len(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(|item| size_of::<serde_json::Value>() + approx_json_bytes(item))
            .sum(),
        serde_json::Value::Object(map) => approx_json_map_bytes(map),
    }
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
        // push's instant — the age bound acting alone. Age evictions come
        // back for the out-of-lock drop, so they are also the observable.
        let mut evicted_seqs = Vec::new();
        for (seq, at_secs) in [(0, 0), (1, 200), (2, 400), (3, 600)] {
            let evicted = ring.push(&event(seq), epoch + Duration::from_secs(at_secs));
            assert!(evicted.by_count.is_none(), "count bound is nowhere near");
            evicted_seqs.extend(evicted.by_age.iter().map(|event| event.seq));
        }
        // At t=600: seq 0 (age 600) and seq 1 (age 400) are out; seq 2
        // (age 200) and seq 3 (age 0) survive.
        assert_eq!(evicted_seqs, [0, 1]);
        assert_eq!(ring.earliest_seq(), Some(2));
        assert_eq!(ring.stats(4).events, 2);
    }

    #[test]
    fn age_bound_is_inclusive_at_exactly_max_age() {
        let mut ring = Ring::new(config(10_000, 300));
        let epoch = Instant::now();
        assert!(ring.push(&event(0), epoch).is_empty());
        // "At most the last 300 s" keeps an entry exactly 300 s old; one
        // second past that, the next push evicts it.
        assert!(
            ring.push(&event(1), epoch + Duration::from_secs(300))
                .is_empty()
        );
        assert_eq!(ring.earliest_seq(), Some(0));
        let evicted = ring.push(&event(2), epoch + Duration::from_secs(301));
        assert_eq!(evicted.by_age.len(), 1);
        assert_eq!(ring.earliest_seq(), Some(1));
    }

    #[test]
    fn count_bound_evicts_independently_of_age() {
        let mut ring = Ring::new(config(3, 300));
        let now = Instant::now();
        // Count evictions come back one at a time, FIFO, once the ring
        // is at its bound — and never together with an age batch here.
        let mut evicted_seqs = Vec::new();
        for seq in 0..5 {
            let evicted = ring.push(&event(seq), now);
            assert!(
                evicted.by_age.is_empty(),
                "nothing is old enough to age out"
            );
            evicted_seqs.extend(evicted.by_count.iter().map(|event| event.seq));
        }
        assert_eq!(evicted_seqs, [0, 1]);
        assert_eq!(ring.earliest_seq(), Some(2));
        assert_eq!(ring.stats(5).events, 3);
    }

    #[test]
    fn zero_max_events_retains_nothing() {
        let mut ring = Ring::new(config(0, 300));
        assert!(ring.push(&event(0), Instant::now()).is_empty());
        assert_eq!(ring.earliest_seq(), None);
        assert!(ring.entries_from(0).is_none());
        assert_eq!(ring.stats(1).approx_bytes, 0);
    }

    #[test]
    fn entries_from_walks_to_the_newest_or_reports_eviction() {
        let mut ring = Ring::new(config(3, 300));
        let now = Instant::now();
        for seq in 0..5 {
            let _ = ring.push(&event(seq), now);
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
    fn detail_maps_are_priced() {
        let mut ring = Ring::new(config(10, 300));
        let dump = "x".repeat(4096);
        let body = EventBody::new(agent_bridge_events::EventKind::RuntimeNotice(
            agent_bridge_events::RuntimeNotice {
                notification_type: "diagnostic".into(),
                message: None,
                detail: serde_json::json!({ "dump": dump, "nested": { "also": dump } })
                    .as_object()
                    .expect("a JSON object literal")
                    .clone(),
            },
        ));
        let event = Arc::new(Event {
            schema_version: SCHEMA_VERSION,
            session_id: Some("s".into()),
            seq: 0,
            monotonic_ns: None,
            ts: "2026-08-13T00:00:00.000Z".into(),
            approval_id: body.approval_id,
            correlation_id: body.correlation_id,
            kind: body.kind,
        });
        let _ = ring.push(&event, Instant::now());
        // The free-form map is the only unbounded non-text container an
        // event can own; the estimate must move with what it carries, or
        // an oversized resident event could hide from the budget row.
        assert!(
            ring.stats(1).approx_bytes > 2 * 4096,
            "estimate {} misses the detail map's bulk",
            ring.stats(1).approx_bytes
        );
    }

    #[test]
    fn byte_accounting_tracks_evictions_and_drain() {
        let mut ring = Ring::new(config(2, 300));
        let now = Instant::now();
        let _ = ring.push(&event(0), now);
        let one = ring.stats(1).approx_bytes;
        assert!(one > 0, "an event has a nonzero estimated cost");
        let _ = ring.push(&event(1), now);
        let _ = ring.push(&event(2), now);
        // Same-shaped events: after evicting down to two, the total is
        // exactly two of them — eviction gave back what insertion added.
        assert_eq!(ring.stats(3).approx_bytes, one * 2);
        drop(ring.drain());
        assert_eq!(ring.stats(3).approx_bytes, 0);
        assert_eq!(ring.stats(3).events, 0);
    }
}
