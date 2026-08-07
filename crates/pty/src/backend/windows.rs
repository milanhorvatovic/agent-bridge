//! Windows: a job object for containment, and ConPTY's rough edges guarded.
//!
//! Windows has no process groups worth signalling and no signals worth
//! sending, so containment is a [job object] the child is bound to, created
//! with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. That flag is the important
//! one: it makes the operating system kill everything inside the job when
//! the last handle to it closes, which covers the case no in-process cleanup
//! can — this runtime being killed outright.
//!
//! ConPTY also runs a console host process of its own for each terminal, and
//! that host is *not* put in the job. Terminating the job would then destroy
//! the terminal along with the child, and output already buffered in it
//! would be lost between "the session ended" and "somebody read the last of
//! what it said". The host is released the other way the platform allows:
//! closing the pseudo-console, which this module does — with a deadline,
//! because that close is one of the four ways ConPTY is known to hang.
//!
//! The four, all guarded here or in the shared sequence next door:
//!
//! - allocation can hang when the console subsystem has not initialised, so
//!   it runs against a deadline (in the parent module, which allocates);
//! - the child blocks on a cursor-position query until the terminal answers,
//!   so the reader answers it (in `reader`, on both platforms);
//! - waiting on the child can hang, so its exit is polled and never waited
//!   on (in the parent module);
//! - closing the pseudo-console can deadlock when buffered output has no
//!   reader ([microsoft/terminal#1810]), so it happens on a helper thread
//!   with a deadline, here.
//!
//! [job object]: https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects
//! [microsoft/terminal#1810]: https://github.com/microsoft/terminal/issues/1810

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::MasterPty;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE, TerminateProcess,
    WaitForSingleObject,
};

use super::{InputPort, lock};
use crate::error::foreign;
use crate::process::{Pid, Signal};

/// How much of a payload write is handed to the operating system at once.
///
/// A write to the terminal's input pipe cannot be recalled once issued, so
/// the deadline is only honoured between pieces: a small piece bounds how
/// far past the deadline a stalled write can run, and bounds the ambiguity
/// about what did and did not reach the child to at most one piece.
const WRITE_PIECE_BYTES: usize = 512;

/// How long closing the pseudo-console gets before it is treated as the
/// known deadlock rather than slow work.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the console host gets to exit on its own after the
/// pseudo-console is closed, before it is killed outright.
const HOST_EXIT_WINDOW: Duration = Duration::from_secs(2);

/// How often a wait for a process to disappear re-checks.
const EXIT_POLL: Duration = Duration::from_millis(25);

/// What this platform adds to the terminal defaults every child is given.
///
/// Nothing. Windows has no locale environment convention — its console
/// encoding is a code page, which ConPTY already holds at UTF-8 — so a
/// locale variable here would be a default nothing reads.
pub(crate) fn env_defaults() -> Vec<(OsString, OsString)> {
    Vec::new()
}

/// The console hosts that existed before this terminal was allocated.
///
/// ConPTY starts its host as a child of whoever created the pseudo-console,
/// so the hosts belonging to *this* terminal are the ones that appear
/// between this census and the next. A machine running several sessions has
/// several, and killing another session's would take its terminal with it.
pub(crate) struct Pending {
    hosts: Vec<u32>,
}

impl Pending {
    pub(crate) fn observe() -> Self {
        Self {
            // A census that cannot be taken is not a reason to refuse a
            // session. It costs the console-host half of cleanup, which the
            // pseudo-console close covers anyway; the log is so that a host
            // found leaked later has an explanation.
            hosts: console_hosts().unwrap_or_else(|err| {
                tracing::warn!(%err, "the console hosts already running could not be listed");
                Vec::new()
            }),
        }
    }

    pub(crate) fn contain(self, child: Pid) -> io::Result<Containment> {
        let job = Job::create_kill_on_close()?;
        job.assign(child.get())?;
        let appeared = console_hosts().unwrap_or_else(|err| {
            tracing::warn!(%err, "this terminal's console host could not be identified");
            Vec::new()
        });
        // A handle is opened now, while the host is certainly alive, and held
        // for the terminal's lifetime. Looking the host up by number again at
        // teardown would be a race with a sharp edge: a process id is reused
        // once its process is gone, so a host that exited on its own could
        // leave this layer holding the number of something else entirely —
        // and the teardown path below *kills* what that number names.
        let hosts = appeared
            .into_iter()
            .filter(|pid| !self.hosts.contains(pid))
            .filter_map(|pid| match open_process(pid, PROCESS_TERMINATE | SYNCHRONIZE) {
                Ok(handle) => Some((pid, handle)),
                // Already gone, or not ours to open. Either way there is
                // nothing left to hold or to kill.
                Err(err) => {
                    tracing::debug!(host = pid, %err, "the console host could not be held open");
                    None
                }
            })
            .collect();
        Ok(Containment { job, hosts })
    }
}

/// The job object holding the child and everything it spawns, plus the
/// console hosts this terminal brought with it.
pub(crate) struct Containment {
    job: Job,
    /// The console hosts this terminal brought with it, each held open so
    /// that the identity cannot change underneath us. The number is kept
    /// alongside for the log only.
    hosts: Vec<(u32, Process)>,
}

impl Containment {
    /// Windows offers no catchable "please stop" for a process in somebody
    /// else's console, so a termination request and a kill are the same
    /// operation. Saying so is better than pretending there is a polite
    /// phase and quietly killing anyway.
    pub(crate) fn signal(&self, signal: Signal) -> io::Result<()> {
        match signal {
            Signal::Terminate | Signal::Kill => self.job.terminate(),
            Signal::Interrupt => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "an interrupt cannot be delivered to a process in another console; \
                 write the interrupt byte to the terminal instead",
            )),
            Signal::WindowChanged => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "there is no resize notification to send separately; resizing the \
                 terminal is what notifies the child",
            )),
        }
    }

    /// Which processes are still in the job — the hosted child and
    /// everything it spawned. Straight from the kernel's own bookkeeping,
    /// which is what makes this cheap here and a walk over live processes
    /// on the other platform.
    pub(crate) fn contained(&self) -> io::Result<Vec<Pid>> {
        // Propagated, not swallowed. A query that failed is not a session
        // holding nothing, and collapsing the two would report a leaked
        // process tree as a clean one — the same distinction `is_empty`
        // below is careful about, and the same one the other platform's
        // implementation keeps.
        Ok(self.job.members()?.into_iter().map(Pid::new).collect())
    }

    pub(crate) fn is_empty(&self) -> bool {
        // Unreadable is not empty: claiming a job holds nothing because the
        // question failed would report a leak as a clean teardown.
        self.job.members().is_ok_and(|members| members.is_empty())
    }

    /// Close the terminal, and make sure its console host went with it.
    pub(crate) fn release(&self, master: Box<dyn MasterPty + Send>) {
        let (sender, closed) = std::sync::mpsc::channel();
        // The close is the documented deadlock, so it happens somewhere this
        // thread can walk away from. On timeout the helper is abandoned: it
        // is stuck in a call that will not return, and joining it would move
        // the deadlock here.
        let spawned = std::thread::Builder::new()
            .name("pty-close".to_string())
            .spawn(move || {
                drop(master);
                let _ = sender.send(());
            });
        match spawned {
            Ok(_) => {
                if closed.recv_timeout(CLOSE_TIMEOUT) == Err(RecvTimeoutError::Timeout) {
                    tracing::warn!(
                        "closing the pseudo-console did not complete; \
                         this is the known ConPTY teardown deadlock"
                    );
                }
            }
            Err(err) => tracing::warn!(%err, "the pseudo-console could not be closed"),
        }
        self.reap_console_hosts();
    }

    /// A console host normally exits when its pseudo-console closes. One
    /// that did not is a leaked process holding a terminal open, so it is
    /// killed rather than left for somebody to find in Task Manager.
    fn reap_console_hosts(&self) {
        let deadline = Instant::now() + HOST_EXIT_WINDOW;
        let mut surviving: Vec<&(u32, Process)> = self.hosts.iter().collect();
        while !surviving.is_empty() && Instant::now() < deadline {
            surviving.retain(|(_, host)| !host.has_exited());
            if surviving.is_empty() {
                return;
            }
            std::thread::sleep(EXIT_POLL);
        }
        for (pid, host) in surviving {
            tracing::warn!(
                host = pid,
                "the console host outlived its terminal; killing it"
            );
            if let Err(err) = host.terminate() {
                tracing::warn!(host = pid, %err, "the console host could not be killed");
            }
        }
    }
}

/// The two ends of the terminal.
///
/// Unlike POSIX, these come from the terminal library: ConPTY's ends are
/// ordinary pipes with no readiness to poll and no way to make a write
/// return early, so there is nothing a descriptor of our own would buy.
pub(crate) fn io_ports(
    master: &dyn MasterPty,
) -> io::Result<(Box<dyn Read + Send>, Arc<dyn InputPort>)> {
    let source = master.try_clone_reader().map_err(foreign)?;
    let sink = master.take_writer().map_err(foreign)?;
    Ok((
        source,
        Arc::new(Terminal {
            sink: Mutex::new(sink),
        }),
    ))
}

/// The terminal's input, written to under a lock that is released between
/// pieces so an interrupt can get in.
struct Terminal {
    sink: Mutex<Box<dyn Write + Send>>,
}

impl InputPort for Terminal {
    /// There is nothing to wait for. A console's input is an ordinary pipe
    /// with no readiness to ask about, and the console host drains it
    /// whether or not the child is reading — which is also why a write that
    /// stalls here is far rarer than on a POSIX terminal.
    fn wait_for_room(&self, _within: Duration) -> io::Result<bool> {
        Ok(true)
    }

    /// One piece at a time, under a lock taken per piece rather than held
    /// across the payload, so a control write lands between pieces instead
    /// of waiting out the deadline behind them.
    ///
    /// A piece already handed to the operating system cannot be recalled, so
    /// this is also what bounds how far past its deadline a stalled write
    /// can run, and bounds what a caller is told about to a single piece.
    fn accept(&self, bytes: &[u8]) -> io::Result<usize> {
        let end = WRITE_PIECE_BYTES.min(bytes.len());
        let mut sink = lock(&self.sink);
        let accepted = sink.write(&bytes[..end])?;
        sink.flush()?;
        Ok(accepted)
    }
}

/// A job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
///
/// Created without breakaway permission on purpose: a child that tries to
/// escape the job is refused at spawn rather than escaping silently.
struct Job(HANDLE);

// SAFETY: a job object is a kernel object addressed by handle, and every
// call this module makes on it is documented as safe to issue from any
// thread. The handle itself is never mutated after creation.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

/// How many members one membership query can return.
///
/// A session hosts one CLI and the handful of processes it shells out to. A
/// job holding more than this is a runaway, which a larger buffer would hide
/// rather than fix.
const MAX_JOB_MEMBERS: usize = 256;

/// `JOBOBJECT_BASIC_PROCESS_ID_LIST` with room for a real list: the declared
/// struct ends in a one-element array the caller is expected to extend.
#[repr(C)]
struct MemberList {
    header: JOBOBJECT_BASIC_PROCESS_ID_LIST,
    rest: [usize; MAX_JOB_MEMBERS - 1],
}

impl Job {
    fn create_kill_on_close() -> io::Result<Self> {
        // SAFETY: null attributes and a null name are the documented
        // defaults for an unnamed job with default security.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Self(handle);
        // SAFETY: a zeroed limit block is valid, and the one flag set below
        // is the whole configuration.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: a live job handle, an info block matching the class, and
        // its size passed alongside.
        let applied = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if applied == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Bind a running process into the job. Everything it spawns afterwards
    /// inherits membership; anything it spawned before does not, which is
    /// why this happens as soon as the child exists.
    fn assign(&self, pid: u32) -> io::Result<()> {
        let process = open_process(pid, PROCESS_SET_QUOTA | PROCESS_TERMINATE)?;
        // SAFETY: both handles are live for the duration of the call.
        let assigned = unsafe { AssignProcessToJobObject(self.0, process.0) };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The processes currently in the job, straight from the kernel's own
    /// bookkeeping — the Windows counterpart of asking a process group who
    /// is in it.
    fn members(&self) -> io::Result<Vec<u32>> {
        // SAFETY: a zeroed list is a valid out-parameter; the kernel fills
        // the header and entries on success, which is checked below.
        let mut list: MemberList = unsafe { std::mem::zeroed() };
        // SAFETY: a live job handle, and a buffer that really is
        // `MemberList` bytes long starting with the header the class expects.
        let queried = unsafe {
            QueryInformationJobObject(
                self.0,
                JobObjectBasicProcessIdList,
                (&raw mut list).cast(),
                size_of::<MemberList>() as u32,
                std::ptr::null_mut(),
            )
        };
        if queried == 0 {
            return Err(io::Error::last_os_error());
        }
        let count = (list.header.NumberOfProcessIdsInList as usize).min(MAX_JOB_MEMBERS);
        // SAFETY: `repr(C)` lays `rest` directly after the header's
        // one-element array, so `count` entries are contiguous from its
        // start. Taking the address avoids materialising a reference to a
        // slice that runs past the declared array.
        let entries = unsafe {
            std::slice::from_raw_parts(
                (&raw const list.header.ProcessIdList).cast::<usize>(),
                count,
            )
        };
        Ok(entries.iter().map(|pid| *pid as u32).collect())
    }

    /// Kill everything in the job at once. There is no gentler step to
    /// escalate from.
    fn terminate(&self) -> io::Result<()> {
        // SAFETY: a live job handle and a plain exit code.
        if unsafe { TerminateJobObject(self.0, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // `KILL_ON_JOB_CLOSE` makes this the safety net: whatever is still
        // inside dies with the handle, including when this process is going
        // down in a way that runs no cleanup of its own.
        // SAFETY: the handle came from `CreateJobObjectW` and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// A process handle that closes itself.
struct Process(HANDLE);

impl Process {
    /// Whether the process has ended.
    ///
    /// Asked of the handle rather than of the number: a handle names one
    /// process for as long as it is held, so this cannot answer about a
    /// different process that inherited the id.
    fn has_exited(&self) -> bool {
        // SAFETY: a live handle opened with `SYNCHRONIZE`; a zero timeout
        // polls rather than waits.
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }

    fn terminate(&self) -> io::Result<()> {
        // SAFETY: a live handle opened for termination, and a plain exit code.
        if unsafe { TerminateProcess(self.0, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

// SAFETY: a process handle is a kernel object; the calls this module makes
// on it are documented as safe from any thread, and it is never mutated
// after `OpenProcess` returns it.
unsafe impl Send for Process {}
unsafe impl Sync for Process {}

impl Drop for Process {
    fn drop(&mut self) {
        // SAFETY: the handle came from `OpenProcess` and is closed once.
        unsafe { CloseHandle(self.0) };
    }
}

fn open_process(pid: u32, access: u32) -> io::Result<Process> {
    // SAFETY: plain arguments; the returned handle is checked before use.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        return Err(io::Error::last_os_error());
    }
    Ok(Process(handle))
}

/// The console hosts currently parented to this process.
///
/// `CreatePseudoConsole` starts the host as a child of the caller, so
/// parentage is what distinguishes the ones this runtime is responsible for
/// from every other console on the machine.
fn console_hosts() -> io::Result<Vec<u32>> {
    // SAFETY: reads this process's own identity and touches no memory.
    let me = unsafe { GetCurrentProcessId() };
    // SAFETY: a process snapshot takes no input memory; the handle is
    // checked and closed on every path below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let snapshot = Process(snapshot);
    // SAFETY: a zeroed entry with `dwSize` set is the documented starting
    // state for `Process32FirstW`.
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    // SAFETY: a live snapshot handle and a valid out-pointer.
    let mut more = unsafe { Process32FirstW(snapshot.0, &mut entry) };
    if more == 0 {
        // A snapshot cannot legitimately be empty — this process is in it —
        // so a failing first read is a real error. Returning an empty list
        // instead would silently turn every console-host check into a pass.
        return Err(io::Error::last_os_error());
    }
    let mut hosts = Vec::new();
    while more != 0 {
        let name = entry_name(&entry);
        if entry.th32ParentProcessID == me && (name == "conhost.exe" || name == "openconsole.exe") {
            hosts.push(entry.th32ProcessID);
        }
        // SAFETY: as for `Process32FirstW`.
        more = unsafe { Process32NextW(snapshot.0, &mut entry) };
    }
    // Running out of entries is the walk's only legitimate end; any other
    // error means the census stopped early and a host could be hiding in the
    // part that was never read.
    let ended = io::Error::last_os_error();
    if ended.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
        return Err(ended);
    }
    Ok(hosts)
}

/// The executable name from a snapshot entry, lowercased for comparison.
fn entry_name(entry: &PROCESSENTRY32W) -> String {
    let end = entry
        .szExeFile
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(entry.szExeFile.len());
    String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase()
}
