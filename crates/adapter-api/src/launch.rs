//! How to start one interactive CLI, and how to ask it to stop.
//!
//! Both are **data, not control flow** — an adapter states intent, and the
//! session layer carries it out against the terminal it owns. That split is
//! the architecture's central asymmetry: the adapter never touches the
//! terminal, so a second adapter is a new description rather than new
//! plumbing.
//!
//! These are the pre-freeze shapes of the adapter contract's launch and
//! shutdown halves. The `Adapter` trait freezes around them in its own
//! change; until then their consumers are the session layer's create seam
//! and close path, and the shapes may still move with those consumers.

use std::path::PathBuf;
use std::time::Duration;

/// What launching this CLI takes: argument vector, environment, working
/// directory, and a geometry hint.
///
/// The session layer converts this into the terminal layer's spawn request.
/// Nothing here is terminal-specific on purpose: an adapter knows which
/// binary and flags make its CLI usable, and has no business knowing how a
/// pseudo-terminal is allocated.
// `Debug` is hand-written below: argument and environment *values* are
// content that can carry credentials, and a derive would hand them to any
// incidental `{:?}`.
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// The program to execute. A bare name resolves against `PATH`; an
    /// adapter that needs an exact binary states an absolute path.
    pub program: PathBuf,
    /// Arguments, not including the program itself.
    pub args: Vec<String>,
    /// Environment entries merged over the terminal layer's defaults.
    /// Which names are *rejected* is a policy decision layered above this
    /// shape, in its own change; this carries only what the adapter wants
    /// set.
    pub env: Vec<(String, String)>,
    /// Working directory for the CLI. `None` inherits the runtime's.
    pub cwd: Option<PathBuf>,
    /// Geometry hint as `(cols, rows)`. A caller-supplied geometry on
    /// `session.create` outranks it; `None` defers to the terminal layer's
    /// default.
    pub dimensions: Option<(u16, u16)>,
}

impl std::fmt::Debug for LaunchSpec {
    /// Shape and identity only, never content: the program and working
    /// directory name *what* runs and *where* — the same facts the
    /// session log records — while argument and environment values are
    /// withheld as counts, because either can carry exactly the
    /// credentials a debug line must not put on disk.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchSpec")
            .field("program", &self.program)
            .field("args", &self.args.len())
            .field("env", &self.env.len())
            .field("cwd", &self.cwd)
            .field("dimensions", &self.dimensions)
            .finish()
    }
}

impl LaunchSpec {
    /// A spec for `program` with no arguments and everything else deferred.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            dimensions: None,
        }
    }
}

/// How this CLI prefers to be asked to exit, consumed by the session
/// layer as the first step of a non-forced close.
///
/// A hint is an accelerator, never a guarantee: the session waits a bounded
/// drain window for a voluntary exit and escalates to the terminal layer's
/// termination sequence when the CLI does not take it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownHint {
    /// An *ordered sequence* of input writes with optional settling
    /// pauses. A raw-mode TUI needs its exit command and the confirming
    /// keystroke as separate writes with a pause between — one atomic blob
    /// arrives faster than the interface can react to it.
    Input(Vec<InputStep>),
    /// Deliver a signal to the CLI's process group.
    Signal(ShutdownSignal),
    /// Close the CLI's input to signal end-of-file, for CLIs that exit on
    /// it.
    ///
    /// **Not yet deliverable.** The terminal layer exposes no
    /// per-direction close — a terminal's input *is* the terminal — so
    /// the runtime currently substitutes its drain window and termination
    /// escalation: the session still closes, without the graceful exit
    /// this variant promises. It is declared anyway so the contract shape
    /// is settled ahead of the trait freeze; an adapter should prefer an
    /// input sequence or a signal until a terminal-layer capability makes
    /// the EOF real.
    CloseStdin,
}

/// One step of a [`ShutdownHint::Input`] sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputStep {
    /// Write this text into the CLI's input.
    Write(String),
    /// Give the CLI this long to react before the next write.
    Settle(Duration),
}

/// The signal half of a [`ShutdownHint`]. Only the two signals a CLI
/// plausibly documents as its exit request — termination escalation beyond
/// them is the runtime's job, not a hint an adapter gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownSignal {
    /// `SIGINT` — the exit request for a cooked-mode CLI that treats it as
    /// one.
    Interrupt,
    /// `SIGTERM` — the conventional polite stop.
    ///
    /// POSIX only in effect: Windows has no catchable equivalent — the
    /// platform's terminate is a kill — so the runtime treats this hint
    /// as undeliverable there and the close proceeds through the drain
    /// window and escalation, reporting `drained` honestly.
    Terminate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_spec_defers_everything_but_the_program() {
        let spec = LaunchSpec::new("claude");
        assert_eq!(spec.program, PathBuf::from("claude"));
        assert!(spec.args.is_empty());
        assert!(spec.env.is_empty());
        assert_eq!(spec.cwd, None);
        assert_eq!(spec.dimensions, None);
    }

    #[test]
    fn the_claude_shaped_hint_is_expressible_as_ordered_steps() {
        // The motivating example: text, a settle, then the confirming
        // keystroke — three steps whose order is the contract.
        let hint = ShutdownHint::Input(vec![
            InputStep::Write("/exit".into()),
            InputStep::Settle(Duration::from_millis(200)),
            InputStep::Write("\r".into()),
        ]);
        let ShutdownHint::Input(steps) = hint else {
            unreachable!()
        };
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[1], InputStep::Settle(Duration::from_millis(200)));
    }
}
