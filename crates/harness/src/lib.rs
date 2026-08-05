//! The conformance runner.
//!
//! Loads a scenario, drives it against either the scripted stand-in CLI or a
//! recorded fixture, and compares the events the runtime emitted against the
//! trace the scenario expects — reporting the difference in a form a human can
//! read and CI can gate on.
//!
//! Two things depend on this crate existing. Conformance is what keeps two
//! adapters honest about meaning the same thing by the same event, rather than
//! each drifting into its own dialect; and replay against recorded sessions is
//! what keeps a real CLI's behavior testable long after that CLI has shipped
//! three new versions. Neither is possible if the comparison lives inside each
//! adapter's own tests.
//!
//! Empty for now — the runner lands once there is a runtime to drive.

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
