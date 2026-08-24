//! The event bus: per-session publish/subscribe with one stamping site.
//!
//! The shape to hold on to is two disjoint routing paths over one shared
//! mechanism. Session events flow from a session's single [`Publisher`] to
//! that session's subscribers; unscoped events (`session_id: null` — the
//! runtime's own notices, adapter failures, transport conditions not tied
//! to a subscription) flow from [`EventBus::publish_global`] to global
//! subscribers. An event constructed on one path can never surface on the
//! other, because isolation by construction cannot regress the way
//! isolation by filtering can.
//!
//! Within a path, the contract is the envelope's: `seq` per session is
//! consecutive from 0 with no gaps, every subscriber independently
//! observes the events it receives in `seq` order, and publishing never
//! blocks the publisher. Delivery is *staged dispatch*: a publisher stamps
//! and stages under the channel's state lock, and exactly one drainer at a
//! time performs the queue sends outside every bus lock — `seq` order held
//! by the single-drainer discipline, the drainer flag protected against a
//! panicking drain by [`DrainGuard`]. The structure exists because
//! `try_send` can fire a receiver's waker, and a waker is caller-supplied
//! code: sends under the lock would hand a hand-rolled waker a path back
//! into the bus and a deadlock. With the sends outside, a re-entrant waker
//! finds every lock free and its publish simply stages for the active
//! drainer.
//!
//! That makes the waker contract worth stating outright, because a waker
//! is the one place consumer code runs inside this bus. Against it the bus
//! guarantees two things: a waker may re-enter any bus method, and a waker
//! that panics costs only its own subscription (the panic is contained at
//! the send, never carried into the publisher or the close path that
//! happened to be delivering). What the bus cannot guarantee is progress
//! against a waker that *blocks* — `wake()` runs on the delivering
//! thread, so a waker that sleeps or waits on a lock stalls that thread
//! for as long as it chooses. Runtime-scheduled receivers, the only kind
//! this workspace has, only enqueue a task there. Moving delivery onto a
//! task of its own would trade that for a worse bargain: publishing would
//! stop working outside a runtime — a property the stream pipeline
//! depends on — and a blocking waker would then stall every subscriber on
//! the channel instead of the one publisher that happened to claim the
//! drain.
//!
//! What a subscriber that stops draining costs is bounded by the
//! flow-control policy carried in [`BackpressureConfig`]: a bounded queue,
//! one overflow slot, and a grace window separating "momentarily behind"
//! from "not draining". A subscriber that fails to drain within grace is
//! disconnected — its stream ends, and the `transport.error` payload of
//! code `subscriber_lagging` naming what was lost rides beside it in
//! [`Subscription::disconnect_error`] for the transport layer to emit on
//! the wire. (Not as an in-stream event: `seq` is canonical, and a
//! synthesized terminal would collide with real history.) The session continues for everyone else;
//! the publish path never waits on anyone. The per-subscriber state
//! machine and the coarse sweep that resolves an idle-stream lag live in
//! [`backpressure`]; the accounting each disconnect feeds lives in
//! [`metrics`].
//!
//! Session channels additionally keep a bounded [`ring`] of recent events,
//! so a subscriber that dropped can re-attach with
//! [`EventBus::subscribe_from`] and receive exactly what it missed — or an
//! honest gap shape naming the oldest event still available
//! ([`replay::ReplayPlan`]). Both bounds, the backfill seam, and the
//! budget instrumentation live in those two modules; what matters here is
//! that ring insertion shares the stamping critical section, and plan
//! computation the attach one, which together are what make "replay, then
//! live, contiguous in `seq`" structural rather than scheduled. A staged
//! event is stamped and ringed before the drainer touches it, so a
//! subscriber attaching mid-drain marks where its live stream begins
//! (`first_live_seq`) and the replay slice covers everything before that
//! mark — the seam holds whether or not a drain is in flight.
//!
//! One piece of this stage is still deliberately deferred: a sealed
//! session's channel stays in the registry map
//! ([`EventBus::seal_session`]) — removing it, like re-registering its id,
//! is the session layer's close-path decision, not one the bus can make
//! alone.

mod backpressure;
mod filter;
mod metrics;
mod publisher;
mod replay;
mod ring;
mod stamp;
mod subscription;

pub use backpressure::BackpressureConfig;
pub use filter::EventFilter;
pub use metrics::BusMetrics;
pub use publisher::Publisher;
pub use replay::ReplayPlan;
pub use ring::{RingConfig, RingStats};
pub use subscription::{DisconnectReason, Subscription};

use subscription::Terminal;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime};

use agent_bridge_events::{
    Event, EventBody, SCHEMA_VERSION, TransportErrorCode, TransportErrorPayload,
};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use backpressure::LagState;
use filter::FilterSet;
use ring::Ring;

/// Tuning the bus accepts at construction.
#[derive(Debug, Clone, Default)]
pub struct BusConfig {
    /// The bus→subscriber flow-control policy: the contractual queue bound
    /// and the lag grace window.
    pub backpressure: BackpressureConfig,
    /// Bounds for each session's replay ring.
    pub ring: RingConfig,
}

/// What the bus can refuse.
///
/// Typed, per the house rule: the layers above map these onto their own
/// surfaces — a protocol error code, a session-close race resolved — and
/// neither mapping can be made against a flattened message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BusError {
    /// The named session has never been registered on this bus.
    #[error("unknown session {0}")]
    UnknownSession(String),
    /// The session already has its one live [`Publisher`]. A second
    /// stamping handle would break gap-free-at-generation by construction,
    /// so it is refused rather than shared — including after the first
    /// handle was dropped, because a dropped handle does not seal and the
    /// bus cannot tell recovery from a duplicate. Reclaiming a live id is a
    /// session-lifecycle question the session layer's close path answers;
    /// until then the bus's only exit for a session is
    /// [`EventBus::seal_session`].
    #[error("publisher already registered for session {0}")]
    PublisherExists(String),
    /// The session has been sealed: it accepts no further publishes and no
    /// new subscribers. What re-registering a session id means is a
    /// session-lifecycle question and lands with the session layer's close
    /// path; until then a sealed id stays sealed.
    #[error("session {0} is sealed")]
    Sealed(String),
    /// A backfill request named a `seq` the session has not stamped yet.
    /// Refused rather than clamped: a caller claiming to have seen events
    /// that never existed has confused its sessions or its bookkeeping,
    /// and attaching it as "nothing missed" would make every later live
    /// event look like a duplicate to it. When the wire's attach method
    /// grows `from_seq`, the transport maps this onto its invalid-params
    /// surface.
    #[error("from_seq {from_seq} is past session {session_id}'s head {head}")]
    FromSeqBeyondHead {
        /// The session the request named.
        session_id: String,
        /// What the caller asked to resume from.
        from_seq: u64,
        /// The session's next unstamped `seq` — the highest valid request.
        head: u64,
    },
}

/// The Core-owned event bus.
///
/// Cheap to clone and share — clones see one bus. One instance is meant to
/// exist per runtime, owned by Core and reached by everything that
/// publishes or subscribes; the bus itself knows nothing of transport,
/// session internals, or adapters. It moves
/// [`agent_bridge_events`] values, and that is all.
///
/// The lag policy's timer half is an async task, and it starts at the
/// first subscription rather than at construction: lag state lives on
/// subscriber slots, so a bus nobody has subscribed to has nothing to
/// sweep, and a publish-only embedding should not pay for a timer that can
/// only ever find an empty list. It spawns once, onto the runtime that
/// first subscribes, and lives and dies with that runtime — a bus
/// outliving it is back to publish-path checks. Used entirely outside a
/// runtime — possible, since publishing is synchronous — deadlines are
/// still checked on every publish, but an idle-stream lag resolves only at
/// the next one. The runtime binary has one runtime for the bus's whole
/// life, which is the case this design carries; anything else is a test's
/// own arrangement.
#[derive(Debug, Clone)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

#[derive(Debug)]
pub(crate) struct BusInner {
    config: BusConfig,
    /// The zero of every `monotonic_ns` this bus stamps: readings are
    /// comparable within a bus's lifetime, which is the runtime process's.
    pub(crate) anchor: Instant,
    pub(crate) sessions: Mutex<HashMap<String, Arc<Channel>>>,
    pub(crate) global: Arc<Channel>,
    /// Slot ids are minted bus-wide so a subscription's identity never
    /// collides across channels, whatever list it detaches from.
    next_slot_id: AtomicU64,
    metrics: BusMetrics,
    sweeper_started: AtomicBool,
}

impl EventBus {
    /// A new, empty bus.
    ///
    /// # Panics
    ///
    /// When `backpressure.queue_bound` is 0 — a queue that can hold
    /// nothing cannot deliver anything — or above the async runtime's
    /// channel capacity ceiling, which would otherwise panic at the first
    /// subscribe, far from the misconfiguration. Either way the bad bound
    /// is a bug at the construction site, refused loudly here.
    pub fn new(config: BusConfig) -> Self {
        assert!(
            config.backpressure.queue_bound >= 1,
            "backpressure.queue_bound must be at least 1"
        );
        // The ceiling is tokio's: `mpsc::channel` panics above its
        // semaphore's permit maximum (usize::MAX >> 3). Restated here to
        // keep the constructor's promise that a bad bound fails at the
        // call site, not on the subscribe path.
        assert!(
            config.backpressure.queue_bound <= usize::MAX >> 3,
            "backpressure.queue_bound exceeds the runtime's channel-capacity ceiling"
        );
        // Same promise for the window: a grace large enough to overflow
        // the monotonic clock would turn a deployment typo into a panic
        // when the first overflow event arms its deadline — on the
        // synchronous publish path, which is the one place in this runtime
        // that must not panic. The cap is far past any tuning: beyond a
        // day the policy is disabled rather than tuned, and disabling it
        // should be a deliberate act, not a very large number.
        assert!(
            config.backpressure.grace <= backpressure::MAX_GRACE,
            "backpressure.grace must be at most {:?}",
            backpressure::MAX_GRACE
        );
        let metrics = BusMetrics::default();
        Self {
            inner: Arc::new(BusInner {
                anchor: Instant::now(),
                sessions: Mutex::new(HashMap::new()),
                global: Arc::new(Channel::global(config.backpressure, metrics.clone())),
                next_slot_id: AtomicU64::new(0),
                metrics,
                sweeper_started: AtomicBool::new(false),
                config,
            }),
        }
    }

    /// Register a session and hand back its one [`Publisher`].
    ///
    /// The single-handle rule is the `seq` contract's foundation: gap-free
    /// at generation is true by construction only if exactly one handle
    /// stamps, so a second call for a live session id is
    /// [`BusError::PublisherExists`], and a sealed id is
    /// [`BusError::Sealed`] rather than silently reusable.
    pub fn register_session(&self, session_id: String) -> Result<Publisher, BusError> {
        let mut sessions = lock(&self.inner.sessions);
        match sessions.entry(session_id) {
            Entry::Occupied(entry) => {
                let refusal = if lock(&entry.get().state).sealed {
                    BusError::Sealed
                } else {
                    BusError::PublisherExists
                };
                Err(refusal(entry.key().clone()))
            }
            Entry::Vacant(entry) => {
                let channel = Arc::new(Channel::for_session(
                    entry.key().clone(),
                    Ring::new(self.inner.config.ring.clone()),
                    self.inner.config.backpressure,
                    self.inner.metrics.clone(),
                ));
                entry.insert(Arc::clone(&channel));
                Ok(Publisher {
                    channel,
                    anchor: self.inner.anchor,
                })
            }
        }
    }

    /// Subscribe to one session's events, filtered.
    ///
    /// Every subscriber independently receives the matching events in
    /// `seq` order, through a bounded queue governed by the
    /// [`BackpressureConfig`] lag policy: a subscriber that stops draining
    /// past its grace window is disconnected, with the `transport.error`
    /// payload of code `subscriber_lagging` beside the ended stream
    /// ([`Subscription::disconnect_error`]) — a stream says why it ended,
    /// never just that it did. Fails on a session this bus
    /// has never seen, and on one that is already sealed — a stream
    /// guaranteed to deliver nothing and then end is more honestly refused
    /// than returned.
    pub fn subscribe(
        &self,
        session_id: &str,
        filter: EventFilter,
    ) -> Result<Subscription, BusError> {
        self.ensure_sweeper();
        let channel = self.session(session_id)?;
        channel.attach(
            FilterSet::new(vec![filter]),
            self.inner.next_slot_id.fetch_add(1, Ordering::Relaxed),
        )
    }

    /// Re-attach to one session's events with backfill — the mechanism
    /// behind `session.attach(from_seq)`, where the re-attaching
    /// subscriber is the runtime's one transport peer returning to its own
    /// session.
    ///
    /// The subscription always lands attached at head; the returned
    /// [`ReplayPlan`] says what came with it — the missed events preloaded
    /// ahead of the live stream, an honest gap naming the oldest `seq`
    /// still available, or simply live-from-head when no `from_seq` was
    /// given. Plan computation, replay-slice capture, and slot
    /// registration share one critical section, so replay and live are
    /// contiguous in `seq`: no event is missed or duplicated at the seam.
    ///
    /// The replay slice passes the same `filter` as the live stream; the
    /// plan's `events_replayed` counts what is delivered (see
    /// [`ReplayPlan::WithinRing`]). The lag grace window stays unarmed
    /// while the replay buffer is still being drained — a subscriber
    /// catching up on instruction is not lagging — and starts at the first
    /// policy touch after the drain. Fails like [`EventBus::subscribe`] on
    /// unknown or sealed sessions, and refuses a `from_seq` past the
    /// session's head ([`BusError::FromSeqBeyondHead`]).
    pub fn subscribe_from(
        &self,
        session_id: &str,
        from_seq: Option<u64>,
        filter: EventFilter,
    ) -> Result<(Subscription, ReplayPlan), BusError> {
        self.ensure_sweeper();
        let channel = self.session(session_id)?;
        channel.attach_from(
            FilterSet::new(vec![filter]),
            self.inner.next_slot_id.fetch_add(1, Ordering::Relaxed),
            from_seq,
        )
    }

    /// One session's ring instrumentation: held events, their estimated
    /// bytes, the replayable range. Feeds the ring's share of the
    /// per-session memory budget the soak harness asserts — and only that
    /// share: events subscribers still hold (queued deliveries, an
    /// undrained replay buffer) stay resident beyond these numbers until
    /// each subscription drains or drops. Nothing enforces on these
    /// numbers.
    pub fn ring_stats(&self, session_id: &str) -> Result<RingStats, BusError> {
        let channel = self.session(session_id)?;
        let state = lock(&channel.state);
        Ok(state.ring.stats(state.next_seq))
    }

    /// The bus's own action accounting — today, the count of subscriptions
    /// sealed for lag, which the runtime's health surface reports as
    /// supervisor actions in a later phase.
    pub fn metrics(&self) -> BusMetrics {
        self.inner.metrics.clone()
    }

    /// Subscribe to the global channel — `session_id: null` events only.
    ///
    /// Each entry in `namespaces` is a dotted-name prefix with the same
    /// tolerant spellings as [`EventFilter::Prefix`]; an event matching any
    /// entry is delivered. An empty list means all global namespaces,
    /// mirroring the default the wire's subscribe method will have when it
    /// lands — the bus-side channel exists now so producers and the
    /// transport meet a finished contract. The same lag policy governs the
    /// global channel's subscribers as governs a session's.
    pub fn subscribe_global(&self, namespaces: Vec<String>) -> Subscription {
        self.ensure_sweeper();
        let filters = namespaces.into_iter().map(EventFilter::Prefix).collect();
        self.inner
            .global
            .attach(
                FilterSet::new(filters),
                self.inner.next_slot_id.fetch_add(1, Ordering::Relaxed),
            )
            .expect("the global channel is never sealed")
    }

    /// Publish an unscoped event onto the global channel, returning its
    /// stamped `seq`.
    ///
    /// Same stamping discipline as a session publish, with `session_id:
    /// null` on the envelope. The envelope defines `seq` as per-session and
    /// assigns the global channel none, so the bus gives global events a
    /// sequence domain of their own — one bus-wide counter for the global
    /// channel, consecutive from 0. That is this bus's inference, not yet a
    /// design contract; it is written down here so the wire layers can
    /// confirm or replace it deliberately.
    pub fn publish_global(&self, body: EventBody) -> u64 {
        self.inner
            .global
            .publish(body, self.inner.anchor)
            .expect("the global channel is never sealed")
    }

    /// Seal a session: no further publishes, no new subscribers.
    ///
    /// The session layer's close path calls this once a session has emitted
    /// its last event. Existing subscribers keep everything already queued
    /// — including anything still staged for an in-flight drainer, which
    /// finishes its deliveries before observing the seal — and then observe
    /// the end of the stream (`recv` → `None`). Idempotent, because close
    /// paths race and a second close arriving late is normal, not an error.
    pub fn seal_session(&self, session_id: &str) -> Result<(), BusError> {
        let channel = self.session(session_id)?;
        // Dropping the senders is what turns "sealed" into an observable
        // end of stream: each receiver drains its queue and then sees the
        // channel closed. The drop happens after the guard is released,
        // because closing a channel can wake its pending receiver, and
        // wakes stay outside critical sections. When a drainer is active
        // it holds the real slot list; the vec taken here is then only
        // mid-drain attaches, and the drainer drops its own slots on
        // observing `sealed` at its next merge. The ring goes with the
        // slots: a sealed session admits no subscriber that could ever
        // request backfill, so holding up to the full ring budget for it
        // would be pure leak — and its entries drop outside the guard too,
        // since freeing the ring's share of megabytes has no business
        // extending a critical section. Its share only: events a
        // subscriber still holds queued stay alive until that subscriber
        // drains or drops.
        let (sealed_slots, stranded, drained_ring) = {
            let mut state = lock(&channel.state);
            state.sealed = true;
            channel.sealed_hint.store(true, Ordering::Relaxed);
            let drained_ring = state.ring.drain();
            // While a drainer is active, the real slot list is out with it
            // and anything sitting here (mid-drain attaches) may be
            // entitled to events already staged. Leaving them for the
            // drainer — which adopts them at its next merge and closes
            // everything once nothing is staged — is what keeps "a publish
            // that returned Ok is delivered" true for them too; taking
            // them now would end those streams short of accepted events.
            let (sealed_slots, stranded) = if state.draining {
                (Vec::new(), VecDeque::new())
            } else {
                // A drain that died between staging and delivery leaves a
                // backlog with no owner. Nothing will adopt it now — this
                // seal is what ends the session — so the events go out
                // with the slots they were owed to, counted rather than
                // quietly abandoned in a channel nobody will read again.
                (
                    std::mem::take(&mut state.subscribers),
                    std::mem::take(&mut state.staged),
                )
            };
            (sealed_slots, stranded, drained_ring)
        };
        close_slots(sealed_slots, &stranded, channel.session_id.as_deref());
        drop(stranded);
        drop(drained_ring);
        tracing::debug!(session_id, "session sealed");
        Ok(())
    }

    fn session(&self, session_id: &str) -> Result<Arc<Channel>, BusError> {
        lock(&self.inner.sessions)
            .get(session_id)
            .cloned()
            .ok_or_else(|| BusError::UnknownSession(session_id.to_owned()))
    }

    /// Spawn the lag sweep once, at the first construction or subscribe
    /// that happens inside a tokio runtime. Retried from every subscribe
    /// because a bus built outside the runtime (a sync setup path) still
    /// deserves its timer once subscribers exist inside one.
    fn ensure_sweeper(&self) {
        if self.inner.sweeper_started.load(Ordering::Relaxed)
            || tokio::runtime::Handle::try_current().is_err()
        {
            return;
        }
        if self
            .inner
            .sweeper_started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            backpressure::spawn_sweeper(&self.inner);
        }
    }
}

/// One fanout list and its sequence counter — a session's, or the global
/// channel's.
///
/// The two routing paths never share a `Channel`, which is what makes
/// session/global isolation structural: there is no filter whose bug could
/// leak an event across, only two lists an event was or was not born into.
#[derive(Debug)]
pub(crate) struct Channel {
    /// `Some` for a session channel — stamped into every envelope — and
    /// `None` for the global channel, whose envelopes carry the null
    /// `session_id` the design assigns unscoped events.
    pub(crate) session_id: Option<String>,
    /// The ring's enabled-ness, cached outside the mutex at construction
    /// (ring configuration never changes after) so the publish path can
    /// decide whether to price an event before taking its lock.
    ring_enabled: bool,
    backpressure: BackpressureConfig,
    metrics: BusMetrics,
    /// A lock-free echo of `ChannelState::sealed`, for the sweep's filter
    /// and nothing else. Every decision that must be right takes the
    /// mutex, because a value read beside a lock cannot be trusted to
    /// still hold when the decision commits; this one only spares the
    /// timer a lock per sealed channel it was going to skip anyway.
    /// Monotonic (false → true), so a stale read costs at most one more
    /// sweep of a channel that has nothing left to sweep.
    pub(crate) sealed_hint: AtomicBool,
    pub(crate) state: Mutex<ChannelState>,
}

/// The bus's own teardown is the last place a subscriber's queue closes:
/// dropping the last `EventBus` clone drops every channel, and with it
/// every slot still registered. Routing that through the contained path
/// keeps the promise whole — a waker that panics costs its subscription
/// and nothing else, including when what ended the stream was the runtime
/// going away.
impl Drop for ChannelState {
    fn drop(&mut self) {
        drop_slots(std::mem::take(&mut self.subscribers));
    }
}

#[derive(Debug)]
pub(crate) struct ChannelState {
    next_seq: u64,
    sealed: bool,
    subscribers: Vec<SubscriberSlot>,
    /// Stamped events an active drainer has not yet delivered. Non-empty
    /// only while `draining` is set (or, transiently, after a panicked
    /// drain — the next claim or sweep adopts the backlog, oldest first):
    /// a publisher that finds a drainer active stages here and returns,
    /// and the drainer picks the batch up at its next merge — which is
    /// what keeps every wake-producing send outside the lock without ever
    /// reordering `seq`.
    ///
    /// Deliberately not bounded by count, and the reasoning is worth
    /// keeping because the shape invites the question. A bound here could
    /// only be enforced two ways, and this policy forbids both: block the
    /// publisher until the drainer catches up, or drop events whose
    /// publishers were told `Ok`. What keeps the queue finite instead is
    /// that the drain converges — delivering one event is strictly less
    /// work than publishing it was (no timestamp to format, no `Arc` to
    /// allocate, no ring insert and eviction, just a `try_send` per
    /// subscriber), so a drainer out-paces the producers feeding it unless
    /// consumer wakers are doing something slow, which is the waker
    /// contract's business and stated with it. The entries are `Arc`
    /// clones of events the ring largely retains anyway, the drain runs to
    /// empty before releasing the flag, and v1's producer is the PTY
    /// pipeline that the stream stage's own flow-control row throttles at
    /// the source. A backlog that outgrows the subscriber bound anyway is
    /// a bug or a misload, and the high-water warning says so rather than
    /// letting it pass unremarked. Real-load evidence for all of this
    /// belongs to the soak stage, which is where a convoy would show up as
    /// a number rather than an argument.
    staged: VecDeque<Arc<Event>>,
    /// Whether the high-water warning fired for the current backlog
    /// episode; reset when the backlog drains so a sustained problem logs
    /// once per episode, not once per event.
    staged_warned: bool,
    /// Whether some caller is currently delivering outside the lock.
    /// Exactly one drainer runs at a time; the flag is reset by
    /// [`DrainGuard`] even if a drain panics, so a poisoned drain cannot
    /// wedge the channel.
    draining: bool,
    /// Subscriptions dropped while their slot was out with a drainer;
    /// applied and cleared at the drainer's next merge.
    pending_detach: Vec<u64>,
    /// Every channel carries one; the global channel's is constructed
    /// disabled, because backfill is a per-session contract and retaining
    /// events nothing can ever request again would be memory spent on no
    /// reader — one shape for every channel, one branch per publish, and
    /// no constructible channel without a ring. Lives beside `next_seq`
    /// deliberately — insertion must share the stamping critical section,
    /// and plan computation the attach one, for the replay seam to be
    /// gap-free by construction.
    ring: Ring,
}

#[derive(Debug)]
pub(crate) struct SubscriberSlot {
    pub(crate) id: u64,
    filters: FilterSet,
    sender: mpsc::Sender<Arc<Event>>,
    /// The channel's `next_seq` at attach. A staged event stamped before
    /// this subscriber existed is skipped for it — on a backfill attach
    /// the replay slice already covers everything below this mark, and on
    /// a plain attach events from before the subscribe were never its to
    /// receive. This is what makes staged dispatch invisible at the
    /// replay seam.
    first_live_seq: u64,
    /// Accepted events this subscription was entitled to and skipped
    /// because its expired deadline deferred to a session close. They are
    /// no longer in the staging queue by then — the drain took them — so
    /// the close path could not otherwise see them, and an accepted
    /// publish would vanish with no account of it.
    deferred_losses: u64,
    /// Where this subscriber stands against the lag policy; moved only by
    /// the channel's single drainer.
    pub(crate) lag: LagState,
    /// Set exactly once, at seal-for-cause; the [`Subscription`] holds the
    /// other end. A typed value beside the stream rather than an in-stream
    /// event, because the envelope's `seq` is canonical and a synthesized
    /// terminal would collide with real history.
    terminal: Arc<OnceLock<Terminal>>,
    /// Whether the subscription has finished draining its preloaded
    /// replay buffer; the grace deadline stays unarmed until it has.
    pub(crate) replay_drained: Arc<AtomicBool>,
}

impl Channel {
    /// The two channel shapes get their own constructors so the
    /// session-id/ring pairing is decided here once — a session channel
    /// with no ring, or a global one retaining events, is not a state a
    /// caller can assemble.
    fn for_session(
        session_id: String,
        ring: Ring,
        backpressure: BackpressureConfig,
        metrics: BusMetrics,
    ) -> Self {
        Self::new(Some(session_id), ring, backpressure, metrics)
    }

    fn global(backpressure: BackpressureConfig, metrics: BusMetrics) -> Self {
        Self::new(
            None,
            Ring::new(RingConfig::disabled()),
            backpressure,
            metrics,
        )
    }

    fn new(
        session_id: Option<String>,
        ring: Ring,
        backpressure: BackpressureConfig,
        metrics: BusMetrics,
    ) -> Self {
        Self {
            session_id,
            ring_enabled: ring.is_enabled(),
            backpressure,
            metrics,
            sealed_hint: AtomicBool::new(false),
            state: Mutex::new(ChannelState {
                next_seq: 0,
                sealed: false,
                subscribers: Vec::new(),
                staged: VecDeque::new(),
                staged_warned: false,
                draining: false,
                pending_detach: Vec::new(),
                ring,
            }),
        }
    }

    /// The choke point: complete the envelope, atomically with the
    /// sequence increment, and hand it to the delivery step.
    ///
    /// The increment, the ring insertion, and the staging share one
    /// critical section deliberately. An atomic counter alone would let
    /// two publishes stamp 5 and 6 and then stage 6 before 5, and "each
    /// subscriber's queue order is `seq` order" would quietly become
    /// "usually"; the ring rides the same section so it and the queues
    /// always agree on what has been published — the exactness a backfill
    /// plan computed under this same lock relies on. The monotonic reading
    /// sits inside the lock for the same reason: a later `seq` never
    /// carries an earlier `monotonic_ns`. The wall-clock read and its
    /// formatting do not — `ts` is documented as not an ordering key, so
    /// it costs the critical section nothing.
    ///
    /// The queue sends happen *after* the lock is released: the
    /// publisher that finds no drainer active takes the drainer role and
    /// delivers what is staged; one that finds a drainer already at work
    /// stages its event and returns, non-blocking either way. Correctness
    /// first — the publish-path benchmark is what says whether this lock
    /// ever becomes worth splitting.
    pub(crate) fn publish(&self, body: EventBody, anchor: Instant) -> Result<u64, BusError> {
        let ts = stamp::rfc3339_millis(SystemTime::now());
        let session_id = self.session_id.clone();
        // Priced before the lock: the estimate can walk a detail map, and
        // an O(payload) walk inside the critical section would hand back
        // the very cost the out-of-lock frees reclaim. Zero when the ring
        // retains nothing — the walk would price a discard.
        let approx_bytes = if self.ring_enabled {
            ring::approx_event_bytes(session_id.as_deref(), &ts, &body)
        } else {
            0
        };
        let mut warn_backlog: Option<usize> = None;
        let (seq, claim, evicted) = {
            let mut state = lock(&self.state);
            if state.sealed {
                return Err(BusError::Sealed(session_id.unwrap_or_default()));
            }
            // One reading serves both stamps: `monotonic_ns` and the ring
            // entry's age must name the same instant, or an event could be
            // ordered younger than the ring believes it is.
            let now = Instant::now();
            let seq = state.next_seq;
            state.next_seq += 1;
            let event = Arc::new(Event {
                schema_version: SCHEMA_VERSION,
                session_id,
                seq,
                monotonic_ns: Some(
                    u64::try_from(now.duration_since(anchor).as_nanos()).unwrap_or(u64::MAX),
                ),
                ts,
                approval_id: body.approval_id,
                correlation_id: body.correlation_id,
                kind: body.kind,
            });
            let evicted = state.ring.push(&event, approx_bytes, now);
            let claim = if state.draining {
                state.staged.push_back(event);
                if state.staged.len() >= self.backpressure.queue_bound && !state.staged_warned {
                    // Surfaced once per backlog episode so a soak run can
                    // see production outrunning delivery; the merge loop
                    // resets the marker when the backlog empties. The
                    // measured depth travels with it: the threshold alone
                    // says a line was crossed, not how far past it the
                    // queue went, which is the number an operator reading
                    // this actually wants.
                    state.staged_warned = true;
                    warn_backlog = Some(state.staged.len());
                }
                None
            } else {
                state.draining = true;
                let seed = if state.staged.is_empty() {
                    Seed::Event(event)
                } else {
                    // A panicked drain left a backlog behind. Order is
                    // FIFO through the staging queue, so this event lines
                    // up behind it and the drain starts from the merge
                    // loop instead of jumping the new event ahead.
                    state.staged.push_back(event);
                    Seed::Backlog
                };
                Some((seed, std::mem::take(&mut state.subscribers)))
            };
            (seq, claim, evicted)
        };
        // Evicted events free here, outside the guard: even one entry's
        // destructor is unbounded in principle (a frame-sized payload, a
        // detail map of many allocations), and the first publish after an
        // idle spell can age out most of the ring at once — a free of
        // unknowable size is the seal path's discipline, not the critical
        // section's.
        drop(evicted);
        if let Some(staged) = warn_backlog {
            tracing::warn!(
                session_id = ?self.session_id,
                staged,
                queue_bound = self.backpressure.queue_bound,
                "staged-dispatch backlog reached the subscriber queue bound; \
                 production is outrunning delivery"
            );
        }
        if let Some((seed, slots)) = claim {
            self.drain(seed, slots);
        }
        Ok(seq)
    }

    /// The timer half of lag detection, called from the bus's coarse
    /// sweep: claim the drainer role if it is free, resolve any expired
    /// grace deadline, and hand back the slots. Skipping a channel whose
    /// drainer is active is correct, not lazy — that drainer checks
    /// deadlines on every delivery, and the next tick revisits. A staged
    /// backlog with no drainer means a drain panicked out from under it;
    /// the sweep adopts it so those events reach their subscribers within
    /// one tick instead of waiting for a publish that may never come.
    pub(crate) fn sweep(&self) {
        let slots = {
            let mut state = lock(&self.state);
            let orphaned_backlog = !state.staged.is_empty();
            if state.draining
                || (!orphaned_backlog
                    && state
                        .subscribers
                        .iter()
                        .all(|slot| matches!(slot.lag, LagState::Healthy)))
            {
                return;
            }
            state.draining = true;
            std::mem::take(&mut state.subscribers)
        };
        self.drain(Seed::Sweep, slots);
    }

    /// The delivery step — the single drainer's loop, entered with the
    /// `draining` flag held and the slot list taken out of the state.
    ///
    /// Every queue send, overflow parking, grace check, and seal happens
    /// here, outside all bus locks, which is the staged-dispatch structure: a waker
    /// fired by a send finds nothing held, and a re-entrant call back into
    /// the bus stages behind this very loop instead of deadlocking. Each
    /// merge re-locks briefly to apply detaches that raced the drain,
    /// adopt subscribers that attached mid-drain, and pick up whatever
    /// publishers staged meanwhile; the loop ends only when nothing is
    /// staged, so a publish that returned `Ok` is delivered (or resolved
    /// per policy) before the drainer flag clears.
    fn drain(&self, seed: Seed, mut slots: Vec<SubscriberSlot>) {
        let mut guard = DrainGuard {
            channel: self,
            defused: false,
        };
        let cx = DeliverCx {
            backpressure: self.backpressure,
            metrics: &self.metrics,
            session_id: self.session_id.as_deref(),
            state: &self.state,
        };
        let now = tokio::time::Instant::now();
        match seed {
            Seed::Event(event) => {
                deliver_and_release(&mut slots, |slot| deliver(slot, &event, now, &cx));
            }
            // Nothing to do before the merge loop: the backlog this drain
            // was claimed for is picked up there, oldest first.
            Seed::Backlog => {}
            Seed::Sweep => {
                deliver_and_release(&mut slots, |slot| {
                    if !try_flush_parked(slot, cx.session_id) {
                        return false;
                    }
                    let drained = backpressure::replay_drained(slot);
                    if slot.lag.expired(now, drained, cx.backpressure.grace) {
                        seal_for_lag(slot, &cx)
                    } else {
                        true
                    }
                });
            }
        }
        let mut batch: VecDeque<Arc<Event>> = VecDeque::new();
        // Events delivered on other publishers' behalf during this call.
        // Past the bound it hands the drainer role back — see the merge.
        let mut delivered: usize = 0;
        let mut handed_off = false;
        loop {
            // Slots removed under the lock are dropped after it: closing a
            // queue can wake its receiver, and wakes stay outside critical
            // sections. Slots a seal is closing additionally get their
            // parked events flushed on the way out.
            let mut removed: Vec<SubscriberSlot> = Vec::new();
            let mut closed: Vec<SubscriberSlot> = Vec::new();
            let mut return_after_merge = false;
            let done = {
                let mut state = lock(&self.state);
                for id in std::mem::take(&mut state.pending_detach) {
                    if let Some(index) = slots.iter().position(|slot| slot.id == id) {
                        removed.push(slots.swap_remove(index));
                    }
                }
                // A publisher that claimed the drainer role does other
                // publishers' delivery work, and with a wide fanout a
                // delivery can cost more per event than a publish does —
                // so under sustained concurrent publishing the claimant
                // could be held here indefinitely, which is not what a
                // synchronous publish promises. Past a bound it hands the
                // role back: the events stay staged, in order, and the
                // next publish — there is one, by hypothesis — or the
                // sweep within a tick adopts them through the same path
                // that picks up an orphaned backlog. Never while sealed,
                // where this merge is what ends the streams.
                if delivered >= self.backpressure.queue_bound
                    && !state.staged.is_empty()
                    && !state.sealed
                {
                    state.subscribers.append(&mut slots);
                    state.draining = false;
                    guard.defused = true;
                    handed_off = true;
                    return_after_merge = true;
                } else {
                    // Swapping (not taking) hands the drained batch's
                    // spare capacity back to the channel, so a contended
                    // spell allocates its staging storage once, not per
                    // merge.
                    std::mem::swap(&mut state.staged, &mut batch);
                }
                if return_after_merge {
                    true
                } else if batch.is_empty() {
                    state.staged_warned = false;
                    // A seal that raced this drain is observed only now,
                    // with nothing left staged: a publish that returned Ok
                    // before the seal has been delivered, and only then do
                    // the streams end — the held slots and any attach the
                    // seal deferred to this drainer alike. Dropping them at
                    // the seal's own merge instead would silently lose
                    // exactly the session's last events — the ones the
                    // close path publishes immediately before sealing.
                    if state.sealed {
                        closed.append(&mut slots);
                        closed.append(&mut state.subscribers);
                    } else {
                        state.subscribers.append(&mut slots);
                    }
                    state.draining = false;
                    guard.defused = true;
                    true
                } else {
                    // Adopt subscribers that attached mid-drain (the vec is
                    // empty once sealed: attach refuses and the seal took
                    // any earlier ones).
                    slots.append(&mut state.subscribers);
                    false
                }
            };
            drop_slots(removed);
            // Nothing can be stranded here: this branch runs only once the
            // staging queue has been drained to empty.
            close_slots(closed, &VecDeque::new(), self.session_id.as_deref());
            if done {
                if handed_off {
                    tracing::debug!(
                        session_id = ?self.session_id,
                        delivered,
                        "publisher handed the drainer role back with work still staged"
                    );
                }
                return;
            }
            // A panicking waker unwinds out of this loop and takes the
            // rest of the batch with it. That is deliberate rather than
            // an oversight, and it costs no subscriber anything: the
            // merge above moved *every* subscriber that existed into
            // `slots`, so the unwind ends all of them — and a
            // subscription attaching afterwards carries a
            // `first_live_seq` above every seq in this batch, so those
            // events were never going to be its either. The durable copy
            // is untouched regardless: ring insertion happens in the
            // stamping critical section, so backfill still replays them.
            // Re-staging the remainder would therefore hand events to an
            // audience that provably cannot exist. (The events staged
            // *after* this batch are a different matter — they may
            // outlive the panic with entitled readers, which is why the
            // drain guard leaves them for the next claim or sweep.)
            for event in batch.drain(..) {
                // Sampled per event, not per batch: a long batch must not
                // let every delivery judge the grace deadline against a
                // reading from before the batch began.
                let now = tokio::time::Instant::now();
                deliver_and_release(&mut slots, |slot| deliver(slot, &event, now, &cx));
                delivered += 1;
            }
        }
    }

    fn attach(
        self: &Arc<Self>,
        filters: FilterSet,
        slot_id: u64,
    ) -> Result<Subscription, BusError> {
        let subscription = {
            let mut state = lock(&self.state);
            if state.sealed {
                return Err(BusError::Sealed(
                    self.session_id.clone().unwrap_or_default(),
                ));
            }
            self.register_slot(&mut state, filters, slot_id, VecDeque::new())
        };
        tracing::debug!(session_id = ?self.session_id, slot_id, "subscribed");
        Ok(subscription)
    }

    /// Attach at head with the backfill outcome decided in the same
    /// critical section — the seam that makes replay-then-live contiguous.
    ///
    /// Everything ordering-relevant happens under one acquisition of the
    /// channel lock: the plan is computed against the ring as it stands,
    /// the replay slice is cloned out, and the subscriber slot registers at
    /// head. A publish therefore lands entirely in the slice or entirely
    /// in the live stream — an event staged for an in-flight drainer is
    /// already in the ring and below this subscriber's `first_live_seq`,
    /// so it arrives through the slice and is skipped live. The slice
    /// rides the `Subscription` as a preloaded buffer drained ahead of the
    /// live queue, so a 10k-event replay never has to fit the bounded
    /// channel.
    fn attach_from(
        self: &Arc<Self>,
        filters: FilterSet,
        slot_id: u64,
        from_seq: Option<u64>,
    ) -> Result<(Subscription, ReplayPlan), BusError> {
        let (subscription, plan) = {
            let mut state = lock(&self.state);
            if state.sealed {
                return Err(BusError::Sealed(
                    self.session_id.clone().unwrap_or_default(),
                ));
            }
            let session_id = self.session_id.as_deref().expect(
                "backfill is subscribed per session id, which never names the global channel",
            );
            let (plan, replay_slice) =
                replay::plan(&state.ring, state.next_seq, from_seq, &filters, session_id)?;
            (
                self.register_slot(&mut state, filters, slot_id, replay_slice),
                plan,
            )
        };
        tracing::debug!(session_id = ?self.session_id, slot_id, ?plan, "subscribed with backfill");
        Ok((subscription, plan))
    }

    /// The registration shared by both attach paths, under the caller's
    /// already-held state lock: mint the bounded queue, mark where the
    /// live stream begins, and hand back the subscription over whatever
    /// replay the caller preloaded. Only what precedes
    /// registration differs between the paths (plain attach preloads
    /// nothing; backfill computes its plan first, which is also why this
    /// helper must not touch the session id — the global channel has
    /// none). The grace window has nothing to wait on until a preloaded
    /// replay exists, so the drained flag starts at whether one does.
    fn register_slot(
        self: &Arc<Self>,
        state: &mut ChannelState,
        filters: FilterSet,
        slot_id: u64,
        replay: VecDeque<Arc<Event>>,
    ) -> Subscription {
        let (sender, receiver) = mpsc::channel(self.backpressure.queue_bound);
        let terminal = Arc::new(OnceLock::new());
        let replay_drained = Arc::new(AtomicBool::new(replay.is_empty()));
        state.subscribers.push(SubscriberSlot {
            id: slot_id,
            filters,
            sender,
            first_live_seq: state.next_seq,
            deferred_losses: 0,
            lag: LagState::Healthy,
            terminal: Arc::clone(&terminal),
            replay_drained: Arc::clone(&replay_drained),
        });
        Subscription {
            replay,
            receiver,
            channel: Arc::clone(self),
            slot_id,
            terminal,
            replay_drained,
        }
    }

    /// Remove one subscriber's slot; called from `Subscription::drop`.
    pub(crate) fn detach(&self, slot_id: u64) {
        // `if let` rather than a panic: this runs during unwinding when a
        // subscriber's task dies, and a poisoned lock there must not turn
        // one panic into an abort. The slot is moved out under the lock
        // and dropped after it — closing its channel can fire a wake, and
        // wakes stay outside critical sections. When the slot is out with
        // an active drainer instead, the detach is recorded and applied at
        // that drainer's next merge. `swap_remove` is fine: slot order in
        // the fanout list carries no meaning, only each queue's own order
        // does.
        let removed = if let Ok(mut state) = self.state.lock() {
            let index = state.subscribers.iter().position(|slot| slot.id == slot_id);
            match index {
                Some(index) => Some(state.subscribers.swap_remove(index)),
                None => {
                    if state.draining {
                        state.pending_detach.push(slot_id);
                    }
                    None
                }
            }
        } else {
            None
        };
        drop_slots(removed);
        tracing::debug!(session_id = ?self.session_id, slot_id, "unsubscribed");
    }
}

/// What a drain was entered for: a just-stamped event to deliver, a
/// backlog a panicked drain left behind (the claiming publish's own event
/// is staged at its tail, keeping FIFO), or a sweep resolving deadlines
/// with nothing new to say.
enum Seed {
    Event(Arc<Event>),
    Backlog,
    Sweep,
}

/// Resets the drainer flag if a drain unwinds — a waker is arbitrary code
/// and may panic — so one panicking subscriber cannot leave the channel
/// with a drainer bit set forever and every later publish staging into a
/// queue nobody will ever drain. The slots the drain held die with its
/// stack, which ends those streams; the channel itself stays serviceable,
/// and whatever the failed drain left staged is adopted — oldest first,
/// ahead of any newer event — by the next publish's claim or by the
/// sweep's next tick, so an orphaned backlog is late, never lost or
/// reordered.
struct DrainGuard<'a> {
    channel: &'a Channel,
    defused: bool,
}

impl Drop for DrainGuard<'_> {
    fn drop(&mut self) {
        if self.defused {
            return;
        }
        // Plain `lock()` would re-panic during this unwind; losing the
        // reset matters more than reporting the poison here.
        let (orphaned, stranded) = match self.channel.state.lock() {
            Ok(mut state) => {
                state.draining = false;
                // A seal that landed while this drain held the slots left
                // its own work to the drainer's next merge — which this
                // unwind means never comes. Nothing else will do it
                // either: the sweep skips a sealed channel and every
                // publish is refused, so subscribers that attached
                // mid-drain would wait on a stream that never ends, and
                // the staged tail would be lost with no account of it.
                if state.sealed {
                    (
                        std::mem::take(&mut state.subscribers),
                        std::mem::take(&mut state.staged),
                    )
                } else {
                    (Vec::new(), VecDeque::new())
                }
            }
            Err(_) => (Vec::new(), VecDeque::new()),
        };
        close_slots(orphaned, &stranded, self.channel.session_id.as_deref());
    }
}

/// Everything a delivery decision needs beyond the slot and the event —
/// bundled so the policy sites read as policy, not parameter plumbing.
struct DeliverCx<'a> {
    backpressure: BackpressureConfig,
    metrics: &'a BusMetrics,
    session_id: Option<&'a str>,
    /// The channel's own state, taken briefly on the expiry path so the
    /// close-versus-lag decision and the verdict it produces land in one
    /// critical section — see [`seal_for_lag`].
    state: &'a Mutex<ChannelState>,
}

/// Hand one event to one subscriber, without ever waiting; returns whether
/// the slot stays in the fanout list. Runs outside every bus lock, under
/// the single-drainer discipline — which is what lets the lag states be
/// plain moves and the sends fire wakers safely.
fn deliver(
    slot: &mut SubscriberSlot,
    event: &Arc<Event>,
    now: tokio::time::Instant,
    cx: &DeliverCx<'_>,
) -> bool {
    // Recovery outranks judgment, and the deadline outranks the event: a
    // parked overflow flushes the moment room exists — before the expiry
    // check, so a caught-up subscriber is healed, never sealed over an
    // event the bus simply had not handed over yet — and an expired grace
    // window then seals even when this particular event would have been
    // filtered, because promptness must not depend on the traffic mix.
    if !try_flush_parked(slot, cx.session_id) {
        return false;
    }
    let replay_drained = backpressure::replay_drained(slot);
    if slot.lag.expired(now, replay_drained, cx.backpressure.grace) {
        if !seal_for_lag(slot, cx) {
            return false;
        }
        // The seal deferred to a session close, so this subscription's
        // stream is ending without this event — which it was entitled to,
        // and which the close path cannot find because the drain already
        // took it out of the staging queue. Counted here instead.
        if event.seq >= slot.first_live_seq && slot.filters.admits(event) {
            slot.deferred_losses += 1;
        }
        return true;
    }
    if !slot.filters.admits(event) {
        return true;
    }
    if event.seq < slot.first_live_seq {
        // Stamped before this subscriber attached: on a backfill attach
        // the replay slice already carries it; on a plain attach it was
        // never this subscriber's to receive.
        return true;
    }
    match std::mem::replace(&mut slot.lag, LagState::Healthy) {
        LagState::Healthy => {
            if free_permits(slot) >= 1 {
                if !push_to_queue(slot, Arc::clone(event)) {
                    return false;
                }
            } else {
                // A fresh deadline per park is the chosen hysteresis rule:
                // every flush ends its episode, so a subscriber bouncing
                // at the bound is draining — slowly, losslessly — which is
                // throughput's problem, not this policy's.
                tracing::debug!(
                    session_id = ?cx.session_id,
                    slot_id = slot.id,
                    seq = event.seq,
                    "subscriber queue full; event parked, grace window opens"
                );
                slot.lag = LagState::Parked {
                    parked: Arc::clone(event),
                    deadline: if replay_drained {
                        backpressure::ArmedState::Armed(now + cx.backpressure.grace)
                    } else {
                        backpressure::ArmedState::AwaitingReplayDrain
                    },
                };
            }
        }
        LagState::Parked {
            parked: _,
            deadline,
        } => {
            // The flush attempt above failed, so the queue and the
            // overflow slot are both full: the stream this subscriber
            // observes is now gapped, which no later drain can repair.
            // The parked event drops here and is part of the loss; the
            // count rides to the terminal event so nothing is silent.
            tracing::warn!(
                session_id = ?cx.session_id,
                slot_id = slot.id,
                seq = event.seq,
                "subscriber overflowed past its parked event; disconnect due at the grace deadline"
            );
            slot.lag = LagState::Lossy { deadline, lost: 2 };
        }
        LagState::Lossy { deadline, lost } => {
            slot.lag = LagState::Lossy {
                deadline,
                lost: lost + 1,
            };
        }
    }
    true
}

/// Flush the parked overflow event the moment queue room exists; false
/// when the receiver is gone. The flush is what "drained within grace"
/// means observably — it ends the episode and closes the grace window —
/// and it runs at every policy observation point (each delivery, each
/// sweep visit), so a subscriber that catches up during an idle stream is
/// handed its parked tail event within one sweep tick instead of waiting
/// for a publish that may never come.
fn try_flush_parked(slot: &mut SubscriberSlot, session_id: Option<&str>) -> bool {
    if matches!(slot.lag, LagState::Parked { .. }) && free_permits(slot) >= 1 {
        let LagState::Parked { parked, .. } = std::mem::replace(&mut slot.lag, LagState::Healthy)
        else {
            unreachable!("matched Parked above");
        };
        if !push_to_queue(slot, parked) {
            return false;
        }
        tracing::debug!(
            session_id = ?session_id,
            slot_id = slot.id,
            "subscriber drained within grace; parked overflow flushed in order"
        );
    }
    true
}

/// Close subscriber slots on a session seal, outside every lock: a
/// policy-parked event gets its last chance at the queue — the receiver
/// may have drained exactly the room it needs — and a loss that cannot be
/// avoided is announced to the subscriber through its terminal cell (with
/// no disconnect verdict: the session ending is not a lag judgment), as
/// well as logged, because an accepted publish must never vanish without
/// a trace the subscriber itself can see. Dropping the senders is what
/// turns the seal into each stream's observable end.
fn close_slots(
    mut slots: Vec<SubscriberSlot>,
    stranded: &VecDeque<Arc<Event>>,
    session_id: Option<&str>,
) {
    for slot in &mut slots {
        let _ = try_flush_parked(slot, session_id);
        // Staged events this subscription was entitled to — admitted by
        // its filter, stamped after it attached — are part of what it
        // never received, whatever its lag state says.
        let stranded_for_slot = stranded
            .iter()
            .filter(|event| event.seq >= slot.first_live_seq && slot.filters.admits(event))
            .count() as u64;
        // Everything this subscription was owed and will not get: what the
        // staging queue still held, and what a delivery already skipped on
        // its way to deferring to this close.
        let extra = stranded_for_slot + slot.deferred_losses;
        let lost = match &slot.lag {
            LagState::Healthy if extra == 0 => continue,
            LagState::Healthy => extra,
            LagState::Parked { .. } => 1 + extra,
            LagState::Lossy { lost, .. } => *lost + extra,
        };
        // A shortfall at close, not a lag verdict: the grace window may
        // never have expired, and the session is ending rather than
        // continuing, so this carries a count of its own instead of the
        // `subscriber_lagging` code whose published contract says the
        // opposite of both. set() can fail only if a lag seal raced this
        // close to the same slot, which the single-drainer discipline
        // excludes; a duplicate announcement would be a bug, so say so.
        // `let _` rather than an expect: a slot is closed exactly once —
        // the closer owns it — but this also runs from a drain's unwind,
        // and a panic inside a Drop that is already unwinding aborts the
        // process. An impossible case is not worth that trade.
        let _ = slot
            .terminal
            .set(Terminal::SealedWithLoss { events_lost: lost });
        tracing::warn!(
            session_id = ?session_id,
            slot_id = slot.id,
            events_lost = lost,
            "session sealed with events the subscriber never received; \
             announced through its undelivered_at_seal"
        );
    }
    drop_slots(slots);
}

/// Queue permits a send may take. Reading `capacity` races nothing —
/// only the single drainer sends, and the receiver draining can only make
/// room, never take it.
fn free_permits(slot: &SubscriberSlot) -> usize {
    slot.sender.capacity()
}

/// Deliver to every slot, then release the ones that are finished through
/// the contained drop path.
///
/// `retain_mut` would be the obvious spelling and is the wrong one: it
/// drops a rejected slot inline, inside its own iteration, and dropping a
/// slot's last sender wakes the receiver parked on it. A lag seal sends
/// nothing — the disconnect payload travels beside the stream — so that
/// drop is the *only* wake a sealed subscriber gets, and a panicking waker
/// would ride it straight out of the publisher or the sweep that happened
/// to be delivering, past the containment the send site already provides.
/// Rejected slots are collected and handed to [`drop_slots`] instead.
fn deliver_and_release(
    slots: &mut Vec<SubscriberSlot>,
    mut keep: impl FnMut(&mut SubscriberSlot) -> bool,
) {
    let finished: Vec<SubscriberSlot> = slots.extract_if(.., |slot| !keep(slot)).collect();
    drop_slots(finished);
}

/// Drop subscriber slots outside every lock, containing the wake that a
/// closing queue fires.
///
/// Dropping the last sender wakes a parked receiver, so this is the second
/// place consumer code runs inside the bus — and the more dangerous one:
/// an unwind here would abort the close path part-way through announcing
/// other subscribers' losses, and a wake that panics while another unwind
/// is already in progress aborts the process outright. Contained per slot,
/// a panicking waker costs only the subscription that installed it.
fn drop_slots<I>(slots: I)
where
    I: IntoIterator<Item = SubscriberSlot>,
{
    for slot in slots {
        let slot_id = slot.id;
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(slot)));
        if dropped.is_err() {
            tracing::error!(
                slot_id,
                "a subscriber's waker panicked as its stream closed; contained to that \
                 subscription"
            );
        }
    }
}

/// Push one event into the slot's queue; false when the slot is finished
/// with — the receiver is gone (the cleanup path for a `Subscription`
/// dropped mid-publish), or its waker panicked.
///
/// The send is the one place in this bus where consumer code runs: a
/// `try_send` that finds a parked receiver calls its waker, and a waker is
/// supplied by whoever polls [`Subscription::recv`]. A runtime-scheduled
/// receiver's waker only enqueues its task, but a hand-rolled one can run
/// anything, and this policy exists precisely so that one badly-behaved
/// subscriber cannot damage a session. Letting its panic unwind would
/// carry it into whichever thread happened to be delivering — a publisher,
/// or the close path mid-way through announcing other subscribers' losses
/// — and kill an uninvolved party for a fault it had no part in. So the
/// unwind stops here: the offending subscription is dropped, loudly, and
/// everyone else's delivery continues. Only `try_send` is inside the
/// guard, so a panic from this crate's own logic (the unreachable below,
/// an allocator failure) still propagates as it should.
fn push_to_queue(slot: &SubscriberSlot, event: Arc<Event>) -> bool {
    let sent =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| slot.sender.try_send(event)));
    match sent {
        Ok(Ok(())) => true,
        Ok(Err(TrySendError::Closed(_))) => false,
        Ok(Err(TrySendError::Full(_))) => {
            unreachable!("sends check a free permit first, and only the drainer sends")
        }
        Err(_) => {
            tracing::error!(
                slot_id = slot.id,
                "a subscriber's waker panicked during delivery; that subscription is dropped \
                 and the session continues"
            );
            false
        }
    }
}

/// End one subscription for lag: publish the `transport.error` payload
/// through the subscription's out-of-band terminal cell, record the
/// action, and drop the slot. Returns whether the slot stays — `false`
/// once sealed, because the seal *is* the removal, which is what makes
/// sealing idempotent under racing triggers: a second trigger finds no
/// slot to seal.
///
/// The one case where an expired deadline does *not* seal is a session
/// that closed underneath this drain. Sealing leaves the slots with
/// whichever drainer holds them, so a drain in flight can reach an expired
/// deadline moments after `seal_session` returned; both facts are true
/// then, and the close wins — the alternative tells a subscriber it was
/// disconnected for lag by a session that was ending regardless, and
/// counts a supervisor action for a disconnect nobody performed. Deciding
/// that requires the authoritative flag rather than the sweep's hint, and
/// requires deciding and committing together: a check outside the lock
/// leaves room for the seal to land before the verdict does. So the read
/// of `sealed` and the write of the terminal share one critical section —
/// short, and holding nothing arbitrary: the payload is built before it,
/// the counter and the log come after.
///
/// The public surface cannot force that interleaving — a waker registered
/// to fire mid-pass fires when its future is dropped, before the window
/// opens — so the guard is a unit test that assembles the state a drain
/// would meet instead of racing for it
/// ([`tests::a_seal_that_landed_first_denies_the_lag_verdict`]). It leaves
/// the sweep's hint unset, so a verdict consulting anything but the
/// authoritative flag under this lock fails it.
///
/// The terminal is deliberately not an event in the stream: `seq` is
/// canonical — per-session, gap-free at generation — so a synthesized
/// terminal event would either duplicate a real event's `seq` or pre-use
/// the next one, and a consumer treating it as a resume cursor would skip
/// real history. The typed payload beside the stream carries the same
/// explanation without touching the sequence domain, whatever the
/// subscription's filter admits — why a stream ends is part of every
/// subscription's contract.
fn seal_for_lag(slot: &mut SubscriberSlot, cx: &DeliverCx<'_>) -> bool {
    // `events_lost` counts what the policy dropped while the subscription
    // was live: the parked event and everything counted into a lossy
    // episode. The event whose delivery happens to observe the expiry is
    // not part of it — it is the first event past the stream's end, no
    // different from every later one the disconnected subscriber will not
    // see — which keeps the count identical whether a publish or the
    // sweep observed the same expired deadline.
    let lost = match &slot.lag {
        LagState::Parked { .. } => 1,
        LagState::Lossy { lost, .. } => *lost,
        LagState::Healthy => {
            unreachable!("only an expired grace deadline seals, and Healthy carries none")
        }
    };
    let grace_ms = u64::try_from(cx.backpressure.grace.as_millis()).unwrap_or(u64::MAX);
    let mut detail = serde_json::Map::new();
    detail.insert("events_lost".to_owned(), lost.into());
    detail.insert(
        "queue_bound".to_owned(),
        u64::try_from(cx.backpressure.queue_bound)
            .unwrap_or(u64::MAX)
            .into(),
    );
    detail.insert("grace_ms".to_owned(), grace_ms.into());
    let losses = if lost == 1 {
        "1 event was lost".to_owned()
    } else {
        format!("{lost} events were lost")
    };
    let terminal = Terminal::Lagging(TransportErrorPayload {
        code: TransportErrorCode::SubscriberLagging,
        message: format!(
            "subscriber failed to drain within the {grace_ms} ms grace window; {losses}"
        ),
        detail,
    });
    {
        let state = lock(cx.state);
        if state.sealed {
            // The session closed first. Keeping the slot hands it to the
            // merge a step later, where the close path announces the
            // shortfall without a verdict.
            return true;
        }
        slot.terminal
            .set(terminal)
            .expect("a slot seals at most once: the seal removes it from the fanout list");
    }
    cx.metrics.record_disconnect_subscriber();
    tracing::warn!(
        session_id = ?cx.session_id,
        slot_id = slot.id,
        events_lost = lost,
        grace_ms,
        "slow subscriber disconnected (subscriber_lagging); the session continues"
    );
    false
}

/// The bus's lock discipline in one place: every critical section is
/// short, nothing is awaited or called back into while holding one — the
/// wake-producing sends live in the drain step, outside every lock — and
/// none of the code inside can panic short of the allocator failing. A
/// poisoned lock is therefore not a state this bus can reach from its own
/// code, and recovering the map or a fanout list in unknown shape would be
/// worse than saying so loudly. (The drain itself runs caller-supplied
/// wakers; its unwind path is [`DrainGuard`]'s, which is why that one
/// tolerates poison instead of using this.)
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .expect("bus lock poisoned: a publish or subscribe panicked")
}

#[cfg(test)]
mod tests {
    use agent_bridge_events::{EventKind, LifecycleTurnStarted};

    use super::*;

    fn body() -> EventBody {
        EventBody::new(EventKind::LifecycleTurnStarted(LifecycleTurnStarted {}))
    }

    fn subscriber_count(bus: &EventBus, session_id: &str) -> usize {
        lock(
            &lock(&bus.inner.sessions)
                .get(session_id)
                .cloned()
                .unwrap()
                .state,
        )
        .subscribers
        .len()
    }

    #[test]
    fn subscribe_drop_10k_cycles_no_leak() {
        let bus = EventBus::new(BusConfig::default());
        let publisher = bus.register_session("s".into()).unwrap();
        for _ in 0..10_000 {
            let subscription = bus.subscribe("s", EventFilter::All).unwrap();
            drop(subscription);
        }
        assert_eq!(subscriber_count(&bus, "s"), 0, "session slots leaked");
        // The publish path still works and no dead slot is visited: the
        // event goes nowhere and the list stays empty.
        publisher.publish(body()).unwrap();
        assert_eq!(subscriber_count(&bus, "s"), 0);

        for _ in 0..10_000 {
            drop(bus.subscribe_global(Vec::new()));
        }
        assert_eq!(
            lock(&bus.inner.global.state).subscribers.len(),
            0,
            "global slots leaked"
        );
    }

    #[test]
    fn a_second_publisher_for_a_live_session_is_refused() {
        let bus = EventBus::new(BusConfig::default());
        let _publisher = bus.register_session("s".into()).unwrap();
        assert!(matches!(
            bus.register_session("s".into()),
            Err(BusError::PublisherExists(id)) if id == "s"
        ));
    }

    #[test]
    fn unknown_sessions_are_refused_by_name() {
        let bus = EventBus::new(BusConfig::default());
        assert!(matches!(
            bus.subscribe("ghost", EventFilter::All),
            Err(BusError::UnknownSession(id)) if id == "ghost"
        ));
        assert_eq!(
            bus.seal_session("ghost"),
            Err(BusError::UnknownSession("ghost".into()))
        );
    }

    #[test]
    fn a_sealed_session_refuses_everything_and_seals_idempotently() {
        let bus = EventBus::new(BusConfig::default());
        let publisher = bus.register_session("s".into()).unwrap();
        bus.seal_session("s").unwrap();
        bus.seal_session("s").unwrap();
        assert_eq!(publisher.publish(body()), Err(BusError::Sealed("s".into())));
        assert!(matches!(
            bus.subscribe("s", EventFilter::All),
            Err(BusError::Sealed(_))
        ));
        assert!(matches!(
            bus.register_session("s".into()),
            Err(BusError::Sealed(_))
        ));
    }

    #[test]
    #[should_panic(expected = "queue_bound")]
    fn a_zero_queue_bound_is_refused_at_construction() {
        let _ = EventBus::new(BusConfig {
            backpressure: BackpressureConfig {
                queue_bound: 0,
                ..BackpressureConfig::default()
            },
            ..BusConfig::default()
        });
    }

    #[test]
    #[should_panic(expected = "queue_bound")]
    fn an_unbounded_queue_bound_is_refused_at_construction() {
        // usize::MAX as an "unbounded" spelling would otherwise pass the
        // constructor and panic inside the channel at the first subscribe.
        let _ = EventBus::new(BusConfig {
            backpressure: BackpressureConfig {
                queue_bound: usize::MAX,
                ..BackpressureConfig::default()
            },
            ..BusConfig::default()
        });
    }

    /// The backlog-adoption path, exercised where it can be reached.
    ///
    /// Consumer wakers can no longer orphan a backlog — panics are
    /// contained at the send and at the slot drop — so the state this
    /// recovers from is now only reachable from a panic in this crate's
    /// own code or the allocator. That is exactly why the recovery stays,
    /// and why its test builds the state directly: a drain that died
    /// between staging and delivery, leaving events whose publishers were
    /// told `Ok`. The next claim must adopt them first, ahead of its own
    /// newer event.
    #[test]
    fn a_backlog_orphaned_by_a_dead_drain_is_adopted_before_newer_events() {
        let bus = EventBus::new(BusConfig::default());
        let publisher = bus.register_session("s".into()).unwrap();
        let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();
        let channel = lock(&bus.inner.sessions).get("s").cloned().unwrap();

        // A drain that stamped, staged, and then died — the drainer flag
        // reset by its guard, the staged event left behind.
        let orphan = {
            let mut state = lock(&channel.state);
            let seq = state.next_seq;
            state.next_seq += 1;
            let orphan = Arc::new(Event {
                schema_version: SCHEMA_VERSION,
                session_id: Some("s".to_owned()),
                seq,
                monotonic_ns: None,
                ts: "2026-08-18T00:00:00.000Z".to_owned(),
                approval_id: None,
                correlation_id: None,
                kind: body().kind,
            });
            state.staged.push_back(Arc::clone(&orphan));
            orphan
        };

        publisher.publish(body()).unwrap();

        let received: Vec<u64> = std::iter::from_fn(|| subscription.receiver.try_recv().ok())
            .map(|event| event.seq)
            .collect();
        assert_eq!(
            received,
            [orphan.seq, orphan.seq + 1],
            "the orphaned event must be adopted ahead of the claiming publish's own"
        );
    }

    /// The close-versus-lag decision, built directly rather than raced.
    ///
    /// Through the public surface this interleaving cannot be forced — a
    /// waker registered to fire mid-pass fires when its future is dropped
    /// instead, long before the window opens — so the state is assembled
    /// here the way a drain would meet it: a subscriber already past its
    /// deadline, and a session sealed a moment earlier. The seal hint is
    /// deliberately left false, so a verdict that consulted the hint
    /// instead of the authoritative flag would still seal and fail this
    /// test.
    #[tokio::test(start_paused = true)]
    async fn a_seal_that_landed_first_denies_the_lag_verdict() {
        use backpressure::{ArmedState, LagState};

        let bus = EventBus::new(BusConfig::default());
        let _publisher = bus.register_session("s".into()).unwrap();
        let subscription = bus.subscribe("s", EventFilter::All).unwrap();
        let channel = lock(&bus.inner.sessions).get("s").cloned().unwrap();
        // Somewhere for the deadline to be behind.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        {
            let mut state = lock(&channel.state);
            let deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(500);
            state.subscribers[0].lag = LagState::Lossy {
                deadline: ArmedState::Armed(deadline),
                lost: 3,
            };
            // The session closed first; the hint the sweep filters on is
            // left untouched on purpose.
            state.sealed = true;
        }

        channel.sweep();

        assert_eq!(
            subscription.disconnect_reason(),
            None,
            "a session that sealed first must not produce a lag verdict"
        );
        assert_eq!(subscription.disconnect_error(), None);
        assert_eq!(
            subscription.undelivered_at_seal(),
            Some(3),
            "the losses are announced as a shortfall at close"
        );
        assert_eq!(
            bus.metrics().disconnect_subscriber_count(),
            0,
            "no supervisor action for a disconnect nobody performed"
        );
    }

    /// A seal that finds a backlog with no owner counts it rather than
    /// abandoning it. The state is built directly for the same reason as
    /// the test above: consumer wakers can no longer orphan a backlog, so
    /// what is being guarded is the recovery, not a route to it.
    #[test]
    fn a_seal_counts_a_backlog_no_drain_will_ever_adopt() {
        let bus = EventBus::new(BusConfig::default());
        let publisher = bus.register_session("s".into()).unwrap();
        let subscription = bus.subscribe("s", EventFilter::All).unwrap();
        let channel = lock(&bus.inner.sessions).get("s").cloned().unwrap();

        // Two events accepted from their publisher's point of view, left
        // staged by a drain that died before delivering them.
        for _ in 0..2 {
            let mut state = lock(&channel.state);
            let seq = state.next_seq;
            state.next_seq += 1;
            state.staged.push_back(Arc::new(Event {
                schema_version: SCHEMA_VERSION,
                session_id: Some("s".to_owned()),
                seq,
                monotonic_ns: None,
                ts: "2026-08-19T00:00:00.000Z".to_owned(),
                approval_id: None,
                correlation_id: None,
                kind: body().kind,
            }));
        }
        drop(publisher);

        bus.seal_session("s").unwrap();

        assert_eq!(
            subscription.undelivered_at_seal(),
            Some(2),
            "accepted events stranded by a dead drain are counted at close"
        );
        assert_eq!(subscription.disconnect_reason(), None);
    }

    /// A drain that unwinds out of a channel already sealed behind it
    /// leaves nobody to finish the close: the sweep skips sealed channels
    /// and every publish is refused. So the guard finishes it — the
    /// subscribers that attached mid-drain get their streams ended and
    /// their share of the stranded backlog counted, rather than waiting on
    /// a stream that never ends.
    #[test]
    fn an_unwind_out_of_a_sealed_channel_still_closes_its_subscribers() {
        let bus = EventBus::new(BusConfig::default());
        let publisher = bus.register_session("s".into()).unwrap();
        let mut subscription = bus.subscribe("s", EventFilter::All).unwrap();
        let channel = lock(&bus.inner.sessions).get("s").cloned().unwrap();

        // The state a drain would be holding when it dies: it owns the
        // drainer role, a seal has landed behind it, one subscriber
        // attached mid-drain, and a staged tail is waiting.
        {
            let mut state = lock(&channel.state);
            state.draining = true;
            state.sealed = true;
            for _ in 0..2 {
                let seq = state.next_seq;
                state.next_seq += 1;
                state.staged.push_back(Arc::new(Event {
                    schema_version: SCHEMA_VERSION,
                    session_id: Some("s".to_owned()),
                    seq,
                    monotonic_ns: None,
                    ts: "2026-08-19T00:00:00.000Z".to_owned(),
                    approval_id: None,
                    correlation_id: None,
                    kind: body().kind,
                }));
            }
        }
        drop(publisher);

        // The unwind itself.
        drop(DrainGuard {
            channel: &channel,
            defused: false,
        });

        assert!(
            matches!(
                subscription.receiver.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ),
            "the subscriber was left waiting on a stream that never ends"
        );
        assert_eq!(
            subscription.undelivered_at_seal(),
            Some(2),
            "the stranded backlog is counted rather than vanishing with the channel"
        );
        assert!(!lock(&channel.state).draining, "the drainer flag is reset");
    }

    #[test]
    fn the_publisher_names_its_session() {
        let bus = EventBus::new(BusConfig::default());
        let publisher = bus.register_session("named".into()).unwrap();
        assert_eq!(publisher.session_id(), "named");
    }
}
