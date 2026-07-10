//! The stand-in interactive fixture: a deterministic, credential-free child
//! that behaves like an interactive TUI under a PTY — paint on start, await
//! input, repaint on command, exit on command. Driven by the probe's
//! `standin` lane; never run by hand except to debug it.

// The fixture's whole purpose is painting to its terminal, which is stdout.
#![allow(clippy::disallowed_macros)]

fn main() {
    std::process::exit(agent_bridge_interactive_probe::standin::child_main());
}
