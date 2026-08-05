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
//! Empty for now — the bus lands in stages, publish before replay before
//! backpressure.

#![forbid(unsafe_code)]

#[cfg(test)]
mod tests {
    /// A placeholder so this crate builds and runs a test binary from the day
    /// it exists, rather than the day it first has behavior — a test harness
    /// that has never run is not a test harness. Delete it with the first real
    /// test.
    #[test]
    fn test_harness_is_wired() {}
}
