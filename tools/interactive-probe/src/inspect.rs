//! Post-exit OS inspection — what "nothing left behind" means, measured:
//! open-fd / handle counts (delta-based, so runner noise cannot flake a
//! lane), POSIX process-group enumeration by `getpgid` scan (the on-demand
//! traversal the runtime's PTY layer will use), and on Windows the job
//! object the probe itself creates and binds (the runtime's future
//! job-per-child pattern) plus the ConPTY console-host census.
//!
//! It lives in this library — next to the PTY plumbing every probe shares —
//! because two consumers must agree on it exactly: the cleanup probe's
//! fixture lanes and the live `/exit` lane. A fix to how emptiness is
//! measured must land in both at once, not drift per copy.

use std::time::{Duration, Instant};

/// What [`open_channels`] counts on this platform, for report labels.
pub const CHANNEL_KIND: &str = if cfg!(windows) { "handle" } else { "fd" };

/// How often emptiness and baseline polls re-check. Process death and fd
/// release are kernel-side immediate; the poll only rides out reap and
/// thread-exit races.
const POLL: Duration = Duration::from_millis(25);

/// The number of open file descriptors (POSIX) or handles (Windows) this
/// process holds right now. Compared as before/after deltas only — the
/// absolute number includes whatever the runtime and test harness already
/// hold, which is noise.
pub fn open_channels() -> Result<usize, String> {
    platform::open_channels()
}

/// Wait for the open-channel count to come back to `baseline` plus at most
/// `allowed_residue`. A moment of grace is built in rather than asserted
/// around: the reader thread drops its cloned fd microseconds *after* it
/// reports end-of-stream, so a single snapshot would race a release that
/// is already in flight.
///
/// The residue is an exact declaration, not a tolerance: it exists for a
/// platform cost the caller has *measured and named* — an OS-retained
/// handle the process holds no reference to and cannot close — and one
/// handle beyond it still fails. Callers with nothing to declare pass 0.
pub fn await_baseline(
    baseline: usize,
    allowed_residue: usize,
    timeout: Duration,
) -> Result<String, String> {
    let started = Instant::now();
    let target = baseline + allowed_residue;
    let mut count = open_channels()?;
    while count > target {
        if started.elapsed() >= timeout {
            return Err(format!(
                "open {CHANNEL_KIND} count is still {count} after {}ms (baseline {baseline}, allowed platform residue +{allowed_residue}, delta +{}) — something the run opened was never released",
                timeout.as_millis(),
                count - baseline,
            ));
        }
        std::thread::sleep(POLL);
        count = open_channels()?;
    }
    let settled = started.elapsed().as_millis();
    if count <= baseline {
        Ok(format!(
            "{CHANNEL_KIND}_delta=0 (baseline {baseline}, now {count}, settled in {settled}ms)"
        ))
    } else {
        Ok(format!(
            "{CHANNEL_KIND}_delta=+{} — entirely inside the declared platform residue of +{allowed_residue} (baseline {baseline}, now {count}, settled in {settled}ms)",
            count - baseline,
        ))
    }
}

#[cfg(unix)]
pub use platform::{
    GroupStanding, adopt_orphans, await_group_empty, group_has_members, kill_pid, pgid_of,
    process_alive, reap_adopted, signal_group, standing_in, surviving_members,
};

#[cfg(windows)]
pub use platform::{Job, console_hosts_parented_here, new_console_hosts};

#[cfg(unix)]
mod platform {
    use super::POLL;
    use std::time::{Duration, Instant};

    /// Count entries in the per-process fd directory. The `read_dir` itself
    /// holds one fd while iterating, so the count is one high — identically
    /// so at baseline and after, which is all a delta comparison needs.
    pub fn open_channels() -> Result<usize, String> {
        #[cfg(target_os = "linux")]
        const FD_DIR: &str = "/proc/self/fd";
        #[cfg(target_os = "macos")]
        const FD_DIR: &str = "/dev/fd";
        // The supported platforms are exactly these; a new Unix target must
        // bring its own fd-enumeration mechanism deliberately, not inherit
        // an undefined name and a puzzling compiler error.
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        compile_error!(
            "open_channels has no fd-count mechanism for this Unix target; add one to inspect::platform"
        );
        Ok(std::fs::read_dir(FD_DIR)
            .map_err(|err| format!("reading {FD_DIR} failed: {err}"))?
            .count())
    }

    /// Make this process the reaper for its orphaned descendants (Linux):
    /// a grandchild whose parent dies reparents here instead of to PID 1,
    /// so the probe can collect it deterministically. Containerized CI is
    /// the motivating environment — its PID 1 is typically a shell that
    /// never reaps, and an unreaped orphan's zombie satisfies `kill(pid,0)`
    /// and `getpgid` forever, reading as a survivor that cannot be killed.
    /// The other platforms' inits reap orphans promptly, so this is a
    /// documented no-op there. Returns what was arranged, for step details.
    pub fn adopt_orphans() -> &'static str {
        #[cfg(target_os = "linux")]
        {
            // SAFETY: prctl with PR_SET_CHILD_SUBREAPER takes plain
            // integer arguments and touches no memory.
            if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == 0 {
                "orphaned descendants reparent to this probe and are reaped by it (subreaper)"
            } else {
                "prctl(PR_SET_CHILD_SUBREAPER) failed; orphan reaping stays with init"
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            "orphan reaping stays with the platform init"
        }
    }

    /// Collect any zombie children — including adopted orphans — without
    /// blocking; returns how many were reaped. Emptiness polls call this
    /// each iteration: an adopted orphan counts as existing until reaped,
    /// and after `adopt_orphans` nobody else will reap it. Harmless when
    /// there is nothing to collect. Only for callers whose direct children
    /// are already reaped — `waitpid(-1)` would otherwise steal a live
    /// child's exit status from its owner.
    pub fn reap_adopted() -> u32 {
        let mut reaped = 0;
        loop {
            // SAFETY: waitpid with WNOHANG returns immediately; a null
            // status pointer is allowed.
            let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if pid <= 0 {
                return reaped;
            }
            reaped += 1;
        }
    }

    /// The process group a live PID belongs to, observed from outside — the
    /// probe-side cross-check against what a fixture reports about itself.
    pub fn pgid_of(pid: i32) -> Result<i32, String> {
        // SAFETY: getpgid takes a plain pid and touches no memory.
        let pgid = unsafe { libc::getpgid(pid) };
        if pgid == -1 {
            return Err(format!(
                "getpgid({pid}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(pgid)
    }

    /// Where a PID stands relative to a process group, as `getpgid` reports
    /// it — the same per-PID question the runtime's on-demand group
    /// enumeration asks.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum GroupStanding {
        /// Alive and in the group.
        Member,
        /// Alive and outside the group. `pgid` is the group it is actually
        /// in, or `None` where the OS refuses to say (EPERM for a process
        /// in another session — which is itself proof it left ours).
        Outside { pgid: Option<i32> },
        /// No such process.
        Gone,
    }

    pub fn standing_in(pgid: i32, pid: i32) -> GroupStanding {
        // SAFETY: getpgid takes a plain pid and touches no memory.
        let actual = unsafe { libc::getpgid(pid) };
        if actual == pgid {
            return GroupStanding::Member;
        }
        if actual != -1 {
            return GroupStanding::Outside { pgid: Some(actual) };
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => GroupStanding::Gone,
            // EPERM: some systems refuse getpgid across session boundaries;
            // being unreadable from our session *is* being outside it.
            _ => GroupStanding::Outside { pgid: None },
        }
    }

    /// The `design`-mandated enumeration mechanism: scan recorded candidate
    /// PIDs with `getpgid` and return the ones still in the group. Empty
    /// means the group holds none of the processes the run created.
    pub fn surviving_members(pgid: i32, candidates: &[i32]) -> Vec<i32> {
        candidates
            .iter()
            .copied()
            .filter(|pid| standing_in(pgid, *pid) == GroupStanding::Member)
            .collect()
    }

    /// Does the group have *any* member left — including ones the run never
    /// recorded? `kill(-pgid, 0)` delivers nothing and reports existence;
    /// it complements the candidate scan by catching unknown members.
    pub fn group_has_members(pgid: i32) -> Result<bool, String> {
        // SAFETY: kill with signal 0 validates, never delivers.
        if unsafe { libc::kill(-pgid, 0) } == 0 {
            return Ok(true);
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) => Ok(false),
            // EPERM means a member exists that is not ours to signal.
            Some(libc::EPERM) => Ok(true),
            _ => Err(format!(
                "kill(-{pgid}, 0) failed: {}",
                std::io::Error::last_os_error()
            )),
        }
    }

    pub fn process_alive(pid: i32) -> bool {
        // SAFETY: kill with signal 0 validates, never delivers.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        // EPERM still means "exists" — just not ours to signal.
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    /// Deliver `signal` to every member of the group — the PTY-owned
    /// terminate sequence's delivery primitive.
    pub fn signal_group(pgid: i32, signal: i32) -> Result<(), String> {
        // SAFETY: kill takes only plain values; the negative pid addresses
        // the process group.
        if unsafe { libc::kill(-pgid, signal) } != 0 {
            return Err(format!(
                "kill(-{pgid}, {signal}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// SIGKILL one PID — the reap path for a detected escapee, which
    /// group-scoped delivery deliberately cannot reach.
    pub fn kill_pid(pid: i32) -> Result<(), String> {
        // SAFETY: kill takes only plain values.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            return Err(format!(
                "kill({pid}, SIGKILL) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// Poll until the group reports no members, returning how long that
    /// took. The poll rides out reaping — a killed member counts as
    /// existing until collected — and does its own share of it: adopted
    /// orphans are this process's to reap (see [`adopt_orphans`]), so each
    /// iteration collects what has arrived.
    pub fn await_group_empty(pgid: i32, timeout: Duration) -> Result<u128, String> {
        let started = Instant::now();
        loop {
            reap_adopted();
            if !group_has_members(pgid)? {
                return Ok(started.elapsed().as_millis());
            }
            if started.elapsed() >= timeout {
                return Err(format!(
                    "process group {pgid} still has members after {}ms",
                    timeout.as_millis()
                ));
            }
            std::thread::sleep(POLL);
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessHandleCount, OpenProcess,
        PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    pub fn open_channels() -> Result<usize, String> {
        let mut count: u32 = 0;
        // SAFETY: the pseudo-handle from GetCurrentProcess is always valid;
        // count is a valid out-pointer.
        if unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) } == 0 {
            return Err(format!(
                "GetProcessHandleCount failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(count as usize)
    }

    /// A job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — the
    /// containment boundary the runtime will put every spawned child in.
    /// Deliberately *without* breakaway permission, so an escape attempt is
    /// denied at spawn rather than silently succeeding. Closing the handle
    /// (drop) kills whatever is still inside: even a probe that dies
    /// mid-run cannot leak its tree.
    pub struct Job(HANDLE);

    /// How many PIDs one membership query can return. The trees under test
    /// hold at most three processes; a list this size overflowing is not a
    /// bigger buffer's problem, it is a runaway fixture.
    const MAX_JOB_PIDS: usize = 256;

    /// `JOBOBJECT_BASIC_PROCESS_ID_LIST` with room for a real list — the
    /// windows-sys struct declares a one-element array the caller is
    /// expected to extend, C-style.
    #[repr(C)]
    struct PidList {
        header: JOBOBJECT_BASIC_PROCESS_ID_LIST,
        rest: [usize; MAX_JOB_PIDS - 1],
    }

    impl Job {
        pub fn create_kill_on_close() -> Result<Self, String> {
            // SAFETY: null attributes and name are documented defaults.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(format!(
                    "CreateJobObjectW failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let job = Self(handle);
            // SAFETY: a zeroed extended-limit block is valid; the one flag
            // set below is the whole configuration.
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: live job handle; the info block matches the class and
            // its size is passed alongside.
            let applied = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if applied == 0 {
                return Err(format!(
                    "SetInformationJobObject(KILL_ON_JOB_CLOSE) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(job)
        }

        /// Bind a live process into the job. Its *future* children inherit
        /// membership; anything it spawned beforehand does not — which is
        /// why the probes bind the root before telling it to grow its tree.
        pub fn assign(&self, pid: u32) -> Result<(), String> {
            // SAFETY: OpenProcess takes plain values; the handle is checked
            // before use and closed on every path.
            let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
            if process.is_null() {
                return Err(format!(
                    "OpenProcess({pid}) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: both handles are live; assignment takes no memory.
            let assigned = unsafe { AssignProcessToJobObject(self.0, process) };
            let assign_error = std::io::Error::last_os_error();
            // SAFETY: the handle came from OpenProcess above.
            unsafe { CloseHandle(process) };
            if assigned == 0 {
                return Err(format!(
                    "AssignProcessToJobObject({pid}) failed: {assign_error}"
                ));
            }
            Ok(())
        }

        /// The PIDs currently in the job — the Windows analogue of the
        /// POSIX getpgid scan, straight from the kernel's own bookkeeping.
        pub fn pids(&self) -> Result<Vec<u32>, String> {
            // SAFETY: a zeroed list is a valid out-parameter; the kernel
            // fills header and entries on success, which is checked.
            let mut list: PidList = unsafe { std::mem::zeroed() };
            // SAFETY: live job handle; the buffer really is `PidList` bytes
            // long and starts with the header the class expects.
            let queried = unsafe {
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicProcessIdList,
                    (&raw mut list).cast(),
                    std::mem::size_of::<PidList>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if queried == 0 {
                return Err(format!(
                    "QueryInformationJobObject(pid list) failed: {} — more than {MAX_JOB_PIDS} members would be a runaway fixture, not a small buffer",
                    std::io::Error::last_os_error()
                ));
            }
            let count = (list.header.NumberOfProcessIdsInList as usize).min(MAX_JOB_PIDS);
            // SAFETY: repr(C) lays `rest` directly after the header's
            // one-element array, so `count` entries are contiguous from its
            // start; addr_of avoids materializing an out-of-bounds slice
            // reference through the header field.
            let entries = unsafe {
                std::slice::from_raw_parts(
                    std::ptr::addr_of!(list.header.ProcessIdList).cast::<usize>(),
                    count,
                )
            };
            Ok(entries.iter().map(|pid| *pid as u32).collect())
        }

        /// Kill everything in the job — the Windows terminate sequence in
        /// its entirety (there is no polite OS phase to escalate from).
        pub fn terminate(&self, exit_code: u32) -> Result<(), String> {
            // SAFETY: live job handle, plain value argument.
            if unsafe { TerminateJobObject(self.0, exit_code) } == 0 {
                return Err(format!(
                    "TerminateJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            Ok(())
        }

        /// Poll until the job reports zero members, returning how long that
        /// took — the Windows counterpart of `await_group_empty`.
        pub fn await_empty(&self, timeout: std::time::Duration) -> Result<u128, String> {
            let started = std::time::Instant::now();
            loop {
                let pids = self.pids()?;
                if pids.is_empty() {
                    return Ok(started.elapsed().as_millis());
                }
                if started.elapsed() >= timeout {
                    return Err(format!(
                        "the job still holds {pids:?} after {}ms",
                        timeout.as_millis()
                    ));
                }
                std::thread::sleep(super::POLL);
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // KILL_ON_JOB_CLOSE makes this close the safety net: whatever
            // is still inside dies with the handle.
            // SAFETY: the handle came from CreateJobObjectW.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// The PIDs of ConPTY console hosts (`conhost.exe` / `OpenConsole.exe`)
    /// whose parent is this process. `CreatePseudoConsole` spawns the host
    /// as a child of the caller, so a before/after diff of this census is
    /// the "console host gone" assertion.
    pub fn console_hosts_parented_here() -> Result<Vec<u32>, String> {
        // SAFETY: GetCurrentProcessId reads the PEB, nothing more.
        let me = unsafe { GetCurrentProcessId() };
        // SAFETY: a process snapshot takes no input memory; the handle is
        // checked and closed on every path.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "CreateToolhelp32Snapshot failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // SAFETY: a zeroed entry with dwSize set is the documented starting
        // state for Process32FirstW.
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut hosts = Vec::new();
        // SAFETY: live snapshot handle; entry is a valid out-pointer.
        let mut more = unsafe { Process32FirstW(snapshot, &mut entry) };
        if more == 0 {
            // A process snapshot cannot legitimately be empty — this
            // process is in it — so a failing first read is an error. It
            // must surface as one: an empty census here would silently
            // turn the console-host leak check into a guaranteed pass.
            let err = std::io::Error::last_os_error();
            // SAFETY: the handle came from CreateToolhelp32Snapshot above.
            unsafe { CloseHandle(snapshot) };
            return Err(format!("Process32FirstW failed: {err}"));
        }
        while more != 0 {
            let len = entry
                .szExeFile
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
            if entry.th32ParentProcessID == me
                && (name == "conhost.exe" || name == "openconsole.exe")
            {
                hosts.push(entry.th32ProcessID);
            }
            // SAFETY: as for Process32FirstW.
            more = unsafe { Process32NextW(snapshot, &mut entry) };
        }
        // The walk's only legitimate end is running out of entries; any
        // other terminating error means the census is incomplete and a
        // leaked host could be hiding in the unread remainder.
        let end = std::io::Error::last_os_error();
        // SAFETY: the handle came from CreateToolhelp32Snapshot above.
        unsafe { CloseHandle(snapshot) };
        if end.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
            return Err(format!("Process32NextW ended early: {end}"));
        }
        Ok(hosts)
    }

    /// Console hosts alive now that were not in the `before` census — the
    /// ones a run created and, after teardown, the ones it leaked. Hosts
    /// already present at baseline are pre-existing noise, not this run's.
    pub fn new_console_hosts(before: &[u32]) -> Result<Vec<u32>, String> {
        Ok(console_hosts_parented_here()?
            .into_iter()
            .filter(|pid| !before.contains(pid))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Channel counts are process-wide state and the test harness runs
    /// tests concurrently, so a raw before/after comparison can be crossed
    /// by a neighbouring test opening a file. Retry until one attempt sees
    /// a clean window; only a systematic miscount fails all of them.
    #[test]
    fn open_channels_sees_an_acquisition_and_its_release() {
        let mut last = String::new();
        for _ in 0..10 {
            let baseline = open_channels().unwrap();
            let held = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
            let during = open_channels().unwrap();
            drop(held);
            let after = open_channels().unwrap();
            if during == baseline + 1 && after == baseline {
                return;
            }
            last = format!("baseline {baseline}, during {during}, after {after}");
        }
        panic!("no attempt saw the +1/-1 pattern; last: {last}");
    }

    #[test]
    fn await_baseline_reports_a_leak_instead_of_settling() {
        // A deliberately held channel must fail the settle with the delta
        // named, not pass or hang. Same concurrency caveat as above: a
        // neighbouring test *closing* channels mid-window can sink the
        // count to baseline despite the held file, so retry until one
        // attempt sees the leak; only an implementation that never reports
        // one fails every attempt.
        for _ in 0..10 {
            let baseline = open_channels().unwrap();
            let _held = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
            if let Err(err) = await_baseline(baseline, 0, Duration::from_millis(80)) {
                assert!(err.contains("delta"), "the delta must be named: {err}");
                return;
            }
        }
        panic!("a held channel settled to baseline on every attempt — the leak is never reported");
    }

    #[test]
    fn a_declared_residue_is_reported_not_silently_absorbed() {
        // One held channel inside a declared residue of one settles — but
        // the detail must say the residue was consumed, never plain
        // delta=0. Retried for the same concurrency reason as above.
        for _ in 0..10 {
            let baseline = open_channels().unwrap();
            let _held = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
            if let Ok(detail) = await_baseline(baseline, 1, Duration::from_millis(80)) {
                if detail.contains("delta=0") {
                    // Concurrent closes hid the held file; try again.
                    continue;
                }
                assert!(
                    detail.contains("declared platform residue"),
                    "the residue must be named: {detail}"
                );
                return;
            }
        }
        panic!("a held channel never settled inside a declared residue of one");
    }

    #[cfg(unix)]
    mod pgroup {
        use super::super::*;

        fn spawn_sleeper() -> std::process::Child {
            std::process::Command::new("sleep")
                .arg("30")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("sleep must spawn")
        }

        #[test]
        fn the_scan_finds_a_planted_member_and_its_departure() {
            // A plain child shares our process group: the enumeration
            // mechanism must find it while it lives and lose it once it is
            // killed and reaped.
            // SAFETY: getpgrp takes nothing and cannot fail.
            let pgid = unsafe { libc::getpgrp() };
            let mut child = spawn_sleeper();
            let pid = i32::try_from(child.id()).unwrap();

            assert_eq!(standing_in(pgid, pid), GroupStanding::Member);
            assert_eq!(surviving_members(pgid, &[pid]), vec![pid]);
            assert!(process_alive(pid));
            assert!(group_has_members(pgid).unwrap());

            child.kill().unwrap();
            // A concurrent test's emptiness poll runs reap_adopted, and
            // waitpid(-1) is process-global: it may collect this child
            // first, turning our own wait into ECHILD. Both outcomes mean
            // the same thing — the child is reaped — so the error is not
            // one.
            let _ = child.wait();
            assert_eq!(standing_in(pgid, pid), GroupStanding::Gone);
            assert!(surviving_members(pgid, &[pid]).is_empty());
        }

        #[test]
        fn a_process_in_another_group_reads_as_outside() {
            // PID 1 (init/launchd) is alive and definitively not in our
            // group; some systems answer getpgid for it, others refuse
            // across the session boundary — both spell "outside".
            // SAFETY: getpgrp takes nothing and cannot fail.
            let pgid = unsafe { libc::getpgrp() };
            assert!(matches!(
                standing_in(pgid, 1),
                GroupStanding::Outside { .. }
            ));
            assert!(surviving_members(pgid, &[1]).is_empty());
        }

        #[test]
        fn an_empty_group_awaits_immediately() {
            // A sleeper is moved into a group of its own making via the
            // probe-side primitives... which POSIX does not offer for a
            // foreign process — so the empty case is exercised on a group
            // id that certainly has no members: a fresh child's PID after
            // it is gone (pgid == pid only for group leaders; a dead
            // non-leader's pid is a vacant group id).
            let mut child = spawn_sleeper();
            let pid = i32::try_from(child.id()).unwrap();
            child.kill().unwrap();
            // Reaped by us or by a concurrent test's reap_adopted — either
            // way the group id is vacant (see the sibling test's note).
            let _ = child.wait();
            let ms = await_group_empty(pid, Duration::from_secs(5)).unwrap();
            assert!(ms < 5_000);
        }
    }

    #[cfg(windows)]
    mod job {
        use super::super::*;
        use std::time::Instant;

        fn spawn_sleeper() -> std::process::Child {
            // ping with a count is the portable Windows sleeper: cmd's
            // `timeout` refuses to run without a console stdin.
            std::process::Command::new("ping")
                .args(["-n", "30", "127.0.0.1"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("ping must spawn")
        }

        #[test]
        fn a_job_tracks_membership_and_terminate_empties_it() {
            let job = Job::create_kill_on_close().unwrap();
            let mut child = spawn_sleeper();
            job.assign(child.id()).unwrap();
            assert!(
                job.pids().unwrap().contains(&child.id()),
                "the assigned child must appear in the job's pid list"
            );

            job.terminate(0).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if job.pids().unwrap().is_empty() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "the job did not empty within 5s of TerminateJobObject"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            child.wait().expect("the terminated child must be reapable");
        }

        #[test]
        fn the_console_host_census_answers() {
            // Contents depend on the harness's console arrangement; the
            // census itself must simply work.
            console_hosts_parented_here().unwrap();
        }
    }
}
