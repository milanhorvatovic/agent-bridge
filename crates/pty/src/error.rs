//! Everything this layer can fail at, as one typed enum.
//!
//! Typed rather than stringly because two consumers have to *decide* on
//! these, not just report them: a session decides whether a failure is worth
//! ending over, and the transport decides which protocol error code it
//! becomes. Neither decision can be made against a flattened message.
//!
//! Each variant also names the stable `pty.error` code the layer above tags
//! the emitted event with. This crate emits no events itself — it has no
//! dependency on the event taxonomy and deliberately keeps none, because a
//! byte pipe that knows the event schema is a byte pipe that will eventually
//! be asked to fill it in.

use std::io;

use crate::process::{ExitStatus, Signal};

/// A failure of the terminal, or of the process hosted in it.
#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    /// No pseudo-terminal could be allocated. Usually resource exhaustion —
    /// the per-process descriptor limit, or the system-wide pty count — and
    /// on Windows also the console subsystem failing to initialise within
    /// the allocation deadline.
    #[error("pseudo-terminal allocation failed: {0}")]
    AllocFailed(#[source] io::Error),

    /// The terminal was allocated but the program could not be started in
    /// it: a missing binary, a path that is not executable, or an argument
    /// list the OS rejected.
    #[error("the child could not be started in the terminal: {0}")]
    ChildExecFailed(#[source] io::Error),

    /// A write went unaccepted for the whole per-write deadline, which means
    /// the child has stopped reading its input.
    ///
    /// The bytes that never made it are carried back so the caller can
    /// decide — retry, discard, or end the session. They are deliberately
    /// absent from the message: input can carry anything the operator typed.
    #[error("the child stopped reading its input; {} bytes were not written", .unwritten.len())]
    StdinBlocked {
        /// The unwritten suffix, in order. Never partially applied — these
        /// bytes did not reach the child.
        unwritten: Vec<u8>,
    },

    /// The operation could not be carried out because the child is already
    /// gone. Distinct from a failure of the operation itself: nothing is
    /// wrong with the terminal, there is simply nobody left in it.
    #[error("the child has already exited ({0})")]
    ChildExitedEarly(ExitStatus),

    /// The signal could not be delivered. On Windows this is also how a
    /// signal with no equivalent reports itself, because the honest answer
    /// to "send SIGWINCH" on a platform without one is that it did not
    /// happen, not that it did.
    #[error("{signal} did not take effect on the child's process group: {source}")]
    SignalFailed {
        /// What was being delivered.
        signal: Signal,
        /// Why the operating system refused.
        #[source]
        source: io::Error,
    },

    /// The geometry reached the terminal, but the child had not yet produced
    /// a byte and so may never have been attached to observe it.
    ///
    /// The resize itself succeeded — this reports the race, not a failure to
    /// apply. The remedy is to reissue once the child has spoken, which is
    /// why it is an error a caller must handle rather than a detail it can
    /// discover later from a CLI rendering at the wrong width.
    #[error("the terminal was resized before the child had taken possession of it")]
    ResizeBeforeReady,

    /// The terminal itself failed an operation — a write, a resize, a read
    /// of its geometry — for a reason that is not the child having gone
    /// away. Rare, and always an operating-system fault rather than a
    /// misuse: a session that sees this cannot continue on this terminal.
    #[error("the terminal failed: {0}")]
    TerminalFailed(#[source] io::Error),
}

impl PtyError {
    /// The stable `pty.error` code the layer above tags the emitted event
    /// with.
    ///
    /// The strings are contract, not diagnostics: a caller routes on them,
    /// so renaming one is a breaking change to the published event schema
    /// even though nothing in this crate would notice.
    pub fn code(&self) -> &'static str {
        match self {
            PtyError::AllocFailed(_) => "pty_alloc_failed",
            PtyError::ChildExecFailed(_) => "child_exec_failed",
            PtyError::StdinBlocked { .. } => "stdin_blocked",
            PtyError::ChildExitedEarly(_) => "child_exited_early",
            PtyError::SignalFailed { .. } => "signal_delivery_failed",
            PtyError::ResizeBeforeReady => "early_resize",
            PtyError::TerminalFailed(_) => "pty_io_failed",
        }
    }
}

/// Carry a foreign error into `io::Error` by its rendered form.
///
/// The terminal library reports failures as an opaque error chain of its
/// own. Keeping that type out of this crate's signatures is what lets the
/// library be replaced without the layers above noticing, so the chain is
/// flattened to text at exactly this boundary and nowhere deeper. `{:#}`
/// renders the whole chain rather than only its outermost cause, which is
/// usually the uninformative half.
pub(crate) fn foreign(err: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{err:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of each variant. Listed by hand rather than derived so that a new
    /// variant fails to compile here until somebody decides which code it
    /// carries — the mapping is published contract, and a default would let
    /// a new failure mode inherit somebody else's name silently.
    fn one_of_each() -> Vec<PtyError> {
        vec![
            PtyError::AllocFailed(io::Error::from(io::ErrorKind::OutOfMemory)),
            PtyError::ChildExecFailed(io::Error::from(io::ErrorKind::NotFound)),
            PtyError::StdinBlocked {
                unwritten: b"unsent".to_vec(),
            },
            PtyError::ChildExitedEarly(ExitStatus::Code(1)),
            PtyError::SignalFailed {
                signal: Signal::Terminate,
                source: io::Error::from(io::ErrorKind::PermissionDenied),
            },
            PtyError::ResizeBeforeReady,
            PtyError::TerminalFailed(io::Error::from(io::ErrorKind::BrokenPipe)),
        ]
    }

    #[test]
    fn error_code_mapping_is_total() {
        let codes: Vec<&str> = one_of_each().iter().map(PtyError::code).collect();
        assert_eq!(
            codes,
            [
                "pty_alloc_failed",
                "child_exec_failed",
                "stdin_blocked",
                "child_exited_early",
                "signal_delivery_failed",
                "early_resize",
                "pty_io_failed",
            ],
            "every variant maps to its published code, in this order"
        );
    }

    #[test]
    fn no_two_failures_share_a_code() {
        // Shared codes are worse than missing ones: a consumer routing on
        // the code cannot tell the two failures apart, and nothing fails
        // loudly enough for anyone to notice.
        let mut codes: Vec<&str> = one_of_each().iter().map(PtyError::code).collect();
        let total = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), total);
    }

    #[test]
    fn a_blocked_write_reports_how_much_was_lost_and_never_what() {
        // Input is whatever the operator typed, which can include a secret
        // pasted into a prompt. The count is actionable; the bytes are not.
        let error = PtyError::StdinBlocked {
            unwritten: b"hunter2".to_vec(),
        };
        let message = error.to_string();
        assert!(message.contains('7'), "the count must be there: {message}");
        assert!(
            !message.contains("hunter2"),
            "the bytes must not be: {message}"
        );
    }

    #[test]
    fn a_foreign_error_keeps_its_whole_chain() {
        // The outermost cause of a terminal-library failure is usually
        // "openpty failed"; the useful half is what it wrapped.
        let flattened = foreign("openpty failed: too many open files");
        assert!(
            flattened.to_string().contains("too many open files"),
            "the inner cause must survive: {flattened}"
        );
    }
}
