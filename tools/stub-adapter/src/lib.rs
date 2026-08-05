//! The stub adapter: the launch path in its smallest honest form.
//!
//! [`run_scenario`] launches the scripted stand-in CLI (`fake-cli`) on a
//! scenario file as a real child process, drains both output streams, and
//! reports how the run ended. That is the whole surface, and the smallness
//! is deliberate: the adapter *interface* — how the runtime will describe
//! launching, watching, and shutting down a CLI — is a design decision that
//! belongs to the runtime work ahead, and a stub that committed to a trait
//! now would quietly pre-commit that decision. A bare function cannot. What
//! it proves, under CI on all three OSes, is the end-to-end launch path:
//! the scripted CLI spawns, streams, and exits the same way everywhere.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long a scenario child may run before it is killed and reported as a
/// failure. The committed scenarios finish in milliseconds; the deadline
/// exists for the misbehaving case — a child that stalls or never exits
/// must fail this lane in seconds, with the scenario named, not burn a CI
/// job's whole wall-clock budget. Same discipline (and the same order of
/// magnitude) as the deadlines the probe lanes enforce.
const SCENARIO_DEADLINE: Duration = Duration::from_secs(30);

/// How often the deadline loop checks whether the child has exited.
const EXIT_POLL: Duration = Duration::from_millis(20);

/// How a scenario run ended: the child's exit status and how many bytes it
/// wrote on each stream. A summary, not an event contract — the structured
/// event stream is the events crate's territory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitReport {
    /// The child's exit code, when it exited with one (`None` when it was
    /// terminated by a signal).
    pub exit_code: Option<i32>,
    /// Bytes the child wrote to stdout — the scripted byte surface.
    pub stdout_bytes: u64,
    /// Bytes the child wrote to stderr — diagnostics only.
    pub stderr_bytes: u64,
}

impl ExitReport {
    /// Did the child exit cleanly (exit code 0)?
    pub fn clean(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// Run one scenario file through the fake CLI and report how it ended.
///
/// The child gets a closed stdin: the committed starter scenarios script
/// output and exit only, and a scenario that awaits input under this stub
/// fails its await — visibly, in the report — rather than hanging. A child
/// that neither exits nor fails within [`SCENARIO_DEADLINE`] is killed and
/// reported as an error naming the scenario: nothing this function does is
/// unbounded, so a misbehaving scenario can never wedge the CI lane that
/// runs it.
pub fn run_scenario(scenario: &Path) -> Result<ExitReport, String> {
    let mut child = Command::new(fake_cli_path()?)
        .arg(scenario)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("spawning fake-cli on {} failed: {err}", scenario.display()))?;

    // Both streams drain on their own threads: with both piped, reading
    // them sequentially deadlocks the moment the un-read pipe's buffer
    // fills — and this thread must stay free to hold the child to its
    // deadline, which a blocking drain or a blocking wait() could not.
    let mut stdout = child.stdout.take().expect("stdout was requested piped");
    let stdout_reader = std::thread::spawn(move || count_bytes(&mut stdout));
    let mut stderr = child.stderr.take().expect("stderr was requested piped");
    let stderr_reader = std::thread::spawn(move || count_bytes(&mut stderr));

    // Poll for exit against the deadline. On expiry, kill the child and
    // reap it — the kill closes its pipes, so the drain threads finish and
    // the joins below cannot hang — then report the timeout as the error,
    // with the scenario named.
    let deadline = Instant::now() + SCENARIO_DEADLINE;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                timed_out = true;
                let _ = child.kill();
                break child.wait();
            }
            Ok(None) => std::thread::sleep(EXIT_POLL),
            Err(err) => {
                // The child's state is unknowable; kill so an error return
                // cannot leave a stray process, and reap what remains.
                let _ = child.kill();
                let _ = child.wait();
                break Err(err);
            }
        }
    };
    let stdout_result = stdout_reader.join();
    let stderr_result = stderr_reader.join();
    if timed_out {
        return Err(format!(
            "fake-cli on {} exceeded the {}s deadline and was killed",
            scenario.display(),
            SCENARIO_DEADLINE.as_secs()
        ));
    }

    let status = status.map_err(|err| {
        format!(
            "waiting for fake-cli on {} failed: {err}",
            scenario.display()
        )
    })?;
    let stdout_bytes = stdout_result
        .map_err(|_| "the stdout reader thread panicked".to_owned())?
        .map_err(|err| {
            format!(
                "reading fake-cli stdout for {} failed: {err}",
                scenario.display()
            )
        })?;
    let stderr_bytes = stderr_result
        .map_err(|_| "the stderr reader thread panicked".to_owned())?
        .map_err(|err| {
            format!(
                "reading fake-cli stderr for {} failed: {err}",
                scenario.display()
            )
        })?;
    Ok(ExitReport {
        exit_code: status.code(),
        stdout_bytes,
        stderr_bytes,
    })
}

fn count_bytes(reader: &mut dyn Read) -> std::io::Result<u64> {
    let mut sink = CountingSink::default();
    std::io::copy(reader, &mut sink)?;
    Ok(sink.bytes)
}

/// A write sink that counts and discards. The stub reports byte counts, not
/// content: content assertions belong to the golden traces, and buffering a
/// stream the stub does not inspect would only add an unbounded allocation.
#[derive(Default)]
struct CountingSink {
    bytes: u64,
}

impl std::io::Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.bytes += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The fake-cli binary, found next to the running artifact: a binary runs
/// from `target/<profile>/`, a test executable from `target/<profile>/deps/`
/// — the standard sibling-binary lookup, with a build hint when it is
/// missing.
fn fake_cli_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|err| format!("current_exe failed: {err}"))?;
    let mut dir = exe
        .parent()
        .ok_or_else(|| "current_exe has no parent directory".to_owned())?
        .to_path_buf();
    if dir.file_name().is_some_and(|name| name == "deps") {
        dir.pop();
    }
    let path = dir.join(format!("fake-cli{}", std::env::consts::EXE_SUFFIX));
    if !path.is_file() {
        return Err(format!(
            "{} not found — build it first: cargo build -p agent-bridge-fake-cli",
            path.display()
        ));
    }
    Ok(path)
}
