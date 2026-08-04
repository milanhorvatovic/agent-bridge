//! One measured session: a child under a real PTY, its output arriving as
//! timestamped chunks, and a teardown that ends the same way whether the run
//! passed or failed.
//!
//! The plumbing itself is the interactive probe's — the same allocation,
//! reader, and teardown every other probe uses, with the ConPTY hazards
//! already guarded. Measuring a private copy of the read path would measure
//! the wrong path.
//!
//! Two things here are specific to measurement:
//!
//! **The terminal is deliberately wide.** ConPTY re-renders its child's
//! output rather than piping it, and re-rendering hard-wraps at the terminal
//! width. A generated line wrapped mid-payload cannot be checked against the
//! line it was generated from, so the terminal is allocated far wider than
//! any line the lanes emit and the wrap never happens. That is a measurement
//! decision, not a default: it is why "no corruption" from this probe means
//! "no corruption", rather than "no corruption we could distinguish from
//! reflow".
//!
//! **The child is found, not invoked through cargo.** Every lane spawns the
//! binary directly — a `cargo run` in the middle of a latency measurement
//! would put cargo's own startup inside the numbers.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent_bridge_interactive_probe::pty::{
    EndInfo, ReaderEvent, SharedWriter, alloc_pty, force_kill, spawn_reader, teardown,
};
use agent_bridge_interactive_probe::rig::child_env_defaults;
use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair};

/// Terminal dimensions for the generated-stream lanes. Wide enough that no
/// line they emit comes close to wrapping under a re-rendering terminal; see
/// the module note. The replay lane overrides these with the dimensions its
/// recording was captured at, so a re-rendering terminal reflows the recorded
/// escape sequences onto the screen they were painted for.
pub const COLS: u16 = 200;
pub const ROWS: u16 = 50;

/// How long any single blocking step — allocation, teardown, a child's exit —
/// may take before it is reported as a failure rather than waited on. The
/// measured runs themselves have their own, much longer, budgets.
pub const STEP_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Session {
    pub events: mpsc::Receiver<ReaderEvent>,
    pub writer: SharedWriter,
    master: Option<Box<dyn MasterPty + Send>>,
    child: Box<dyn Child>,
    end: Option<EndInfo>,
}

impl Session {
    /// Spawn `argv` under a fresh PTY of the given size, with the runtime's
    /// child-environment defaults, and start draining the master.
    pub fn spawn(argv: &[OsString], cols: u16, rows: u16) -> Result<Self, String> {
        let (PtyPair { master, slave }, _alloc_ms) = alloc_pty(cols, rows, STEP_TIMEOUT)?;
        let mut command = CommandBuilder::new(&argv[0]);
        command.args(&argv[1..]);
        for (key, value) in child_env_defaults(cols, rows) {
            command.env(key, value);
        }
        let child = slave
            .spawn_command(command)
            .map_err(|err| format!("child spawn failed: {err:#}"))?;
        // Release our copy of the child end: holding it open would keep the
        // master from ever seeing end-of-stream after the child exits.
        drop(slave);

        let reader = master
            .try_clone_reader()
            .map_err(|err| format!("cloning the master reader failed: {err:#}"))?;
        let writer = SharedWriter::new(
            master
                .take_writer()
                .map_err(|err| format!("taking the master writer failed: {err:#}"))?,
        );
        let events = spawn_reader(reader, writer.clone(), Arc::new(AtomicU32::new(0)));
        Ok(Self {
            events,
            writer,
            master: Some(master),
            child,
            end: None,
        })
    }

    /// Note that the reader reached end-of-stream, so teardown does not wait
    /// for an end it has already been handed.
    pub fn note_end(&mut self, end: EndInfo) {
        self.end.get_or_insert(end);
    }

    /// One tick of a read loop: the next chunk, a quiet tick, or the end of
    /// the stream. Lanes must complete on their *own* expectations rather
    /// than on `Ended` — a terminal that re-renders (ConPTY) reports no
    /// end-of-stream until the master closes, so a loop that waited for the
    /// end would sit out its stall guard on one platform with the run
    /// already complete. That is not a theoretical hazard: it is exactly
    /// how the first Windows CI run of these lanes failed.
    pub fn pump(&mut self, tick: Duration) -> Result<Pump, String> {
        match self.events.recv_timeout(tick) {
            Ok(ReaderEvent::Data { at, bytes }) => Ok(Pump::Data { at, bytes }),
            Ok(ReaderEvent::End(info)) => {
                self.note_end(info);
                Ok(Pump::Ended)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(Pump::Quiet),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("the reader ended without reporting end-of-stream".to_string())
            }
        }
    }

    /// Whether the child has exited, without blocking on it. `None` means
    /// its state could not be read, which proves neither alive nor exited.
    pub fn exited(&mut self) -> Option<bool> {
        self.child.try_wait().ok().map(|status| status.is_some())
    }

    /// Wait for the child to exit on its own, then close the terminal.
    /// The normal end of a lane whose child finishes its script.
    pub fn finish(mut self) -> Result<String, String> {
        let child_detail = wait_for_exit(self.child.as_mut(), STEP_TIMEOUT);
        let master = self.master.take().expect("the master outlives the session");
        let teardown_detail = teardown(master, &self.events, self.end.take(), STEP_TIMEOUT, None)?;
        Ok(format!("{child_detail}; {teardown_detail}"))
    }

    /// End a run the probe is stopping — a soak that reached its deadline
    /// with the child still streaming. The child is killed and the kill is
    /// confirmed by reaping, because a probe reports what happened rather
    /// than assuming a signal worked.
    pub fn stop(mut self) -> Result<String, String> {
        let kill_detail = force_kill(self.child.as_mut());
        let master = self.master.take().expect("the master outlives the session");
        let teardown_detail = teardown(master, &self.events, self.end.take(), STEP_TIMEOUT, None)?;
        Ok(format!("{kill_detail}; {teardown_detail}"))
    }
}

/// Poll the child to its exit against a deadline, killing it if it overruns.
/// A blocking `wait()` is a known ConPTY hang, so it is never called.
fn wait_for_exit(child: &mut dyn Child, timeout: Duration) -> String {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return format!(
                    "child exited with code {} after {} ms",
                    status.exit_code(),
                    started.elapsed().as_millis()
                );
            }
            Ok(None) if started.elapsed() >= timeout => {
                return format!(
                    "child still running after {} s; {}",
                    timeout.as_secs(),
                    force_kill(child)
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => return format!("child wait failed: {err}"),
        }
    }
}

pub enum Pump {
    Data { at: Instant, bytes: Vec<u8> },
    Quiet,
    Ended,
}

/// How often a lane's read loop wakes to check its watch when nothing is
/// arriving.
pub const PUMP_TICK: Duration = Duration::from_millis(250);

/// How long a stream must stay quiet after its child exits before the lane
/// treats the run as over. Short: an exited child's buffered output flushes
/// promptly, and everything this fallback ends was going to end at a stall
/// guard otherwise.
pub const EXIT_QUIET_GRACE: Duration = Duration::from_secs(2);

/// The end-of-run judgement for a lane whose completion condition might
/// never be met — a faulty stream, a lost tail. The stream is over when the
/// child has exited and nothing has arrived for a grace period; how long the
/// stream has been silent overall is tracked for the lane's stall guard.
pub struct EndWatch {
    last_data: Instant,
    exit_quiet_since: Option<Instant>,
}

impl Default for EndWatch {
    fn default() -> Self {
        Self::new()
    }
}

impl EndWatch {
    pub fn new() -> Self {
        Self {
            last_data: Instant::now(),
            exit_quiet_since: None,
        }
    }

    /// Data arrived: the stream is alive, whatever the child's state.
    pub fn data(&mut self) {
        self.last_data = Instant::now();
        self.exit_quiet_since = None;
    }

    /// Call on a quiet tick: whether the run should be treated as ended —
    /// the child has exited and the stream stayed quiet past the grace.
    pub fn ended(&mut self, session: &mut Session) -> bool {
        if session.exited() == Some(true) {
            let since = *self.exit_quiet_since.get_or_insert_with(Instant::now);
            since.elapsed() >= EXIT_QUIET_GRACE
        } else {
            self.exit_quiet_since = None;
            false
        }
    }

    /// How long since anything arrived — the lane's stall guard reads this.
    pub fn since_data(&self) -> Duration {
        self.last_data.elapsed()
    }
}

/// A binary built into the same profile directory as this one — cargo puts
/// every workspace binary side by side. A test executable runs out of the
/// `deps` subdirectory of that same profile directory, so one level of
/// `deps` is stepped over; nothing else is searched, because "which binary
/// is being measured" is not a question a probe should answer heuristically.
pub fn sibling_binary(name: &str) -> Result<PathBuf, String> {
    let me = std::env::current_exe().map_err(|err| format!("current_exe failed: {err}"))?;
    let mut dir = me
        .parent()
        .ok_or_else(|| "current_exe has no parent directory".to_string())?;
    if dir.ends_with("deps") {
        dir = dir
            .parent()
            .ok_or_else(|| "the deps directory has no parent".to_string())?;
    }
    let path = dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    if path.exists() {
        Ok(path)
    } else {
        Err(format!(
            "binary not found at {} — build the workspace first",
            path.display()
        ))
    }
}

/// A scenario written for one run and deleted with it.
///
/// The lanes ask for streams no one would hand-author — a half-hour of
/// traffic, ten thousand round trips — so the probe writes the scenario that
/// produces them. Which is also why the duration lives here rather than in
/// the fake CLI: a scenario whose length depended on how long it happened to
/// run would forfeit the determinism the rest of the corpus is built on.
pub struct ScenarioFile {
    path: PathBuf,
}

impl ScenarioFile {
    pub fn write(name: &str, json: &str) -> Result<Self, String> {
        // Uniqueness needs the process *and* a per-call counter: lanes run
        // concurrently inside one process (a test harness, a background
        // load), and two lanes sharing a path means one lane's child quietly
        // runs the other lane's script.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "agent-bridge-perf-{}-{serial}-{name}.json",
            std::process::id()
        ));
        std::fs::write(&path, json).map_err(|err| format!("{}: {err}", path.display()))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The argv that runs this scenario under the fake CLI.
    pub fn argv(&self) -> Result<Vec<OsString>, String> {
        Ok(vec![
            sibling_binary("fake-cli")?.into_os_string(),
            self.path.as_os_str().to_os_string(),
        ])
    }
}

impl Drop for ScenarioFile {
    fn drop(&mut self) {
        // A leftover scenario file is harmless but untidy, and a failure to
        // remove it must not mask the failure that is already being reported.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// A JSON string literal — scenario text is assembled, and a payload with an
/// unescaped quote in it would produce a scenario that fails to parse for a
/// reason nobody would guess from the message.
pub fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scenario_file_is_removed_with_the_run() {
        let path = {
            let scenario = ScenarioFile::write("drop-test", "{}").expect("write must succeed");
            let path = scenario.path().to_path_buf();
            assert!(path.exists());
            path
        };
        assert!(!path.exists(), "the scenario file must not outlive its run");
    }

    #[test]
    fn json_strings_escape_what_would_break_the_scenario() {
        assert_eq!(json_string("mark {ts}\n"), "\"mark {ts}\\n\"");
        assert_eq!(json_string("say \"hi\"\\"), "\"say \\\"hi\\\"\\\\\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn scenario_text_round_trips_through_a_json_parser() {
        // The lanes assemble scenario text rather than serialising it, so
        // the escaping above is load-bearing: this is the test that would
        // catch it drifting from what a parser accepts.
        let text = format!(
            r#"{{"name":"t","steps":[{{"emit":{},"channel":"stdout"}},{{"exit":0}}]}}"#,
            json_string("mark {ts}\n\"quoted\"\t\u{1}")
        );
        let parsed = agent_bridge_fake_cli::scenario::parse(&text)
            .unwrap_or_else(|err| panic!("assembled scenario must parse: {err}"));
        assert_eq!(parsed.steps.len(), 2);
    }
}
