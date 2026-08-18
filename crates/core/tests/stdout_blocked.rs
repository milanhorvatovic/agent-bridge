//! The die-loudly exit sequence demonstrated at process level: a child whose
//! parent never reads its stdout pipe floods a [`BoundedWriter`] wired to
//! that pipe, and must end in the die-loudly order — one fatal, a sealed
//! writer, a deliberate exit — rather than a wedge. Runs on all three
//! OSes because pipe semantics (capacities, blocking behavior) differ
//! enough to warrant it; the precise state-machine timing lives in the
//! paused-clock unit tests.
//!
//! The child is this same test binary re-invoked with a marker variable —
//! the test re-enters itself — so no extra binary rides the crate's
//! dependency graph for one test's sake.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

use agent_bridge_core::{BoundedWriter, WriterConfig, WriterError};
use bytes::Bytes;

const CHILD_ENV: &str = "AGENT_BRIDGE_STDOUT_BLOCKED_CHILD";

/// The exit code the child reserves for "die-loudly ran to completion";
/// anything else is a diagnosis (0: the fatal never fired and the child's
/// own watchdog gave up — the wedge this test exists to forbid).
const DIED_LOUDLY: i32 = 23;

#[test]
fn stdout_blocked_subprocess_die_loudly() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
    }

    let mut child = Command::new(std::env::current_exe().expect("the test binary's own path"))
        .arg("--exact")
        .arg("stdout_blocked_subprocess_die_loudly")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the child half");

    // The whole point: never read stdout while the child runs. The child
    // must exit on its own — die-loudly, not the parent's mercy — but the
    // parent still carries a deadline of its own: a regression in exactly
    // the behavior under test must fail this test, not hang it, and the
    // child's internal watchdog only helps while its runtime can still be
    // scheduled.
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll the child") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the child neither died loudly nor gave up: wedged past the parent deadline");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        status.code(),
        Some(DIED_LOUDLY),
        "the child must exit deliberately after die-loudly, not wedge or give up"
    );

    // Post-mortem: the pipe still holds the flood this parent never took,
    // and the child said what happened exactly once on stderr.
    let mut flood = Vec::new();
    child
        .stdout
        .take()
        .expect("piped above")
        .read_to_end(&mut flood)
        .expect("drain the abandoned pipe");
    assert!(!flood.is_empty(), "the flood never reached the pipe");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("piped above")
        .read_to_string(&mut stderr)
        .expect("read the child's stderr");
    assert_eq!(
        stderr.matches("STDOUT_BLOCKED_FATAL").count(),
        1,
        "exactly one fatal, ever; child stderr:\n{stderr}"
    );
}

/// The child half: flood our own stdout — the pipe the parent abandoned —
/// and encode the die-loudly outcome in the exit code.
fn run_child() -> ! {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("a current-thread runtime for the drain task");
    let code = runtime.block_on(async {
        let (writer, mut fatal) = BoundedWriter::new(
            tokio::io::stdout(),
            WriterConfig {
                capacity_bytes: 64 * 1024,
                drain_deadline: Duration::from_millis(500),
                farewell: Bytes::from_static(b"<transport.error stdout_blocked>\n"),
            },
        );
        // Well past any OS pipe capacity (Windows anonymous pipes are the
        // smallest, Linux/macOS default 64 KiB), so the sink must stall.
        let frame = Bytes::from(vec![b'x'; 1024]);
        for _ in 0..1024 {
            // Death mid-flood is death observed; stop feeding it.
            if writer.enqueue(frame.clone()) == Err(WriterError::Sealed) {
                break;
            }
        }
        // The child's own watchdog: if die-loudly never fires, exit 0 and
        // let the parent's assertion name the wedge.
        match tokio::time::timeout(Duration::from_secs(30), fatal.fired()).await {
            Ok(()) => {
                if writer.enqueue(frame) != Err(WriterError::Sealed) {
                    eprintln!("writer accepted a frame after die-loudly");
                    return 3;
                }
                eprintln!("STDOUT_BLOCKED_FATAL");
                DIED_LOUDLY
            }
            Err(_) => 0,
        }
    });
    std::process::exit(code);
}
