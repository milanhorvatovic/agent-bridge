//! The runtime: the session registry and the event bus.
//!
//! Every event a session produces is published here, and this is the single
//! place a sequence number is stamped. That one choke point is what makes
//! ordering total per session regardless of which task produced the event — an
//! approval arriving over a side channel, a line read from a transcript, and a
//! repaint observed on the screen all pass through it, so the order a client
//! receives is the order things happened. Sequence numbers assigned in more
//! than one place would be a different, weaker promise.
//!
//! Subscribers read through bounded queues under the contractual flow-control
//! policy: a subscriber that cannot keep up is given a grace window and then
//! disconnected with a terminal `transport.error` naming what was lost, rather
//! than buffered without limit — unbounded buffering converts one slow client
//! into a runtime-wide memory problem, and a bounded queue that fills is a
//! backpressure signal worth acting on, not an error to hide. The producer
//! never blocks on the slowest consumer.
//!
//! The bus lands in stages — publish, then replay, then backpressure — and all
//! three are in: [`EventBus`] carries per-session publish/subscribe with
//! event-type and namespace filtering, the global channel for events scoped to
//! no session, and multi-subscriber fanout under the one-choke-point `seq`
//! contract; the replay window, a per-session ring bounded by event count and
//! age that lets a dropped subscriber re-attach with `from_seq` and receive
//! exactly what it missed or an honest gap signal; and the lag policy above,
//! configured by [`BackpressureConfig`] and accounted through [`BusMetrics`].
//! The registry and the runtime's own health surface arrive with the layers
//! that need them.

#![forbid(unsafe_code)]

mod bus;

pub use bus::{
    BackpressureConfig, BusConfig, BusError, BusMetrics, DisconnectReason, EventBus, EventFilter,
    Publisher, ReplayPlan, RingConfig, RingStats, Subscription,
};
