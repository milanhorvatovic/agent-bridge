//! macOS and Linux: process groups for containment, and the terminal
//! descriptor for I/O.
//!
//! Containment is nearly free here and it is worth saying why. The terminal
//! library makes every child a session leader before it execs, so the child
//! is already the leader of a brand-new process group whose id is its own
//! pid, and everything it spawns inherits that group unless it deliberately
//! leaves. Addressing the group therefore needs no setup at all — only the
//! discipline of never addressing the bare pid, because a CLI that shells
//! out for a tool call would leave that shell behind.
//!
//! I/O goes through a descriptor of our own rather than the library's reader
//! and writer, for three reasons that all come back to control: a write must
//! be able to stop at a deadline, which needs a non-blocking descriptor; an
//! interrupt must be able to overtake a stalled write, which needs a write
//! path that holds no lock; and the library's writer sends an end-of-file
//! byte into the child's input when it is dropped, which is a side effect
//! this layer should be choosing deliberately rather than inheriting.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use portable_pty::MasterPty;

use super::InputPort;
use crate::env::NameCase;
use crate::process::{Pid, Signal};

/// The locale variables carry one message and one only: the child's output
/// is UTF-8. A CLI that finds no locale set falls back to the C locale and
/// starts emitting `?` for anything outside ASCII.
///
/// macOS ships no `C.UTF-8`; glibc systems ship no `en_US.UTF-8` unless
/// somebody generated it. Naming the wrong one is worse than naming none,
/// because a locale the system cannot resolve is silently ignored and the
/// child falls back to C anyway.
#[cfg(target_os = "macos")]
const UTF8_LOCALE: &str = "en_US.UTF-8";
#[cfg(not(target_os = "macos"))]
const UTF8_LOCALE: &str = "C.UTF-8";

/// POSIX environment names are bytes, compared byte for byte: `PATH` and
/// `Path` are two variables.
pub(crate) const NAME_CASE: NameCase = NameCase::Sensitive;

/// What this platform adds to the terminal defaults every child is given.
pub(crate) fn env_defaults() -> Vec<(OsString, OsString)> {
    vec![
        (OsString::from("LC_ALL"), OsString::from(UTF8_LOCALE)),
        (OsString::from("LANG"), OsString::from(UTF8_LOCALE)),
    ]
}

/// What was true before the terminal was allocated.
///
/// Nothing, on this platform: a POSIX terminal creates no helper processes
/// that would have to be told apart from the machine's own. The type exists
/// so the sequence in the parent module reads the same on both platforms.
pub(crate) struct Pending;

impl Pending {
    pub(crate) fn observe() -> Self {
        Self
    }

    pub(crate) fn contain(self, child: Pid) -> io::Result<Containment> {
        let pid = child.get() as libc::pid_t;
        // SAFETY: `getpgid` takes a pid by value and touches no memory.
        let reported = unsafe { libc::getpgid(pid) };
        if reported > 0 && reported != pid {
            // The child is a session leader, so its group id is its pid. If
            // the kernel disagrees, the assumption underneath every signal
            // this layer sends is wrong, and following the kernel is the
            // only safe reading.
            tracing::warn!(
                %child,
                pgid = reported,
                "the child leads no process group of its own; signalling the one it is in"
            );
        }
        Ok(Containment {
            // A failed `getpgid` means the child has already exited. Its pid
            // is still the right group id — that is what `setsid` made it —
            // and the group may still hold descendants that outlived it.
            pgid: if reported > 0 { reported } else { pid },
        })
    }
}

/// The child's process group: every descendant that did not deliberately
/// leave it.
pub(crate) struct Containment {
    pgid: libc::pid_t,
}

impl Containment {
    /// Deliver `signal` to every member of the group.
    ///
    /// A group with no members left reports success. The caller asked for
    /// the group to receive something, and a group nobody is in has nothing
    /// to receive it — treating that as a failure would make every
    /// termination that raced a clean exit look broken.
    pub(crate) fn signal(&self, signal: Signal) -> io::Result<()> {
        // SAFETY: `killpg` takes a group id and a signal number by value.
        if unsafe { libc::killpg(self.pgid, number(signal)) } == 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ESRCH) => Ok(()),
            _ => Err(err),
        }
    }

    /// Close the terminal.
    ///
    /// Nothing here can block and nothing else has to be cleaned up
    /// alongside it: a POSIX terminal is a pair of descriptors with no
    /// helper process behind them, so closing ours is the whole of it. The
    /// child's own end stays open until the child lets go, which is what
    /// keeps output readable right up to the moment it exits.
    pub(crate) fn release(&self, master: Box<dyn MasterPty + Send>) {
        drop(master);
    }

    /// Which processes are still in the group — the hosted child and
    /// everything it spawned that did not deliberately leave.
    ///
    /// POSIX offers no call that asks a group who is in it, so each platform
    /// is asked the question it can answer: macOS keeps a process-group
    /// index the kernel will hand over directly, and Linux exposes the group
    /// id per process under `/proc`. Both are a walk over live processes, so
    /// this is a diagnostic to be asked on demand — never per read, and
    /// never in the termination loop, which has the far cheaper
    /// [`Containment::is_empty`] for the only question it needs.
    #[cfg(target_os = "macos")]
    pub(crate) fn contained(&self) -> io::Result<Vec<Pid>> {
        // Room beyond what the sizing call reports: the group is free to
        // grow between the two calls, and the answer would then be silently
        // truncated to whatever fits.
        const SLACK: usize = 16;
        let width = size_of::<libc::pid_t>();
        // SAFETY: a null buffer of length zero asks for a size hint, which
        // this call gives in bytes — generously, and unlike the filled call
        // below, which answers in processes.
        let needed = unsafe { libc::proc_listpgrppids(self.pgid, std::ptr::null_mut(), 0) };
        if needed < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut members: Vec<libc::pid_t> = vec![0; needed as usize / width + SLACK];
        let capacity = libc::c_int::try_from(members.len() * width).unwrap_or(libc::c_int::MAX);
        // SAFETY: the buffer really is `capacity` bytes long, and only the
        // prefix the call reports as written is read back out of it.
        let written =
            unsafe { libc::proc_listpgrppids(self.pgid, members.as_mut_ptr().cast(), capacity) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        // A count of processes, not a byte length — the two calls do not
        // agree on units, and dividing this one by the width of a process id
        // silently truncates the answer to nothing for any small group.
        // Clamped, because a group larger than the buffer must not be read
        // past the end of it.
        members.truncate((written as usize).min(members.len()));
        Ok(members
            .into_iter()
            // A zero would be the untouched tail of the buffer rather than
            // a process, and a negative id is not one either.
            .filter(|pid| *pid > 0)
            .map(|pid| Pid::new(pid as u32))
            .collect())
    }

    /// See the macOS counterpart for why this is a diagnostic rather than
    /// something the termination path uses.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn contained(&self) -> io::Result<Vec<Pid>> {
        let mut members = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue; // not a process directory
            };
            // A process that exited between the listing and this read is
            // simply not a member any more.
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            if group_of(&stat) == Some(self.pgid) {
                members.push(Pid::new(pid));
            }
        }
        Ok(members)
    }

    /// Whether the group still holds anything.
    pub(crate) fn is_empty(&self) -> bool {
        // Signal zero validates without delivering — the cheapest question
        // POSIX offers about a whole group at once.
        // SAFETY: as for `killpg` above.
        if unsafe { libc::killpg(self.pgid, 0) } == 0 {
            return false;
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => true,
            // Anything else — `EPERM` above all — means somebody is in
            // there, just not somebody this process may signal.
            _ => false,
        }
    }
}

/// The process group id out of a `/proc/<pid>/stat` line.
///
/// It is the fifth field, and the second is the executable name — in
/// parentheses, and free to contain spaces and parentheses of its own. So
/// the parse starts after the *last* `)` rather than splitting the line,
/// which is the standard way to read this file and the only one that
/// survives a program called `(ba) d name`.
#[cfg(all(unix, not(target_os = "macos")))]
fn group_of(stat: &str) -> Option<libc::pid_t> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    // What follows the name is state, parent, then group.
    after_name.split_whitespace().nth(2)?.parse().ok()
}

/// The POSIX number behind each signal this layer sends.
fn number(signal: Signal) -> libc::c_int {
    match signal {
        Signal::Interrupt => libc::SIGINT,
        Signal::WindowChanged => libc::SIGWINCH,
        Signal::Terminate => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    }
}

/// The reading and writing ends of the terminal.
///
/// Both sit on one duplicated descriptor. Duplicating shares the underlying
/// open file — which is also why setting it non-blocking below reaches the
/// library's own descriptor: they are two names for the same thing. That is
/// harmless precisely because nothing else reads or writes it.
pub(crate) fn io_ports(
    master: &dyn MasterPty,
) -> io::Result<(Box<dyn Read + Send>, Arc<dyn InputPort>)> {
    let raw = master
        .as_raw_fd()
        .ok_or_else(|| io::Error::other("the terminal exposes no descriptor"))?;
    let terminal = Arc::new(Terminal(duplicate(raw)?));
    set_non_blocking(terminal.0.as_raw_fd())?;
    Ok((Box::new(Reader(Arc::clone(&terminal))), terminal))
}

/// The duplicated terminal descriptor, shared by the reader and the writer.
struct Terminal(OwnedFd);

impl Terminal {
    /// One non-blocking read.
    fn read_once(&self, buffer: &mut [u8]) -> io::Result<usize> {
        // SAFETY: the descriptor is owned and open for the call's duration,
        // and the kernel writes at most `buffer.len()` bytes into a buffer
        // that long.
        let read =
            unsafe { libc::read(self.0.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
        if read < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }

    /// One non-blocking write; returns how much the terminal accepted.
    fn write_once(&self, bytes: &[u8]) -> io::Result<usize> {
        // SAFETY: as for `read_once`; the kernel reads at most `bytes.len()`
        // bytes from a buffer that long.
        let written =
            unsafe { libc::write(self.0.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(written as usize)
    }

    /// Wait for the terminal to become ready, or for `within` to pass.
    /// `false` means the wait timed out.
    fn wait(&self, events: libc::c_short, within: Option<Duration>) -> io::Result<bool> {
        // An absolute deadline, not a duration re-used per attempt. A signal
        // can cut the wait short at any moment, and resuming with the
        // *original* timeout would give the retry the full budget again —
        // so a stream of signals could stretch a bounded wait indefinitely,
        // and with it the write deadline built on top of this.
        let deadline = within.map(|within| Instant::now() + within);
        loop {
            let timeout = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(false);
                    }
                    // A sub-millisecond remainder is still time left, and
                    // rounding it down to zero would turn the wait into a
                    // spin.
                    i32::try_from(left.as_millis().max(1)).unwrap_or(i32::MAX)
                }
                None => -1,
            };
            let mut watch = libc::pollfd {
                fd: self.0.as_raw_fd(),
                events,
                revents: 0,
            };
            // SAFETY: one descriptor is passed, and the array really is one
            // element long.
            let ready = unsafe { libc::poll(&mut watch, 1, timeout) };
            if ready >= 0 {
                return Ok(ready > 0);
            }
            let err = io::Error::last_os_error();
            // A signal cut the wait short; nothing has changed about what we
            // are waiting for, so resume against what is left of the
            // deadline.
            if err.kind() != io::ErrorKind::Interrupted {
                return Err(err);
            }
        }
    }
}

impl InputPort for Terminal {
    fn wait_for_room(&self, within: Duration) -> io::Result<bool> {
        self.wait(libc::POLLOUT, Some(within))
    }

    fn accept(&self, bytes: &[u8]) -> io::Result<usize> {
        self.write_once(bytes)
    }
}

/// The blocking `Read` the reader thread expects, over a non-blocking
/// descriptor: wait for something to arrive, then take it.
struct Reader(Arc<Terminal>);

impl Read for Reader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            // No deadline: a session is idle far more often than it is
            // talking, and a reader that woke up to find nothing would burn
            // a core doing it.
            self.0.wait(libc::POLLIN, None)?;
            match self.0.read_once(buffer) {
                // Readiness can be revoked between the wait and the read —
                // by a hangup, or by a reader that never exists here but
                // that the kernel does not promise about. Wait again.
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                // Reading a terminal whose child has let go of the other end
                // fails with `EIO` here rather than returning zero bytes.
                // That is an ordinary end of stream, and translating it is
                // this descriptor's job: a reader that saw it raw would
                // report every clean exit as a crash.
                Err(err) if err.raw_os_error() == Some(libc::EIO) => return Ok(0),
                outcome => return outcome,
            }
        }
    }
}

/// Duplicate a descriptor into one this crate owns and will close.
///
/// Borrowing the library's descriptor number instead would leave the reader
/// thread writing into whatever the operating system handed that number to
/// next, once the terminal was closed underneath it.
fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: `dup` takes a descriptor by value and returns a fresh one.
    let duplicated = unsafe { libc::dup(fd) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the descriptor was just created by `dup` and is owned by
    // nothing else, which is exactly the claim `from_raw_fd` requires.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

/// Put the terminal in non-blocking mode, so a write can stop at a deadline
/// rather than at the child's convenience.
fn set_non_blocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: both calls take a descriptor and integer arguments only.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn the_group_survives_an_executable_name_that_looks_like_the_format() {
        use super::group_of;
        // The ordinary case, and then the reason this is not a `split`: a
        // name may hold spaces and parentheses, and a parser that split the
        // whole line would read part of the name as the group.
        assert_eq!(group_of("42 (bash) S 7 99 99 0 -1"), Some(99));
        assert_eq!(group_of("42 (weird ) name) S 7 99 99 0 -1"), Some(99));
        assert_eq!(group_of("42 ((()) S 7 99 99 0 -1"), Some(99));
        assert_eq!(group_of("nonsense"), None);
    }
}
