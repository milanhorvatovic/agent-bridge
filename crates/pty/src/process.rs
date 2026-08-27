//! The vocabulary of process control: who the child is, what can be sent to
//! it, and how it ended.
//!
//! These are deliberately this crate's own types rather than the terminal
//! library's. A caller two layers up should be able to read an exit status
//! or address a signal without taking a dependency on which library
//! allocated the terminal — that choice is meant to be replaceable, and a
//! type that leaks through the interface is not.

/// The operating-system identifier of a hosted child.
///
/// On POSIX this is also the child's process-group id: the terminal library
/// makes every child a session leader before exec, which makes it the leader
/// of a fresh group with the same number. Group-addressed delivery therefore
/// needs nothing but this value. On Windows it identifies the process inside
/// the job object that contains it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pid(u32);

impl Pid {
    pub(crate) fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw identifier, for an operator report or a supervisor that has
    /// to reach the process from outside this runtime.
    pub fn get(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for Pid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// What [`Pty::signal`](crate::Pty::signal) delivers to the child's process
/// group or job.
///
/// Signals are the *shutdown and notification* path, and they are not how a
/// generation is interrupted: an interactive CLI running its terminal in raw
/// mode treats a delivered `SIGINT` as a request to exit, so interrupting
/// one means writing the Ctrl+C byte instead — see
/// [`Pty::interrupt`](crate::Pty::interrupt). Both exist because which one
/// works is a property of the CLI, not of this layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// `SIGINT`. Interrupts a CLI that leaves its terminal in cooked mode,
    /// and shuts down one that does not.
    Interrupt,
    /// `SIGWINCH`. [`Pty::resize`](crate::Pty::resize) already raises this
    /// as part of changing the geometry; sending it alone re-notifies a
    /// child that missed the first one.
    WindowChanged,
    /// `SIGTERM`. The polite half of the termination sequence.
    Terminate,
    /// `SIGKILL`. Cannot be caught, and is therefore the sequence's last
    /// step rather than its first.
    Kill,
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Signal::Interrupt => "SIGINT",
            Signal::WindowChanged => "SIGWINCH",
            Signal::Terminate => "SIGTERM",
            Signal::Kill => "SIGKILL",
        })
    }
}

/// How a hosted child ended.
///
/// A sum type rather than a code-plus-optional-signal pair: a process exits
/// on its own or it is killed, never both, and a shape that can express both
/// at once invites a reader to check them in the wrong order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitStatus {
    /// The child returned this status code of its own accord.
    Code(u32),
    /// The child was killed by a signal, named as the operating system
    /// reports it. POSIX only — Windows terminates processes with a code.
    Killed(String),
}

impl ExitStatus {
    /// Whether the child ended the way a successful program does. A killed
    /// child never did, however the termination was requested.
    pub fn success(&self) -> bool {
        matches!(self, ExitStatus::Code(0))
    }

    pub(crate) fn from_portable(status: &portable_pty::ExitStatus) -> Self {
        match status.signal() {
            Some(signal) => ExitStatus::Killed(signal.to_string()),
            None => ExitStatus::Code(status.exit_code()),
        }
    }
}

impl std::fmt::Display for ExitStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExitStatus::Code(code) => write!(f, "exit code {code}"),
            ExitStatus::Killed(signal) => write!(f, "killed by {signal}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_zero_exit_of_the_childs_own_making_is_success() {
        assert!(ExitStatus::Code(0).success());
        assert!(!ExitStatus::Code(1).success());
        assert!(!ExitStatus::Killed("SIGTERM".to_string()).success());
    }

    #[test]
    fn a_signal_ended_child_keeps_the_signal_name_the_os_gave() {
        // The terminal library reports a kill as a named signal and a normal
        // exit as a code; conflating them would let "killed by SIGTERM" read
        // as "exited 1", which is the difference between a shutdown that
        // worked and a CLI that failed.
        let killed = portable_pty::ExitStatus::with_signal("SIGTERM");
        assert_eq!(
            ExitStatus::from_portable(&killed),
            ExitStatus::Killed("SIGTERM".to_string())
        );
        let exited = portable_pty::ExitStatus::with_exit_code(3);
        assert_eq!(ExitStatus::from_portable(&exited), ExitStatus::Code(3));
    }

    #[test]
    fn signals_print_the_name_an_operator_would_search_for() {
        assert_eq!(Signal::Interrupt.to_string(), "SIGINT");
        assert_eq!(Signal::Kill.to_string(), "SIGKILL");
    }
}
