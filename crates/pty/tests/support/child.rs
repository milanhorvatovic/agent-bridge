//! The other half of every test here: the program that runs *inside* the
//! terminal.
//!
//! It is this same test binary, re-invoked with a role argument. That is why
//! these targets run their own `main` rather than the test harness — a test
//! harness would want to parse those arguments as filters — and it is what
//! keeps the fixtures dependency-free: a terminal test needs a child that
//! puts its terminal into a particular mode, spawns a descendant, or refuses
//! to read, and none of those can be faked from outside.
//!
//! Everything the fixtures say is one line of `key=value` fields, so the
//! parent can wait for a marker without depending on wording. Lines are kept
//! short because a console reflows output at the terminal width, and a
//! report wrapped mid-field would not parse.

use std::io::Write;
use std::time::{Duration, Instant};

/// How long a fixture that is waiting to be told something will wait before
/// giving up and exiting.
///
/// A backstop, not a timeout anything depends on: every scenario ends its
/// own fixture. It exists so that a test killed part-way through cannot
/// leave a process idling on a build machine for the rest of the day.
const IDLE_LIMIT: Duration = Duration::from_secs(120);

/// Text with characters of two, three, and four bytes, for proving that a
/// read boundary falling inside one changes nothing.
pub const UTF8_CORPUS: &str = "héllo 🌍 — ascii, 2-byte é, 3-byte —, 4-byte 🌍";

/// Environment variables the `env` fixture reports back.
pub const REPORTED_ENV: [&str; 6] = ["TERM", "COLORTERM", "COLUMNS", "LINES", "LC_ALL", "PLANTED"];

/// Run the named fixture role and exit. Never returns.
pub fn run(role: &str, args: &[String]) -> ! {
    match role {
        "echo" => {
            line(&format!("echo={}", args.first().map_or("", String::as_str)));
            exit(0)
        }
        "env" => {
            for name in REPORTED_ENV {
                // An absent variable is reported as absent rather than
                // skipped: "the default was not set" and "the fixture never
                // looked" must not read the same to the parent.
                let value = std::env::var(name).unwrap_or_else(|_| "<unset>".to_string());
                line(&format!("env {name}={value}"));
            }
            line("done");
            exit(0)
        }
        "utf8" => {
            line("corpus-begin");
            // One byte at a time, each flushed: the parent cannot choose
            // where a read boundary falls, but a child that dribbles makes
            // the boundaries fall everywhere, including inside characters.
            for byte in UTF8_CORPUS.as_bytes() {
                let mut out = std::io::stdout();
                let _ = out.write_all(std::slice::from_ref(byte));
                let _ = out.flush();
                std::thread::sleep(Duration::from_millis(1));
            }
            line("");
            line("corpus-end");
            exit(0)
        }
        "raw" => keyboard(true),
        "cooked" => keyboard(false),
        "winsize" => winsize(),
        "tree" => tree(),
        "idle" => {
            // No output at all, ever: the fixture a resize test needs in
            // order to ask what happens before the child has spoken.
            idle();
            exit(0)
        }
        "deaf" => {
            line("ready");
            // Never reads its input. The terminal's buffer fills, and a
            // parent writing into it eventually has nowhere to put the bytes
            // — which is the only way to observe a blocked write.
            idle();
            exit(0)
        }
        other => {
            line(&format!("error unknown-role={other}"));
            exit(2)
        }
    }
}

/// Report a line to the parent, through the terminal.
fn line(text: &str) {
    println!("{text}");
}

fn exit(code: i32) -> ! {
    // Flushed by `println!`'s line buffering already; this is the belt for
    // any fixture that ever writes without a newline.
    let _ = std::io::stdout().flush();
    std::process::exit(code)
}

/// Sit still until the idle backstop passes.
fn idle() {
    std::thread::sleep(IDLE_LIMIT);
}

/// Read what is typed and report it, in raw or cooked mode.
///
/// The distinction is the whole point of the interrupt contract. In raw mode
/// the terminal stops turning a typed Ctrl+C into a signal, so the byte
/// arrives at the read like any other and the program decides what it means.
/// In cooked mode the terminal synthesises the signal and the byte is never
/// delivered. An interactive CLI runs in the first mode, which is why
/// interrupting one means writing the byte rather than sending the signal.
fn keyboard(raw: bool) -> ! {
    terminal::watch_interrupt();
    // Set for *both* modes rather than only for raw. The difference under
    // test is one flag, and leaving the other mode at whatever the platform
    // happens to start in would make the scenario an assertion about that
    // default. What took effect is reported, so a failure on a machine
    // nobody can attach to says which mode the terminal was actually in.
    let applied = terminal::set_mode(raw);
    line(&format!(
        "ready mode={} terminal={applied}",
        if raw { "raw" } else { "cooked" }
    ));
    let deadline = Instant::now() + IDLE_LIMIT;
    while Instant::now() < deadline {
        match terminal::next_event(Duration::from_millis(100)) {
            Some(terminal::Event::Byte(byte)) => {
                line(&format!("byte=0x{byte:02x}"));
                if byte == b'q' {
                    line("done");
                    exit(0);
                }
            }
            Some(terminal::Event::Interrupted) => line("signal=interrupt"),
            Some(terminal::Event::Resized) | None => {}
        }
    }
    exit(0)
}

/// Report the terminal's geometry, and every change to it.
fn winsize() -> ! {
    terminal::watch_resize();
    let (cols, rows) = terminal::size();
    line(&format!("ready cols={cols} rows={rows}"));
    let deadline = Instant::now() + IDLE_LIMIT;
    while Instant::now() < deadline {
        if let Some(terminal::Event::Resized) = terminal::next_event(Duration::from_millis(100)) {
            let (cols, rows) = terminal::size();
            line(&format!("winsize cols={cols} rows={rows}"));
        }
    }
    exit(0)
}

/// Spawn a descendant and report it, then wait to be cleaned up.
///
/// The descendant is what makes containment observable: a signal sent to the
/// process this crate started would never reach it, and a runtime that only
/// killed the process it spawned would leave one of these behind after every
/// tool call an interactive CLI makes.
fn tree() -> ! {
    let own = std::env::current_exe().expect("a fixture must be able to find itself");
    // Deliberately never waited on. This fixture exists to be killed along
    // with its descendant, and one that reaped its own child would remove
    // the thing every containment scenario goes looking for.
    #[allow(clippy::zombie_processes)]
    let grandchild = std::process::Command::new(own)
        .arg("idle")
        // Detached from the terminal: a descendant that inherited it could
        // hold the stream open after the child is gone, which would make the
        // test measure the wrong thing.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("the grandchild must spawn");
    line(&format!("grandchild={}", grandchild.id()));
    line("ready");
    idle();
    exit(0)
}

/// What the fixture's terminal can tell it, on this platform.
#[cfg(unix)]
mod terminal {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Something the terminal delivered.
    pub enum Event {
        /// A byte was typed.
        Byte(u8),
        /// An interrupt signal arrived — which, in raw mode, must never
        /// happen from a typed Ctrl+C.
        Interrupted,
        /// The geometry changed.
        Resized,
    }

    static INTERRUPTED: AtomicBool = AtomicBool::new(false);
    static RESIZED: AtomicBool = AtomicBool::new(false);

    /// Signal handlers may do almost nothing safely; setting a flag another
    /// thread polls is the one thing they may always do.
    extern "C" fn note_interrupt(_signal: libc::c_int) {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }

    extern "C" fn note_resize(_signal: libc::c_int) {
        RESIZED.store(true, Ordering::Relaxed);
    }

    pub fn watch_interrupt() {
        // SAFETY: installing a handler takes a signal number and a function
        // pointer; the handler itself touches only an atomic.
        unsafe {
            libc::signal(
                libc::SIGINT,
                note_interrupt as *const () as libc::sighandler_t,
            )
        };
    }

    pub fn watch_resize() {
        // SAFETY: as for the interrupt handler above.
        unsafe {
            libc::signal(
                libc::SIGWINCH,
                note_resize as *const () as libc::sighandler_t,
            )
        };
    }

    /// Put the terminal in the mode the scenario needs, and say what took
    /// effect.
    ///
    /// Raw is the mode an interactive CLI uses: no line editing, no echo,
    /// and — the part under test — no signals synthesised from control
    /// characters. Cooked asks for the opposite explicitly rather than
    /// trusting the platform to have started there.
    pub fn set_mode(raw: bool) -> String {
        // SAFETY: a zeroed termios is overwritten by `tcgetattr` before it
        // is read, and every call takes a descriptor and a pointer to it.
        unsafe {
            let mut mode: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut mode) != 0 {
                return format!("unreadable({})", std::io::Error::last_os_error());
            }
            if raw {
                libc::cfmakeraw(&mut mode);
            } else {
                mode.c_lflag |= libc::ISIG | libc::ICANON;
            }
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &mode) != 0 {
                return format!("unset({})", std::io::Error::last_os_error());
            }
            // Read back rather than trust the request: a terminal grants
            // what it chooses to, and which part it granted is the scenario.
            let mut applied: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut applied) != 0 {
                return "unverified".to_string();
            }
            format!("isig={}", u8::from(applied.c_lflag & libc::ISIG != 0))
        }
    }

    pub fn size() -> (u16, u16) {
        // SAFETY: a zeroed winsize is a valid out-parameter for TIOCGWINSZ.
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) != 0 {
                return (0, 0);
            }
            (size.ws_col, size.ws_row)
        }
    }

    /// Wait up to `within` for the terminal to deliver something.
    pub fn next_event(within: std::time::Duration) -> Option<Event> {
        if INTERRUPTED.swap(false, Ordering::Relaxed) {
            return Some(Event::Interrupted);
        }
        if RESIZED.swap(false, Ordering::Relaxed) {
            return Some(Event::Resized);
        }
        let mut watch = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = i32::try_from(within.as_millis().max(1)).unwrap_or(i32::MAX);
        // SAFETY: one descriptor, and the array really is one element long.
        let ready = unsafe { libc::poll(&mut watch, 1, timeout) };
        if ready <= 0 {
            // A signal cutting the wait short is why this returns nothing
            // rather than looping: the caller comes straight back, and the
            // flag checks at the top are what it comes back for.
            return None;
        }
        let mut byte = 0u8;
        // SAFETY: reads at most one byte into a one-byte buffer.
        let read = unsafe { libc::read(libc::STDIN_FILENO, (&raw mut byte).cast(), 1) };
        if read == 1 {
            Some(Event::Byte(byte))
        } else {
            None
        }
    }
}

#[cfg(windows)]
mod terminal {
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_WINDOW_INPUT, GetConsoleMode, GetConsoleScreenBufferInfo, GetStdHandle,
        INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
        SetConsoleCtrlHandler, SetConsoleMode, WINDOW_BUFFER_SIZE_EVENT,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    use windows_sys::core::BOOL;

    pub enum Event {
        Byte(u8),
        Interrupted,
        Resized,
    }

    static INTERRUPTED: AtomicBool = AtomicBool::new(false);

    /// The console's answer to a signal handler. Returning true claims the
    /// event, which is what stops the default handler from ending the
    /// process before the fixture can report what it saw.
    unsafe extern "system" fn note_interrupt(_kind: u32) -> BOOL {
        INTERRUPTED.store(true, Ordering::Relaxed);
        1
    }

    fn input() -> HANDLE {
        // SAFETY: returns a handle this process already owns.
        unsafe { GetStdHandle(STD_INPUT_HANDLE) }
    }

    fn mode(handle: HANDLE) -> u32 {
        let mut mode = 0;
        // SAFETY: a live console handle and a valid out-pointer.
        unsafe { GetConsoleMode(handle, &mut mode) };
        mode
    }

    pub fn watch_interrupt() {
        // SAFETY: registers a handler that touches only an atomic.
        if unsafe { SetConsoleCtrlHandler(Some(note_interrupt), 1) } == 0 {
            // Reported rather than swallowed: without the handler, a cooked
            // scenario fails as "the interrupt never arrived", which sends a
            // reader looking at the layer instead of at this fixture.
            super::line(&format!(
                "error ctrl-handler={}",
                std::io::Error::last_os_error()
            ));
        }
    }

    pub fn watch_resize() {
        let handle = input();
        // Window events arrive through the same input queue as keystrokes,
        // and only once this is asked for.
        // SAFETY: a live console handle and a plain mode word.
        unsafe { SetConsoleMode(handle, mode(handle) | ENABLE_WINDOW_INPUT) };
    }

    /// Put the console in the mode the scenario needs, and say what took
    /// effect.
    ///
    /// Line assembly and echo are cleared in both modes — they are noise
    /// here, not the subject. The flag under test is
    /// `ENABLE_PROCESSED_INPUT`, which decides whether the console turns the
    /// interrupt character into a control event or passes it through as
    /// input, so it is set and cleared explicitly rather than left at a
    /// default.
    pub fn set_mode(raw: bool) -> String {
        let handle = input();
        let mut requested = mode(handle) & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT);
        if raw {
            requested &= !ENABLE_PROCESSED_INPUT;
        } else {
            requested |= ENABLE_PROCESSED_INPUT;
        }
        // SAFETY: a live console handle and a plain mode word.
        if unsafe { SetConsoleMode(handle, requested) } == 0 {
            return format!("unset({})", std::io::Error::last_os_error());
        }
        // Read back rather than trust the request: a console grants what it
        // chooses to, and which part it granted is the scenario.
        format!(
            "processed={}",
            u8::from(mode(handle) & ENABLE_PROCESSED_INPUT != 0)
        )
    }

    pub fn size() -> (u16, u16) {
        // SAFETY: a zeroed info block is a valid out-parameter, and the
        // handle is one this process owns.
        unsafe {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(GetStdHandle(STD_OUTPUT_HANDLE), &mut info) == 0 {
                return (0, 0);
            }
            // The *window* rather than the buffer: a console buffer can be
            // taller than what is shown, and it is the shown size that a
            // resize changes.
            let width = info.srWindow.Right - info.srWindow.Left + 1;
            let height = info.srWindow.Bottom - info.srWindow.Top + 1;
            (width.max(0) as u16, height.max(0) as u16)
        }
    }

    pub fn next_event(within: std::time::Duration) -> Option<Event> {
        if INTERRUPTED.swap(false, Ordering::Relaxed) {
            return Some(Event::Interrupted);
        }
        let handle = input();
        let timeout = u32::try_from(within.as_millis().max(1)).unwrap_or(u32::MAX);
        // SAFETY: waits on a handle this process owns.
        if unsafe { WaitForSingleObject(handle, timeout) } != WAIT_OBJECT_0 {
            return None;
        }
        // SAFETY: a zeroed record is a valid out-parameter; the count of
        // records read is checked before the record is inspected.
        unsafe {
            let mut record: INPUT_RECORD = std::mem::zeroed();
            let mut read = 0;
            if ReadConsoleInputW(handle, &mut record, 1, &mut read) == 0 || read != 1 {
                return None;
            }
            match record.EventType as u32 {
                KEY_EVENT => {
                    let key = record.Event.KeyEvent;
                    // Key-up records repeat what key-down already reported,
                    // and a record with no character is a modifier being
                    // pressed on its own.
                    if key.bKeyDown == 0 || key.uChar.UnicodeChar == 0 {
                        return None;
                    }
                    u8::try_from(key.uChar.UnicodeChar).ok().map(Event::Byte)
                }
                WINDOW_BUFFER_SIZE_EVENT => Some(Event::Resized),
                _ => None,
            }
        }
    }
}
