//! One subscriber's read side: a bounded queue of shared events.
//!
//! Events arrive as `Arc<Event>` — stamped once, shared N ways — because a
//! payload can be large (the frame cap upstream is measured in mebibytes)
//! and cloning it per subscriber would turn fanout width into a memory
//! multiplier.

use std::sync::Arc;

use agent_bridge_events::Event;
use tokio::sync::mpsc;

use super::Channel;

/// A live subscription, as returned by
/// [`EventBus::subscribe`](super::EventBus::subscribe) and
/// [`EventBus::subscribe_global`](super::EventBus::subscribe_global).
///
/// Dropping it is unsubscribing: the subscriber's slot leaves the fanout
/// list immediately and its queue is freed, so subscribe/drop churn leaves
/// nothing behind. Removal is eager rather than swept at the next publish
/// because a session can be subscribed to and abandoned many times without
/// anything being published in between.
#[derive(Debug)]
pub struct Subscription {
    pub(crate) receiver: mpsc::Receiver<Arc<Event>>,
    pub(crate) channel: Arc<Channel>,
    pub(crate) slot_id: u64,
}

impl Subscription {
    /// The next matching event, in `seq` order.
    ///
    /// `None` means the stream is over: the session was sealed and every
    /// queued event has been drained. The interim overflow behavior can
    /// also drop events for a subscriber that fell behind a full queue —
    /// the backpressure stage replaces that with the contractual
    /// overflow-and-disconnect policy, which will say *why* a stream ended
    /// rather than only that it did.
    ///
    /// A *global* subscription's stream never ends this way today: the
    /// global channel has no seal, so a consumer that must terminate
    /// observes shutdown by other means until the wire layers give the
    /// runtime a close path. The same holds for a session that is dropped
    /// without ever being sealed.
    pub async fn recv(&mut self) -> Option<Arc<Event>> {
        self.receiver.recv().await
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.channel.detach(self.slot_id);
    }
}
