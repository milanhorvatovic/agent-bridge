//! The contract one CLI adapter implements.
//!
//! An adapter describes a single interactive CLI: how to launch it (argument
//! vector, environment policy, working directory), what patterns in its output
//! mean, which structured side channels it offers in place of reading the
//! screen, how to ask it to stop, and how to interrogate its version. This is
//! the seam the whole runtime is built around, so it is deliberately narrow
//! and carries nothing terminal-specific — an adapter states intent, and the
//! runtime decides how to carry that intent out.
//!
//! That narrowness is what keeps a second adapter cheap and a non-terminal
//! integration possible later without redesigning the runtime. It is also why
//! pattern matching is split in two: the pattern records and the matching
//! protocol live here, while the engine that runs them lives in the stream
//! crate. An adapter says what to look for; it never runs the search itself.
//!
//! Empty for now — the trait is frozen deliberately, in its own change, once
//! there is enough runtime behind it to know the shape is right.

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
