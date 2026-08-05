//! One live CLI session, from launch to exit.
//!
//! The state machine that owns a session's lifecycle: starting the child,
//! tracking the approvals waiting on an answer — more than one can be
//! outstanding at a time, so this is a set and not a slot — orchestrating
//! interrupts, and running the shutdown sequence through to the point where
//! the process and everything it spawned are provably gone.
//!
//! Single-writer ownership is the rule that makes the rest safe. Many readers
//! may observe a session; exactly one may write to it, and every state
//! transition is serialized through the one task that owns the session. Input
//! arriving from a client, an approval answered from a side channel, and a
//! timeout firing therefore queue behind each other instead of racing, so a
//! reconnecting client and a live subscriber can never disagree about what the
//! session is doing.
//!
//! Empty for now — the state machine lands with the layers it drives.

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
