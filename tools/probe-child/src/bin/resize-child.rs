//! Resize-observation fixture — the controlled child the resize probe
//! spawns under a PTY to see, from the inside, what a terminal resize
//! actually delivers. It reports as `probe-child event=…` lines on stdout
//! (the PTY slave, so the spawning probe reads them back through the
//! master):
//!
//! - `armed`, then `ready`: the notification channel is installed —
//!   a `SIGWINCH` handler on POSIX, `ENABLE_WINDOW_INPUT` on Windows — and
//!   the read loop is about to start. Ready carries the terminal size the
//!   fixture sampled at startup. A resize issued *before* ready is the
//!   early-launch edge case the probe deliberately exercises: the
//!   notification for it may be lost (handler not yet installed, window
//!   input not yet enabled), and on Windows the resize itself may be
//!   silently dropped in the attach window — on POSIX the size is kernel
//!   state and survives. Which of those happened is exactly what the
//!   probe reads back out of this fixture's reports.
//! - `winch`: one per delivered notification — the handler fired (POSIX)
//!   or a window-buffer-size event arrived on the console input queue
//!   (Windows) — with the window geometry read at report time; never
//!   emitted from polling. On Windows the event's raw payload rides along
//!   as `buf`: it is the buffer size, which diverges from the window on a
//!   shrink (conhost keeps the buffer height for scrollback), so it is
//!   recorded for drift detection rather than reported as the geometry.
//! - `dims`: the answer to the probe's on-demand request byte — the live
//!   terminal size next to `COLUMNS`/`LINES` re-read from the environment
//!   at that moment. After a resize the two channels genuinely diverge:
//!   env is set once at spawn and never moves; only the terminal changes.
//!
//! On POSIX the handler only bumps an atomic — everything else is
//! off-limits in signal context — and a watcher thread turns the count
//! into winch reports. On Windows the notification is in-band: the read
//! loop consumes console input records, so key events and size events
//! arrive through one `ReadConsoleInputW` loop.
//!
//! One hard formatting constraint: the probe spawns this fixture at 80
//! columns — the dimensions under test — and ConPTY reflows child output
//! to the PTY width, so every report shape must stay comfortably inside
//! one 80-column row or it would hard-wrap mid-`key=value` and never parse
//! (a unit test holds each shape to that budget).
//!
//! The fixture exits 0 on the quit byte (`q`) or on end-of-input, 2 on a
//! usage error, 3 when the terminal cannot be configured, 4 on a read
//! error, and a watchdog exits 9 if no quit arrives in time, so an
//! orphaned run can never outlive its probe for long.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use agent_bridge_probe_child::{
    DIMS_BYTE, EVENT_ARMED, EVENT_BYTE, EVENT_DIMS, EVENT_EOF, EVENT_QUIT, EVENT_READY,
    EVENT_WATCHDOG, EVENT_WINCH, QUIT_BYTE, WINCH_VIA, byte_hex, format_report,
};

/// Resize notifications delivered so far. On POSIX the `SIGWINCH` handler
/// writes it and the watcher thread turns it into winch reports; on Windows
/// the read loop owns it. Either way it is the `winches` total the quit and
/// eof reports carry.
static WINCHES: AtomicU32 = AtomicU32::new(0);

/// The platform-specific fields the armed report carries — the observation
/// channel that was actually installed.
type TermFields = Vec<(&'static str, String)>;

const DEFAULT_WATCHDOG_SECS: u64 = 120;
const WATCH_POLL: Duration = Duration::from_millis(20);

fn main() {
    let watchdog = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("resize-child: {message}");
            std::process::exit(2);
        }
    };
    let (saved, armed) = match terminal::configure() {
        Ok(configured) => configured,
        Err(detail) => {
            eprintln!("resize-child: terminal setup failed: {detail}");
            std::process::exit(3);
        }
    };
    report(EVENT_ARMED, &armed);

    // Nothing may be written to the child before this line arrives: it
    // promises the notification channel is armed and the read loop about to
    // start. The size it carries is what the fixture sampled at startup —
    // under an early resize that is already the post-resize size, because
    // the size lives in the kernel/console, not in the notification.
    let (cols, rows) = dims_or_zero();
    report(EVENT_READY, &ready_fields(std::process::id(), cols, rows));

    terminal::spawn_watcher(Instant::now() + watchdog, watchdog, saved);
    let code = terminal::read_loop();
    terminal::restore(&saved);
    std::process::exit(code);
}

fn parse_args<I: Iterator<Item = String>>(mut args: I) -> Result<Duration, String> {
    const USAGE: &str = "usage: resize-child [--watchdog-secs N]";
    let mut watchdog = Duration::from_secs(DEFAULT_WATCHDOG_SECS);
    while let Some(arg) = args.next() {
        match arg.as_str() {
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
    Ok(watchdog)
}

/// One locked write per line so the read loop and the watcher thread can
/// never interleave mid-line. The `\r\n` is explicit: a report must start
/// at column zero even though output post-processing is left on.
fn report(event: &str, fields: &[(&str, String)]) {
    use std::io::Write;
    let line = format_report(event, fields);
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\r\n");
    let _ = out.flush();
}

fn ready_fields(pid: u32, cols: u16, rows: u16) -> Vec<(&'static str, String)> {
    vec![
        ("pid", pid.to_string()),
        ("cols", cols.to_string()),
        ("rows", rows.to_string()),
    ]
}

/// The armed report's fields: what delivers resize notifications on this
/// platform, plus — on Windows — the raw console-mode bits before and after
/// configuring, so a run records the exact console contract it observed.
fn armed_fields(mode: Option<String>) -> TermFields {
    let mut fields = vec![("via", WINCH_VIA.to_string())];
    if let Some(mode) = mode {
        fields.push(("mode", mode));
    }
    fields
}

fn winch_fields(seq: u32, cols: u16, rows: u16) -> Vec<(&'static str, String)> {
    vec![
        ("seq", seq.to_string()),
        ("cols", cols.to_string()),
        ("rows", rows.to_string()),
    ]
}

fn dims_fields(
    seq: u32,
    cols: u16,
    rows: u16,
    env_columns: String,
    env_lines: String,
) -> Vec<(&'static str, String)> {
    vec![
        ("seq", seq.to_string()),
        ("cols", cols.to_string()),
        ("rows", rows.to_string()),
        ("env_columns", env_columns),
        ("env_lines", env_lines),
    ]
}

fn winch_total() -> (&'static str, String) {
    ("winches", WINCHES.load(Ordering::SeqCst).to_string())
}

/// The live terminal size, or 0×0 when the read fails: the probe's
/// assertions then fail loudly on an impossible geometry — with the read
/// error visible in the output tail via stderr, which is the same PTY —
/// rather than the fixture dying quietly mid-scenario.
fn dims_or_zero() -> (u16, u16) {
    match terminal::dims() {
        Ok(dims) => dims,
        Err(detail) => {
            eprintln!("resize-child: reading the terminal size failed: {detail}");
            (0, 0)
        }
    }
}

/// An env value as a report field: `-` when unset (or undecodable), and
/// whitespace collapsed so a hand-run with a mangled value can never break
/// the one-line report format.
fn env_dim(name: &str) -> String {
    sanitized_dim(std::env::var(name).ok())
}

fn sanitized_dim(value: Option<String>) -> String {
    value.map_or_else(
        || "-".to_string(),
        |value| value.replace(char::is_whitespace, "_"),
    )
}

/// What one input byte means to this fixture.
enum ByteAction {
    Dims,
    Quit,
    Data,
}

fn classify(byte: u8) -> ByteAction {
    match byte {
        DIMS_BYTE => ByteAction::Dims,
        QUIT_BYTE => ByteAction::Quit,
        _ => ByteAction::Data,
    }
}

/// Handle one decoded input byte; `true` means the quit byte arrived and
/// the read loop should end. Unexpected bytes are reported, not dropped —
/// under this probe they are always a bug worth seeing.
fn handle_byte(byte: u8, dims_seq: &mut u32) -> bool {
    match classify(byte) {
        ByteAction::Dims => {
            *dims_seq += 1;
            let (cols, rows) = dims_or_zero();
            report(
                EVENT_DIMS,
                &dims_fields(*dims_seq, cols, rows, env_dim("COLUMNS"), env_dim("LINES")),
            );
            false
        }
        ByteAction::Quit => {
            report(EVENT_QUIT, &[winch_total()]);
            true
        }
        ByteAction::Data => {
            report(EVENT_BYTE, &[("hex", byte_hex(byte))]);
            false
        }
    }
}

#[cfg(unix)]
mod terminal {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use super::{EVENT_WATCHDOG, TermFields, WATCH_POLL, WINCHES};

    /// Plain copyable data so the watchdog thread can carry its own copy —
    /// it exits the process directly and must restore first.
    #[derive(Clone, Copy)]
    pub struct Saved(libc::termios);

    extern "C" fn on_sigwinch(_signal: libc::c_int) {
        // Signal context: bumping an atomic is the entire safe repertoire.
        // The watcher thread turns the count into winch reports.
        WINCHES.fetch_add(1, Ordering::SeqCst);
    }

    pub fn configure() -> Result<(Saved, TermFields), String> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: isatty only inspects the fd; no memory is exchanged.
        if unsafe { libc::isatty(fd) } == 0 {
            return Err("stdin is not a terminal — spawn this fixture under a PTY".to_string());
        }

        install_sigwinch_handler()?;

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
        // channel free of echo noise.
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
        Ok((saved, super::armed_fields(None)))
    }

    fn install_sigwinch_handler() -> Result<(), String> {
        // SAFETY: zeroed is a valid starting point; every field the kernel
        // consults is assigned below (sigemptyset initializes the mask).
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: sa_mask is a valid out-parameter for sigemptyset.
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        action.sa_sigaction = on_sigwinch as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SA_RESTART: the blocking stdin read resumes after the handler
        // runs, so the byte observation stays untangled from the resize
        // observation.
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: action is fully initialized; a null old-action is allowed.
        if unsafe { libc::sigaction(libc::SIGWINCH, &action, std::ptr::null_mut()) } != 0 {
            return Err(format!("sigaction failed: {}", last_os_error()));
        }
        Ok(())
    }

    /// The live terminal size, straight from the kernel — the state a
    /// resize mutates, as opposed to the env values frozen at spawn.
    pub fn dims() -> Result<(u16, u16), String> {
        // SAFETY: a zeroed winsize is a valid out-parameter; ioctl fills it
        // on success, which is checked.
        let mut size: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: stdin is a live tty (configure checked); size is a valid
        // winsize out-pointer for TIOCGWINSZ.
        if unsafe { libc::ioctl(libc::STDIN_FILENO, libc::TIOCGWINSZ, &mut size) } != 0 {
            return Err(format!("TIOCGWINSZ failed: {}", last_os_error()));
        }
        Ok((size.ws_col, size.ws_row))
    }

    /// Report handler firings as they happen and enforce the watchdog. A
    /// 20ms poll is far inside every settle window the probe applies, and
    /// polling an atomic is the only reporting channel that is safe from
    /// signal context. The dimensions are read when the report is written —
    /// moments after the handler fired — because the handler itself may do
    /// nothing but bump the atomic.
    ///
    /// The watcher carries its own copy of the saved terminal state: it
    /// exits the process directly on watchdog expiry, and for a human who
    /// ran the fixture by hand the watchdog is the likeliest exit, so
    /// skipping the restore here would strand a real shell raw and
    /// echo-less.
    pub fn spawn_watcher(deadline: Instant, watchdog: Duration, saved: Saved) {
        std::thread::spawn(move || {
            let mut reported: u32 = 0;
            loop {
                let fired = WINCHES.load(Ordering::SeqCst);
                while reported < fired {
                    reported += 1;
                    let (cols, rows) = super::dims_or_zero();
                    super::report(
                        super::EVENT_WINCH,
                        &super::winch_fields(reported, cols, rows),
                    );
                }
                if Instant::now() >= deadline {
                    super::report(
                        EVENT_WATCHDOG,
                        &[("after_secs", watchdog.as_secs().to_string())],
                    );
                    restore(&saved);
                    std::process::exit(9);
                }
                std::thread::sleep(WATCH_POLL);
            }
        });
    }

    /// Dispatch every stdin byte until the quit byte, end-of-input, or a
    /// read error; returns the process exit code.
    pub fn read_loop() -> i32 {
        use std::io::Read;

        let mut dims_seq: u32 = 0;
        // Big enough for a burst, and comfortably over the four bytes Rust's
        // console reader needs to hold one arbitrary UTF-8 character.
        let mut buf = [0u8; 256];
        let mut stdin = std::io::stdin().lock();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => {
                    super::report(super::EVENT_EOF, &[super::winch_total()]);
                    return 0;
                }
                Ok(n) => {
                    for &byte in &buf[..n] {
                        if super::handle_byte(byte, &mut dims_seq) {
                            return 0;
                        }
                    }
                }
                // A signal without SA_RESTART semantics can cut the read
                // short; the byte stream itself has not ended.
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    eprintln!("resize-child: stdin read failed: {err}");
                    return 4;
                }
            }
        }
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
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, ENABLE_WINDOW_INPUT, GetConsoleMode, GetConsoleScreenBufferInfo,
        GetStdHandle, INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleMode, WINDOW_BUFFER_SIZE_EVENT,
    };

    use super::{EVENT_WATCHDOG, TermFields, WATCH_POLL, WINCHES};

    /// Only the mode bits, not the handle: the stdin handle is
    /// process-global and re-acquired at restore time, which keeps this
    /// plain copyable data the watchdog thread can carry its own copy of —
    /// it exits the process directly and must restore first.
    #[derive(Clone, Copy)]
    pub struct Saved {
        mode_bits: CONSOLE_MODE,
    }

    pub fn configure() -> Result<(Saved, TermFields), String> {
        let handle = stdin_handle()?;
        let mut before: CONSOLE_MODE = 0;
        // SAFETY: handle is live (checked above); `before` is a valid
        // out-pointer.
        if unsafe { GetConsoleMode(handle, &mut before) } == 0 {
            return Err(format!(
                "stdin is not a console — spawn this fixture under a ConPTY ({})",
                std::io::Error::last_os_error()
            ));
        }

        // ENABLE_WINDOW_INPUT is the subscription: window-buffer-size
        // events only reach the input queue while it is set. Line input,
        // echo, and processed input go off for a clean byte-wise channel,
        // mirroring the POSIX raw setup.
        let requested = (before
            & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT))
            | ENABLE_WINDOW_INPUT;
        // SAFETY: live console handle, plain value argument.
        if unsafe { SetConsoleMode(handle, requested) } == 0 {
            return Err(format!(
                "SetConsoleMode failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let saved = Saved { mode_bits: before };
        // Verify what the console actually holds, not what was requested —
        // without ENABLE_WINDOW_INPUT applied this fixture can only ever
        // time out, and that failure must be typed at setup, not diagnosed
        // from silence.
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
        if applied & ENABLE_WINDOW_INPUT == 0 {
            restore(&saved);
            return Err(format!(
                "the console did not apply ENABLE_WINDOW_INPUT (mode {before:#x}->{applied:#x}) — resize events would never arrive"
            ));
        }
        Ok((
            saved,
            super::armed_fields(Some(format!("{before:#x}->{applied:#x}"))),
        ))
    }

    /// The live terminal size, straight from the console. The window rect,
    /// not the buffer: the window is what applications size their UI to —
    /// the analogue of the POSIX winsize — and the two genuinely differ on
    /// a shrink, where conhost keeps the buffer height for scrollback.
    pub fn dims() -> Result<(u16, u16), String> {
        // SAFETY: GetStdHandle only looks up a slot in the PEB.
        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err("no stdout handle".to_string());
        }
        // SAFETY: a zeroed CONSOLE_SCREEN_BUFFER_INFO is a valid
        // out-parameter; the call fills it on success, which is checked.
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = unsafe { std::mem::zeroed() };
        // SAFETY: live console handle; info is a valid out-pointer.
        if unsafe { GetConsoleScreenBufferInfo(handle, &mut info) } == 0 {
            return Err(format!(
                "GetConsoleScreenBufferInfo failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let width = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
        let height = i32::from(info.srWindow.Bottom) - i32::from(info.srWindow.Top) + 1;
        // A window rect that does not fit u16 is not a terminal geometry;
        // 0 fails the probe's assertions loudly, same as a failed read.
        Ok((
            u16::try_from(width).unwrap_or(0),
            u16::try_from(height).unwrap_or(0),
        ))
    }

    /// Watchdog only: resize notifications are in-band on the console
    /// input queue, so the read loop itself reports them.
    pub fn spawn_watcher(deadline: Instant, watchdog: Duration, saved: Saved) {
        std::thread::spawn(move || {
            loop {
                if Instant::now() >= deadline {
                    super::report(
                        EVENT_WATCHDOG,
                        &[("after_secs", watchdog.as_secs().to_string())],
                    );
                    restore(&saved);
                    std::process::exit(9);
                }
                std::thread::sleep(WATCH_POLL);
            }
        });
    }

    /// Consume console input records until the quit byte or end-of-input;
    /// returns the process exit code. Key events carry the probe's bytes;
    /// window-buffer-size events are the resize notifications under test.
    pub fn read_loop() -> i32 {
        let handle = match stdin_handle() {
            Ok(handle) => handle,
            Err(detail) => {
                eprintln!("resize-child: {detail}");
                return 4;
            }
        };
        let mut dims_seq: u32 = 0;
        loop {
            let mut record: INPUT_RECORD = INPUT_RECORD::default();
            let mut read: u32 = 0;
            // SAFETY: live console handle; record and read are valid
            // out-pointers for exactly one record.
            if unsafe { ReadConsoleInputW(handle, &mut record, 1, &mut read) } == 0 {
                // The master side closed the console — the end of input,
                // the analogue of the POSIX 0-byte read.
                super::report(super::EVENT_EOF, &[super::winch_total()]);
                return 0;
            }
            if read == 0 {
                continue;
            }
            match u32::from(record.EventType) {
                KEY_EVENT => {
                    // SAFETY: EventType says the union holds a key event.
                    let key = unsafe { record.Event.KeyEvent };
                    if key.bKeyDown == 0 {
                        continue;
                    }
                    // SAFETY: uChar is a union of two character encodings of
                    // the same data; the u16 view is always in-bounds.
                    let ch = unsafe { key.uChar.UnicodeChar };
                    if ch == 0 {
                        continue; // a bare modifier or dead key
                    }
                    match u8::try_from(ch) {
                        Ok(byte) => {
                            if super::handle_byte(byte, &mut dims_seq) {
                                return 0;
                            }
                        }
                        // Never sent by the probe; reported rather than
                        // dropped, like any other unexpected input.
                        Err(_) => {
                            super::report(super::EVENT_BYTE, &[("hex", format!("{ch:#06x}"))])
                        }
                    }
                }
                WINDOW_BUFFER_SIZE_EVENT => {
                    // SAFETY: EventType says the union holds the size event.
                    let size = unsafe { record.Event.WindowBufferSizeEvent }.dwSize;
                    let seq = WINCHES.fetch_add(1, Ordering::SeqCst) + 1;
                    // The event payload is the BUFFER size, and on a shrink
                    // conhost keeps the buffer height for scrollback — the
                    // first Windows CI run delivered 80x40 for a 120x40 ->
                    // 80x24 shrink. The window rect, read at report time
                    // (mirroring the POSIX watcher's ioctl), is the geometry
                    // an application lays out against; the raw payload is
                    // recorded alongside so a conhost behavior change shows
                    // up in the log instead of silently shifting meaning.
                    let (cols, rows) = super::dims_or_zero();
                    let mut fields = super::winch_fields(seq, cols, rows);
                    fields.push(("buf", format!("{}x{}", size.X, size.Y)));
                    super::report(super::EVENT_WINCH, &fields);
                }
                _ => {} // focus, menu, and mouse noise
            }
        }
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

    fn stdin_handle() -> Result<HANDLE, String> {
        // SAFETY: GetStdHandle only looks up a slot in the PEB.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err("no stdin handle".to_string());
        }
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The line budget the module doc names: a report reaching column 80
    /// would hard-wrap under ConPTY reflow and never parse, so every shape
    /// must fit with margin for the values the resize scenarios produce.
    const MAX_REPORT_CHARS: usize = 78;

    #[test]
    fn args_select_the_watchdog() {
        let args = ["--watchdog-secs", "7"].map(String::from);
        assert_eq!(
            parse_args(args.into_iter()).unwrap(),
            Duration::from_secs(7)
        );
        assert_eq!(
            parse_args(std::iter::empty()).unwrap(),
            Duration::from_secs(DEFAULT_WATCHDOG_SECS)
        );
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
        assert!(parse_args(["--watchdog-secs".to_string()].into_iter()).is_err());
    }

    #[test]
    fn the_control_bytes_dispatch_and_data_stays_data() {
        assert!(matches!(classify(DIMS_BYTE), ByteAction::Dims));
        assert!(matches!(classify(QUIT_BYTE), ByteAction::Quit));
        assert!(matches!(classify(b'x'), ByteAction::Data));
        assert!(matches!(classify(0x03), ByteAction::Data));
    }

    #[test]
    fn env_values_are_report_safe() {
        assert_eq!(sanitized_dim(None), "-");
        assert_eq!(sanitized_dim(Some("80".to_string())), "80");
        // A mangled hand-run value must not break the one-line format.
        assert_eq!(sanitized_dim(Some("8 0\n".to_string())), "8_0_");
    }

    #[test]
    fn every_report_shape_fits_an_80_column_terminal() {
        // The probe spawns this fixture at 80 columns — the dimensions
        // under test — and ConPTY reflows output to the PTY width, so a
        // line reaching column 80 would wrap mid-`key=value` and never
        // parse. Values here are the ceilings of what the resize scenarios
        // produce: dimensions up to 120×40 (three digits), sequence numbers
        // in the tens, spawn-time env values, a full-width pid, and the
        // Windows armed shape with generous console-mode bits — the longest
        // via string of the two platforms.
        let lines = [
            format_report(
                EVENT_ARMED,
                &[
                    ("via", "window-buffer-size-event".to_string()),
                    ("mode", "0x1f3f7->0x1f3d8".to_string()),
                ],
            ),
            format_report(EVENT_READY, &ready_fields(u32::MAX, 999, 999)),
            format_report(EVENT_WINCH, &winch_fields(u32::MAX, 999, 999)),
            // The Windows shape: the raw buffer size rides along, and on a
            // shrink its height is the pre-resize one — up to four digits.
            format_report(EVENT_WINCH, &{
                let mut fields = winch_fields(u32::MAX, 999, 999);
                fields.push(("buf", "9999x9999".to_string()));
                fields
            }),
            format_report(
                EVENT_DIMS,
                &dims_fields(999, 999, 999, "999".to_string(), "999".to_string()),
            ),
            format_report(EVENT_QUIT, &[("winches", u32::MAX.to_string())]),
            format_report(EVENT_EOF, &[("winches", u32::MAX.to_string())]),
            format_report(EVENT_WATCHDOG, &[("after_secs", "86400".to_string())]),
        ];
        for line in lines {
            assert!(
                line.len() <= MAX_REPORT_CHARS,
                "report shape overflows the 80-column budget ({} chars): {line}",
                line.len()
            );
        }
    }
}
