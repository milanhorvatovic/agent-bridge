//! Interrupt-observation fixture — the controlled child the signal probe
//! spawns under a PTY to see, from the inside, what an "interrupt" actually
//! delivers: a byte on stdin, or a signal to a handler. It reports both
//! observations as `probe-child event=…` lines on stdout (the PTY slave, so
//! the spawning probe reads them back through the master).
//!
//! Two modes, differing in exactly one terminal bit — whether the terminal
//! may turn the interrupt character into a signal:
//!
//! - `raw`: `ISIG` off (POSIX) / `ENABLE_PROCESSED_INPUT` off (Windows).
//!   This is the mode full-screen interactive CLIs run in: 0x03 is ordinary
//!   data and must arrive on stdin with no handler firing.
//! - `cooked`: `ISIG` on / `ENABLE_PROCESSED_INPUT` on. The same 0x03 never
//!   reaches stdin — the tty line discipline synthesizes `SIGINT` (POSIX) /
//!   the console host raises `CTRL_C_EVENT` (Windows) instead.
//!
//! Echo and line-buffering are off in *both* modes so the report channel
//! stays clean and reads are byte-wise; the signal bit is deliberately the
//! only variable between the two runs.
//!
//! Bytes are reported by the read loop; handler firings by a small watcher
//! thread, because a blocking stdin read has no portable way to notice a
//! signal (Windows delivers ctrl events on their own thread to begin with).
//! An interrupt handler itself only bumps an atomic — everything else it
//! could do is off-limits in signal context.
//!
//! The fixture exits 0 on the quit byte (`q`) or on end-of-input, and a
//! watchdog exits 9 if neither arrives in time, so an orphaned run can
//! never outlive its probe for long.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use agent_bridge_probe_child::{
    EVENT_BYTE, EVENT_EOF, EVENT_INTERRUPT, EVENT_QUIT, EVENT_READY, EVENT_WATCHDOG, INTERRUPT_VIA,
    QUIT_BYTE, byte_hex, format_report,
};

/// Handler firings not yet reported. Written by the signal / ctrl handler,
/// read by the watcher thread.
static INTERRUPTS: AtomicU32 = AtomicU32::new(0);

/// The platform-specific fields the ready report carries — the terminal
/// state that was actually applied, as `key=value` pairs.
type TermFields = Vec<(&'static str, String)>;

const DEFAULT_WATCHDOG_SECS: u64 = 120;
const WATCH_POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Debug)]
enum Mode {
    Raw,
    Cooked,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Raw => "raw",
            Mode::Cooked => "cooked",
        }
    }
}

fn main() {
    let (mode, watchdog) = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("probe-child: {message}");
            std::process::exit(2);
        }
    };
    let (saved, term_fields) = match terminal::configure(mode) {
        Ok(configured) => configured,
        Err(detail) => {
            eprintln!("probe-child: terminal setup failed: {detail}");
            std::process::exit(3);
        }
    };

    // Nothing may be written to the child before this line arrives: it
    // promises the terminal is configured and the handler installed, so
    // whatever the probe sends next is observed under the requested mode.
    let mut fields = vec![
        ("mode", mode.name().to_string()),
        ("os", std::env::consts::OS.to_string()),
        ("pid", std::process::id().to_string()),
    ];
    fields.extend(term_fields);
    report(EVENT_READY, &fields);

    spawn_watcher(Instant::now() + watchdog, watchdog);
    let code = read_loop();
    terminal::restore(&saved);
    std::process::exit(code);
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<(Mode, Duration), String> {
    const USAGE: &str = "usage: probe-child <raw|cooked> [--watchdog-secs N]";
    let mut mode: Option<Mode> = None;
    let mut watchdog = Duration::from_secs(DEFAULT_WATCHDOG_SECS);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "raw" if mode.is_none() => mode = Some(Mode::Raw),
            "cooked" if mode.is_none() => mode = Some(Mode::Cooked),
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
    }
    mode.map(|mode| (mode, watchdog))
        .ok_or_else(|| format!("a mode is required. {USAGE}"))
}

/// One locked write per line so the read loop and the watcher thread can
/// never interleave mid-line. The `\r\n` is explicit: a report must start
/// at column zero even if a future mode turns off output post-processing.
fn report(event: &str, fields: &[(&str, String)]) {
    let line = format_report(event, fields);
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
}

/// Report handler firings as they happen and enforce the watchdog. A 20ms
/// poll is far inside every settle window the probe applies, and polling an
/// atomic is the only reporting channel that is safe from signal context.
fn spawn_watcher(deadline: Instant, watchdog: Duration) {
    std::thread::spawn(move || {
        let mut reported: u32 = 0;
        loop {
            let fired = INTERRUPTS.load(Ordering::SeqCst);
            while reported < fired {
                reported += 1;
                report(
                    EVENT_INTERRUPT,
                    &[
                        ("count", reported.to_string()),
                        ("via", INTERRUPT_VIA.to_string()),
                    ],
                );
            }
            if Instant::now() >= deadline {
                report(
                    EVENT_WATCHDOG,
                    &[("after_secs", watchdog.as_secs().to_string())],
                );
                std::process::exit(9);
            }
            std::thread::sleep(WATCH_POLL);
        }
    });
}

/// Report every stdin byte until the quit byte, end-of-input, or a read
/// error; returns the process exit code. The quit byte is control flow, not
/// data, so it ends the run instead of being reported as a byte.
fn read_loop() -> i32 {
    let mut bytes: u64 = 0;
    // Big enough for a burst, and comfortably over the four bytes Rust's
    // Windows console reader needs to hold one arbitrary UTF-8 character.
    let mut buf = [0u8; 256];
    let mut stdin = std::io::stdin().lock();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) => {
                report(EVENT_EOF, &[("bytes", bytes.to_string())]);
                return 0;
            }
            Ok(n) => {
                for &byte in &buf[..n] {
                    if byte == QUIT_BYTE {
                        report(
                            EVENT_QUIT,
                            &[
                                ("bytes", bytes.to_string()),
                                ("interrupts", INTERRUPTS.load(Ordering::SeqCst).to_string()),
                            ],
                        );
                        return 0;
                    }
                    bytes += 1;
                    report(EVENT_BYTE, &[("hex", byte_hex(byte))]);
                }
            }
            // A signal without SA_RESTART semantics can cut the read short;
            // the byte stream itself has not ended.
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => {
                eprintln!("probe-child: stdin read failed: {err}");
                return 4;
            }
        }
    }
}

#[cfg(unix)]
mod terminal {
    use std::sync::atomic::Ordering;

    use super::{INTERRUPTS, Mode, TermFields};

    pub struct Saved(libc::termios);

    extern "C" fn on_sigint(_signal: libc::c_int) {
        // Signal context: bumping an atomic is the entire safe repertoire.
        // The watcher thread turns the count into report lines.
        INTERRUPTS.fetch_add(1, Ordering::SeqCst);
    }

    pub fn configure(mode: Mode) -> Result<(Saved, TermFields), String> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: isatty only inspects the fd; no memory is exchanged.
        if unsafe { libc::isatty(fd) } == 0 {
            return Err("stdin is not a terminal — spawn this fixture under a PTY".to_string());
        }

        // The handler goes in before ISIG can be enabled: a stray interrupt
        // character in the gap would otherwise take the default disposition
        // and kill the fixture.
        install_sigint_handler()?;

        // SAFETY: a zeroed termios is a valid out-parameter; tcgetattr fully
        // initializes it on success, which is checked.
        let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: fd is a live tty (checked above), attrs a valid termios.
        if unsafe { libc::tcgetattr(fd, &mut attrs) } != 0 {
            return Err(format!("tcgetattr failed: {}", last_os_error()));
        }
        let saved = Saved(attrs);

        // Byte-wise, echo-free reads in both modes, so one written byte is
        // one read and the report channel carries no echo noise. Whether the
        // line discipline may turn the interrupt character into a signal —
        // ISIG — is deliberately the only bit the two modes disagree on.
        attrs.c_lflag &= !(libc::ICANON | libc::ECHO);
        match mode {
            Mode::Raw => attrs.c_lflag &= !libc::ISIG,
            Mode::Cooked => attrs.c_lflag |= libc::ISIG,
        }
        attrs.c_cc[libc::VMIN] = 1;
        attrs.c_cc[libc::VTIME] = 0;
        // SAFETY: same live fd; attrs was initialized by tcgetattr above.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &attrs) } != 0 {
            return Err(format!("tcsetattr failed: {}", last_os_error()));
        }

        // Report what the terminal actually holds, not what was requested:
        // tcsetattr succeeds even when it applied only part of the change.
        // SAFETY: as for the first tcgetattr.
        let mut applied: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut applied) } != 0 {
            return Err(format!("tcgetattr (verify) failed: {}", last_os_error()));
        }
        let isig = applied.c_lflag & libc::ISIG != 0;
        // SAFETY: getpgrp takes nothing and cannot fail.
        let pgid = unsafe { libc::getpgrp() };
        Ok((
            saved,
            vec![
                // The probe signals the process group; the group id the
                // child itself observes is the authoritative target.
                ("pgid", pgid.to_string()),
                ("isig", if isig { "on" } else { "off" }.to_string()),
            ],
        ))
    }

    fn install_sigint_handler() -> Result<(), String> {
        // SAFETY: zeroed is a valid starting point; every field the kernel
        // consults is assigned below (sigemptyset initializes the mask).
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: sa_mask is a valid out-parameter for sigemptyset.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        action.sa_sigaction = on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SA_RESTART: the blocking stdin read resumes after the handler
        // runs. Reporting belongs to the watcher thread; a read that failed
        // with EINTR on every interrupt would entangle the byte observation
        // with the signal observation.
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: action is fully initialized; a null old-action is allowed.
        if unsafe { libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) } != 0 {
            return Err(format!("sigaction failed: {}", last_os_error()));
        }
        Ok(())
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
mod terminal {
    use std::sync::atomic::Ordering;

    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, CTRL_C_EVENT, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleCtrlHandler, SetConsoleMode,
    };
    use windows_sys::core::BOOL;

    use super::{INTERRUPTS, Mode, TermFields};

    pub struct Saved {
        handle: HANDLE,
        mode_bits: CONSOLE_MODE,
    }

    unsafe extern "system" fn on_ctrl(ctrl_type: u32) -> BOOL {
        // Claim only Ctrl+C, and claim it fully (returning 1) — the process
        // must survive the event to report it. Anything else falls through
        // to the default handler.
        if ctrl_type == CTRL_C_EVENT {
            INTERRUPTS.fetch_add(1, Ordering::SeqCst);
            1
        } else {
            0
        }
    }

    pub fn configure(mode: Mode) -> Result<(Saved, TermFields), String> {
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

        // The handler goes in before processed input can be enabled: a
        // stray 0x03 in the gap would otherwise terminate the fixture.
        // SAFETY: the handler is a static function that only touches an
        // atomic; 1 = add it to the handler chain.
        if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) } == 0 {
            return Err(format!(
                "SetConsoleCtrlHandler failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        // Character-wise, echo-free reads in both modes. Whether the console
        // host may turn 0x03 into a CTRL_C_EVENT — ENABLE_PROCESSED_INPUT,
        // the ISIG analogue — is deliberately the only bit the two modes
        // disagree on.
        let mut requested = before & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT);
        match mode {
            Mode::Raw => requested &= !ENABLE_PROCESSED_INPUT,
            Mode::Cooked => requested |= ENABLE_PROCESSED_INPUT,
        }
        // SAFETY: live console handle, plain value argument.
        if unsafe { SetConsoleMode(handle, requested) } == 0 {
            return Err(format!(
                "SetConsoleMode failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        // Report what the console actually holds, not what was requested.
        let mut applied: CONSOLE_MODE = 0;
        // SAFETY: as for the first GetConsoleMode.
        if unsafe { GetConsoleMode(handle, &mut applied) } == 0 {
            return Err(format!(
                "GetConsoleMode (verify) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let processed = applied & ENABLE_PROCESSED_INPUT != 0;
        Ok((
            Saved {
                handle,
                mode_bits: before,
            },
            vec![
                (
                    "processed_input",
                    if processed { "on" } else { "off" }.to_string(),
                ),
                // The raw bits, before and after, so a Windows run records
                // the exact console contract it observed.
                ("console_mode", format!("{before:#x}->{applied:#x}")),
            ],
        ))
    }

    pub fn restore(saved: &Saved) {
        // Best effort, mirroring the POSIX restore.
        // SAFETY: the handle and bits came from configure on this process.
        let _ = unsafe { SetConsoleMode(saved.handle, saved.mode_bits) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_select_mode_and_watchdog() {
        let args = ["cooked", "--watchdog-secs", "7"].map(String::from);
        let (mode, watchdog) = parse_args(args.into_iter()).unwrap();
        assert!(matches!(mode, Mode::Cooked));
        assert_eq!(watchdog, Duration::from_secs(7));
    }

    #[test]
    fn a_mode_is_required() {
        let err = parse_args(std::iter::empty()).unwrap_err();
        assert!(err.contains("mode is required"), "unexpected error: {err}");
    }

    #[test]
    fn a_second_mode_is_rejected() {
        let args = ["raw", "cooked"].map(String::from);
        assert!(parse_args(args.into_iter()).is_err());
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
    }

    #[test]
    fn mode_names_match_the_cli_spelling() {
        // The ready report echoes these names and the probe asserts on
        // them; they must be the exact strings the CLI accepts.
        assert_eq!(Mode::Raw.name(), "raw");
        assert_eq!(Mode::Cooked.name(), "cooked");
    }
}
