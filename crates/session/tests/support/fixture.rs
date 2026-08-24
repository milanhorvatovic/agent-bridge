//! The child half of every lifecycle scenario: this same test binary,
//! re-invoked in a role.
//!
//! The roles are deliberately tiny fake CLIs — each one is the smallest
//! process that exercises one contract edge: exiting before speaking,
//! crashing after, honoring an exit command, ignoring one, surviving the
//! interrupt byte, or spawning the descendant a containment check needs.
//! The raw-mode terminal plumbing is the same shape the terminal crate's
//! own suite uses; copied rather than imported because integration-test
//! code cannot be shared across crates, and proven on all three CI
//! platforms there.
//!
//! The session under test discards fixture output (the matcher pipeline
//! that will read it is Phase 2), so a role that must tell the scenario
//! something — a grandchild's pid — writes it to a file the scenario
//! names, never to the terminal.

use std::io::{BufRead, Write};
use std::time::Duration;

/// A backstop so a killed test cannot leave a fixture idling on a build
/// machine; every scenario ends its own fixture well before this.
const IDLE_LIMIT: Duration = Duration::from_secs(120);

/// Run the named role and exit. Never returns.
pub fn run(role: &str, args: &[String]) -> ! {
    match role {
        // Exits before ever producing output: the `Connecting → Closed`
        // edge.
        "instant-exit" => exit(0),
        // Speaks (so the session reaches Running), then dies with a
        // non-zero code: the post-`Running` failure routing.
        "crash" => {
            line("hello");
            std::thread::sleep(Duration::from_millis(300));
            exit(3)
        }
        // The cooperative CLI: announces itself, then honors the scripted
        // exit command a `ShutdownHint` types at it. Unknown lines are
        // ignored, like a real CLI ignoring chatter.
        "cooperative" => {
            line("ready");
            let stdin = std::io::stdin();
            let mut input = String::new();
            loop {
                input.clear();
                match stdin.lock().read_line(&mut input) {
                    Ok(0) | Err(_) => exit(0),
                    Ok(_) => {
                        if input.trim() == "exit" {
                            line("bye");
                            exit(0);
                        }
                    }
                }
            }
        }
        // Announces itself and then never reads input: the hint falls on
        // deaf ears and the close path must escalate.
        "deaf" => {
            line("ready");
            std::thread::sleep(IDLE_LIMIT);
            exit(0)
        }
        // Raw mode: the interrupt byte arrives as a byte and is survived,
        // exactly as an interactive CLI would take it. Reports each byte
        // so a human debugging a failed run can see what arrived.
        "raw" => {
            let applied = terminal::set_raw();
            line(&format!("ready terminal={applied}"));
            let deadline = std::time::Instant::now() + IDLE_LIMIT;
            while std::time::Instant::now() < deadline {
                if let Some(byte) = terminal::next_byte(Duration::from_millis(100)) {
                    line(&format!("byte=0x{byte:02x}"));
                    if byte == b'q' {
                        exit(0);
                    }
                }
            }
            exit(0)
        }
        // Grows a process tree on command, reporting the descendant's pid
        // through the named file, then waits to be cleaned up along with
        // it.
        "tree" => {
            let pid_file = args.first().cloned().unwrap_or_default();
            line("ready");
            let stdin = std::io::stdin();
            let mut input = String::new();
            loop {
                input.clear();
                match stdin.lock().read_line(&mut input) {
                    Ok(0) | Err(_) => idle_out(),
                    Ok(_) => match input.trim() {
                        "t" => {
                            let own = std::env::current_exe()
                                .expect("a fixture must be able to find itself");
                            // Deliberately never waited on: this fixture
                            // exists to be killed along with its
                            // descendant, and reaping it would remove what
                            // the containment scenario goes looking for.
                            #[allow(clippy::zombie_processes)]
                            let grandchild = std::process::Command::new(own)
                                .arg("idle")
                                // Detached from the terminal, so it cannot
                                // hold the stream open after the child is
                                // gone.
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn()
                                .expect("the grandchild must spawn");
                            std::fs::write(&pid_file, grandchild.id().to_string())
                                .expect("the pid file must be writable");
                            line("spawned");
                        }
                        "exit" => exit(0),
                        _ => {}
                    },
                }
            }
        }
        // Sits silently until killed — the grandchild of `tree`.
        "idle" => idle_out(),
        other => {
            line(&format!("error unknown-role={other}"));
            exit(2)
        }
    }
}

fn line(text: &str) {
    println!("{text}");
}

fn exit(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    std::process::exit(code)
}

fn idle_out() -> ! {
    std::thread::sleep(IDLE_LIMIT);
    exit(0)
}

/// Raw-mode input, per platform — trimmed from the terminal crate's own
/// fixture. Raw is the mode an interactive CLI runs in: the terminal stops
/// turning a typed Ctrl+C into a signal, and the byte arrives at the read
/// like any other.
#[cfg(unix)]
mod terminal {
    use std::time::Duration;

    pub fn set_raw() -> String {
        // SAFETY: a zeroed termios is overwritten by `tcgetattr` before it
        // is read; every call takes a descriptor and a pointer to it.
        unsafe {
            let mut mode: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut mode) != 0 {
                return format!("unreadable({})", std::io::Error::last_os_error());
            }
            libc::cfmakeraw(&mut mode);
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &mode) != 0 {
                return format!("unset({})", std::io::Error::last_os_error());
            }
            let mut applied: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut applied) != 0 {
                return "unverified".to_string();
            }
            format!("isig={}", u8::from(applied.c_lflag & libc::ISIG != 0))
        }
    }

    pub fn next_byte(within: Duration) -> Option<u8> {
        let mut watch = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout = i32::try_from(within.as_millis().max(1)).unwrap_or(i32::MAX);
        // SAFETY: one descriptor, and the array really is one element long.
        let ready = unsafe { libc::poll(&mut watch, 1, timeout) };
        if ready <= 0 {
            return None;
        }
        let mut byte = 0u8;
        // SAFETY: reads at most one byte into a one-byte buffer.
        let read = unsafe { libc::read(libc::STDIN_FILENO, (&raw mut byte).cast(), 1) };
        (read == 1).then_some(byte)
    }
}

#[cfg(windows)]
mod terminal {
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, GetConsoleMode, GetStdHandle,
        INPUT_RECORD, KEY_EVENT, ReadConsoleInputW, STD_INPUT_HANDLE, SetConsoleMode,
    };
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

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

    pub fn set_raw() -> String {
        let handle = input();
        let requested =
            mode(handle) & !(ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT);
        // SAFETY: a live console handle and a plain mode word.
        if unsafe { SetConsoleMode(handle, requested) } == 0 {
            return format!("unset({})", std::io::Error::last_os_error());
        }
        format!(
            "processed={}",
            u8::from(mode(handle) & ENABLE_PROCESSED_INPUT != 0)
        )
    }

    pub fn next_byte(within: Duration) -> Option<u8> {
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
            if record.EventType as u32 != KEY_EVENT {
                return None;
            }
            let key = record.Event.KeyEvent;
            // Key-up repeats what key-down reported; a record with no
            // character is a bare modifier.
            if key.bKeyDown == 0 || key.uChar.UnicodeChar == 0 {
                return None;
            }
            u8::try_from(key.uChar.UnicodeChar).ok()
        }
    }
}
