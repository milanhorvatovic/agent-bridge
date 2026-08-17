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
//! Subscribers read through bounded queues. A subscriber that cannot keep up
//! is disconnected under a stated lag policy rather than buffered without
//! limit, because unbounded buffering converts one slow client into a
//! runtime-wide memory problem — and a bounded queue that fills is a
//! backpressure signal worth acting on, not an error to hide. The registry,
//! the replay window a reconnecting client draws from, and the health and
//! statistics behind the runtime's own methods live here as well.
//!
//! The bus lands in stages, publish before replay before backpressure, and
//! two are in: [`EventBus`] carries per-session publish/subscribe with
//! event-type and namespace filtering, the global channel for events
//! scoped to no session, and multi-subscriber fanout under the
//! one-choke-point `seq` contract — plus the replay window itself, a
//! per-session ring bounded by event count and age that lets a
//! dropped subscriber re-attach with `from_seq` and receive exactly what
//! it missed, or an honest gap signal naming the oldest event still
//! available. The queues are already bounded, but the bound is a
//! generous interim stand-in — the contractual lag policy is the stage
//! that follows, and the registry and the runtime's own health surface
//! arrive with the layers that need them.

#![forbid(unsafe_code)]

mod bus;

pub use bus::{
    BusConfig, BusError, EventBus, EventFilter, Publisher, ReplayPlan, RingConfig, RingStats,
    Subscription,
};
