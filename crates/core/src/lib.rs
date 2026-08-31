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
//! never blocks on the slowest consumer. The same die-loudly stance guards the
//! process boundary: [`BoundedWriter`] is the bounded write buffer the
//! transport wires to stdout, and a caller that stops reading gets a
//! runtime that exits instead of wedging — announced by a fatal signal and
//! a log that are guaranteed, plus a final `transport.error` frame
//! attempted on the way out, which a parent that has genuinely stopped
//! reading may never receive.
//!
//! The bus lands in stages — publish, then replay, then backpressure — and all
//! three are in: [`EventBus`] carries per-session publish/subscribe with
//! event-type and namespace filtering, the global channel for events scoped to
//! no session, and multi-subscriber fanout under the one-choke-point `seq`
//! contract; the replay window, a per-session ring bounded by event count and
//! age that lets a dropped subscriber re-attach with `from_seq` and receive
//! exactly what it missed or an honest gap signal; and the lag policy above,
//! configured by [`BackpressureConfig`] and accounted through [`BusMetrics`].
//!
//! The [`SessionRegistry`] is in too: serialized creates minting UUIDv4 ids
//! under the soft-warn / hard-refuse caps, the create seam that registers a
//! session on the bus and hands its actor the one [`Publisher`] behind the
//! session crate's sink seam, per-id lookup spanning live sessions and the
//! 120-second retention of closed ones, and the reaper whose actions the
//! Phase-3 health surface will report. The runtime's own health surface
//! arrives with the layer that needs it.

#![forbid(unsafe_code)]

mod bus;
mod io;
mod registry;

pub use bus::{
    BackpressureConfig, BusConfig, BusError, BusMetrics, DisconnectReason, EventBus, EventFilter,
    Publisher, ReplayPlan, RingConfig, RingStats, Subscription,
};
pub use io::bounded_writer::{
    BoundedWriter, FatalSignal, MAX_CAPACITY_BYTES, ShutdownOutcome, WriterConfig, WriterError,
};
pub use registry::{
    AdapterSeam, CreateOptions, RegistryConfig, RegistryError, SessionEntry, SessionRegistry,
};

// Core is the façade the layers above it reach the session through: transport
// and the binary depend on core, not on the session crate directly, so the
// types a session operation names — its id, its handle, its errors, an
// approval id and decision — are re-exported here rather than pulled from a
// dependency edge the workspace layout does not grant them. A method handler
// that maps `SessionError` onto a protocol code needs the type by name, and
// this is where it gets it.
pub use agent_bridge_session::{
    ApprovalDecision, ApprovalId, ApprovalResolution, InputStep, InvalidSessionId, LaunchSpec,
    SessionConfig, SessionError, SessionHandle, SessionId, SessionMetadata, SessionState,
    ShutdownHint, ShutdownSignal, SubscriberId,
};
