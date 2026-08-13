//! One subscriber's read side: a bounded queue of shared events.
//!
//! Events arrive as `Arc<Event>` — stamped once, shared N ways — because a
//! payload can be large (the frame cap upstream is measured in mebibytes)
//! and cloning it per subscriber would turn fanout width into a memory
//! multiplier.

use std::collections::VecDeque;
use std::sync::Arc;

use agent_bridge_events::Event;
use tokio::sync::mpsc;

use super::Channel;

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
}

impl Subscription {
    /// The next matching event, in `seq` order — replayed history first
    /// where the subscription carries any, then the live stream.
    ///
    /// `None` means the stream is over: the session was sealed and every
    /// queued event has been drained. The interim overflow behavior can
    /// also drop events for a subscriber that fell behind a full queue —
    /// the backpressure stage replaces that with the contractual
    /// overflow-and-disconnect policy, which will say *why* a stream ended
    /// rather than only that it did. (Whether time spent draining a replay
    /// buffer counts against that policy's lag grace is that stage's
    /// decision to make; the buffer is visible to it here.)
    ///
    /// A *global* subscription's stream never ends this way today: the
    /// global channel has no seal, so a consumer that must terminate
    /// observes shutdown by other means until the wire layers give the
    /// runtime a close path. The same holds for a session that is dropped
    /// without ever being sealed.
    pub async fn recv(&mut self) -> Option<Arc<Event>> {
        if let Some(event) = self.replay.pop_front() {
            return Some(event);
        }
        self.receiver.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.channel.detach(self.slot_id);
    }
}
