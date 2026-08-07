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
//! **There is one output stream, not two.** A child in a terminal has its
//! standard output and standard error connected to the same device, so the
//! two arrive interleaved with nothing to tell them apart — that is the
//! operating system's doing, not a simplification made here, and it is why
//! this crate exposes one [`ReadStream`] rather than pretending to a split it
//! cannot perform. A caller that needs to recognise a CLI's error output
//! recognises it by what the CLI writes, not by which descriptor it came
//! from.
//!
//! **This crate deals in bytes and process control only.** It does not
//! interpret output, does not reconstruct what a terminal would display, and
//! knows nothing about adapters or events; an operating-system failure
//! surfaces as a typed error that the layer above converts into an event.
//! Keeping it a plain byte pipe is what lets everything above it be tested
//! against a scripted stand-in instead of a real terminal.
//!
//! # Using it
//!
//! ```no_run
//! use std::time::Duration;
//! use agent_bridge_pty::{ReadChunk, Spawned, SpawnSpec, spawn};
//!
//! let Spawned { pty, output } = spawn(&SpawnSpec::new("bash"))?;
//! pty.write(b"echo hello\r")?;
//!
//! // Read on its own thread. The stream ends when the *terminal* closes,
//! // which on Windows means when the handle is dropped — so draining to
//! // `End` before letting go of the handle would wait for something that
//! // cannot arrive until you do.
//! let reader = std::thread::spawn(move || {
//!     while let Ok(chunk) = output.recv() {
//!         match chunk {
//!             ReadChunk::Output(bytes) => { /* hand the bytes upward */ }
//!             ReadChunk::Invalid { .. } => { /* record the substitution */ }
//!             ReadChunk::End(_) => break,
//!         }
//!     }
//! });
//!
//! pty.terminate(Duration::from_secs(5))?;
//! drop(pty); // closes the terminal, which is what ends the stream
//! reader.join().expect("the reader thread must not panic");
//! # Ok::<(), agent_bridge_pty::PtyError>(())
//! ```
//!
//! The handle is shareable: every operation takes `&self`, so a session can
//! keep one copy behind an `Arc` and still interrupt from a different task
//! than the one that is writing. That is a requirement rather than a
//! convenience — an interrupt is worth nothing if it has to wait for the
//! stalled write it was meant to cut short.

// Not `forbid(unsafe_code)`, unlike most of the workspace: containment is
// operating-system work — process groups and session leaders on POSIX, job
// objects on Windows — and reaches below what a safe wrapper exposes.

use std::time::Duration;

mod backend;
mod env;
mod error;
mod process;
mod reader;
mod spec;

pub use env::EnvStrip;
pub use error::PtyError;
pub use process::{ExitStatus, Pid, Signal};
pub use reader::{EndOfStream, ReadChunk, ReadStream};
pub use spec::{DEFAULT_WRITE_TIMEOUT, Dimensions, SpawnSpec};

/// One child process, hosted in its own terminal.
///
/// The operations are those a terminal genuinely offers, and no more: this
/// is the boundary at which "move these bytes" stops and "work out what the
/// bytes mean" begins.
pub trait Pty: Send + Sync {
    /// Send `bytes` to the child's input, retrying the unaccepted suffix
    /// until the spec's write deadline.
    ///
    /// Writes serialize: a second call waits for the first to finish rather
    /// than interleaving into it, so a line typed by one task cannot arrive
    /// cut in half by another. On deadline the call fails with
    /// [`PtyError::StdinBlocked`] carrying the exact suffix that never went,
    /// which is what makes retrying it safe.
    fn write(&self, bytes: &[u8]) -> Result<(), PtyError>;

    /// Change the terminal's geometry and notify the child.
    ///
    /// Fails with [`PtyError::ResizeBeforeReady`] when the child has not yet
    /// produced any output: the geometry is applied to the terminal either
    /// way, but a child that has not attached to it cannot be notified, so
    /// the caller reissues once the session is up.
    fn resize(&self, dimensions: Dimensions) -> Result<(), PtyError>;

    /// Stop what the CLI is doing without ending the session, by writing the
    /// Ctrl+C byte into the terminal exactly as typing it would.
    ///
    /// **Not signal delivery.** An interactive CLI puts its terminal in raw
    /// mode, which tells the line discipline to stop turning that keystroke
    /// into `SIGINT`; the CLI reads the byte and runs its own handler. A
    /// real `SIGINT` reaches the same CLI's shutdown path instead and ends
    /// the session — which is why the two are separate operations here, and
    /// why an adapter states which its CLI needs.
    ///
    /// Never queues behind a stalled [`Pty::write`].
    fn interrupt(&self) -> Result<(), PtyError>;

    /// Deliver a signal to the child's whole process group or job — never to
    /// the root process alone.
    ///
    /// Group-addressed because a CLI that runs a tool call spawns a shell to
    /// do it, and a signal that reached only the process this crate started
    /// would leave that shell running. See [`Signal`] for what each one
    /// means here, and [`Pty::interrupt`] for why `SIGINT` is not the way to
    /// interrupt a generation.
    fn signal(&self, signal: Signal) -> Result<(), PtyError>;

    /// End the session: ask the process group to stop, wait `grace`, then
    /// kill whatever is left.
    ///
    /// Returns once the child has exited *and* nothing remains in its
    /// process group or job. Deciding when to close is the caller's; running
    /// the sequence is this layer's, because the mechanics differ by
    /// platform and nothing above should have to know that.
    fn terminate(&self, grace: Duration) -> Result<ExitStatus, PtyError>;

    /// Whether the hosted child is still running.
    fn alive(&self) -> bool;

    /// The child's process identifier — for an operator report, or for a
    /// supervisor that has to reach the tree from outside this runtime.
    ///
    /// On POSIX this is also the process group every descendant is in: the
    /// child is made a session leader before exec, so it leads a group whose
    /// id is its own pid and nothing has to be recorded separately.
    fn child_pid(&self) -> Pid;

    /// Every process still inside the session: the hosted child, and
    /// whatever it spawned that did not deliberately leave.
    ///
    /// For an operator asking what a session is running, or a supervisor
    /// deciding what it still has to clean up — the questions a single
    /// process id cannot answer, because a CLI that shells out for a tool
    /// call has more than one.
    ///
    /// **Ask this on demand, never in a loop.** On one platform the kernel
    /// keeps the list and hands it over; on the other it is a walk over
    /// every live process. Code that only needs to know whether *anything*
    /// is left should not use this — [`Pty::terminate`] already leaves the
    /// session empty and says so.
    ///
    /// The failure is an operating-system read failing, not the session
    /// going wrong, which is why it is an `io::Error` and has no place in
    /// the error taxonomy the other operations map onto.
    fn contained(&self) -> std::io::Result<Vec<Pid>>;
}

/// What [`spawn`] hands back: the control surface, and the output.
///
/// They are separate values because they are used from different places —
/// one task reads the terminal for as long as the session lasts while others
/// write to it — and because a terminal has exactly one reader. Handing the
/// stream out here rather than through a method makes that a fact of the
/// type instead of a rule somebody has to remember.
pub struct Spawned {
    /// Everything done *to* the session.
    pub pty: Box<dyn Pty>,
    /// Everything the child says. Ends with [`ReadChunk::End`].
    pub output: ReadStream,
}

/// Allocate a terminal, start `spec` inside it, and begin reading.
pub fn spawn(spec: &SpawnSpec) -> Result<Spawned, PtyError> {
    backend::spawn(spec)
}
