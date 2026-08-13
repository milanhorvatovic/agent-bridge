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
//! consecutive from 0 with no gaps, every subscriber independently receives
//! every matching event in `seq` order, and publishing never blocks the
//! publisher. The critical section that makes the first two true together —
//! increment and fanout under one short lock — is in [`Channel::publish`];
//! the `try_send` discipline that makes the third true is in [`deliver`].
//!
//! Three pieces of this stage are deliberately interim, each marked at its
//! single swap site: the per-subscriber queue bound is a generous stand-in
//! until the backpressure stage lands its contractual bound and lag policy
//! ([`BusConfig`]), overflow today is warn-and-drop for the affected
//! subscriber alone ([`deliver`]) rather than that policy's
//! overflow-grace-disconnect sequence, and a sealed session's channel stays
//! in the registry map ([`EventBus::seal_session`]) — removing it, like
//! re-registering its id, is the session layer's close-path decision, not
//! one the bus can make alone.

mod filter;
mod publisher;
mod stamp;
mod subscription;

pub use filter::EventFilter;
pub use publisher::Publisher;
pub use subscription::Subscription;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use agent_bridge_events::{Event, EventBody, SCHEMA_VERSION};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;

use filter::FilterSet;

/// Tuning the bus accepts at construction.
#[derive(Debug, Clone)]
pub struct BusConfig {
    /// How many undelivered events one subscriber's queue holds before
    /// delivery to that subscriber starts failing. Must be at least 1.
    ///
    /// INTERIM: the default is a generous stand-in, not the contract. The
    /// backpressure stage replaces it with the contractual bound (default
    /// 1024) plus the grace-window and slow-subscriber disconnect policy;
    /// the field name is fixed now so that change is a policy swap, not a
    /// rework.
    pub subscriber_queue_bound: usize,
}

impl Default for BusConfig {
    fn default() -> Self {
        Self {
            subscriber_queue_bound: 16_384,
        }
    }
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
}

/// The Core-owned event bus.
///
/// Cheap to clone and share — clones see one bus. One instance is meant to
/// exist per runtime, owned by Core and reached by everything that
/// publishes or subscribes; the bus itself knows nothing of transport,
/// session internals, or adapters. It moves
/// [`agent_bridge_events`] values, and that is all.
#[derive(Debug, Clone)]
pub struct EventBus {
    inner: Arc<BusInner>,
}

#[derive(Debug)]
struct BusInner {
    config: BusConfig,
    /// The zero of every `monotonic_ns` this bus stamps: readings are
    /// comparable within a bus's lifetime, which is the runtime process's.
    anchor: Instant,
    sessions: Mutex<HashMap<String, Arc<Channel>>>,
    global: Arc<Channel>,
    /// Slot ids are minted bus-wide so a subscription's identity never
    /// collides across channels, whatever list it detaches from.
    next_slot_id: AtomicU64,
}

impl EventBus {
    /// A new, empty bus.
    ///
    /// # Panics
    ///
    /// When `subscriber_queue_bound` is 0 — a queue that can hold nothing
    /// cannot deliver anything — or above the async runtime's channel
    /// capacity ceiling, which would otherwise panic at the first
    /// subscribe, far from the misconfiguration. Either way the bad bound
    /// is a bug at the construction site, refused loudly here.
    pub fn new(config: BusConfig) -> Self {
        assert!(
            config.subscriber_queue_bound >= 1,
            "subscriber_queue_bound must be at least 1"
        );
        // The ceiling is tokio's: `mpsc::channel` panics above its
        // semaphore's permit maximum (usize::MAX >> 3). Restating it here
        // keeps the constructor's promise that a bad bound fails at the
        // call site, not on the subscribe path.
        assert!(
            config.subscriber_queue_bound <= usize::MAX >> 3,
            "subscriber_queue_bound exceeds the runtime's channel-capacity ceiling"
        );
        Self {
            inner: Arc::new(BusInner {
                config,
                anchor: Instant::now(),
                sessions: Mutex::new(HashMap::new()),
                global: Arc::new(Channel::new(None)),
                next_slot_id: AtomicU64::new(0),
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
                let channel = Arc::new(Channel::new(Some(entry.key().clone())));
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
    /// Every subscriber independently receives every event its filter
    /// admits, in `seq` order. Fails on a session this bus has never seen,
    /// and on one that is already sealed — a stream guaranteed to deliver
    /// nothing and then end is more honestly refused than returned.
    pub fn subscribe(
        &self,
        session_id: &str,
        filter: EventFilter,
    ) -> Result<Subscription, BusError> {
        let channel = self.session(session_id)?;
        channel.attach(
            FilterSet::new(vec![filter]),
            self.inner.config.subscriber_queue_bound,
            self.inner.next_slot_id.fetch_add(1, Ordering::Relaxed),
        )
    }

    /// Subscribe to the global channel — `session_id: null` events only.
    ///
    /// Each entry in `namespaces` is a dotted-name prefix with the same
    /// tolerant spellings as [`EventFilter::Prefix`]; an event matching any
    /// entry is delivered. An empty list means all global namespaces,
    /// mirroring the default the wire's subscribe method will have when it
    /// lands — the bus-side channel exists now so producers and the
    /// transport meet a finished contract.
    pub fn subscribe_global(&self, namespaces: Vec<String>) -> Subscription {
        let filters = namespaces.into_iter().map(EventFilter::Prefix).collect();
        self.inner
            .global
            .attach(
                FilterSet::new(filters),
                self.inner.config.subscriber_queue_bound,
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
    /// and then observe the end of the stream (`recv` → `None`).
    /// Idempotent, because close paths race and a second close arriving
    /// late is normal, not an error.
    pub fn seal_session(&self, session_id: &str) -> Result<(), BusError> {
        let channel = self.session(session_id)?;
        {
            let mut state = lock(&channel.state);
            state.sealed = true;
            // Dropping the senders is what turns "sealed" into an
            // observable end of stream: each receiver drains its queue and
            // then sees the channel closed.
            state.subscribers.clear();
        }
        tracing::debug!(session_id, "session sealed");
        Ok(())
    }

    fn session(&self, session_id: &str) -> Result<Arc<Channel>, BusError> {
        lock(&self.inner.sessions)
            .get(session_id)
            .cloned()
            .ok_or_else(|| BusError::UnknownSession(session_id.to_owned()))
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
    pub(crate) state: Mutex<ChannelState>,
}

#[derive(Debug)]
pub(crate) struct ChannelState {
    next_seq: u64,
    sealed: bool,
    subscribers: Vec<SubscriberSlot>,
}

#[derive(Debug)]
struct SubscriberSlot {
    id: u64,
    filters: FilterSet,
    sender: mpsc::Sender<Arc<Event>>,
    /// Events dropped since this subscriber's current overflow episode
    /// began; 0 while it keeps up. Interim-policy bookkeeping that lets
    /// the publish path log an episode's edges instead of every loss.
    dropped_in_episode: u64,
}

impl Channel {
    fn new(session_id: Option<String>) -> Self {
        Self {
            session_id,
            state: Mutex::new(ChannelState {
                next_seq: 0,
                sealed: false,
                subscribers: Vec::new(),
            }),
        }
    }

    /// The choke point: complete the envelope and fan it out, atomically
    /// with the sequence increment.
    ///
    /// The increment and the queue pushes share one critical section
    /// deliberately. An atomic counter alone would let two publishes
    /// stamp 5 and 6 and then push 6 before 5, and "each subscriber's
    /// queue order is `seq` order" would quietly become "usually". The
    /// monotonic reading sits inside the lock for the same reason: a later
    /// `seq` never carries an earlier `monotonic_ns`. The wall-clock read
    /// and its formatting do not — `ts` is documented as not an ordering
    /// key, so it costs the critical section nothing. Correctness first —
    /// the publish-path benchmark is what says whether this lock ever
    /// becomes worth splitting.
    pub(crate) fn publish(&self, body: EventBody, anchor: Instant) -> Result<u64, BusError> {
        let ts = stamp::rfc3339_millis(SystemTime::now());
        let session_id = self.session_id.clone();
        let mut edges: Vec<OverflowEdge> = Vec::new();
        let (seq, event) = {
            let mut state = lock(&self.state);
            if state.sealed {
                return Err(BusError::Sealed(session_id.unwrap_or_default()));
            }
            let seq = state.next_seq;
            state.next_seq += 1;
            let event = Arc::new(Event {
                schema_version: SCHEMA_VERSION,
                session_id,
                seq,
                monotonic_ns: Some(u64::try_from(anchor.elapsed().as_nanos()).unwrap_or(u64::MAX)),
                ts,
                approval_id: body.approval_id,
                correlation_id: body.correlation_id,
                kind: body.kind,
            });
            state
                .subscribers
                .retain_mut(|slot| deliver(slot, &event, &mut edges));
            (seq, event)
        };
        // Reported only after the guard is gone: a tracing subscriber is
        // arbitrary code, and the one rule of this bus's critical sections
        // is that nothing is called back into while one is held. Only the
        // *edges* of an overflow episode are logged — begin, and end with
        // the drop count — so a stalled subscriber under sustained load
        // costs two log lines, not one per lost event, and publisher
        // latency never rides the tracing sink.
        for edge in edges {
            match edge {
                OverflowEdge::Began { slot_id } => tracing::warn!(
                    session_id = ?event.session_id,
                    slot_id,
                    seq,
                    event_type = event.kind.event_type(),
                    "subscriber queue full; dropping its events until it drains (interim policy)"
                ),
                OverflowEdge::Ended { slot_id, dropped } => tracing::warn!(
                    session_id = ?event.session_id,
                    slot_id,
                    seq,
                    dropped,
                    "subscriber caught up; events were dropped during the overflow episode"
                ),
            }
        }
        Ok(seq)
    }

    fn attach(
        self: &Arc<Self>,
        filters: FilterSet,
        queue_bound: usize,
        slot_id: u64,
    ) -> Result<Subscription, BusError> {
        let (sender, receiver) = mpsc::channel(queue_bound);
        {
            let mut state = lock(&self.state);
            if state.sealed {
                return Err(BusError::Sealed(
                    self.session_id.clone().unwrap_or_default(),
                ));
            }
            state.subscribers.push(SubscriberSlot {
                id: slot_id,
                filters,
                sender,
                dropped_in_episode: 0,
            });
        }
        tracing::debug!(session_id = ?self.session_id, slot_id, "subscribed");
        Ok(Subscription {
            receiver,
            channel: Arc::clone(self),
            slot_id,
        })
    }

    /// Remove one subscriber's slot; called from `Subscription::drop`.
    pub(crate) fn detach(&self, slot_id: u64) {
        // `if let` rather than a panic: this runs during unwinding when a
        // subscriber's task dies, and a poisoned lock there must not turn
        // one panic into an abort. The slot's sender dies with the list
        // entry either way.
        if let Ok(mut state) = self.state.lock() {
            state.subscribers.retain(|slot| slot.id != slot_id);
        }
        tracing::debug!(session_id = ?self.session_id, slot_id, "unsubscribed");
    }
}

/// The two loggable boundaries of a subscriber's overflow episode. Between
/// them, drops are counted, not logged.
enum OverflowEdge {
    Began { slot_id: u64 },
    Ended { slot_id: u64, dropped: u64 },
}

/// Hand one event to one subscriber, without ever waiting; returns whether
/// the slot stays in the fanout list.
///
/// This function is the backpressure stage's single swap site. INTERIM: a
/// full queue drops the event *for that subscriber alone*, counted per
/// episode with only the episode's edges surfaced for logging once the
/// lock is released — acceptable only while the bound is generous and
/// nothing user-facing ships on this path; the contractual policy
/// (overflow slot, grace window, disconnect — never a silent drop)
/// replaces this body, and an episode that never ends before the session
/// seals reports only its beginning. A closed queue means the receiver is
/// gone, so the slot goes too; that is the cleanup path for a
/// `Subscription` dropped mid-publish, where eager detach and this sweep
/// race benignly.
///
/// One nuance of the never-call-back-in rule: `try_send` may fire the
/// receiver's waker while the channel lock is held. A runtime-scheduled
/// receiver's waker only enqueues its task, which is the safe side of the
/// rule; a hand-rolled waker that ran consumer code inline in `wake()`
/// would reintroduce the callback this discipline exists to exclude. None
/// exists in this workspace, and the delivery mechanics are this same swap
/// site's to change if one ever must.
fn deliver(slot: &mut SubscriberSlot, event: &Arc<Event>, edges: &mut Vec<OverflowEdge>) -> bool {
    if !slot.filters.admits(event) {
        return true;
    }
    match slot.sender.try_send(Arc::clone(event)) {
        Ok(()) => {
            if slot.dropped_in_episode > 0 {
                edges.push(OverflowEdge::Ended {
                    slot_id: slot.id,
                    dropped: slot.dropped_in_episode,
                });
                slot.dropped_in_episode = 0;
            }
            true
        }
        Err(TrySendError::Full(_)) => {
            if slot.dropped_in_episode == 0 {
                edges.push(OverflowEdge::Began { slot_id: slot.id });
            }
            slot.dropped_in_episode += 1;
            true
        }
        Err(TrySendError::Closed(_)) => false,
    }
}

/// The bus's lock discipline in one place: every critical section is short,
/// nothing is awaited or called back into while holding one (the one
/// nuance, channel wakers, is documented at [`deliver`]), and none of
/// the code inside can panic short of the allocator failing — so a
/// poisoned lock is not a state this bus can reach from its own code, and
/// recovering the map or a fanout list in unknown shape would be worse
/// than saying so loudly.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
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
    #[should_panic(expected = "subscriber_queue_bound")]
    fn a_zero_queue_bound_is_refused_at_construction() {
        let _ = EventBus::new(BusConfig {
            subscriber_queue_bound: 0,
        });
    }

    #[test]
    #[should_panic(expected = "subscriber_queue_bound")]
    fn an_unbounded_queue_bound_is_refused_at_construction() {
        // usize::MAX as an "unbounded" spelling would otherwise pass the
        // constructor and panic inside the channel at the first subscribe.
        let _ = EventBus::new(BusConfig {
            subscriber_queue_bound: usize::MAX,
        });
    }

    #[test]
    fn the_publisher_names_its_session() {
        let bus = EventBus::new(BusConfig::default());
        let publisher = bus.register_session("named".into()).unwrap();
        assert_eq!(publisher.session_id(), "named");
    }
}
