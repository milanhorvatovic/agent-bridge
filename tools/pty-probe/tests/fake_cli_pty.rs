//! The PTY-hosted scenario case: the deterministic fake CLI's cold-start
//! scenario runs under a real PTY (ConPTY on Windows), and its scripted
//! banner and prompt must arrive through the master. The pipe-based smoke
//! driver in the fake CLI's own tests proves the script; this test proves
//! the script survives the surface the runtime will actually host it on —
//! closing the loop between the PTY probe and the conformance corpus from
//! both sides.
//!
//! Assertions are substring-based, not byte-exact, on purpose: the PTY layer
//! is entitled to translate output (LF becomes CRLF under ONLCR, ConPTY
//! brackets output with escape sequences), and what this test owns is that
//! the scripted bytes arrive, in order, through a real PTY.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, MasterPty, PtyPair, PtySize, native_pty_system};

const BANNER: &str = "fake-cli: session ready";
const PROMPT: &str = "> ";
const TIMEOUT: Duration = Duration::from_secs(15);

#[test]
fn pty_hosted_cold_start() {
    let fake_cli = build_fake_cli();
    let scenario = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus/fake/cold-start/scenario.json")
        .canonicalize()
        .expect("the cold-start corpus scenario must exist");

    let pair = alloc_pty_guarded();
    let mut command = CommandBuilder::new(&fake_cli);
    command.arg(&scenario);
    let mut child = pair
        .slave
        .spawn_command(command)
        .expect("fake-cli must spawn under the pty");
    // Release our copy of the child end: holding it open would keep the
    // master from ever seeing end-of-stream after the child exits.
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .expect("cloning the master reader must succeed");
    let writer = pair
        .master
        .take_writer()
        .expect("taking the master writer must succeed");
    let chunks = spawn_reader(reader, writer);

    // Collect output until the banner and, after it, the prompt have both
    // arrived — never draining to end-of-stream first, because on Windows
    // the master may only report the end once it is closed.
    let deadline = Instant::now() + TIMEOUT;
    let mut output: Vec<u8> = Vec::new();
    loop {
        let text = String::from_utf8_lossy(&output).into_owned();
        if let Some(banner_at) = text.find(BANNER)
            && text[banner_at + BANNER.len()..].contains(PROMPT)
        {
            break;
        }
        let now = Instant::now();
        assert!(
            now < deadline,
            "banner and prompt not observed within {}s; got: {text:?}",
            TIMEOUT.as_secs()
        );
        match chunks.recv_timeout(deadline - now) {
            Ok(chunk) => output.extend_from_slice(&chunk),
            Err(mpsc::RecvTimeoutError::Timeout) => {} // deadline re-checked at loop top
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "the pty stream ended before the scripted output arrived; got: {:?}",
                    String::from_utf8_lossy(&output)
                );
            }
        }
    }

    // The scenario scripts exit 0; poll rather than blocking-wait, since a
    // blocking wait is a known ConPTY hang.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                assert!(
                    Instant::now() < deadline,
                    "fake-cli did not exit within {}s of emitting its script",
                    TIMEOUT.as_secs()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => panic!("waiting for fake-cli failed: {err}"),
        }
    };
    assert!(
        status.success(),
        "the scenario scripts a clean exit, got {status:?}"
    );

    // Close the master and prove the reader observes end-of-stream — the
    // drain also keeps ConPTY's close from deadlocking on unread output.
    close_master_guarded(pair.master);
    let drain_deadline = Instant::now() + TIMEOUT;
    loop {
        match chunks.recv_timeout(drain_deadline.saturating_duration_since(Instant::now())) {
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!(
                    "the reader did not reach end-of-stream within {}s of closing the master",
                    TIMEOUT.as_secs()
                );
            }
        }
    }
}

/// Allocate the PTY on a helper thread, exactly as the probe binary does:
/// `openpty` can hang on Windows when the console subsystem is not yet
/// initialised, and a hang must surface as a failed test with a diagnostic,
/// not a lane stuck until its wall-clock budget expires. On timeout the
/// helper thread is deliberately leaked — the test is about to panic.
fn alloc_pty_guarded() -> PtyPair {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(native_pty_system().openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        }));
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(Ok(pair)) => pair,
        Ok(Err(err)) => panic!("pty allocation failed: {err:#}"),
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "pty allocation did not complete within {}s{}",
            TIMEOUT.as_secs(),
            if cfg!(windows) {
                " — matches the known ConPTY hang when the console subsystem is uninitialised"
            } else {
                ""
            }
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("pty allocation thread died without a result")
        }
    }
}

/// Close the master on a helper thread, exactly as the probe binary does:
/// `ClosePseudoConsole` can deadlock when buffered output has no reader
/// draining it (microsoft/terminal#1810), and a deadlock must become a
/// diagnosed failure, not a hung lane. On timeout the helper thread is
/// deliberately leaked — the test is about to panic.
fn close_master_guarded(master: Box<dyn MasterPty + Send>) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        drop(master);
        let _ = tx.send(());
    });
    // The two failure shapes are kept distinct: a timeout points at the
    // known deadlock, a dead helper thread points at a panic inside the
    // close itself — conflating them would send a debugger down the wrong
    // path.
    match rx.recv_timeout(TIMEOUT) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!(
            "closing the pty master did not complete within {}s{}",
            TIMEOUT.as_secs(),
            if cfg!(windows) {
                " — matches the known ClosePseudoConsole deadlock (microsoft/terminal#1810)"
            } else {
                ""
            }
        ),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the master-close thread died without reporting completion")
        }
    }
}

/// Build the fake CLI through the same cargo that runs this test, so the
/// binary under the PTY is always the one from the commit under test — a
/// stale artifact passing would be worse than a missing one failing. The
/// profile must match this test's own, or a `--release` (or custom-profile)
/// run would probe a binary from a different directory than it builds into.
fn build_fake_cli() -> PathBuf {
    let mut profile_dir = std::env::current_exe().expect("the test executable has a path");
    profile_dir.pop(); // the test executable's file name
    if profile_dir.ends_with("deps") {
        profile_dir.pop();
    }
    // Derive the profile from the directory this test runs out of and pass
    // it through generically, so `cargo test --profile <name>` builds the
    // binary into the same directory the test then loads it from. The one
    // naming quirk is cargo's own: the `dev` profile (and the `test`
    // profile that inherits it) outputs into `target/debug`.
    let dir_name = profile_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the profile directory has a UTF-8 name");
    let profile = if dir_name == "debug" { "dev" } else { dir_name };

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = Command::new(cargo);
    build.args([
        "build",
        "--quiet",
        "--package",
        "agent-bridge-fake-cli",
        "--profile",
        profile,
    ]);
    let status = build.status().expect("cargo must be runnable");
    assert!(status.success(), "building the fake CLI failed: {status}");

    let binary = profile_dir.join(format!("fake-cli{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.is_file(),
        "built fake-cli not found at {}",
        binary.display()
    );
    binary
}

/// Read the master on a dedicated thread, forwarding chunks over a channel
/// and closing it at end-of-stream. Mirrors the probe binary's reader in the
/// one behavior that matters here: it answers ConPTY's cursor-position query
/// (`ESC[6n`, emitted at startup, and blocking the child until a reply
/// arrives), scanning across chunk boundaries because the query can arrive
/// split.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    mut writer: Box<dyn Write + Send>,
) -> mpsc::Receiver<Vec<u8>> {
    const CURSOR_QUERY: &[u8] = b"\x1b[6n";
    const CURSOR_REPLY: &[u8] = b"\x1b[1;1R";
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut scan_tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    let chunk = &buf[..n];
                    let mut scan = std::mem::take(&mut scan_tail);
                    scan.extend_from_slice(chunk);
                    for window in scan.windows(CURSOR_QUERY.len()) {
                        if window == CURSOR_QUERY {
                            let _ = writer.write_all(CURSOR_REPLY).and_then(|()| writer.flush());
                        }
                    }
                    scan_tail = scan[scan.len().saturating_sub(CURSOR_QUERY.len() - 1)..].to_vec();
                    if tx.send(chunk.to_vec()).is_err() {
                        return; // the test gave up on this stream
                    }
                }
                // A signal can cut a blocking read short; resume.
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                // A master read on a closed PTY surfaces as an error on some
                // platforms (EIO on Linux) rather than a 0-byte read; both
                // mean the stream ended.
                Err(_) => return,
            }
        }
    });
    rx
}
