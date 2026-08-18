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

/// What the bus wrote beside a stream at its end. The two ends are
/// different facts and stay different values: the bus disconnecting a
/// subscriber for lag is the `transport.error` the wire already has a code
/// for, while a session seal that could not hand over everything it had
/// accepted is a shortfall at close — the grace window may never have
/// expired, and the session is ending rather than continuing, so labelling
/// it `subscriber_lagging` would hand a consumer routing on that code a
/// false diagnosis.
#[derive(Debug)]
pub(crate) enum Terminal {
    /// The bus ended this subscription for failing to drain within grace.
    Lagging(TransportErrorPayload),
    /// The session's seal ended the stream while this subscription still
    /// had accepted events it could not be handed.
    SealedWithLoss { events_lost: u64 },
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
    /// [`Subscription::undelivered_at_seal`] — never silently absorbed,
    /// and never dressed up as the lag disconnect it is not.
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
        match self.terminal.get() {
            Some(Terminal::Lagging(_)) => Some(DisconnectReason::Lagging),
            Some(Terminal::SealedWithLoss { .. }) | None => None,
        }
    }

    /// The `transport.error` payload of code `subscriber_lagging` — with
    /// the loss count in its detail — for the transport layer to emit on
    /// the wire ahead of `session.eof`. Present only after a lag
    /// disconnect: that code's published contract says the runtime
    /// disconnected a lagging subscriber *and the session continues*, so a
    /// shortfall at session close is reported by
    /// [`Subscription::undelivered_at_seal`] instead of borrowing a code
    /// that would misdescribe it.
    pub fn disconnect_error(&self) -> Option<&TransportErrorPayload> {
        match self.terminal.get() {
            Some(Terminal::Lagging(payload)) => Some(payload),
            Some(Terminal::SealedWithLoss { .. }) | None => None,
        }
    }

    /// How many accepted events this subscription was never handed when
    /// the session's seal ended its stream — a parked event no queue room
    /// could take, or the losses of a lag episode the session ended before
    /// the grace deadline did. `None` when the stream ended complete or
    /// ended in a lag disconnect (see
    /// [`Subscription::disconnect_error`]). The loss reaches the
    /// subscriber either way; what the wire makes of a session ending
    /// short is the transport layer's call, which is why this stays a
    /// count rather than a borrowed error code.
    pub fn undelivered_at_seal(&self) -> Option<u64> {
        match self.terminal.get() {
            Some(Terminal::SealedWithLoss { events_lost }) => Some(*events_lost),
            Some(Terminal::Lagging(_)) | None => None,
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.channel.detach(self.slot_id);
    }
}
