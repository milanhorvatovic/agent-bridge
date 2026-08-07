//! The one place that knows which operating system this is.
//!
//! Everything above this module compiles against [`Pty`] and never asks what
//! platform it is on — that is the property the whole two-backend design
//! exists to buy, and it survives only if the branch stays here. The two
//! platform modules below supply exactly three things a terminal cannot be
//! run without and that POSIX and Windows genuinely disagree about:
//! containment of the process tree, the pair of I/O ports onto the terminal,
//! and how the terminal is closed. The sequence that uses them — allocate,
//! spawn, contain, read, terminate — is written once, here.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};

use crate::env;
use crate::error::{PtyError, foreign};
use crate::process::{ExitStatus, Pid, Signal};
use crate::reader;
use crate::spec::{Dimensions, SpawnSpec};
use crate::{Pty, Spawned};

#[cfg(unix)]
mod posix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use posix as platform;
#[cfg(windows)]
use windows as platform;

/// The byte a terminal sends when Ctrl+C is typed.
///
/// This — not a signal — is how an interactive CLI is interrupted. A CLI
/// running its terminal in raw mode has told the line discipline to stop
/// synthesising signals from keystrokes, so it expects to read the byte and
/// run its own handler; a `SIGINT` delivered to such a process instead hits
/// its shutdown path, which ends the session rather than the generation.
const CTRL_C: u8 = 0x03;

/// How long allocation may take before it is treated as a failure.
///
/// Generous, because it is not a performance budget: allocation is
/// microseconds when it works. It exists because on Windows the call can
/// hang outright when the console subsystem has not initialised, and a
/// runtime that hangs on session create is worse than one that reports it
/// could not allocate.
const ALLOC_TIMEOUT: Duration = Duration::from_secs(10);

/// How often a wait for the child to exit re-checks. Process death is
/// immediate kernel-side; the interval only rides out the reap.
const EXIT_POLL: Duration = Duration::from_millis(20);

/// How long a forced kill gets to be observed before it is reported as not
/// having taken effect.
const FORCE_WINDOW: Duration = Duration::from_secs(2);

/// How long a failed terminal operation waits to see whether the child's
/// exit was the reason, before reporting the failure as the terminal's own.
///
/// A write to a terminal whose child has gone fails at the same instant the
/// child exits, so the two race; without this window a clean exit would be
/// reported as a terminal fault roughly whenever the scheduler felt like it.
const REAP_WINDOW: Duration = Duration::from_millis(250);

/// How long a control write waits for room in the child's input.
///
/// Short on purpose. A control write is an interrupt, or a reply to a query
/// the child is blocked on, and both are worth nothing late: if the input is
/// so full that a handful of bytes cannot be placed in a second, the caller
/// needs to hear that rather than wait out the payload deadline for it.
const CONTROL_DEADLINE: Duration = Duration::from_secs(1);

/// The child's input, as far as the retry loop needs to see it.
///
/// Two operations, because a terminal accepts what it has room for and no
/// more: ask whether there is room, then hand over what fits. Everything
/// about deadlines and unwritten suffixes is built on top of these, in one
/// place, so the two platforms differ only in what "room" means — a
/// descriptor reporting itself writable on POSIX, and nothing at all on
/// Windows, where a console pipe has no readiness to ask about.
pub(crate) trait InputPort: Send + Sync {
    /// Wait up to `within` for room. `false` means the wait expired with
    /// none.
    fn wait_for_room(&self, within: Duration) -> io::Result<bool>;

    /// Hand over as much of `bytes` as the terminal takes right now, and say
    /// how much that was.
    fn accept(&self, bytes: &[u8]) -> io::Result<usize>;
}

/// Why a write did not finish.
pub(crate) enum WriteStalled {
    /// The child stopped accepting input. Carries the suffix that never went
    /// — exact, so a caller may retry it without risking a double write.
    Deadline { unwritten: Vec<u8> },
    /// The terminal itself refused.
    Failed(io::Error),
}

/// Hand over every byte, retrying the unaccepted suffix until `deadline`.
pub(crate) fn write_all_by(
    port: &dyn InputPort,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), WriteStalled> {
    let mut written = 0;
    while written < bytes.len() {
        let stalled = || WriteStalled::Deadline {
            unwritten: bytes[written..].to_vec(),
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(stalled());
        }
        match port.wait_for_room(remaining) {
            Ok(true) => {}
            Ok(false) => return Err(stalled()),
            Err(err) => return Err(WriteStalled::Failed(err)),
        }
        match port.accept(&bytes[written..]) {
            // Room was reported and then nothing was taken. Not progress,
            // but not an error either; the deadline above is what keeps this
            // from spinning.
            Ok(0) => continue,
            Ok(accepted) => written += accepted,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(err) => return Err(WriteStalled::Failed(err)),
        }
    }
    Ok(())
}

/// Write a short control sequence, waiting only briefly for room.
///
/// Takes no lock of its own, and must not: an interrupt issued while a
/// payload write is stalled is exactly the case this exists for. A sequence
/// this short reaches the terminal whole, so the worst a concurrent payload
/// write can do is land before or after it.
pub(crate) fn write_control(port: &dyn InputPort, bytes: &[u8]) -> Result<(), WriteStalled> {
    write_all_by(port, bytes, Instant::now() + CONTROL_DEADLINE)
}

/// A live terminal and the process inside it.
pub(crate) struct Terminal {
    /// Taken only by [`Drop`], which has exclusive access — so every other
    /// method finds it present.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    /// The exit status, once observed. A child is reaped once; remembering
    /// the answer keeps `alive` and `terminate` from racing each other for
    /// it.
    reaped: Mutex<Option<ExitStatus>>,
    input: Arc<dyn InputPort>,
    /// Held for the whole of a payload write, so a second write queues
    /// behind the first rather than interleaving its bytes into it.
    /// Control writes deliberately do not take it.
    write_turn: Mutex<()>,
    containment: platform::Containment,
    pid: Pid,
    write_timeout: Duration,
    /// Raised by the reader the first time the child produces a byte — the
    /// only evidence this layer has that the child took its terminal.
    spoken: Arc<AtomicBool>,
}

impl Pty for Terminal {
    fn write(&self, bytes: &[u8]) -> Result<(), PtyError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + self.write_timeout;
        let _turn = lock(&self.write_turn);
        self.finish_write(write_all_by(self.input.as_ref(), bytes, deadline))
    }

    fn resize(&self, dimensions: Dimensions) -> Result<(), PtyError> {
        let guard = lock(&self.master);
        let Some(master) = guard.as_ref() else {
            return Err(PtyError::TerminalFailed(io::Error::other(
                "the terminal has been closed",
            )));
        };
        master
            .resize(PtySize {
                rows: dimensions.rows,
                cols: dimensions.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|err| PtyError::TerminalFailed(foreign(err)))?;
        // The geometry is applied either way; what is uncertain is whether
        // anyone was there to notice. A child that has not yet produced a
        // byte may not have attached to the terminal, and a notification
        // sent to a terminal with nobody on it is simply lost.
        if !self.spoken.load(Ordering::Relaxed) {
            return Err(PtyError::ResizeBeforeReady);
        }
        Ok(())
    }

    fn interrupt(&self) -> Result<(), PtyError> {
        // Deliberately outside the write turn: an interrupt issued while a
        // write is stalled is exactly the case it exists for, and queueing
        // it behind that write would make it arrive after the deadline it
        // was meant to cut short.
        self.finish_write(write_control(self.input.as_ref(), &[CTRL_C]))
    }

    fn signal(&self, signal: Signal) -> Result<(), PtyError> {
        self.containment
            .signal(signal)
            .map_err(|source| PtyError::SignalFailed { signal, source })
    }

    fn terminate(&self, grace: Duration) -> Result<ExitStatus, PtyError> {
        let status = match self.reaped_status() {
            Some(status) => status,
            None => self.run_termination(grace)?,
        };
        // The root process exiting is not the same as the session being
        // gone: a CLI that spawned a shell for a tool call leaves it behind,
        // and it holds file descriptors and CPU whether or not anyone is
        // reading it.
        self.empty_the_containment();
        Ok(status)
    }

    fn alive(&self) -> bool {
        self.reaped_status().is_none()
    }

    fn child_pid(&self) -> Pid {
        self.pid
    }

    fn contained(&self) -> io::Result<Vec<Pid>> {
        self.containment.contained()
    }
}

impl Terminal {
    /// Turn a stalled write into the error the caller sees.
    fn finish_write(&self, outcome: Result<(), WriteStalled>) -> Result<(), PtyError> {
        match outcome {
            Ok(()) => Ok(()),
            Err(WriteStalled::Deadline { unwritten }) => Err(PtyError::StdinBlocked { unwritten }),
            Err(WriteStalled::Failed(err)) => Err(self.blame(err)),
        }
    }

    /// Decide whether a failed terminal operation was the child leaving.
    ///
    /// A terminal write fails once nothing holds the other end open, which
    /// on both platforms means the child and everything it handed the
    /// terminal to are gone. That is an ordinary end of session, not a
    /// fault, and reporting it as one would have every clean exit look like
    /// a broken terminal.
    fn blame(&self, err: io::Error) -> PtyError {
        match self.await_exit(REAP_WINDOW) {
            Some(status) => PtyError::ChildExitedEarly(status),
            None => PtyError::TerminalFailed(err),
        }
    }

    /// Ask, then insist. Returns the status the child ended with.
    fn run_termination(&self, grace: Duration) -> Result<ExitStatus, PtyError> {
        if let Err(err) = self.containment.signal(Signal::Terminate) {
            // Not fatal on its own: the forced step below reaches the same
            // processes by the same route, and if that also fails the caller
            // hears about it then.
            tracing::debug!(pid = %self.pid, %err, "the termination request could not be delivered");
        }
        if let Some(status) = self.await_exit(grace) {
            return Ok(status);
        }
        tracing::info!(
            pid = %self.pid,
            grace_ms = grace.as_millis(),
            "the child did not stop when asked; killing its process group"
        );
        self.containment
            .signal(Signal::Kill)
            .map_err(|source| PtyError::SignalFailed {
                signal: Signal::Kill,
                source,
            })?;
        self.await_exit(FORCE_WINDOW)
            .ok_or_else(|| PtyError::SignalFailed {
                signal: Signal::Kill,
                source: io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the child was still running afterwards",
                ),
            })
    }

    /// Leave nothing of the session behind.
    ///
    /// Reported rather than raised, and deliberately: the caller asked for
    /// the session to end, and it has — the child is gone and its exit
    /// status is the answer. What can remain afterwards is a descendant that
    /// SIGKILL cannot reach, or an orphan already dead and waiting for
    /// whatever inherited it to collect the corpse; neither is something a
    /// caller can act on, and neither should turn a successful close into a
    /// failure. What it *is* worth is a line in the log, because a process
    /// tree that outlives its session is how a runtime leaks.
    fn empty_the_containment(&self) {
        if self.containment.is_empty() {
            return;
        }
        tracing::info!(
            pid = %self.pid,
            "the child is gone but its process tree is not; killing what is left"
        );
        if let Err(err) = self.containment.signal(Signal::Kill) {
            tracing::warn!(pid = %self.pid, %err, "what remained could not be killed");
            return;
        }
        let deadline = Instant::now() + FORCE_WINDOW;
        while !self.containment.is_empty() {
            if Instant::now() >= deadline {
                tracing::warn!(
                    pid = %self.pid,
                    "the child's process group still holds processes after being killed"
                );
                return;
            }
            std::thread::sleep(EXIT_POLL);
        }
    }

    /// The child's exit status once it has one, without blocking.
    fn reaped_status(&self) -> Option<ExitStatus> {
        let mut reaped = lock(&self.reaped);
        if let Some(status) = reaped.as_ref() {
            return Some(status.clone());
        }
        match lock(&self.child).try_wait() {
            Ok(Some(status)) => {
                let status = ExitStatus::from_portable(&status);
                *reaped = Some(status.clone());
                Some(status)
            }
            Ok(None) => None,
            Err(err) => {
                // Unreadable is not the same as exited. Reporting it as
                // "still running" costs a termination sequence that
                // escalates for nothing; reporting the reverse would leave a
                // live CLI behind believing it was cleaned up.
                tracing::warn!(pid = %self.pid, %err, "the child's state could not be read");
                None
            }
        }
    }

    /// Wait up to `within` for the child to exit.
    fn await_exit(&self, within: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(status) = self.reaped_status() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(EXIT_POLL);
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // Dropping the handle means the session is over, and a session that
        // is over must not leave a CLI running against a terminal nobody
        // reads. Windows job objects do this themselves when the job handle
        // closes; matching it here means the two platforms behave alike, and
        // it is also what lets the reader thread finish — it ends when the
        // child releases the terminal, and not before.
        if self.alive() {
            if let Err(err) = self.containment.signal(Signal::Kill) {
                tracing::warn!(pid = %self.pid, %err, "the child's process group survived the handle");
            }
            // Killing is not collecting. A child that is signalled and never
            // waited on stays a zombie for the lifetime of the runtime, which
            // is precisely the leak this layer exists to prevent — and one
            // per abandoned session is not a rounding error. Nothing was
            // asked to stop politely here, so this costs one poll interval in
            // practice rather than the window it is bounded by.
            if self.await_exit(FORCE_WINDOW).is_none() {
                tracing::warn!(pid = %self.pid, "the child could not be collected after being killed");
            }
        }
        if let Some(master) = lock(&self.master).take() {
            self.containment.release(master);
        }
    }
}

/// Allocate a terminal, spawn `spec` inside it, and start reading it.
pub(crate) fn spawn(spec: &SpawnSpec) -> Result<Spawned, PtyError> {
    let (dimensions, defaulted) = spec.resolved_dimensions();
    if defaulted {
        // Worth a line at info: a CLI rendering into 80 columns when the
        // caller's own terminal is twice that reads as a wrapping bug, and
        // this is the only record of who chose the width.
        tracing::info!(
            geometry = %dimensions,
            "no terminal geometry was requested; using the default"
        );
    }

    let pending = platform::Pending::observe();
    let pair = allocate(dimensions)?;
    let mut child = pair
        .slave
        .spawn_command(build_command(spec, dimensions))
        .map_err(|err| PtyError::ChildExecFailed(foreign(err)))?;
    // The child holds its own end of the terminal now. Ours is the last
    // reference that would keep the terminal open after the child exits, and
    // holding it would turn every clean exit into a stream that never ends.
    drop(pair.slave);

    let pid = match child.process_id() {
        Some(pid) => Pid::new(pid),
        None => {
            abandon(&mut child);
            return Err(PtyError::ChildExecFailed(io::Error::other(
                "the operating system reported no process id for the child",
            )));
        }
    };
    let containment = match pending.contain(pid) {
        Ok(containment) => containment,
        Err(err) => {
            // A child that cannot be contained cannot be cleaned up, and one
            // that cannot be cleaned up should never have been started.
            abandon(&mut child);
            return Err(PtyError::ChildExecFailed(err));
        }
    };

    let spoken = Arc::new(AtomicBool::new(false));
    let ports = platform::io_ports(pair.master.as_ref()).map_err(PtyError::TerminalFailed);
    let (source, input) = match ports {
        Ok(ports) => ports,
        Err(err) => {
            // The group first, for anything the child managed to spawn in the
            // moment it was alive; then the child itself, to collect it.
            let _ = containment.signal(Signal::Kill);
            abandon(&mut child);
            return Err(err);
        }
    };

    // Built before the reader starts so that a reader which fails to start
    // is cleaned up by dropping this, rather than by a second copy of the
    // teardown sequence written out here.
    let terminal = Terminal {
        master: Mutex::new(Some(pair.master)),
        child: Mutex::new(child),
        reaped: Mutex::new(None),
        input: Arc::clone(&input),
        write_turn: Mutex::new(()),
        containment,
        pid,
        write_timeout: spec.write_timeout,
        spoken: Arc::clone(&spoken),
    };
    let output = reader::spawn(source, input, spoken, format!("pty-reader-{pid}"))
        .map_err(PtyError::AllocFailed)?;
    tracing::debug!(%pid, geometry = %dimensions, "a child is hosted in a terminal");
    Ok(Spawned {
        pty: Box::new(terminal),
        output,
    })
}

/// The command the child is exec'd from, environment and all.
fn build_command(spec: &SpawnSpec, dimensions: Dimensions) -> CommandBuilder {
    let mut command = CommandBuilder::new(&spec.program);
    command.args(&spec.args);
    if let Some(cwd) = &spec.cwd {
        command.cwd(cwd);
    }
    // Cleared and rebuilt rather than adjusted: composition already accounts
    // for what this process inherited, and leaving the builder's own copy of
    // it underneath would let a variable the strip rule rejected reach the
    // child by a route the rule never saw.
    command.env_clear();
    for (name, value) in env::compose(
        std::env::vars_os(),
        &platform::env_defaults(),
        &spec.env,
        dimensions,
        &spec.strip,
    ) {
        command.env(name, value);
    }
    command
}

/// Kill a child this spawn is walking away from, and collect it.
///
/// Every early return below has to do both. Killing alone leaves a zombie
/// for the lifetime of the runtime, and a session that failed to start is
/// the last place to introduce the leak this layer exists to prevent. The
/// wait is bounded because a failed spawn still has to return.
fn abandon(child: &mut Box<dyn Child + Send + Sync>) {
    let _ = child.kill();
    let deadline = Instant::now() + FORCE_WINDOW;
    loop {
        // An unreadable child cannot be collected either; stopping is all
        // that is left, and the kill above is already issued.
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() >= deadline => return,
            Ok(None) => std::thread::sleep(EXIT_POLL),
        }
    }
}

/// Allocate the terminal on a helper thread, against a deadline.
///
/// The thread exists for one reason: on Windows the allocation can hang
/// indefinitely when the console subsystem has not come up, and there is no
/// way to ask it not to. On timeout the helper is abandoned rather than
/// joined — it is blocked in the call that will not return — which costs one
/// parked thread in a session that is failing anyway.
fn allocate(dimensions: Dimensions) -> Result<PtyPair, PtyError> {
    let size = PtySize {
        rows: dimensions.rows,
        cols: dimensions.cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let (sender, allocated) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("pty-alloc".to_string())
        .spawn(move || {
            let _ = sender.send(native_pty_system().openpty(size));
        })
        .map_err(PtyError::AllocFailed)?;
    match allocated.recv_timeout(ALLOC_TIMEOUT) {
        Ok(Ok(pair)) => Ok(pair),
        Ok(Err(err)) => Err(PtyError::AllocFailed(foreign(err))),
        Err(RecvTimeoutError::Timeout) => Err(PtyError::AllocFailed(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "allocation did not complete within {}s",
                ALLOC_TIMEOUT.as_secs()
            ),
        ))),
        Err(RecvTimeoutError::Disconnected) => Err(PtyError::AllocFailed(io::Error::other(
            "the allocating thread ended without a result",
        ))),
    }
}

/// Take a lock, recovering rather than panicking if a previous holder died.
///
/// The state behind these locks is a terminal and a child process. A handle
/// whose lock is poisoned still has to be able to kill that child, and
/// panicking here would leave the process running with nothing left that
/// could reach it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A terminal that accepts a little at a time and then runs out of room
    /// for good.
    ///
    /// Standing in for the terminal, never for the child: what is under test
    /// is the retry loop's arithmetic — that a partial accept resumes where
    /// it stopped, and that the suffix handed back is exactly what never
    /// went. A real terminal cannot be made to run out of room on demand,
    /// and on some platforms discards input rather than ever running out, so
    /// this is the only place those two properties can be pinned down rather
    /// than observed when the weather is right.
    struct Trickle {
        per_accept: usize,
        /// How many more accepts have room; zero means the terminal has
        /// stopped taking anything at all.
        accepts_left: Mutex<usize>,
        taken: Mutex<Vec<u8>>,
    }

    impl Trickle {
        fn new(per_accept: usize, accepts_left: usize) -> Self {
            Self {
                per_accept,
                accepts_left: Mutex::new(accepts_left),
                taken: Mutex::new(Vec::new()),
            }
        }
    }

    impl InputPort for Trickle {
        fn wait_for_room(&self, _within: Duration) -> io::Result<bool> {
            Ok(*lock(&self.accepts_left) > 0)
        }

        fn accept(&self, bytes: &[u8]) -> io::Result<usize> {
            let mut left = lock(&self.accepts_left);
            if *left == 0 {
                return Err(io::Error::from(io::ErrorKind::WouldBlock));
            }
            *left -= 1;
            let taking = self.per_accept.min(bytes.len());
            lock(&self.taken).extend_from_slice(&bytes[..taking]);
            Ok(taking)
        }
    }

    /// A deadline far enough out that only the port decides the outcome.
    fn unhurried() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn write_retries_the_suffix_until_everything_is_accepted() {
        let port = Trickle::new(3, usize::MAX);
        let payload = b"one write, several syscalls";
        assert!(write_all_by(&port, payload, unhurried()).is_ok());
        assert_eq!(lock(&port.taken).as_slice(), payload, "in order, and whole");
    }

    #[test]
    fn a_stalled_write_hands_back_exactly_what_never_went() {
        // Two accepts of three bytes, then no more room. Retrying the suffix
        // is only safe if the boundary is exact — a byte counted on both
        // sides would reach the child twice.
        let port = Trickle::new(3, 2);
        match write_all_by(&port, b"abcdefghij", unhurried()) {
            Err(WriteStalled::Deadline { unwritten }) => {
                assert_eq!(lock(&port.taken).as_slice(), b"abcdef");
                assert_eq!(unwritten, b"ghij");
            }
            Err(WriteStalled::Failed(err)) => panic!("expected a stall, got {err}"),
            Ok(()) => panic!("a port with no room accepted everything"),
        }
    }

    #[test]
    fn a_write_that_never_starts_hands_back_all_of_it() {
        let port = Trickle::new(4, 0);
        match write_all_by(&port, b"nothing goes", unhurried()) {
            Err(WriteStalled::Deadline { unwritten }) => {
                assert_eq!(unwritten, b"nothing goes");
                assert!(lock(&port.taken).is_empty());
            }
            _ => panic!("expected the whole payload back"),
        }
    }

    #[test]
    fn an_expired_deadline_writes_nothing_at_all() {
        // A caller that passes an already-spent deadline gets its payload
        // back, rather than one opportunistic write it did not ask for.
        let port = Trickle::new(4, usize::MAX);
        let spent = Instant::now() - Duration::from_secs(1);
        match write_all_by(&port, b"too late", spent) {
            Err(WriteStalled::Deadline { unwritten }) => {
                assert_eq!(unwritten, b"too late");
                assert!(lock(&port.taken).is_empty());
            }
            _ => panic!("expected the whole payload back"),
        }
    }

    /// A terminal that has failed outright.
    struct Broken(io::ErrorKind);

    impl InputPort for Broken {
        fn wait_for_room(&self, _within: Duration) -> io::Result<bool> {
            Ok(true)
        }

        fn accept(&self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(self.0))
        }
    }

    #[test]
    fn a_broken_terminal_is_a_failure_not_a_stall() {
        // The distinction a caller acts on: a stall is retryable, and a
        // broken terminal is the end of the session.
        match write_all_by(&Broken(io::ErrorKind::BrokenPipe), b"anything", unhurried()) {
            Err(WriteStalled::Failed(err)) => assert_eq!(err.kind(), io::ErrorKind::BrokenPipe),
            _ => panic!("expected the failure to surface as one"),
        }
    }
}
