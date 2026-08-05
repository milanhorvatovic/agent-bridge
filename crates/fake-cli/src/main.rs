//! fake-cli — a deterministic scripted stand-in for an interactive CLI.
//!
//! The conformance corpus scripts this binary instead of a real CLI: every
//! scenario under `tests/corpus/` pairs a script for this interpreter with
//! the golden trace of events the runtime is expected to emit when hosting
//! it. Because the same scenario produces the same bytes on every run and
//! every OS, scenario runs need no credentials, no network, and no real CLI —
//! and a differential failure across platforms is signal, never flake.
//!
//! It is a strict interpreter, not a framework: read the scenario file named
//! by the single argument, execute its steps in order, exit per the script.
//!
//! Binary contract:
//!   fake-cli <scenario.json>
//!
//! The exit code is the scripted `exit` step's code. Failures exit non-zero
//! with a diagnostic on stderr — stderr is safe to write because stdout is
//! the scripted byte surface: a bad usage or unreadable/invalid scenario
//! exits 2 before any scripted byte is written; a scripted `await_stdin`
//! failing (diverging input, closed stdin, or timeout) exits with the
//! executor's failure codes.

use std::process::exit;

use agent_bridge_fake_cli::{exec, scenario};

const EXIT_LOAD: i32 = 2;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("fake-cli: usage: fake-cli <scenario.json>");
        exit(EXIT_LOAD);
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("fake-cli: {path}: cannot read scenario: {err}");
            exit(EXIT_LOAD);
        }
    };
    let scenario = match scenario::parse(&text) {
        Ok(scenario) => scenario,
        Err(message) => {
            eprintln!("fake-cli: {path}: {message}");
            exit(EXIT_LOAD);
        }
    };
    exec::run(&scenario)
}
