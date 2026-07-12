//! Process-tree fixture — the controlled child the cleanup probe spawns
//! under a PTY to stand in for a real interactive CLI's process shape: a
//! root that spawns descendants (Claude Code spawns subshells for tool
//! execution) and must leave nothing behind when its session ends. It
//! reports as `probe-child event=…` lines on stdout (the PTY slave, so the
//! spawning probe reads them back through the master).
//!
//! The tree is grown on command (the `t` byte), not at startup: on Windows
//! the probe binds this root to its job object first, and a descendant that
//! spawned before the binding would sit outside the job — the race would
//! invalidate every membership assertion. Two descendants then appear:
//!
//! - **in-group**: a plain spawn, sharing the root's process group (POSIX)
//!   or job object (Windows) — the member that group/job-scoped cleanup is
//!   expected to cover.
//! - **escape**: a spawn that leaves the group — `setsid` in the forked
//!   child on POSIX; `CREATE_BREAKAWAY_FROM_JOB` on Windows, where a job
//!   without breakaway permission *denies* the spawn and the denial is the
//!   report. The POSIX escapee is the honest limitation made concrete:
//!   group-scoped cleanup cannot see it, so the probe must *detect* it (and
//!   reap it) from the recorded PID.
//!
//! Descendants are silent sleepers with all three stdio streams null: a
//! descendant holding the PTY slave open would keep the master from ever
//! seeing end-of-stream after the root exits, which is one of the leak
//! shapes the probe exists to catch — the fixture must not build it in.
//!
//! Two modes, differing in how the polite shutdown path is treated:
//!
//! - `clean`: default dispositions. On the quit byte the root reaps the
//!   in-group descendant — what a well-behaved CLI does for the children it
//!   knows — and exits 0. The escapee is deliberately left running: it left
//!   the group, so group-scoped bookkeeping does not know it exists.
//! - `stubborn`: the root and both descendants ignore the polite signal
//!   (SIGTERM handler on POSIX, console ctrl handler on Windows), so the
//!   probe's terminate escalation has something real to escalate past. Each
//!   survived request is reported.
//!
//! The fixture exits 0 on the quit byte (`q`) or on end-of-input, 2 on a
//! usage error, 3 when the terminal cannot be configured, 4 on a read
//! error, 5 when the tree cannot be grown, and a watchdog exits 9 — after
//! killing any descendants — if no quit arrives in time, so an orphaned run
//! can never outlive its probe for long.

use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_bridge_probe_child::{
    ESCAPE_DENIED, EVENT_BYTE, EVENT_EOF, EVENT_QUIT, EVENT_READY, EVENT_TERM, EVENT_TREE,
    EVENT_WATCHDOG, QUIT_BYTE, TERM_VIA, TREE_BYTE, byte_hex, format_report,
};

/// Polite-termination requests survived so far (stubborn mode only).
/// Written by the signal / ctrl handler, read by the watcher thread.
static TERMS: AtomicU32 = AtomicU32::new(0);

const DEFAULT_WATCHDOG_SECS: u64 = 120;
const WATCH_POLL: Duration = Duration::from_millis(20);

/// The descendant sleeper's subcommand name — matched exactly by the arg
/// parser, never typed by a human.
const SLEEPER_ARG: &str = "sleep";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Clean,
    Stubborn,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Clean => "clean",
            Mode::Stubborn => "stubborn",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Root,
    Sleeper,
}

fn main() {
    let (role, mode, watchdog) = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("tree-child: {message}");
            std::process::exit(2);
        }
    };
    match role {
        Role::Sleeper => sleeper(mode, watchdog),
        Role::Root => root(mode, watchdog),
    }
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<(Role, Mode, Duration), String> {
    const USAGE: &str = "usage: tree-child <clean|stubborn> [--watchdog-secs N]";
    let mut role = Role::Root;
    let mut mode: Option<Mode> = None;
    let mut watchdog = Duration::from_secs(DEFAULT_WATCHDOG_SECS);
    let mut first = true;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            // The sleeper role is only ever spelled by the root spawning
            // itself, and only as the first argument.
            SLEEPER_ARG if first => role = Role::Sleeper,
            "clean" if mode.is_none() => mode = Some(Mode::Clean),
            "stubborn" if mode.is_none() => mode = Some(Mode::Stubborn),
            "--watchdog-secs" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--watchdog-secs needs a value. {USAGE}"))?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid --watchdog-secs value: {value}"))?;
                watchdog = Duration::from_secs(secs);
            }
            other => return Err(format!("unexpected argument: {other}. {USAGE}")),
        }
        first = false;
    }
    mode.map(|mode| (role, mode, watchdog))
        .ok_or_else(|| format!("a mode is required. {USAGE}"))
}

/// One locked, single-buffer write per line so the read loop and the
/// watcher thread can never interleave mid-line. The `\r\n` is explicit: a
/// report must start at column zero even if a future mode turns off output
/// post-processing.
fn report(event: &str, fields: &[(&str, String)]) {
    use std::io::Write;
    let mut line = format_report(event, fields);
    line.push_str("\r\n");
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
}

/// The descendant: a silent sleeper. Its stdio is null (the root spawns it
/// that way), so it reports nothing and — deliberately — holds no PTY fd.
/// In stubborn mode it ignores the polite signal, so a group-wide SIGTERM
/// during the probe's grace window must not thin the tree. It only ever
/// exits by being killed, or by its own watchdog.
fn sleeper(mode: Mode, watchdog: Duration) -> ! {
    if mode == Mode::Stubborn {
        platform::ignore_polite_signal();
    }
    let deadline = Instant::now() + watchdog;
    while Instant::now() < deadline {
        std::thread::sleep(WATCH_POLL);
    }
    std::process::exit(9);
}

/// The descendants the root has spawned, shared with the watcher thread so
/// a watchdog exit can take the tree down with it instead of leaking it.
#[derive(Default)]
struct Tree {
    ingroup: Option<Child>,
    escapee: Option<Child>,
}

impl Tree {
    /// Kill and reap everything still held. Best effort: the fixture is
    /// exiting, and a kill that failed because the child already died is
    /// success by another name.
    fn raze(&mut self) {
        for child in [self.ingroup.take(), self.escapee.take()]
            .into_iter()
            .flatten()
        {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn root(mode: Mode, watchdog: Duration) -> ! {
    let saved = match platform::configure() {
        Ok(saved) => saved,
        Err(detail) => {
            eprintln!("tree-child: terminal setup failed: {detail}");
            std::process::exit(3);
        }
    };
    if mode == Mode::Stubborn
        && let Err(detail) = platform::survive_polite_signal()
    {
        platform::restore(&saved);
        eprintln!("tree-child: installing the polite-signal handler failed: {detail}");
        std::process::exit(3);
    }

    // Nothing may be written to the child before this line arrives: it
    // promises the terminal is configured and — in stubborn mode — the
    // polite signal already survivable, so everything the probe does next
    // is observed under the requested mode.
    let mut fields = vec![
        ("mode", mode.name().to_string()),
        ("pid", std::process::id().to_string()),
    ];
    fields.extend(platform::ready_fields());
    report(EVENT_READY, &fields);

    let tree = Arc::new(Mutex::new(Tree::default()));
    spawn_watcher(Instant::now() + watchdog, watchdog, saved, tree.clone());
    let code = read_loop(mode, watchdog, &tree);
    platform::restore(&saved);
    std::process::exit(code);
}

/// Report survived polite-termination requests as they happen and enforce
/// the watchdog. Polling an atomic is the only reporting channel that is
/// safe from signal context; 20ms is far inside every settle window the
/// probe applies.
///
/// The watcher carries its own copy of the saved terminal state and a
/// handle on the tree: it exits the process directly on watchdog expiry,
/// and must both restore the terminal (for a human who ran the fixture by
/// hand) and take the descendants down (an orphaned run that leaked its
/// tree would be the exact failure the probe exists to catch).
fn spawn_watcher(
    deadline: Instant,
    watchdog: Duration,
    saved: platform::Saved,
    tree: Arc<Mutex<Tree>>,
) {
    std::thread::spawn(move || {
        let mut reported: u32 = 0;
        loop {
            let survived = TERMS.load(Ordering::SeqCst);
            while reported < survived {
                reported += 1;
                report(
                    EVENT_TERM,
                    &[
                        ("count", reported.to_string()),
                        ("via", TERM_VIA.to_string()),
                    ],
                );
            }
            if Instant::now() >= deadline {
                report(
                    EVENT_WATCHDOG,
                    &[("after_secs", watchdog.as_secs().to_string())],
                );
                tree.lock().unwrap().raze();
                platform::restore(&saved);
                std::process::exit(9);
            }
            std::thread::sleep(WATCH_POLL);
        }
    });
}

/// Dispatch every stdin byte until the quit byte, end-of-input, or a read
/// error; returns the process exit code.
fn read_loop(mode: Mode, watchdog: Duration, tree: &Arc<Mutex<Tree>>) -> i32 {
    use std::io::Read;

    let mut grown = false;
    let mut buf = [0u8; 256];
    let mut stdin = std::io::stdin().lock();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => {
                // The master side vanished mid-run: nothing is observing
                // anymore, so take the whole tree down — escapee included —
                // rather than leak it behind an abandoned probe.
                tree.lock().unwrap().raze();
                report(EVENT_EOF, &[terms_total()]);
                return 0;
            }
            Ok(n) => {
                for &byte in &buf[..n] {
                    match byte {
                        TREE_BYTE if !grown => {
                            grown = true;
                            match grow_tree(mode, watchdog, tree) {
                                Ok(fields) => report(EVENT_TREE, &fields),
                                Err(detail) => {
                                    eprintln!("tree-child: growing the tree failed: {detail}");
                                    tree.lock().unwrap().raze();
                                    return 5;
                                }
                            }
                        }
                        QUIT_BYTE => {
                            let mut fields = quit_outcome(tree);
                            fields.push(terms_total());
                            report(EVENT_QUIT, &fields);
                            return 0;
                        }
                        // A repeated tree request is a probe bug; report it
                        // as the unexpected data it is rather than dropping
                        // it or growing a second tree.
                        other => report(EVENT_BYTE, &[("hex", byte_hex(other))]),
                    }
                }
            }
            // A signal without SA_RESTART semantics can cut the read short;
            // the byte stream itself has not ended.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                eprintln!("tree-child: stdin read failed: {err}");
                tree.lock().unwrap().raze();
                return 4;
            }
        }
    }
}

fn terms_total() -> (&'static str, String) {
    ("terms", TERMS.load(Ordering::SeqCst).to_string())
}

/// Spawn both descendants and return the tree report's fields. The
/// descendants get the same mode (a stubborn tree is stubborn throughout)
/// and the root's watchdog budget, and every stdio stream is null — a
/// descendant on the PTY slave would keep the master alive past the root's
/// exit, manufacturing the very leak the probe watches for.
fn grow_tree(
    mode: Mode,
    watchdog: Duration,
    tree: &Arc<Mutex<Tree>>,
) -> Result<Vec<(&'static str, String)>, String> {
    let me = std::env::current_exe().map_err(|err| format!("current_exe failed: {err}"))?;
    let sleeper = |escape: bool| -> std::io::Result<Child> {
        let mut command = Command::new(&me);
        command
            .arg(SLEEPER_ARG)
            .arg(mode.name())
            .args(["--watchdog-secs", &watchdog.as_secs().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if mode == Mode::Stubborn {
            // The disposition must be in force the instant this report line
            // makes the descendant's existence known — the probe's group
            // SIGTERM can arrive before a freshly exec'd sleeper reaches
            // its own setup code, and a stubborn tree thinned by that race
            // would fail the escalation lane for the wrong reason. On
            // POSIX an *ignored* signal (unlike a handled one) survives
            // exec, so it is preset between fork and exec.
            platform::preset_stubbornness(&mut command);
        }
        if escape {
            platform::escape_the_group(&mut command);
        }
        command.spawn()
    };

    let ingroup = sleeper(false).map_err(|err| format!("in-group spawn failed: {err}"))?;
    let ingroup_pid = ingroup.id();

    // The escape attempt: a denial is a first-class outcome — under a
    // Windows job without breakaway permission it is the *expected* one,
    // and the report must say so rather than failing the run.
    let escape = match sleeper(true) {
        Ok(child) => {
            let pid = child.id().to_string();
            tree.lock().unwrap().escapee = Some(child);
            pid
        }
        Err(err) if platform::is_escape_denied(&err) => ESCAPE_DENIED.to_string(),
        Err(err) => {
            // The in-group descendant is already running; the caller razes
            // the tree on this error path.
            tree.lock().unwrap().ingroup = Some(ingroup);
            return Err(format!("escape spawn failed: {err}"));
        }
    };
    tree.lock().unwrap().ingroup = Some(ingroup);

    Ok(vec![
        ("ingroup", ingroup_pid.to_string()),
        ("escape", escape),
    ])
}

/// The quit byte's cleanup: reap the in-group descendant — the child a
/// well-behaved CLI knows about — and *leave the escapee running*. It left
/// the process group, so group-scoped bookkeeping does not know it exists;
/// detecting and reaping it from the recorded PID is the probe's job, and
/// the honest limitation under test. Returns the quit report's fields.
fn quit_outcome(tree: &Arc<Mutex<Tree>>) -> Vec<(&'static str, String)> {
    let mut tree = tree.lock().unwrap();
    let ingroup = match tree.ingroup.take() {
        Some(mut child) => {
            let _ = child.kill();
            match child.wait() {
                Ok(_) => "reaped",
                Err(_) => "unreaped",
            }
        }
        None => "none",
    };
    // Dropping the handle neither kills nor reaps: the escapee stays
    // running and is reparented when the root exits.
    let escape = if tree.escapee.take().is_some() {
        "left"
    } else {
        "none"
    };
    vec![
        ("ingroup", ingroup.to_string()),
        ("escape", escape.to_string()),
    ]
}

#[cfg(unix)]
mod platform {
    use std::process::Command;

    use std::sync::atomic::Ordering;

    use super::TERMS;

    /// Plain copyable data so the watchdog thread can carry its own copy —
    /// it exits the process directly and must restore first.
    #[derive(Clone, Copy)]
    pub struct Saved(libc::termios);

    extern "C" fn on_sigterm(_signal: libc::c_int) {
        // Signal context: bumping an atomic is the entire safe repertoire.
        // The watcher thread turns the count into report lines.
        TERMS.fetch_add(1, Ordering::SeqCst);
    }

    pub fn configure() -> Result<Saved, String> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: isatty only inspects the fd; no memory is exchanged.
        if unsafe { libc::isatty(fd) } == 0 {
            return Err("stdin is not a terminal — spawn this fixture under a PTY".to_string());
        }

        // SAFETY: a zeroed termios is a valid out-parameter; tcgetattr fully
        // initializes it on success, which is checked.
        let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd is a live tty (checked above), attrs a valid termios.
        if unsafe { libc::tcgetattr(fd, &mut attrs) } != 0 {
            return Err(format!("tcgetattr failed: {}", last_os_error()));
        }
        let saved = Saved(attrs);

        // Byte-wise, echo-free, signal-free reads — the raw mode full-screen
        // interactive CLIs run in, and the mode that keeps the report
        // channel free of echo noise. The polite-shutdown path under test is
        // a *delivered* SIGTERM, so ISIG has no part to play here.
        attrs.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        attrs.c_cc[libc::VMIN] = 1;
        attrs.c_cc[libc::VTIME] = 0;
        // SAFETY: same live fd; attrs was initialized by tcgetattr above.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &attrs) } != 0 {
            return Err(format!("tcsetattr failed: {}", last_os_error()));
        }

        // Verify what the terminal actually holds, not what was requested:
        // tcsetattr succeeds even when it applied only part of the change,
        // and echo leaking into the report channel would corrupt every
        // scenario.
        // SAFETY: as for the first tcgetattr.
        let mut applied: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut applied) } != 0 {
            // The terminal was already mutated; do not leave it that way
            // behind an error the caller cannot recover from.
            restore(&saved);
            return Err(format!("tcgetattr (verify) failed: {}", last_os_error()));
        }
        if applied.c_lflag & (libc::ICANON | libc::ECHO) != 0 {
            restore(&saved);
            return Err(
                "the terminal kept ICANON/ECHO on — echo would pollute the report channel"
                    .to_string(),
            );
        }
        Ok(saved)
    }

    /// The ready report's platform fields: the process group the probe's
    /// group-scoped assertions target, as the fixture itself observes it.
    pub fn ready_fields() -> Vec<(&'static str, String)> {
        // SAFETY: getpgrp takes nothing and cannot fail.
        let pgid = unsafe { libc::getpgrp() };
        vec![("pgid", pgid.to_string())]
    }

    /// Stubborn mode, root: count SIGTERM instead of dying to it, so the
    /// probe's escalation has something real to escalate past — and gets a
    /// report proving the polite request arrived and was survived.
    pub fn survive_polite_signal() -> Result<(), String> {
        // SAFETY: zeroed is a valid starting point; every field the kernel
        // consults is assigned below (sigemptyset initializes the mask).
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: sa_mask is a valid out-parameter for sigemptyset.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        action.sa_sigaction = on_sigterm as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SA_RESTART: the blocking stdin read resumes after the handler
        // runs; the watcher thread owns the reporting.
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: action is fully initialized; a null old-action is allowed.
        if unsafe { libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) } != 0 {
            return Err(format!("sigaction failed: {}", last_os_error()));
        }
        Ok(())
    }

    /// Stubborn mode, sleeper: ignore SIGTERM outright. The sleeper's stdio
    /// is null, so there is nothing to report — surviving is its whole job.
    /// Belt to [`preset_stubbornness`]'s braces: the preset closed the
    /// startup race; this re-assertion keeps a hand-run sleeper honest.
    pub fn ignore_polite_signal() {
        // SAFETY: SIG_IGN is a valid disposition for a catchable signal.
        unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) };
    }

    /// Set the sleeper's SIGTERM to ignored *before exec*, in the forked
    /// child: `spawn` only returns once exec happened, so by the time the
    /// tree report can announce the descendant, the disposition is already
    /// in force — no window in which a group-wide SIGTERM could thin a
    /// stubborn tree.
    pub fn preset_stubbornness(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: signal(SIG_IGN) is async-signal-safe and touches no
        // memory shared with the parent — safe post-fork, pre-exec.
        unsafe {
            command.pre_exec(|| {
                // The preset is the load-bearing race closure: a silently
                // failed signal() would hand back a sleeper the group
                // SIGTERM can kill mid-startup. Failing the spawn — with
                // the OS error carried out through the spawn result — is
                // the honest outcome.
                if libc::signal(libc::SIGTERM, libc::SIG_IGN) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
        };
    }

    /// The escape: `setsid` in the forked child, before exec — a new
    /// session and a new process group, exactly the move a daemonizing
    /// grandchild makes, and the one group-scoped cleanup cannot follow.
    pub fn escape_the_group(command: &mut Command) {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is async-signal-safe and touches no memory shared
        // with the parent — safe in the post-fork, pre-exec window.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            })
        };
    }

    /// POSIX has no spawn-time veto on `setsid`: a fork-child that is not
    /// already a group leader cannot be refused, so no error reads as a
    /// denied escape.
    pub fn is_escape_denied(_err: &std::io::Error) -> bool {
        false
    }

    pub fn restore(saved: &Saved) {
        // Best effort: the probe destroys this terminal moments later. The
        // restore matters when a human runs the fixture in a real shell.
        // SAFETY: the saved termios came from tcgetattr on this same fd.
        let _ = unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved.0) };
    }

    fn last_os_error() -> std::io::Error {
        std::io::Error::last_os_error()
    }
}

#[cfg(windows)]
mod platform {
    use std::process::Command;
    use std::sync::atomic::Ordering;

    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, CTRL_BREAK_EVENT, CTRL_C_EVENT, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
        SetConsoleCtrlHandler, SetConsoleMode,
    };
    use windows_sys::Win32::System::Threading::CREATE_BREAKAWAY_FROM_JOB;
    use windows_sys::core::BOOL;

    use super::TERMS;

    /// Only the mode bits, not the handle: the stdin handle is
    /// process-global and re-acquired at restore time, which keeps this
    /// plain copyable data the watchdog thread can carry its own copy of —
    /// it exits the process directly and must restore first.
    #[derive(Clone, Copy)]
    pub struct Saved {
        mode_bits: CONSOLE_MODE,
    }

    unsafe extern "system" fn on_ctrl(ctrl_type: u32) -> BOOL {
        // Claim the polite events fully (returning 1) — the process must
        // survive them to report them. Anything else falls through to the
        // default handler.
        if ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT {
            TERMS.fetch_add(1, Ordering::SeqCst);
            1
        } else {
            0
        }
    }

    pub fn configure() -> Result<Saved, String> {
        // SAFETY: GetStdHandle only looks up a slot in the PEB.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err("no stdin handle".to_string());
        }
        let mut before: CONSOLE_MODE = 0;
        // SAFETY: handle is live (checked above); `before` is a valid
        // out-pointer.
        if unsafe { GetConsoleMode(handle, &mut before) } == 0 {
            return Err(format!(
                "stdin is not a console — spawn this fixture under a ConPTY ({})",
                std::io::Error::last_os_error()
            ));
        }

        // Character-wise, echo-free reads with the console's ctrl synthesis
        // off, mirroring the POSIX raw setup: control bytes are data here.
        let requested = before & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
        // SAFETY: live console handle, plain value argument.
        if unsafe { SetConsoleMode(handle, requested) } == 0 {
            return Err(format!(
                "SetConsoleMode failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let saved = Saved { mode_bits: before };
        // Verify what the console actually holds, not what was requested —
        // echo leaking into the report channel would corrupt every scenario.
        let mut applied: CONSOLE_MODE = 0;
        // SAFETY: as for the first GetConsoleMode.
        if unsafe { GetConsoleMode(handle, &mut applied) } == 0 {
            // The console was already mutated; do not leave it that way
            // behind an error the caller cannot recover from.
            restore(&saved);
            return Err(format!(
                "GetConsoleMode (verify) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if applied & (ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT) != 0 {
            restore(&saved);
            return Err(format!(
                "the console kept line input / echo on (mode {before:#x}->{applied:#x}) — echo would pollute the report channel"
            ));
        }
        Ok(saved)
    }

    /// The ready report's platform fields. Windows has no process-group id
    /// worth reporting — the probe's containment boundary is the job object
    /// it creates and binds itself.
    pub fn ready_fields() -> Vec<(&'static str, String)> {
        Vec::new()
    }

    /// Stubborn mode, root: count and swallow the polite console events.
    /// Job-object termination — the actual Windows escalation under test —
    /// cannot be swallowed, which is exactly the point.
    pub fn survive_polite_signal() -> Result<(), String> {
        // SAFETY: the handler is a static function that only touches an
        // atomic; 1 = add it to the handler chain.
        if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) } == 0 {
            return Err(format!(
                "SetConsoleCtrlHandler failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    /// Stubborn mode, sleeper: ignore Ctrl+C. Best effort — the sleeper may
    /// have no usable console at all, and the escalation under test
    /// (job-object termination) does not consult dispositions anyway.
    pub fn ignore_polite_signal() {
        // SAFETY: a null handler with add=1 sets the process's
        // ignore-Ctrl+C flag; no memory is exchanged.
        unsafe { SetConsoleCtrlHandler(None, 1) };
    }

    /// Nothing to preset on Windows: there is no cross-process way to arm
    /// a ctrl-ignore before the child runs, and nothing needs it — the
    /// probe cannot deliver console ctrl events across consoles, so the
    /// startup race the POSIX preset closes does not exist here.
    pub fn preset_stubbornness(_command: &mut Command) {}

    /// The escape attempt: ask CreateProcess to break the child away from
    /// the job. Under a job without breakaway permission the OS refuses the
    /// spawn outright — the denial, not an escaped process, is the expected
    /// Windows observation.
    pub fn escape_the_group(command: &mut Command) {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_BREAKAWAY_FROM_JOB);
    }

    pub fn is_escape_denied(err: &std::io::Error) -> bool {
        err.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32)
    }

    pub fn restore(saved: &Saved) {
        // Best effort, mirroring the POSIX restore. The handle is looked up
        // fresh: it is process-global, and not storing it keeps `Saved`
        // free of thread-affine state.
        // SAFETY: GetStdHandle only looks up a slot in the PEB; the bits
        // came from GetConsoleMode on this same process.
        unsafe {
            let handle = GetStdHandle(STD_INPUT_HANDLE);
            if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
                let _ = SetConsoleMode(handle, saved.mode_bits);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_select_mode_and_watchdog() {
        let args = ["stubborn", "--watchdog-secs", "7"].map(String::from);
        let (role, mode, watchdog) = parse_args(args.into_iter()).unwrap();
        assert_eq!(role, Role::Root);
        assert_eq!(mode, Mode::Stubborn);
        assert_eq!(watchdog, Duration::from_secs(7));
    }

    #[test]
    fn the_sleeper_role_is_selected_by_its_leading_argument() {
        let args = [SLEEPER_ARG, "clean", "--watchdog-secs", "5"].map(String::from);
        let (role, mode, _) = parse_args(args.into_iter()).unwrap();
        assert_eq!(role, Role::Sleeper);
        assert_eq!(mode, Mode::Clean);
        // Anywhere but first, the sleeper subcommand is just an unexpected
        // argument.
        let args = ["clean", SLEEPER_ARG].map(String::from);
        assert!(parse_args(args.into_iter()).is_err());
    }

    #[test]
    fn a_mode_is_required_and_unknown_arguments_are_rejected() {
        assert!(parse_args(std::iter::empty()).is_err());
        assert!(parse_args([SLEEPER_ARG.to_string()].into_iter()).is_err());
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
        assert!(parse_args(["clean".to_string(), "stubborn".to_string()].into_iter()).is_err());
    }

    #[test]
    fn mode_names_match_the_cli_spelling() {
        // The ready report echoes these names and the probe asserts on
        // them; they must be the exact strings the CLI accepts.
        assert_eq!(Mode::Clean.name(), "clean");
        assert_eq!(Mode::Stubborn.name(), "stubborn");
    }

    #[test]
    fn razing_an_empty_tree_is_a_no_op() {
        // The watchdog and error paths raze unconditionally; an ungrown
        // tree must not make that a panic.
        Tree::default().raze();
    }
}
