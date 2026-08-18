//! One subscriber's read side: a bounded queue of shared events.
//!
//! Events arrive as `Arc<Event>` — stamped once, shared N ways — because a
//! payload can be large (the frame cap upstream is measured in mebibytes)
//! and cloning it per subscriber would turn fanout width into a memory
//! multiplier.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use agent_bridge_events::{Event, TransportErrorPayload};
use tokio::sync::mpsc;

use super::Channel;

/// What the bus wrote beside a stream at its end: the disconnect verdict,
/// when the bus ended the subscription for cause, and the loss
/// announcement either kind of end can carry.
#[derive(Debug)]
pub(crate) struct Terminal {
    /// `Some` only for a bus-initiated disconnect; a session seal that
    /// merely could not hand everything over announces its loss with no
    /// verdict attached.
    pub(crate) reason: Option<DisconnectReason>,
    pub(crate) error: TransportErrorPayload,
}

/// Why the bus ended a subscription for cause, from
/// [`Subscription::disconnect_reason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// Sealed by the bus for failing to drain within the lag grace window.
    /// [`Subscription::disconnect_error`] carries the full
    /// `transport.error` payload of code `subscriber_lagging`, stating
    /// what was lost. The transport layer emits that payload on the wire
    /// and follows it with `session.eof { reason: "subscriber_lagging" }`,
    /// answering the subscriber's subsequent calls with `-32011` — all of
    /// which lands at the transport layer, not here.
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
    /// Written by the bus once, at whichever end-of-stream carries
    /// something to say — a lag disconnect, or a session seal that could
    /// not hand every accepted event over. Deliberately not an event in
    /// the stream: the envelope's `seq` is canonical, per-session, and
    /// gap-free at generation, so a synthesized terminal event would
    /// either duplicate a real event's `seq` or pre-use the next one —
    /// and a consumer that treated it as a resume cursor would skip real
    /// history. What ended the stream travels beside it, as a typed value.
    pub(crate) terminal: Arc<OnceLock<Terminal>>,
    /// Flipped by [`Subscription::recv`] when the replay buffer empties;
    /// the bus keeps the lag grace window unarmed until then, because a
    /// subscriber catching up on instruction is not lagging.
    pub(crate) replay_drained: Arc<AtomicBool>,
}

impl Subscription {
    /// The next matching event, in `seq` order — replayed history first
    /// where the subscription carries any, then the live stream. Every
    /// event this yields is canonical: really stamped, really in the
    /// session's history.
    ///
    /// `None` means the stream is over. Two ends exist: the session was
    /// sealed and every queued event has been drained, or the bus
    /// disconnected this subscriber for lag — distinguished by
    /// [`Subscription::disconnect_reason`], with the full
    /// `transport.error { code: subscriber_lagging }` payload (and its
    /// `events_lost` count — never a silent loss) in
    /// [`Subscription::disconnect_error`]. Events already queued when a
    /// seal landed are still delivered before the end; a session sealed
    /// normally flushes a policy-parked event too where queue room exists,
    /// and a loss it cannot avoid is announced through
    /// [`Subscription::disconnect_error`] with no verdict attached —
    /// never silently absorbed.
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
    /// stream is live, and `None` after a stream that ended without a
    /// bus-initiated cause (session sealed, runtime shutdown). Set at the
    /// moment of the seal, which can be observed before the stream's tail
    /// has been drained.
    pub fn disconnect_reason(&self) -> Option<DisconnectReason> {
        self.terminal.get().and_then(|terminal| terminal.reason)
    }

    /// The `transport.error` payload of code `subscriber_lagging` — with
    /// the loss count in its detail — for the transport layer to emit on
    /// the wire. Present after a lag disconnect, and also after a session
    /// seal that could not hand over everything the subscription had
    /// accepted (a parked event no queue room could take, or a lossy
    /// episode the session ended before its deadline did): the loss is
    /// announced to the subscriber either way, never only logged. `None`
    /// when the stream ended complete.
    pub fn disconnect_error(&self) -> Option<&TransportErrorPayload> {
        self.terminal.get().map(|terminal| &terminal.error)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.channel.detach(self.slot_id);
    }
}
