//! The reference supervisor.
//!
//! Spawns the runtime, owns the process group or job object that contains it,
//! restarts it after an unexpected exit under a stated backoff, and tells a
//! deliberate shutdown from a crash by reading the intent the runtime records
//! before it goes away. It also owns the one case no in-process code can
//! handle: when the runtime is killed outright, something outside it must
//! still reap the CLI processes it was hosting, or they survive as orphans
//! holding terminals open.
//!
//! This is a contributor and CI artifact, not a product binary. Operators are
//! expected to run the runtime under systemd, launchd, or whatever their
//! platform already provides — this is the canonical example of doing that
//! correctly, and the fixture the post-kill cleanup test drives.
//!
//! Empty for now, and silent: it exits zero having done nothing.

// Not `forbid(unsafe_code)`, unlike most of the workspace: containing a child
// process tree means process groups and session leaders on POSIX and job
// objects on Windows, both of which reach below safe wrappers.

fn main() {}

#[cfg(test)]
mod tests {
    /// A placeholder so this crate builds and runs a test binary from the day
    /// it exists, rather than the day it first has behavior — a test harness
    /// that has never run is not a test harness. Delete it with the first real
    /// test.
    #[test]
    fn test_harness_is_wired() {}
}
