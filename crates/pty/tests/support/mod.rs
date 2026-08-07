//! The parent half: driving a real terminal, and reading what comes back.
//!
//! Every scenario here allocates an actual pseudo-terminal and runs an
//! actual process in it. Nothing is mocked, deliberately — a mocked terminal
//! would have to invent the behavior these tests exist to pin down, and
//! would then agree with whatever the implementation happened to do.
//!
//! Scenarios run in one process, one after another. That is a requirement
//! rather than a simplification: open-descriptor counts, process-group
//! membership, and the census of console hosts are all process-wide, so two
//! scenarios running at once would read each other's leftovers as their own.

// This module is compiled into both integration targets, and each one uses a
// different part of it — the descriptor census belongs to the probe ports,
// the process-group questions to the contract suite. Unused *here* therefore
// does not mean unused.
#![allow(dead_code)]

pub mod child;

use std::time::{Duration, Instant};

use agent_bridge_pty::{Dimensions, EndOfStream, Pty, ReadChunk, ReadStream, SpawnSpec, spawn};

/// The geometry scenarios use unless they are testing geometry.
///
/// Wide on purpose: a console reflows output to the terminal's width, and a
/// report line wrapped mid-field would stop parsing. The fixtures' longest
/// line is well under half of this.
pub const WIDE: Dimensions = Dimensions {
    cols: 200,
    rows: 50,
};

/// How long a scenario waits for something it expects to see.
///
/// Generous next to what these fixtures do — they report within
/// milliseconds — because a shared build machine under load is not a failing
/// implementation, and a flaky suite is worse than a slow one.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// One named check.
pub struct Scenario {
    pub name: &'static str,
    /// Returns what it established, for the report line, or why it could
    /// not.
    pub check: fn() -> Result<String, String>,
}

/// Run the suite, or become a fixture when handed a role.
///
/// Every scenario runs even after one fails: a single run should say
/// everything that is wrong, not just the first thing.
pub fn main(suite: &str, scenarios: &[Scenario]) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(role) = args.first() {
        child::run(role, &args[1..]);
    }
    #[cfg(unix)]
    adopt_orphans();

    let mut failed = 0;
    for scenario in scenarios {
        let (status, detail) = match (scenario.check)() {
            Ok(detail) => ("pass", detail),
            Err(detail) => {
                failed += 1;
                ("fail", detail)
            }
        };
        println!(
            "{suite} step={} status={status} detail=\"{}\"",
            scenario.name,
            one_line(&detail)
        );
    }
    println!("{suite} scenarios={} failed={failed}", scenarios.len());
    if failed > 0 {
        std::process::exit(1);
    }
}

/// Flatten a detail into something a `key="value"` line survives.
fn one_line(text: &str) -> String {
    text.replace(['"', '\n', '\r'], " ")
}

/// A live fixture: the terminal, the child in it, and everything it has said.
pub struct Session {
    pub pty: Box<dyn Pty>,
    output: ReadStream,
    /// Every byte the child produced, in order — including runs that were
    /// not valid text, so a scenario can compare against exactly what was
    /// written.
    raw: Vec<u8>,
    /// The undecodable runs, where they occurred.
    invalid: Vec<(u64, Vec<u8>)>,
    /// Why the stream ended, once it has.
    ended: Option<String>,
}

impl Session {
    /// Start this test binary again in the named fixture role.
    pub fn start(role: &str, args: &[&str]) -> Result<Self, String> {
        Self::with(role, args, |_| {})
    }

    /// [`Session::start`], with a chance to adjust the spec first.
    pub fn with(
        role: &str,
        args: &[&str],
        adjust: impl FnOnce(&mut SpawnSpec),
    ) -> Result<Self, String> {
        let own =
            std::env::current_exe().map_err(|err| format!("cannot find this binary: {err}"))?;
        let mut spec = SpawnSpec::new(own);
        spec.args.push(role.into());
        spec.args.extend(args.iter().map(Into::into));
        spec.dimensions = Some(WIDE);
        adjust(&mut spec);
        Self::spawn(&spec)
    }

    pub fn spawn(spec: &SpawnSpec) -> Result<Self, String> {
        let spawned = spawn(spec).map_err(|err| format!("spawn failed: {err}"))?;
        Ok(Self {
            pty: spawned.pty,
            output: spawned.output,
            raw: Vec::new(),
            invalid: Vec::new(),
            ended: None,
        })
    }

    /// Take in whatever has arrived within `slice`.
    pub fn pump(&mut self, slice: Duration) {
        let deadline = Instant::now() + slice;
        while self.ended.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            match self.output.recv_timeout(left) {
                Ok(ReadChunk::Output(bytes)) => self.raw.extend_from_slice(&bytes),
                Ok(ReadChunk::Invalid { offset, bytes }) => {
                    self.raw.extend_from_slice(&bytes);
                    self.invalid.push((offset, bytes));
                }
                Ok(ReadChunk::End(end)) => {
                    self.ended = Some(match end {
                        EndOfStream::Eof => "eof".to_string(),
                        EndOfStream::Failed(err) => format!("read failed: {err}"),
                    });
                }
                Err(_) => return,
            }
        }
    }

    /// Wait until the child's output contains `marker`.
    pub fn wait_for(&mut self, marker: &str) -> Result<(), String> {
        let deadline = Instant::now() + PATIENCE;
        loop {
            if self.visible().contains(marker) {
                return Ok(());
            }
            if let Some(end) = &self.ended {
                return Err(format!(
                    "the stream ended ({end}) before `{marker}`; tail: {}",
                    self.tail()
                ));
            }
            if Instant::now() >= deadline {
                return Err(format!("`{marker}` never appeared; tail: {}", self.tail()));
            }
            self.pump(Duration::from_millis(100));
        }
    }

    /// Wait until the child stops producing output for `quiet`.
    ///
    /// A content-free way to say "it has finished reacting", which beats
    /// matching a banner the fixture is free to reword.
    pub fn settle(&mut self, quiet: Duration) {
        let mut last = self.raw.len();
        let mut unchanged_since = Instant::now();
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            self.pump(Duration::from_millis(50));
            if self.raw.len() != last {
                last = self.raw.len();
                unchanged_since = Instant::now();
            } else if unchanged_since.elapsed() >= quiet {
                return;
            }
        }
    }

    /// Close the terminal and drain to the end of the stream, returning why
    /// it ended.
    ///
    /// Dropping the handle is the portable way to get there: on Windows the
    /// pseudo-console holds its output open until it is closed, so a
    /// terminated child alone never ends the stream. Draining afterwards
    /// also lets the reader thread finish, which is what releases the
    /// descriptor it holds — so anything counting descriptors must come
    /// through here first.
    pub fn close_and_drain(self) -> Result<String, String> {
        let Session {
            pty, output, ended, ..
        } = self;
        drop(pty);
        if let Some(reason) = ended {
            return Ok(reason);
        }
        let deadline = Instant::now() + PATIENCE;
        loop {
            match output.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(ReadChunk::End(EndOfStream::Eof)) => return Ok("eof".to_string()),
                Ok(ReadChunk::End(EndOfStream::Failed(err))) => {
                    return Ok(format!("read failed: {err}"));
                }
                Ok(_) => {}
                Err(_) => {
                    return Err("the stream never ended after the terminal was closed".to_string());
                }
            }
        }
    }

    /// Everything the child has said, as a human would have read it.
    pub fn visible(&self) -> String {
        strip_ansi(&String::from_utf8_lossy(&self.raw))
    }

    /// The undecodable runs reported so far.
    pub fn invalid(&self) -> &[(u64, Vec<u8>)] {
        &self.invalid
    }

    /// The value of the last `name=value` field the child reported.
    pub fn field(&self, name: &str) -> Option<String> {
        let visible = self.visible();
        visible
            .split_whitespace()
            .filter_map(|token| token.strip_prefix(&format!("{name}=")))
            .next_back()
            .map(str::to_string)
    }

    /// How many times the child reported `name=value`.
    pub fn count(&self, field: &str) -> usize {
        self.visible()
            .split_whitespace()
            .filter(|token| token.starts_with(&format!("{field}=")))
            .count()
    }

    pub fn ended(&self) -> Option<&str> {
        self.ended.as_deref()
    }

    /// The last of the output, for a failure message.
    pub fn tail(&self) -> String {
        let visible = self.visible();
        let start = visible
            .char_indices()
            .rev()
            .nth(300)
            .map_or(0, |(at, _)| at);
        one_line(&visible[start..])
    }
}

/// Drop escape sequences so an assertion sees what a person would read.
///
/// A console brackets even trivial output with cursor and colour control, so
/// matching raw bytes would be matching the terminal rather than the child.
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // A control sequence runs to its final byte.
            Some('[') => {
                for ch in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&ch) {
                        break;
                    }
                }
            }
            // An operating-system command runs to a bell or a string
            // terminator.
            Some(']') => {
                while let Some(ch) = chars.next() {
                    if ch == '\x07' {
                        break;
                    }
                    // Peek rather than consume: an escape inside the payload
                    // that is not the terminator must not eat the character
                    // that is.
                    if ch == '\x1b' && chars.clone().next() == Some('\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Anything else is a two-character escape, both consumed.
            _ => {}
        }
    }
    out
}

/// What [`open_channels`] counts on this platform, for report lines.
pub const CHANNEL_KIND: &str = if cfg!(windows) { "handle" } else { "fd" };

/// How many descriptors — handles, on Windows — this process holds right
/// now.
///
/// Only ever compared as a before-and-after difference: the absolute number
/// includes whatever the test binary and its runtime already hold, which is
/// noise.
pub fn open_channels() -> Result<usize, String> {
    #[cfg(target_os = "linux")]
    const FD_DIR: &str = "/proc/self/fd";
    #[cfg(target_os = "macos")]
    const FD_DIR: &str = "/dev/fd";

    #[cfg(unix)]
    {
        // The directory read itself holds one descriptor while iterating, so
        // the count is one high — identically so at both ends, which is all
        // a difference needs.
        //
        // Counted entry by entry rather than with `count`, which would tally
        // a failed listing as one more descriptor: this number decides
        // whether a leak check passes, and an error folded into it is a
        // measurement reporting a descriptor nobody observed.
        let listing =
            std::fs::read_dir(FD_DIR).map_err(|err| format!("reading {FD_DIR} failed: {err}"))?;
        let mut open = 0;
        for entry in listing {
            entry.map_err(|err| format!("listing {FD_DIR} stopped part-way: {err}"))?;
            open += 1;
        }
        Ok(open)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};
        let mut count = 0u32;
        // SAFETY: the pseudo-handle for this process is always valid, and
        // `count` is a valid out-pointer.
        if unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) } == 0 {
            return Err(format!(
                "GetProcessHandleCount failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(count as usize)
    }
}

/// What one pseudo-console cycle irreducibly leaves in this process's handle
/// table on Windows.
///
/// Measured rather than tolerated: `CreatePseudoConsole` puts two handles
/// here that are not the terminal library's own objects, and closing the
/// pseudo-console returns only one of them. The process holds no reference
/// to what is left and cannot close it. So this is an exact declaration —
/// one per session and no more; a second unreturned handle still fails, and
/// so would this one ceasing to be constant.
#[cfg(windows)]
pub const CONPTY_CYCLE_HANDLE_RESIDUE: usize = 1;
#[cfg(not(windows))]
pub const CONPTY_CYCLE_HANDLE_RESIDUE: usize = 0;

/// Wait for the open-descriptor count to come back to `baseline`, plus at
/// most the platform residue the caller declares.
///
/// A moment of grace is built in rather than asserted around: a reader
/// thread releases its descriptor microseconds *after* it reports the stream
/// ended, so a single snapshot would race a release already in flight.
pub fn await_channel_baseline(baseline: usize, allowed_residue: usize) -> Result<String, String> {
    let started = Instant::now();
    let target = baseline + allowed_residue;
    loop {
        let count = open_channels()?;
        if count <= target {
            let residue = count.saturating_sub(baseline);
            return Ok(format!(
                "settled in {}ms, {residue} of an allowed {allowed_residue} {CHANNEL_KIND}(s) \
                 of declared platform residue",
                started.elapsed().as_millis()
            ));
        }
        if started.elapsed() >= PATIENCE {
            return Err(format!(
                "{CHANNEL_KIND} count is {count} against a baseline of {baseline} and an \
                 allowed residue of {allowed_residue} — something each session opened was \
                 never released"
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Wait for a process to be gone for good.
///
/// On POSIX that means collecting it as well as killing it: a descendant
/// whose parent has been killed reparents to this process, and until its
/// corpse is collected every liveness question still answers yes.
pub fn wait_until_gone(pid: u32) -> Result<(), String> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        #[cfg(unix)]
        reap_orphans();
        if !process_alive(pid) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "process {pid} was still running {}s later",
                PATIENCE.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Whether a process still exists.
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: signal zero validates without delivering.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    // Existing but not ours to signal is still existing.
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Assert that the crate under test collected the child it killed, rather
/// than leaving the corpse for this process to trip over.
///
/// A killed child that is never waited on stays a zombie until its parent
/// exits, and this process is that parent: the terminal library spawns the
/// child directly. `kill(pid, 0)` cannot see the difference — a zombie
/// answers exactly as a live process does — so the only honest question is
/// whether there is still something here to collect.
///
/// Polled, because a kill is not instantaneous: asking once immediately
/// after the handle drops sees "not dead yet" and would pass whether or not
/// anything was collected. Must be asked before anything else in the suite
/// reaps, which is why it is separate from [`wait_until_gone`].
#[cfg(unix)]
pub fn assert_collected(pid: u32) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // SAFETY: a non-blocking wait on one pid, with a null status
        // pointer, which is permitted.
        let collected =
            unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
        if collected == pid as libc::pid_t {
            return Err(format!(
                "child {pid} was killed but left uncollected — this test had to reap it"
            ));
        }
        // `-1` is `ECHILD`: there is no such child of ours any more, which
        // is what a collected one looks like from here.
        if collected == -1 {
            return Ok(());
        }
        // Zero: alive, or dead and not yet reported. Give the kill time.
        if Instant::now() >= deadline {
            return Err(format!(
                "child {pid} was still running 5s after the handle dropped"
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Whether a process group still holds anything.
#[cfg(unix)]
pub fn group_has_members(pgid: u32) -> bool {
    reap_orphans();
    // SAFETY: as for `process_alive`.
    if unsafe { libc::killpg(pgid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Become the reaper for orphaned descendants (Linux).
///
/// Without this, a grandchild whose parent has been killed reparents to
/// whatever the container runs as process one — commonly something that
/// never reaps — and its corpse answers "yes, still here" to every liveness
/// question for the rest of the run. That is the container CI lane, not a
/// theoretical case. Other platforms' process one collects orphans promptly.
#[cfg(unix)]
pub fn adopt_orphans() {
    #[cfg(target_os = "linux")]
    // SAFETY: takes plain integers and touches no memory.
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }
}

/// Collect any adopted corpses, so liveness questions see the living only.
#[cfg(unix)]
pub fn reap_orphans() {
    loop {
        // SAFETY: a non-blocking wait with a null status pointer, which is
        // permitted. Safe to call here because this process starts children
        // only through the terminal, and those are reaped by the crate under
        // test before a scenario ever asks.
        let collected = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if collected <= 0 {
            return;
        }
    }
}

#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };
    // Opening is not the question. A terminated process can still be opened
    // for as long as its object survives — anything holding a handle keeps
    // it there — so a check that stopped at `OpenProcess` would report a
    // killed process as running until the last reference went away. The
    // handle is signalled once the process has exited, and that is the
    // question worth asking. It is the same distinction as a zombie on the
    // other platform, and it fooled this suite in exactly the same way.
    // SAFETY: plain arguments; the handle is checked before use and closed
    // on every path.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let exited = WaitForSingleObject(handle, 0) == WAIT_OBJECT_0;
        CloseHandle(handle);
        !exited
    }
}

/// The console hosts this process is currently parenting.
///
/// A pseudo-console starts its host as a child of whoever created it, so a
/// before-and-after census of this list is how "the terminal's console host
/// is gone" is asked on Windows.
#[cfg(windows)]
pub fn console_hosts() -> Vec<u32> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    let mut hosts = Vec::new();
    // SAFETY: a snapshot takes no input memory; the handle is checked before
    // use and closed on every path.
    unsafe {
        let me = GetCurrentProcessId();
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return hosts;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut more = Process32FirstW(snapshot, &mut entry);
        while more != 0 {
            let end = entry
                .szExeFile
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase();
            if entry.th32ParentProcessID == me
                && (name == "conhost.exe" || name == "openconsole.exe")
            {
                hosts.push(entry.th32ProcessID);
            }
            more = Process32NextW(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
    }
    hosts
}
