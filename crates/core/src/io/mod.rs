//! Process-boundary flow-control policy.
//!
//! One member today: the bounded, die-loudly write buffer
//! ([`bounded_writer::BoundedWriter`]) that the transport layer wires to
//! its stdout. It lives in this crate rather than the transport because it
//! *is* flow-control policy — the same design table row as the bus's lag
//! policy, applied at the process boundary — and policy is Core's to own;
//! the transport contributes the framing and the real file descriptor.

pub(crate) mod bounded_writer;
