//! One subscriber's read side: a bounded queue of shared events.
//!
//! Events arrive as `Arc<Event>` — stamped once, shared N ways — because a
//! payload can be large (the frame cap upstream is measured in mebibytes)
//! and cloning it per subscriber would turn fanout width into a memory
//! multiplier.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use agent_bridge_events::Event;
use tokio::sync::mpsc;

use super::Channel;

/// Why the bus ended a subscription for cause, from
/// [`Subscription::disconnect_reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Sealed by the bus for failing to drain within the lag grace window.
    /// The stream's last event was the terminal `transport.error` of code
    /// `subscriber_lagging`. The transport layer translates this to
    /// `session.eof { reason: "subscriber_lagging" }` and answers the
    /// subscriber's subsequent calls with `-32011` — both halves land at
    /// the transport layer, not here.
    Lagging,
}

/// A live subscription, as returned by
/// [`EventBus::subscribe`](super::EventBus::subscribe),
/// [`EventBus::subscribe_global`](super::EventBus::subscribe_global), and —
/// with its replay buffer preloaded —
/// [`EventBus::subscribe_from`](super::EventBus::subscribe_from).
///
/// Dropping it is unsubscribing: the subscriber's slot leaves the fanout
/// list immediately and its queue is freed, so subscribe/drop churn leaves
/// nothing behind. Removal is eager rather than swept at the next publish
/// because a session can be subscribed to and abandoned many times without
/// anything being published in between.
#[derive(Debug)]
pub struct Subscription {
    /// Backfill, delivered before anything live: captured in the same
    /// critical section that registered the slot at head, so draining this
    /// and then the queue reads one contiguous `seq` sequence. Held here
    /// rather than pushed through the queue because a replay can be the
    /// whole ring — larger than any sane queue bound — and `Arc`s make the
    /// buffer refcounts, not copies. Empty on plain subscriptions.
    pub(crate) replay: VecDeque<Arc<Event>>,
    pub(crate) receiver: mpsc::Receiver<Arc<Event>>,
    pub(crate) channel: Arc<Channel>,
    pub(crate) slot_id: u64,
    /// Written by the bus at seal-for-cause, read by
    /// [`Subscription::disconnect_reason`].
    pub(crate) reason: Arc<OnceLock<DisconnectReason>>,
    /// Flipped by [`Subscription::recv`] when the replay buffer empties;
    /// the bus keeps the lag grace window unarmed until then, because a
    /// subscriber catching up on instruction is not lagging.
    pub(crate) replay_drained: Arc<AtomicBool>,
}

impl Subscription {
    /// The next matching event, in `seq` order — replayed history first
    /// where the subscription carries any, then the live stream.
    ///
    /// `None` means the stream is over. Two ends exist: the session was
    /// sealed and every queued event has been drained, or the bus
    /// disconnected this subscriber for lag — in which case the stream's
    /// final event was a terminal `transport.error` of code
    /// `subscriber_lagging` (delivered whatever the subscription's filter
    /// says: why a stream ends is part of every subscription's contract),
    /// and [`Subscription::disconnect_reason`] says so afterwards. Events
    /// already queued when the seal landed are still delivered before the
    /// terminal one; what the policy dropped beyond the queue is counted
    /// in the terminal event's `events_lost` detail, never lost silently.
    ///
    /// A *global* subscription's stream never ends via session seal: the
    /// global channel has no seal, so short of a lag disconnect a consumer
    /// that must terminate observes shutdown by other means until the wire
    /// layers give the runtime a close path. The same holds for a session
    /// that is dropped without ever being sealed.
    pub async fn recv(&mut self) -> Option<Arc<Event>> {
        if let Some(event) = self.replay.pop_front() {
            if self.replay.is_empty() {
                // The grace window may now arm; Relaxed is enough because
                // observing the flip a beat late only starts it one policy
                // touch later.
                self.replay_drained.store(true, Ordering::Relaxed);
            }
            return Some(event);
        }
        self.receiver.recv().await
    }

    /// Why the bus ended this subscription, once it has — `None` while the
    /// stream is live, and `None` after a stream that ended without cause
    /// (session sealed, runtime shutdown). Set at the moment of the seal,
    /// which can be observed before the terminal event has been drained.
    pub fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.reason.get().copied()
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.channel.detach(self.slot_id);
    }
}
