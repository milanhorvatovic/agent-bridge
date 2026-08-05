//! Process hosting — a pseudo-terminal, a child process inside it, and bytes
//! in both directions.
//!
//! Allocation and spawn, reads and writes, resize, interrupt delivery,
//! termination, and the process-group or job-object containment that makes
//! "the session is gone" true of every descendant rather than only of the
//! process this crate spawned. Interrupt is two distinct paths, and both
//! belong here: a control byte written into the terminal, and a signal sent to
//! the process group. Which one an interactive CLI actually honours depends on
//! the mode its terminal is in, so the caller gets to choose.
//!
//! Reads carry encoding state forward. A multi-byte character split across a
//! read boundary is resolved here, so no consumer of this crate ever sees half
//! a character.
//!
//! **This crate deals in bytes and process control only.** It does not
//! interpret output, does not reconstruct what a terminal would display, and
//! knows nothing about adapters or events; an operating-system failure
//! surfaces as a typed error that the layer above converts into an event.
//! Keeping it a plain byte pipe is what lets everything above it be tested
//! against a scripted stand-in instead of a real terminal.
//!
//! Empty for now — the interface lands with the layer it belongs to.

// Not `forbid(unsafe_code)`, unlike most of the workspace: containment is
// operating-system work — process groups and session leaders on POSIX, job
// objects on Windows — and reaches below what a safe wrapper exposes.

#[cfg(test)]
mod tests {
    /// A placeholder so this crate builds and runs a test binary from the day
    /// it exists, rather than the day it first has behavior — a test harness
    /// that has never run is not a test harness. Delete it with the first real
    /// test.
    #[test]
    fn test_harness_is_wired() {}
}
