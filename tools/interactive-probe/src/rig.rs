//! The launch rig: everything needed to put a real Claude Code interactive
//! session under this probe's PTY and drive it, packaged as reusable pieces
//! (later probes consume this rig for detection, cleanup, and workload
//! recording).
//!
//! The load-bearing part is **environment hygiene**. The child environment
//! is composed, never inherited: a short allowlist carried from the parent
//! plus the terminal defaults, and nothing else. Claude Code sets
//! `CLAUDE_CODE_CHILD_SESSION=1` (with `CLAUDECODE=1`) in sessions nested
//! under another Claude Code, and a child launched with that marker leaked
//! into its environment silently stops persisting its transcript — verified
//! single-variable behavior on Claude Code 2.1.x. Composing from an
//! allowlist makes the strip structural; on Linux the probe additionally
//! reads `/proc/<pid>/environ` back as direct proof, and the
//! transcript-liveness step is the behavioral proof everywhere.
//!
//! Launch contract: `claude --session-id <uuid> --settings <hooks.json>`,
//! spawned at 80×24 in a **fresh temporary project directory** so the
//! first-run workspace-trust dialog appears deterministically; the rig
//! answers it with Enter, driven from the screen text — there is no
//! structured channel before the session starts, so the screen is the only
//! fallback, exercised here on purpose.
//!
//! Once a child exists it is torn down on every path out of the lane, by
//! `finish` (graceful `/exit`, falling back to a kill) or `abandon` (kill
//! outright). A probe that leaves an interactive CLI running behind a failed
//! assertion holds a session, and in CI a quota, for the rest of the job.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtyPair};

use crate::capture::CaptureWriter;
use crate::firsttoken::FirstTokenClock;
use crate::hooks::{HookEvent, HookListener, hook_command, settings_json, start_listener};
use crate::pty::{OutputTracker, SharedWriter, alloc_pty, spawn_reader, teardown, wait_child};
use crate::{COLS, Failure, ROWS, print_step};

/// How long a typed line rests between its text and the Enter keystroke, so
/// the TUI's input loop registers the text first — typing text and Enter in
/// one write reliably loses the text to an interactive composer.
pub const TYPE_SETTLE: Duration = Duration::from_millis(350);

/// How long the workspace-trust dialog gets to finish painting before the
/// rig answers it.
const TRUST_DIALOG_SETTLE: Duration = Duration::from_millis(750);

/// How long to wait before pressing Enter at a pre-session screen again.
const TRUST_DIALOG_RETRY: Duration = Duration::from_secs(4);

/// How many Enters a pre-session screen gets before the rig concludes it is
/// not the kind of screen Enter dismisses.
const TRUST_DIALOG_MAX_ENTERS: u32 = 5;

/// Markers that say the screen is waiting on a keypress rather than
/// streaming. Deliberately loose: the rig only needs to know "some dialog is
/// up", and the exact words are the CLI's to change.
const DIALOG_MARKERS: [&str; 4] = ["trust", "yes, proceed", "do you trust", "press enter"];

const IO_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_START_TIMEOUT: Duration = Duration::from_secs(60);
const TRANSCRIPT_TIMEOUT: Duration = Duration::from_secs(15);
pub const TURN_TIMEOUT: Duration = Duration::from_secs(120);
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(20);
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(15);

/// The env defaults every child spawned under the probe's PTY receives.
/// Without them, interactive CLIs degrade: "dumb"-terminal mode, disabled
/// color, or broken UTF-8.
pub fn child_env_defaults(cols: u16, rows: u16) -> Vec<(&'static str, String)> {
    // macOS ships no C.UTF-8 locale; en_US.UTF-8 is its UTF-8-forcing
    // equivalent.
    let locale = if cfg!(target_os = "macos") {
        "en_US.UTF-8"
    } else {
        "C.UTF-8"
    };
    vec![
        ("TERM", "xterm-256color".to_string()),
        ("LC_ALL", locale.to_string()),
        ("LANG", locale.to_string()),
        ("COLUMNS", cols.to_string()),
        ("LINES", rows.to_string()),
        ("COLORTERM", "truecolor".to_string()),
    ]
}

/// Parent variables the child keeps: what a CLI needs to run and
/// authenticate, nothing else. Everything outside this list — including
/// every nested-session marker — is stripped by construction.
const CARRIED_FROM_PARENT: &[&str] = &[
    "HOME",
    "PATH",
    "SHELL",
    "USER",
    "TMPDIR",
    // Auth resolution for the live lane: a config-dir override on dev
    // machines, an API key in CI. Carrying them is what makes "hygienic"
    // different from "sterile".
    "CLAUDE_CONFIG_DIR",
    "ANTHROPIC_API_KEY",
];

/// What a process additionally cannot live without on Windows.
#[cfg(windows)]
const CARRIED_FROM_PARENT_WINDOWS: &[&str] = &[
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "USERNAME",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
];

fn is_carried(name: &str) -> bool {
    // Windows environment names are case-insensitive; POSIX names are not.
    #[cfg(windows)]
    {
        CARRIED_FROM_PARENT
            .iter()
            .chain(CARRIED_FROM_PARENT_WINDOWS)
            .any(|carried| carried.eq_ignore_ascii_case(name))
    }
    #[cfg(not(windows))]
    {
        CARRIED_FROM_PARENT.contains(&name)
    }
}

/// Compose the child environment: allowlisted parent variables, then the
/// terminal defaults on top (defaults win). The parent snapshot is a
/// parameter, not read here, so tests drive composition with planted
/// pollution instead of mutating the process environment.
pub fn compose_child_env(
    cols: u16,
    rows: u16,
    parent: impl IntoIterator<Item = (String, String)>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = parent
        .into_iter()
        .filter(|(name, _)| is_carried(name))
        .collect();
    for (name, value) in child_env_defaults(cols, rows) {
        match env.iter_mut().find(|(existing, _)| existing == name) {
            Some(slot) => slot.1 = value,
            None => env.push((name.to_string(), value)),
        }
    }
    env
}

/// The markers whose presence in the child would break it (transcript
/// suppression) or reveal a leaky spawn path. Used by the direct
/// verification on Linux; the composition above makes them impossible to
/// carry in the first place, which is what the unit test pins.
#[cfg(any(target_os = "linux", test))]
fn forbidden_env_name(name: &str) -> bool {
    name == "CLAUDECODE"
        || name == "NODE_OPTIONS"
        || name.starts_with("CLAUDE_CODE_")
        || name.starts_with("CMUX_")
}

pub struct ProbeConfig {
    /// Binary name or path. A name is resolved on the parent's PATH; note
    /// that PATH shims (session multiplexers wrap `claude` routinely) can
    /// re-pollute the child, which the Linux environ check would catch —
    /// pass an explicit path to bypass a shim.
    pub claude_bin: String,
    /// Optional `--model` passthrough — the live lanes run cheap models.
    pub model: Option<String>,
    /// The first-token budget (spawn → first PTY byte).
    pub first_token_ms: u64,
    /// Where to persist the capture. Defaults beside the temp dir rather than
    /// inside the session workdir, which teardown deletes.
    pub capture_to: Option<PathBuf>,
    pub keep_workdir: bool,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            claude_bin: "claude".to_string(),
            model: None,
            first_token_ms: 2_000,
            capture_to: None,
            keep_workdir: false,
        }
    }
}

/// A live interactive session under the probe's PTY, with its hook listener
/// attached. Constructed by [`launch`], driven by the lanes.
pub struct LiveSession {
    pub session_id: String,
    pub cli_version: String,
    pub workdir: PathBuf,
    pub project_dir: PathBuf,
    pub listener: HookListener,
    pub writer: SharedWriter,
    pub tracker: OutputTracker,
    pub queries_answered: Arc<AtomicU32>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    hook_log: Vec<HookEvent>,
    keep_workdir: bool,
    first_token_ms: u64,
}

/// What establishing the session (first token → SessionStart → hygiene →
/// transcript baseline) learned.
pub struct SessionInfo {
    pub transcript_path: PathBuf,
    pub transcript_baseline: u64,
    pub trust_dialog_driven: bool,
}

/// One completed prompt turn: Enter → `Stop`.
pub struct Turn {
    pub duration: Duration,
    pub hooks: Vec<(String, serde_json::Value)>,
}

impl Turn {
    pub fn hook_names(&self) -> Vec<&str> {
        self.hooks.iter().map(|(name, _)| name.as_str()).collect()
    }
}

/// Steps `version` → `alloc` → `workspace` → `spawn` (exit codes 30–33).
pub fn launch(config: &ProbeConfig) -> Result<LiveSession, Failure> {
    let binary =
        resolve_binary(&config.claude_bin).map_err(|detail| Failure::new("version", 30, detail))?;
    let cli_version =
        query_version(&binary).map_err(|detail| Failure::new("version", 30, detail))?;
    print_step(
        "version",
        "pass",
        &format!("{} — `{}`", cli_version, binary.display()),
    );

    let (pair, alloc_ms) =
        alloc_pty(COLS, ROWS, IO_TIMEOUT).map_err(|detail| Failure::new("alloc", 31, detail))?;
    print_step(
        "alloc",
        "pass",
        &format!("pty allocated at {COLS}x{ROWS} in {alloc_ms}ms"),
    );
    let PtyPair { master, slave } = pair;

    let session_id = uuid::Uuid::new_v4().to_string();
    let short_id = &session_id[..8];
    let workdir = std::env::temp_dir().join(format!("agent-bridge-interactive-probe-{short_id}"));
    // A *fresh* project directory every run: workspace trust is remembered
    // per directory, and the trust dialog only appears — and is only
    // exercised — when the directory has never been trusted.
    let project_dir = workdir.join("project");
    std::fs::create_dir_all(&project_dir).map_err(|err| {
        Failure::new(
            "workspace",
            32,
            format!("creating {} failed: {err}", project_dir.display()),
        )
    })?;

    let listener = start_listener(&workdir, short_id).map_err(|err| {
        Failure::new(
            "workspace",
            32,
            format!("hook listener failed to start: {err}"),
        )
    })?;
    let probe_exe = std::env::current_exe()
        .map_err(|err| Failure::new("workspace", 32, format!("current_exe failed: {err}")))?;
    let settings_path = workdir.join("hook-settings.json");
    let settings = settings_json(&hook_command(&probe_exe, listener.endpoint()));
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .map_err(|err| Failure::new("workspace", 32, format!("writing settings failed: {err}")))?;
    print_step(
        "workspace",
        "pass",
        &format!(
            "fresh project dir {} (untrusted, so the trust dialog will appear); hooks listening at {}",
            project_dir.display(),
            listener.endpoint()
        ),
    );

    let mut command = CommandBuilder::new(&binary);
    command.args([
        "--session-id",
        &session_id,
        "--settings",
        &settings_path.to_string_lossy(),
    ]);
    if let Some(model) = &config.model {
        command.args(["--model", model]);
    }
    command.cwd(&project_dir);
    // Nothing inherited: the composed allowlist-plus-defaults environment
    // *is* the hygiene guarantee.
    command.env_clear();
    for (key, value) in compose_child_env(COLS, ROWS, std::env::vars()) {
        command.env(key, value);
    }

    let spawned_at = Instant::now();
    let mut child = slave
        .spawn_command(command)
        .map_err(|err| Failure::new("spawn", 33, format!("child spawn failed: {err:#}")))?;
    // Release our copy of the child end: holding it open would keep the
    // master from ever seeing end-of-stream after the child exits.
    drop(slave);

    // From here on a live CLI exists but no `LiveSession` owns it yet, so
    // nothing would tear it down on an early return. Every fallible step
    // below therefore kills the child before propagating.
    let mut kill_child_on = |step: &'static str, detail: String| {
        let killed = crate::pty::force_kill(child.as_mut());
        Failure::new(step, 33, format!("{detail}; the child was {killed}"))
    };

    // Outside the workdir on purpose: the workdir is deleted at teardown,
    // and the capture is a deliverable — a step line advertising a path that
    // no longer exists would be worse than no capture at all.
    let capture_path = config.capture_to.clone().unwrap_or_else(|| {
        std::env::temp_dir().join(format!("agent-bridge-capture-{short_id}.ndjson"))
    });
    let capture = CaptureWriter::create(&capture_path, spawned_at).map_err(|err| {
        kill_child_on("spawn", format!("creating the capture file failed: {err}"))
    })?;

    let reader = master
        .try_clone_reader()
        .map_err(|err| kill_child_on("spawn", format!("cloning the reader failed: {err:#}")))?;
    let writer = SharedWriter::new(
        master
            .take_writer()
            .map_err(|err| kill_child_on("spawn", format!("taking the writer failed: {err:#}")))?,
    );
    let queries_answered = Arc::new(AtomicU32::new(0));
    let events = spawn_reader(reader, writer.clone(), queries_answered.clone());
    let tracker = OutputTracker::new(events, FirstTokenClock::new(spawned_at), Some(capture));

    print_step(
        "spawn",
        "pass",
        &format!(
            "spawned `{}` pid={} session-id={session_id}; capturing to {}",
            binary.display(),
            child
                .process_id()
                .map_or_else(|| "unknown".to_string(), |pid| pid.to_string()),
            capture_path.display(),
        ),
    );

    Ok(LiveSession {
        session_id,
        cli_version,
        workdir,
        project_dir,
        listener,
        writer,
        tracker,
        queries_answered,
        master,
        child,
        hook_log: Vec::new(),
        keep_workdir: config.keep_workdir,
        first_token_ms: config.first_token_ms,
    })
}

impl LiveSession {
    /// Pull everything the listener has received so far into the ordered
    /// hook log, returning the log length — a mark to scan from when
    /// waiting for hooks caused by what the caller does next.
    pub fn hook_mark(&mut self) -> usize {
        while let Ok(event) = self.listener.events.try_recv() {
            self.hook_log.push(event);
        }
        self.hook_log.len()
    }

    /// Hooks observed at or after `mark`.
    pub fn hooks_since(&mut self, mark: usize) -> Vec<(String, serde_json::Value)> {
        self.hook_mark();
        self.hook_log[mark..]
            .iter()
            .map(|event| (event.name.clone(), event.payload.clone()))
            .collect()
    }

    /// Wait for a named hook at or after `mark`, pumping terminal output
    /// while waiting (the TUI streams while hooks fire; both channels must
    /// drain or neither makes progress).
    pub fn wait_for_hook(
        &mut self,
        name: &str,
        mark: usize,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        self.wait_for_hook_where(name, |_| true, mark, timeout)
    }

    /// Wait for a hook named `name` whose payload satisfies `pred` — the
    /// permission `Notification` is one of several notification kinds, so
    /// the name alone does not identify it.
    pub fn wait_for_hook_where(
        &mut self,
        name: &str,
        pred: impl Fn(&serde_json::Value) -> bool,
        mark: usize,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        let deadline = Instant::now() + timeout;
        let mut scanned = mark;
        loop {
            let len = self.hook_mark();
            if let Some(event) = self.hook_log[scanned..len]
                .iter()
                .find(|event| event.name == name && pred(&event.payload))
            {
                return Ok(event.payload.clone());
            }
            scanned = len;
            if Instant::now() >= deadline {
                let seen: Vec<&str> = self.hook_log[mark..]
                    .iter()
                    .map(|event| event.name.as_str())
                    .collect();
                // An ended stream is not by itself a reason to stop waiting —
                // `SessionEnd` legitimately races the child's exit, and its
                // payload can still be in flight over the hook channel. It is
                // worth naming in the diagnostic, though: a hook that never
                // came from a child that died early is a different bug from
                // one that never came from a child still running.
                let ended = self
                    .tracker
                    .stream_ended()
                    .map_or_else(String::new, |reason| {
                        format!("; the output stream had already ended ({reason})")
                    });
                return Err(format!(
                    "hook {name} not observed within {}s (hooks seen since mark: [{}]){ended}; screen tail: '{}'",
                    timeout.as_secs(),
                    seen.join(", "),
                    self.tracker.screen_tail(200),
                ));
            }
            self.idle(Duration::from_millis(100))?;
        }
    }

    /// Wait a slice, draining output if there is any left to drain. Once the
    /// stream has ended `pump` returns instantly, so a loop that called it
    /// would spin hot against a dead child for the rest of its timeout.
    fn idle(&mut self, slice: Duration) -> Result<(), String> {
        if self.tracker.stream_ended().is_some() {
            std::thread::sleep(slice);
            return Ok(());
        }
        self.tracker.pump(slice)
    }

    /// Is a keypress-awaiting screen currently up?
    fn screen_shows_dialog(&self) -> bool {
        let tail = self.tracker.screen_tail(600).to_lowercase();
        DIALOG_MARKERS.iter().any(|marker| tail.contains(marker))
    }

    /// Wait until the child stops producing output for `quiet_for`, or give
    /// up after `timeout`. Quiescence is a content-free signal that a
    /// streaming generation has stopped — the alternative, matching the
    /// TUI's "Interrupted" banner, would be a content-exact assertion
    /// against a string the CLI is free to reword.
    ///
    /// A child that *died* also stops producing output, and that is the
    /// opposite of what callers here mean: an interrupt that killed the
    /// process instead of the generation must read as a failure, not as a
    /// clean stop. So an ended stream is an error, never silence.
    pub fn wait_until_quiet(
        &mut self,
        quiet_for: Duration,
        timeout: Duration,
    ) -> Result<Duration, String> {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut last_chunk_at = Instant::now();
        let mut last_seen = self.tracker.chunks_seen();
        loop {
            self.tracker.pump(Duration::from_millis(100))?;
            self.tracker.ensure_live("the output to go quiet")?;
            let seen = self.tracker.chunks_seen();
            if seen != last_seen {
                last_seen = seen;
                last_chunk_at = Instant::now();
            } else if last_chunk_at.elapsed() >= quiet_for {
                return Ok(started.elapsed());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "output never went quiet for {}ms within {}s — it is still streaming",
                    quiet_for.as_millis(),
                    timeout.as_secs()
                ));
            }
        }
    }

    /// Steps `first_token` → `session_start` → `env_hygiene` →
    /// `transcript_path` (exit codes 34–37): prove the child paints within
    /// budget, drive the trust dialog if it appears, receive SessionStart
    /// over the hook channel, verify the environment, and locate the
    /// transcript.
    pub fn establish(&mut self) -> Result<SessionInfo, Failure> {
        let budget = Duration::from_millis(self.first_token_ms);
        let latency = self
            .tracker
            .wait_for_first_chunk(budget)
            .map_err(|detail| {
                Failure::new(
                    "first_token",
                    34,
                    format!(
                        "{detail}; cursor-position queries answered so far: {} (an unanswered query would stall the child before its first paint — the reader always replies, so a timeout here is genuine first-paint latency)",
                        self.queries_answered.load(Ordering::Relaxed)
                    ),
                )
            })?;
        print_step(
            "first_token",
            "pass",
            &format!(
                "first output byte {}ms after spawn (budget {}ms)",
                latency.as_millis(),
                self.first_token_ms
            ),
        );

        // SessionStart arrives over the hook channel once the session is
        // actually up. Before that, a fresh project directory paints the
        // workspace-trust dialog, which only Enter clears — there is no
        // structured channel yet, so the screen is the only signal, and this
        // is the one place the probe reads it.
        //
        // Enter is re-sent, not sent once: a keystroke that lands mid-paint
        // is dropped by the dialog, and on a loaded CI runner that is a
        // coin-flip. A stray Enter after the dialog closes submits an empty
        // prompt, which the composer ignores — so retrying is cheap and not
        // retrying costs the whole `SESSION_START_TIMEOUT`. The same repeat
        // clears any other pre-session screen whose default is "continue".
        let deadline = Instant::now() + SESSION_START_TIMEOUT;
        let mut enters_sent = 0u32;
        let mut last_enter: Option<Instant> = None;
        let session_start = loop {
            let len = self.hook_mark();
            if let Some(event) = self.hook_log[..len]
                .iter()
                .find(|event| event.name == "SessionStart")
            {
                break event.payload.clone();
            }
            // The CLI died before it ever started a session: say that,
            // rather than blaming a dialog that was never painted.
            self.tracker
                .ensure_live("the SessionStart hook")
                .map_err(|detail| Failure::new("session_start", 35, detail))?;

            let due = last_enter.is_none_or(|at| at.elapsed() >= TRUST_DIALOG_RETRY);
            if enters_sent < TRUST_DIALOG_MAX_ENTERS && due && self.screen_shows_dialog() {
                // Let the dialog finish painting and arm its key handler
                // before answering it.
                self.tracker
                    .pump(TRUST_DIALOG_SETTLE)
                    .map_err(|detail| Failure::new("session_start", 35, detail))?;
                self.writer.send(b"\r").map_err(|err| {
                    Failure::new(
                        "session_start",
                        35,
                        format!("answering the trust dialog failed: {err}"),
                    )
                })?;
                enters_sent += 1;
                last_enter = Some(Instant::now());
            }
            if Instant::now() >= deadline {
                return Err(Failure::new(
                    "session_start",
                    35,
                    format!(
                        "SessionStart hook not observed within {}s ({enters_sent} Enter(s) sent at a pre-session screen). If the CLI is showing first-run onboarding rather than the workspace-trust dialog, this is where it stalls. Screen tail: '{}'",
                        SESSION_START_TIMEOUT.as_secs(),
                        self.tracker.screen_tail(300),
                    ),
                ));
            }
            self.idle(Duration::from_millis(200))
                .map_err(|detail| Failure::new("session_start", 35, detail))?;
        };
        let trust_driven = enters_sent > 0;
        let advertised_session = session_start
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("<missing>");
        if advertised_session != self.session_id {
            return Err(Failure::new(
                "session_start",
                35,
                format!(
                    "SessionStart carries session_id {advertised_session}, but the launch preset {}",
                    self.session_id
                ),
            ));
        }
        print_step(
            "session_start",
            "pass",
            &format!(
                "SessionStart over the hook channel; preset --session-id honored; trust dialog driven from the screen: {trust_driven}"
            ),
        );

        let hygiene_detail = self
            .verify_env_hygiene()
            .map_err(|detail| Failure::new("env_hygiene", 36, detail))?;
        print_step("env_hygiene", "pass", &hygiene_detail);

        let transcript_path = PathBuf::from(
            session_start
                .get("transcript_path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    Failure::new(
                        "transcript_path",
                        37,
                        format!("SessionStart payload carries no transcript_path: {session_start}"),
                    )
                })?,
        );
        let transcript_baseline = transcript_size(&transcript_path);
        print_step(
            "transcript_path",
            "pass",
            &format!(
                "SessionStart advertises {} ({}); liveness asserted after the prompt turn",
                transcript_path.display(),
                if transcript_baseline > 0 {
                    format!("{transcript_baseline} bytes already")
                } else {
                    "not written yet".to_string()
                }
            ),
        );

        Ok(SessionInfo {
            transcript_path,
            transcript_baseline,
            trust_dialog_driven: trust_driven,
        })
    }

    /// Type a prompt and wait for the turn to end (`Stop` hook). Returns the
    /// hooks the turn produced and how long it took from Enter to `Stop`.
    pub fn run_turn(&mut self, prompt: &str, timeout: Duration) -> Result<Turn, String> {
        let mark = self.hook_mark();
        let submitted_at = self
            .writer
            .type_line(prompt, TYPE_SETTLE)
            .map_err(|err| format!("typing the prompt failed: {err}"))?;
        self.tracker.clock.note_submit(submitted_at);
        self.wait_for_hook("Stop", mark, timeout)?;
        Ok(Turn {
            duration: submitted_at.elapsed(),
            hooks: self.hooks_since(mark),
        })
    }

    /// Direct environment verification where the OS allows reading another
    /// process's environment (Linux); elsewhere the structural composition
    /// plus the transcript-liveness step carry the proof.
    fn verify_env_hygiene(&mut self) -> Result<String, String> {
        #[cfg(target_os = "linux")]
        {
            let pid = self
                .child
                .process_id()
                .ok_or_else(|| "child pid unavailable for /proc verification".to_string())?;
            let raw = std::fs::read(format!("/proc/{pid}/environ"))
                .map_err(|err| format!("reading /proc/{pid}/environ failed: {err}"))?;
            let mut names = 0usize;
            let mut offending: Vec<String> = Vec::new();
            for entry in raw.split(|byte| *byte == 0) {
                if entry.is_empty() {
                    continue;
                }
                names += 1;
                let name = entry
                    .split(|byte| *byte == b'=')
                    .next()
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                    .unwrap_or_default();
                if forbidden_env_name(&name) {
                    offending.push(name);
                }
            }
            if offending.is_empty() {
                Ok(format!(
                    "/proc/{pid}/environ read directly: {names} variables, no nested-session markers (CLAUDE_CODE_*, CLAUDECODE, CMUX_*, NODE_OPTIONS all absent)"
                ))
            } else {
                Err(format!(
                    "child environment carries forbidden markers: {} — the spawn path leaked them past the composed allowlist",
                    offending.join(", ")
                ))
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(
                "composed-allowlist strip is structural (unit-tested); this OS offers no direct read of another process's environment — transcript liveness below is the behavioral proof"
                    .to_string(),
            )
        }
    }

    /// The graceful shutdown: steps `exit` → `child_exit` (exit codes 40–41).
    /// `/exit` is typed, `SessionEnd` proves the CLI accepted it, and the
    /// process is then reaped — a session that ended cleanly and a process
    /// that actually left are two claims, so they get two checks.
    ///
    /// Whatever happens here, the child is dead and the PTY is closed by the
    /// time this returns: the cleanup below runs on every path, which is
    /// what keeps a failed assertion from leaking a live CLI session (and,
    /// in CI, its quota) into the rest of the job.
    /// A failing step's `status=fail` line is printed exactly once, by the
    /// binary that turns it into an exit code. Everything a lane swallows on
    /// the way there — a forced kill, a cleanup that could not finish — is
    /// announced as `status=warn` instead, so nothing is silently dropped
    /// and nothing is reported twice.
    pub fn finish(mut self, scenario: &str) -> Result<(), Failure> {
        let graceful = self.graceful_exit();
        if let Err(failure) = &graceful {
            let killed = crate::pty::force_kill(self.child.as_mut());
            print_step(
                "forced_exit",
                "warn",
                &format!(
                    "step {} failed, so the child was killed rather than exited: {killed}",
                    failure.step
                ),
            );
        }
        let cleanup = self.cleanup(scenario);
        // The shutdown failure is the more informative one; a cleanup
        // failure only surfaces when the shutdown itself was fine.
        graceful.and(cleanup)
    }

    /// Cleanup for a session whose probe already failed: kill the child
    /// without pretending `/exit` would work on a wedged TUI, then run the
    /// same cleanup. `cause` names the step that failed, so the forced
    /// teardown reads as a consequence rather than a mystery.
    pub fn abandon(mut self, scenario: &str, cause: &Failure) {
        let killed = crate::pty::force_kill(self.child.as_mut());
        print_step(
            "forced_exit",
            "warn",
            &format!(
                "step {} failed, so the session was abandoned rather than exited: {killed}",
                cause.step
            ),
        );
        // `cause` is what the process will exit on; a cleanup problem on top
        // of it is a warning, not a competing failure.
        if let Err(failure) = self.cleanup(scenario) {
            print_step("teardown", "warn", &failure.detail);
        }
    }

    fn graceful_exit(&mut self) -> Result<(), Failure> {
        let mark = self.hook_mark();
        self.writer
            .type_line("/exit", TYPE_SETTLE)
            .map_err(|err| Failure::new("exit", 40, format!("typing /exit failed: {err}")))?;
        // SessionEnd is the structured proof `/exit` was accepted; the
        // process exit below is the separate proof it actually left.
        self.wait_for_hook("SessionEnd", mark, SESSION_END_TIMEOUT)
            .map_err(|detail| Failure::new("exit", 40, detail))?;
        print_step(
            "exit",
            "pass",
            "/exit typed; SessionEnd over the hook channel",
        );

        let exit_detail = wait_child(self.child.as_mut(), CHILD_EXIT_TIMEOUT)
            .map_err(|detail| Failure::new("child_exit", 41, detail))?;
        print_step("child_exit", "pass", &exit_detail);
        Ok(())
    }

    /// Steps `capture` → `teardown` (exit codes 42–43): finalize the capture,
    /// close the master through the deadlock-guarded path with the reader
    /// still draining, drop the workdir. Runs on the success and failure
    /// paths alike.
    fn cleanup(self, scenario: &str) -> Result<(), Failure> {
        let (events, capture, end) = self.tracker.into_teardown_parts();
        if let Some(capture) = capture {
            let captured_on = crate::capture::utc_date(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since_epoch| since_epoch.as_secs()),
            );
            let (path, chunks, bytes) = capture
                .finish(&self.cli_version, COLS, ROWS, captured_on, scenario)
                .map_err(|err| {
                    Failure::new(
                        "capture",
                        42,
                        format!("finalizing the capture failed: {err}"),
                    )
                })?;
            print_step(
                "capture",
                "pass",
                &format!(
                    "{} ({chunks} chunks, {bytes} bytes) + meta side file",
                    path.display()
                ),
            );
        }

        let teardown_detail = teardown(self.master, &events, end, IO_TIMEOUT)
            .map_err(|detail| Failure::new("teardown", 43, detail))?;

        let workdir_note = if self.keep_workdir {
            format!("; workdir kept at {}", self.workdir.display())
        } else {
            // Takes the hook socket with it: the listener's endpoint lives
            // inside the workdir precisely so one removal covers both.
            match std::fs::remove_dir_all(&self.workdir) {
                Ok(()) => "; workdir removed".to_string(),
                Err(err) => format!(
                    "; workdir removal failed (left at {}): {err}",
                    self.workdir.display()
                ),
            }
        };
        print_step(
            "teardown",
            "pass",
            &format!("{teardown_detail}{workdir_note}"),
        );
        Ok(())
    }
}

/// What the capture of a `probe` run is labelled with.
const PROBE_SCENARIO: &str =
    "cold interactive session: launch, workspace-trust dialog, one prompt turn, /exit";

/// The live probe lane: launch → establish → one prompt turn → transcript
/// liveness → `/exit` → teardown. Once the child exists, it is torn down on
/// every path out of this function.
pub fn run_probe(config: &ProbeConfig) -> Result<(), Failure> {
    let mut session = launch(config)?;
    match probe_session(&mut session) {
        Ok(()) => session.finish(PROBE_SCENARIO),
        Err(failure) => {
            session.abandon(PROBE_SCENARIO, &failure);
            Err(failure)
        }
    }
}

fn probe_session(session: &mut LiveSession) -> Result<(), Failure> {
    let info = session.establish()?;

    // One cheap turn. Live-lane assertions are shape-based (a turn happened,
    // hooks fired, output flowed) — never content-exact.
    let turn = session
        .run_turn("Reply with exactly: ok", TURN_TIMEOUT)
        .map_err(|detail| Failure::new("prompt_turn", 38, detail))?;
    let after_submit = session
        .tracker
        .clock
        .first_output_after_submit()
        .map_or_else(
            || "unmeasured".to_string(),
            |d| format!("{}ms", d.as_millis()),
        );
    print_step(
        "prompt_turn",
        "pass",
        &format!(
            "turn completed in {}ms (Stop observed); first output after Enter in {after_submit} — that is the TUI repainting its composer, not the model's first token; turn hooks: [{}]",
            turn.duration.as_millis(),
            turn.hook_names().join(", ")
        ),
    );

    // The transcript both exists and grows: the content channel is alive,
    // which is the behavioral consequence of the hygiene strip.
    let deadline = Instant::now() + TRANSCRIPT_TIMEOUT;
    let grown = loop {
        let size = transcript_size(&info.transcript_path);
        if size > info.transcript_baseline {
            break size;
        }
        if Instant::now() >= deadline {
            return Err(Failure::new(
                "transcript_liveness",
                39,
                format!(
                    "transcript at {} did not grow past {} bytes within {}s of the turn — the content channel is dead (nested-session markers leaked?)",
                    info.transcript_path.display(),
                    info.transcript_baseline,
                    TRANSCRIPT_TIMEOUT.as_secs()
                ),
            ));
        }
        // A transcript that stopped growing because the CLI died is a
        // different finding from one the CLI never wrote.
        session
            .tracker
            .ensure_live("the transcript to grow")
            .map_err(|detail| Failure::new("transcript_liveness", 39, detail))?;
        session
            .tracker
            .pump(Duration::from_millis(200))
            .map_err(|detail| Failure::new("transcript_liveness", 39, detail))?;
    };
    print_step(
        "transcript_liveness",
        "pass",
        &format!(
            "{} grew {} -> {grown} bytes across the turn",
            info.transcript_path.display(),
            info.transcript_baseline
        ),
    );

    let launch_latency = session.tracker.clock.launch_latency().map_or_else(
        || "unmeasured".to_string(),
        |d| format!("{}ms", d.as_millis()),
    );
    print_step(
        "report",
        "pass",
        &format!(
            "cli={} session={} first_token_launch={launch_latency} first_output_after_submit={after_submit} turn_ms={} trust_dialog={} cursor_queries_answered={}",
            session.cli_version,
            session.session_id,
            turn.duration.as_millis(),
            info.trust_dialog_driven,
            session.queries_answered.load(Ordering::Relaxed),
        ),
    );
    Ok(())
}

fn transcript_size(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |meta| meta.len())
}

/// Resolve a binary name on the parent's PATH (a path with separators is
/// taken as-is). The probe resolves explicitly instead of leaving it to the
/// spawn layer so the step log names the actual file that ran.
///
/// On Windows only a real executable will do. A `.cmd` / `.bat` shim — what
/// an npm install of the CLI leaves on PATH — cannot be spawned under a PTY:
/// the PTY layer passes the program as `lpApplicationName` to
/// `CreateProcessW`, and the implicit `cmd.exe` fallback for batch files
/// applies only when that argument is null. Such a shim would satisfy
/// `--version` (a plain `Command`) and then fail at spawn, so it is rejected
/// here with an explanation rather than several steps later with a
/// `%1 is not a valid Win32 application`.
fn resolve_binary(name: &str) -> Result<PathBuf, String> {
    let candidate = PathBuf::from(name);
    if candidate.components().count() > 1 {
        // An explicit path still has to name a file that can be executed. A
        // directory `exists()`, and accepting one only defers the complaint
        // to a confusing OS error inside the version query.
        if candidate.is_dir() {
            return Err(format!("{name} is a directory, not an executable"));
        }
        if !candidate.is_file() {
            return Err(format!("{name} does not exist"));
        }
        reject_batch_shim(&candidate)?;
        return Ok(candidate);
    }
    let path = std::env::var_os("PATH").ok_or_else(|| "PATH is not set".to_string())?;
    let suffixes: &[&str] = if cfg!(windows) { &[".exe", ""] } else { &[""] };
    for dir in std::env::split_paths(&path) {
        for suffix in suffixes {
            let full = dir.join(format!("{name}{suffix}"));
            if full.is_file() {
                return Ok(full);
            }
        }
        if cfg!(windows) {
            for shim in [".cmd", ".bat"] {
                let full = dir.join(format!("{name}{shim}"));
                if full.is_file() {
                    reject_batch_shim(&full)?;
                }
            }
        }
    }
    Err(format!("{name} not found on PATH"))
}

/// A batch shim cannot be spawned under a PTY, wherever it came from — PATH
/// or an explicit `--claude-bin`. Refuse it here, where the reason can be
/// stated, rather than at spawn.
fn reject_batch_shim(path: &Path) -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }
    let is_shim = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
    if is_shim {
        return Err(format!(
            "{} is a batch shim, which cannot be spawned under a PTY. Point --claude-bin at the real executable",
            path.display()
        ));
    }
    Ok(())
}

/// How long the version query gets before it is treated as a wedged binary.
const VERSION_TIMEOUT: Duration = Duration::from_secs(20);

/// Ask the CLI its version. The child is owned, not handed to a helper
/// thread that can be abandoned: a wedged binary must become a failed step
/// *and* a dead process, or the probe leaks exactly the thing it promises to
/// clean up. Exit is polled rather than blocking-waited, the same discipline
/// the PTY child gets, so the timeout can actually fire.
///
/// The pipes cannot deadlock the poll: a child that filled them would stop
/// exiting, the deadline would pass, and it would be killed. `--version`
/// writes one line.
fn query_version(binary: &Path) -> Result<String, String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("running `{} --version` failed: {err}", binary.display()))?;

    let deadline = Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let killed = match child.kill().and_then(|()| child.wait()) {
                        Ok(_) => "it was killed and reaped",
                        Err(_) => "and it could not be killed",
                    };
                    return Err(format!(
                        "`{} --version` did not answer within {}s; {killed}",
                        binary.display(),
                        VERSION_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => {
                let _ = child.kill().and_then(|()| child.wait());
                return Err(format!(
                    "waiting on `{} --version` failed: {err}",
                    binary.display()
                ));
            }
        }
    }
    let output = child.wait_with_output().map_err(|err| {
        format!(
            "reading `{} --version` output failed: {err}",
            binary.display()
        )
    })?;
    if !output.status.success() {
        return Err(format!(
            "`{} --version` exited with {}: {}",
            binary.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent(vars: &[(&str, &str)]) -> Vec<(String, String)> {
        vars.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn env_hygiene_strips_claude_markers() {
        let composed = compose_child_env(
            80,
            24,
            parent(&[
                ("HOME", "/home/u"),
                ("PATH", "/usr/bin"),
                ("CLAUDE_CODE_CHILD_SESSION", "1"),
                ("CLAUDECODE", "1"),
                ("CLAUDE_CODE_ENTRYPOINT", "cli"),
                ("CMUX_SOCKET", "/tmp/cmux.sock"),
                ("NODE_OPTIONS", "--require /shim.js"),
                ("LD_PRELOAD", "/evil.so"),
                ("SOME_RANDOM_VAR", "x"),
            ]),
        );
        let names: Vec<&str> = composed.iter().map(|(k, _)| k.as_str()).collect();
        for stripped in [
            "CLAUDE_CODE_CHILD_SESSION",
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CMUX_SOCKET",
            "NODE_OPTIONS",
            "LD_PRELOAD",
            "SOME_RANDOM_VAR",
        ] {
            assert!(!names.contains(&stripped), "{stripped} must be stripped");
        }
        for carried in ["HOME", "PATH"] {
            assert!(names.contains(&carried), "{carried} must be carried");
        }
    }

    #[test]
    fn composed_env_carries_the_terminal_defaults() {
        let composed = compose_child_env(120, 40, parent(&[("HOME", "/home/u")]));
        let get = |key: &str| {
            composed
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
                .unwrap_or_else(|| panic!("missing env default: {key}"))
        };
        assert_eq!(get("TERM"), "xterm-256color");
        assert_eq!(get("COLUMNS"), "120");
        assert_eq!(get("LINES"), "40");
        assert_eq!(get("COLORTERM"), "truecolor");
        assert!(get("LC_ALL").ends_with("UTF-8"), "LC_ALL must force UTF-8");
        assert_eq!(get("LANG"), get("LC_ALL"));
    }

    #[test]
    fn auth_variables_survive_composition() {
        // The hygiene strip must not sterilize the child into being unable
        // to authenticate.
        let composed = compose_child_env(
            80,
            24,
            parent(&[
                ("CLAUDE_CONFIG_DIR", "/home/u/.claude-personal"),
                ("ANTHROPIC_API_KEY", "sk-test"),
            ]),
        );
        let names: Vec<&str> = composed.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"CLAUDE_CONFIG_DIR"));
        assert!(names.contains(&"ANTHROPIC_API_KEY"));
    }

    #[test]
    fn forbidden_markers_do_not_catch_the_config_dir() {
        // CLAUDE_CONFIG_DIR shares a prefix chunk with CLAUDE_CODE_*; the
        // allowlist carries it and the forbidden check must not flag it.
        assert!(!forbidden_env_name("CLAUDE_CONFIG_DIR"));
        assert!(forbidden_env_name("CLAUDE_CODE_CHILD_SESSION"));
        assert!(forbidden_env_name("CLAUDECODE"));
        assert!(forbidden_env_name("CMUX_ANYTHING"));
        assert!(forbidden_env_name("NODE_OPTIONS"));
    }

    #[test]
    fn an_explicit_path_must_name_an_executable_file() {
        // A directory exists(), and accepting one defers the complaint to an
        // opaque OS error inside the version query.
        let dir = std::env::temp_dir();
        let err = resolve_binary(&dir.to_string_lossy()).unwrap_err();
        assert!(err.contains("is a directory"), "unexpected error: {err}");

        let missing = dir.join("agent-bridge-no-such-binary");
        let err = resolve_binary(&missing.to_string_lossy()).unwrap_err();
        assert!(err.contains("does not exist"), "unexpected error: {err}");
    }

    #[test]
    fn a_bare_name_is_looked_up_on_path() {
        // Whatever the platform, a name with no separators is a PATH lookup,
        // and a name nothing provides says so rather than claiming a
        // missing file.
        let err = resolve_binary("agent-bridge-no-such-binary").unwrap_err();
        assert!(err.contains("not found on PATH"), "unexpected error: {err}");
    }

    #[cfg(windows)]
    #[test]
    fn a_batch_shim_is_refused_wherever_it_comes_from() {
        // portable-pty passes the program as lpApplicationName, so CreateProcessW
        // never applies its cmd.exe fallback and a .cmd cannot spawn under a PTY.
        assert!(reject_batch_shim(Path::new(r"C:\npm\claude.CMD")).is_err());
        assert!(reject_batch_shim(Path::new(r"C:\npm\claude.bat")).is_err());
        assert!(reject_batch_shim(Path::new(r"C:\bin\claude.exe")).is_ok());
    }

    #[test]
    fn defaults_win_over_carried_parent_values() {
        // If the allowlist and the defaults ever overlap, the defaults must
        // win — the terminal contract is not the parent's to override.
        let composed = compose_child_env(80, 24, parent(&[("HOME", "/home/u")]));
        let terms: Vec<&(String, String)> = composed.iter().filter(|(k, _)| k == "TERM").collect();
        assert_eq!(terms.len(), 1);
    }
}
