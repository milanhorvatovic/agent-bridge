//! The runtime binary — the one artifact this project distributes.
//!
//! It loads configuration, brings up logging, wires the layers together,
//! serves the JSON-RPC surface on stdio, and carries the diagnostic subcommand
//! an operator runs when a session will not start. Every other crate in the
//! workspace exists to be linked in here; this is the only place that knows
//! about all of them at once, which is what keeps the layers below it free of
//! knowledge about each other.
//!
//! Empty for now, and **silent by design**: stdout belongs to the protocol, so
//! a skeleton binary that printed a friendly "not implemented yet" would be
//! writing to the one stream it must never touch. It exits zero having done
//! nothing.

#![forbid(unsafe_code)]

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
