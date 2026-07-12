//! The scenario capture driver (`record` lane): one scripted interactive
//! session under the probe's PTY, at a caller-chosen terminal size, with
//! everything the detection prototypes will replay offline recorded into one
//! fixture directory:
//!
//! - `input.bytes` + `input.timing.ndjson` — the raw PTY byte stream, and one
//!   `{"offset", "monotonic_ns"}` record per read boundary so replay can
//!   reproduce split-across-reads pacing.
//! - `steps.ndjson` — the driver's own action log on the same spawn-relative
//!   clock. Each scripted step lands here with its optional `label`; that is
//!   what turns a recording into *labeled* ground truth, because a scorer can
//!   hold classifications against what the driver actually did and when.
//! - `hook-payloads.ndjson` (payloads verbatim, one per line, plus a
//!   `hook-payloads.timing.ndjson` sidecar) and `transcript.jsonl` — the
//!   structured side channels, recorded only for the CLI that provides them.
//! - `manifest.yaml` — CLI, version, dimensions, OS, capture date, and
//!   artifact sizes: the provenance that keeps a fixture identifiable and
//!   accountable to the corpus size budget.
//!
//! Two launch profiles share one step engine. The `claude` profile rides the
//! full launch rig — hook listener, `--settings` injection, trust-dialog
//! driving, transcript discovery — and ends with a typed `/exit`. The
//! `generic` profile hosts any other CLI (the deterministic fake CLI for
//! shakedown, line-oriented CLIs for their own campaigns) under the same
//! composed environment, records no side channels, and ends when the child
//! exits on its own — the script is responsible for driving it there.
//!
//! Scripts are strict JSON in the fake-cli mold: each step carries exactly
//! one kind key, unknown fields are rejections, and a claude-only step in a
//! generic script fails at parse time, never mid-session. Step timestamps are
//! taken when the step starts; `took_ms` closes the bracket, so an input's
//! bytes landed inside `[t_ns, t_ns + took_ms]` and a wait's condition held
//! at `t_ns + took_ms`.

use std::fs::File;
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtyPair};

use crate::capture::{CaptureWriter, meta_path_for, read_capture, utc_date};
use crate::firsttoken::FirstTokenClock;
use crate::hooks::{Decision, HookEvent};
use crate::pty::{
    OutputTracker, SharedWriter, alloc_pty, force_kill, spawn_reader, strip_ansi, teardown,
    wait_child,
};
use crate::rig::{LiveSession, ProbeConfig, TYPE_SETTLE, compose_child_env, resolve_binary};
use crate::{Failure, print_step};

/// The intermediate the rig's capture writer produces; converted into
/// `input.bytes` + `input.timing.ndjson` and removed once the session is
/// down. The corpus commits the converted pair, not this working format.
const CAPTURE_INTERMEDIATE: &str = "capture.ndjson";

/// Every file this lane may leave in the fixture directory. Recording
/// starts by deleting all of them: a stale artifact from a previous run —
/// worse, from a different profile — must never survive into a fresh
/// fixture and read as this session's output.
const ARTIFACT_FILES: [&str; 7] = [
    "input.bytes",
    "input.timing.ndjson",
    "steps.ndjson",
    "hook-payloads.ndjson",
    "hook-payloads.timing.ndjson",
    "transcript.jsonl",
    "manifest.yaml",
];

const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the epilogue gives the child to be gone: the claude `/exit`
/// path (SessionEnd, then the process), and the generic path's scripted
/// self-exit alike.
const EPILOGUE_TIMEOUT: Duration = Duration::from_secs(20);

pub struct RecordConfig {
    pub script: PathBuf,
    pub out: PathBuf,
    pub cols: u16,
    pub rows: u16,
    /// The child binary. Required for a generic script (there is no sensible
    /// default for "some CLI"); defaults to `claude` for a claude script.
    pub cli_bin: Option<String>,
    /// The version label a generic fixture's manifest carries — supplied by
    /// the invoker because a generic child offers no version contract this
    /// lane could rely on. The claude profile asks the CLI itself and
    /// rejects this flag rather than let a manifest lie.
    pub cli_version: Option<String>,
    pub model: Option<String>,
    /// How the recorded CLI was installed (e.g. `npm
    /// @anthropic-ai/claude-code@2.1.201`, `workspace build`). Recorded in
    /// the manifest because "which release, obtained how" is provenance a
    /// version-drift measurement cannot reconstruct later.
    pub install: Option<String>,
    /// The CLI the invoker believes this script records. The campaign sets
    /// it from its `--cli`, so a misfiled script fails here with both names
    /// stated instead of producing fixtures whose manifest contradicts the
    /// corpus directory they landed in.
    pub expect_cli: Option<String>,
    pub first_token_ms: u64,
    pub keep_workdir: bool,
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            script: PathBuf::new(),
            out: PathBuf::new(),
            cols: crate::COLS,
            rows: crate::ROWS,
            cli_bin: None,
            cli_version: None,
            model: None,
            install: None,
            expect_cli: None,
            first_token_ms: 2_000,
            keep_workdir: false,
        }
    }
}

#[derive(Debug)]
pub struct RecordScript {
    pub name: String,
    pub description: String,
    /// The CLI this scenario records — `claude` selects the full launch rig;
    /// any other name is hosted through the generic profile.
    pub cli: String,
    pub args: Vec<String>,
    pub steps: Vec<DriverStep>,
}

impl RecordScript {
    pub fn is_claude(&self) -> bool {
        self.cli == "claude"
    }
}

#[derive(Debug)]
pub struct DriverStep {
    pub kind: StepKind,
    /// Ground-truth annotation carried into `steps.ndjson`. The driver does
    /// not interpret it; the pipeline's scorer does.
    pub label: Option<String>,
}

#[derive(Debug)]
pub enum StepKind {
    /// Type text, settle, then Enter — how the rig submits a prompt line.
    TypeLine { text: String },
    /// One named keystroke, no Enter: menu navigation, number-key answers,
    /// control bytes.
    Press { key: String, bytes: Vec<u8> },
    /// Wait for the output to go quiet — the content-free "generation
    /// stopped" signal.
    WaitQuiet { quiet_ms: u64, timeout_ms: u64 },
    /// Wait until the ANSI-stripped recent output contains `marker`.
    WaitText { marker: String, timeout_ms: u64 },
    /// Wait for a named hook to arrive (claude only). Each wait consumes
    /// hooks in arrival order: it scans from just past the hook the previous
    /// wait matched, so two `Stop` waits match two distinct turns.
    WaitHook { hook: String, timeout_ms: u64 },
    /// Arm the hook listener's answer to the next PreToolUse (claude only).
    SetDecision { decision: Decision },
    /// A fixed settle — pacing for keystrokes a repainting dialog would
    /// otherwise drop.
    Pause { ms: u64 },
}

impl StepKind {
    fn name(&self) -> &'static str {
        match self {
            StepKind::TypeLine { .. } => "type_line",
            StepKind::Press { .. } => "press",
            StepKind::WaitQuiet { .. } => "wait_quiet",
            StepKind::WaitText { .. } => "wait_text",
            StepKind::WaitHook { .. } => "wait_hook",
            StepKind::SetDecision { .. } => "set_decision",
            StepKind::Pause { .. } => "pause",
        }
    }

    /// The kind-specific fields of this step's `steps.ndjson` record.
    fn log_fields(&self) -> Vec<(&'static str, serde_json::Value)> {
        match self {
            StepKind::TypeLine { text } => vec![("text", text.as_str().into())],
            StepKind::Press { key, .. } => vec![("key", key.as_str().into())],
            StepKind::WaitQuiet {
                quiet_ms,
                timeout_ms,
            } => vec![
                ("quiet_ms", (*quiet_ms).into()),
                ("timeout_ms", (*timeout_ms).into()),
            ],
            StepKind::WaitText { marker, timeout_ms } => vec![
                ("marker", marker.as_str().into()),
                ("timeout_ms", (*timeout_ms).into()),
            ],
            StepKind::WaitHook { hook, timeout_ms } => vec![
                ("hook", hook.as_str().into()),
                ("timeout_ms", (*timeout_ms).into()),
            ],
            StepKind::SetDecision { decision } => {
                vec![("decision", decision_str(*decision).into())]
            }
            StepKind::Pause { ms } => vec![("ms", (*ms).into())],
        }
    }
}

fn decision_str(decision: Decision) -> &'static str {
    match decision {
        Decision::NoOpinion => "none",
        Decision::Allow => "allow",
        Decision::Deny => "deny",
        Decision::Ask => "ask",
    }
}

// ---------------------------------------------------------------------------
// Script parsing — strict, index-named rejections, in the fake-cli mold.
// ---------------------------------------------------------------------------

const STEP_KINDS: [&str; 7] = [
    "type_line",
    "press",
    "wait_quiet_ms",
    "wait_text",
    "wait_hook",
    "set_decision",
    "pause_ms",
];

/// Parse a record script. `script_dir` is the script file's own directory;
/// the literal token `{script_dir}` in an arg is replaced with it, so a
/// scenario can name its sibling fixture files without depending on the
/// invoker's working directory.
pub fn parse_script(text: &str, script_dir: &Path) -> Result<RecordScript, String> {
    let root: serde_json::Value =
        serde_json::from_str(text).map_err(|err| format!("invalid JSON: {err}"))?;
    let serde_json::Value::Object(root) = root else {
        return Err("the script must be a JSON object".into());
    };
    for key in root.keys() {
        if !matches!(
            key.as_str(),
            "name" | "description" | "cli" | "args" | "steps"
        ) {
            return Err(format!(
                "unknown top-level field \"{key}\" — a script has \"name\", \"description\", \"cli\", \"args\", and \"steps\""
            ));
        }
    }

    let name = kebab_field(&root, "name")?;
    let cli = kebab_field(&root, "cli")?;
    let description = match root.get("description") {
        Some(serde_json::Value::String(text)) if !text.is_empty() => {
            if text.chars().any(char::is_control) {
                return Err(
                    "\"description\" must be a single line without control characters".into(),
                );
            }
            text.clone()
        }
        _ => return Err("\"description\" must be a non-empty string".into()),
    };

    let is_claude = cli == "claude";
    let args = match root.get("args") {
        None => Vec::new(),
        Some(_) if is_claude => {
            return Err(
                "\"args\" is not accepted for the claude CLI — its launch line is the rig's contract"
                    .into(),
            );
        }
        Some(serde_json::Value::Array(args)) => args
            .iter()
            .enumerate()
            .map(|(index, arg)| match arg {
                serde_json::Value::String(arg) => {
                    Ok(arg.replace("{script_dir}", &script_dir.to_string_lossy()))
                }
                _ => Err(format!("args[{index}] must be a string")),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("\"args\" must be an array of strings".into()),
    };

    let steps = match root.get("steps") {
        Some(serde_json::Value::Array(steps)) if !steps.is_empty() => steps
            .iter()
            .enumerate()
            .map(|(index, step)| parse_step(index, step, is_claude))
            .collect::<Result<Vec<_>, _>>()?,
        Some(serde_json::Value::Array(_)) => return Err("\"steps\" must not be empty".into()),
        Some(_) => return Err("\"steps\" must be an array".into()),
        None => return Err("missing \"steps\"".into()),
    };

    Ok(RecordScript {
        name,
        description,
        cli,
        args,
        steps,
    })
}

/// A required top-level field constrained to kebab case, because both
/// values end up in the manifest and (via the campaign) in corpus directory
/// names, where "anything goes" becomes provenance folklore.
fn kebab_field(
    root: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<String, String> {
    match root.get(field) {
        Some(serde_json::Value::String(value)) if is_kebab(value) => Ok(value.clone()),
        Some(serde_json::Value::String(value)) => Err(format!(
            "\"{field}\" must be kebab-case ([a-z0-9-], starting and ending alphanumeric): \"{value}\""
        )),
        Some(_) => Err(format!("\"{field}\" must be a string")),
        None => Err(format!("missing \"{field}\"")),
    }
}

fn is_kebab(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

fn parse_step(
    index: usize,
    step: &serde_json::Value,
    is_claude: bool,
) -> Result<DriverStep, String> {
    let serde_json::Value::Object(fields) = step else {
        return Err(format!("step {index}: must be a JSON object"));
    };
    let found: Vec<&str> = STEP_KINDS
        .into_iter()
        .filter(|kind| fields.contains_key(*kind))
        .collect();
    let kind_key = match found.as_slice() {
        [one] => *one,
        [] => {
            return Err(format!(
                "step {index}: unknown step kind — found {}; a step carries exactly one of {}",
                name_fields(fields.keys()),
                name_kinds(),
            ));
        }
        many => {
            return Err(format!(
                "step {index}: ambiguous step — carries {}; a step carries exactly one of {}",
                name_fields(many.iter()),
                name_kinds(),
            ));
        }
    };
    if !is_claude && matches!(kind_key, "wait_hook" | "set_decision") {
        return Err(format!(
            "step {index}: \"{kind_key}\" needs the claude hook channel, which this CLI does not provide"
        ));
    }

    let label = match fields.get("label") {
        None => None,
        Some(serde_json::Value::String(label)) if !label.is_empty() => Some(label.clone()),
        Some(_) => {
            return Err(format!(
                "step {index} ({kind_key}): \"label\" must be a non-empty string"
            ));
        }
    };

    let kind = match kind_key {
        "type_line" => {
            reject_unknown(index, kind_key, fields, &["type_line", "label"])?;
            StepKind::TypeLine {
                text: string_field(index, kind_key, fields, "type_line")?,
            }
        }
        "press" => {
            reject_unknown(index, kind_key, fields, &["press", "label"])?;
            let key = string_field(index, kind_key, fields, "press")?;
            let bytes = key_bytes(&key).ok_or_else(|| {
                format!(
                    "step {index} (press): unknown key \"{key}\" — named keys are {}, or one printable ASCII character",
                    NAMED_KEYS
                        .iter()
                        .map(|(name, _)| format!("\"{name}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            StepKind::Press { key, bytes }
        }
        "wait_quiet_ms" => {
            reject_unknown(
                index,
                kind_key,
                fields,
                &["wait_quiet_ms", "timeout_ms", "label"],
            )?;
            StepKind::WaitQuiet {
                quiet_ms: positive_field(index, kind_key, fields, "wait_quiet_ms")?,
                timeout_ms: positive_field(index, kind_key, fields, "timeout_ms")?,
            }
        }
        "wait_text" => {
            reject_unknown(
                index,
                kind_key,
                fields,
                &["wait_text", "timeout_ms", "label"],
            )?;
            StepKind::WaitText {
                marker: string_field(index, kind_key, fields, "wait_text")?,
                timeout_ms: positive_field(index, kind_key, fields, "timeout_ms")?,
            }
        }
        "wait_hook" => {
            reject_unknown(
                index,
                kind_key,
                fields,
                &["wait_hook", "timeout_ms", "label"],
            )?;
            StepKind::WaitHook {
                hook: string_field(index, kind_key, fields, "wait_hook")?,
                timeout_ms: positive_field(index, kind_key, fields, "timeout_ms")?,
            }
        }
        "set_decision" => {
            reject_unknown(index, kind_key, fields, &["set_decision", "label"])?;
            let raw = string_field(index, kind_key, fields, "set_decision")?;
            let decision = match raw.as_str() {
                "allow" => Decision::Allow,
                "deny" => Decision::Deny,
                "ask" => Decision::Ask,
                "none" => Decision::NoOpinion,
                other => {
                    return Err(format!(
                        "step {index} (set_decision): unknown decision \"{other}\" — one of \"allow\", \"deny\", \"ask\", \"none\""
                    ));
                }
            };
            StepKind::SetDecision { decision }
        }
        "pause_ms" => {
            reject_unknown(index, kind_key, fields, &["pause_ms", "label"])?;
            StepKind::Pause {
                ms: positive_field(index, kind_key, fields, "pause_ms")?,
            }
        }
        _ => unreachable!("kind_key comes from STEP_KINDS"),
    };
    Ok(DriverStep { kind, label })
}

fn string_field(
    index: usize,
    kind: &str,
    fields: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<String, String> {
    match fields.get(field) {
        Some(serde_json::Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        _ => Err(format!(
            "step {index} ({kind}): \"{field}\" must be a non-empty string"
        )),
    }
}

fn positive_field(
    index: usize,
    kind: &str,
    fields: &serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<u64, String> {
    fields
        .get(field)
        .and_then(serde_json::Value::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            format!("step {index} ({kind}): \"{field}\" must be a positive integer (milliseconds)")
        })
}

fn reject_unknown(
    index: usize,
    kind: &str,
    fields: &serde_json::Map<String, serde_json::Value>,
    allowed: &[&str],
) -> Result<(), String> {
    for key in fields.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!("step {index} ({kind}): unknown field \"{key}\""));
        }
    }
    Ok(())
}

fn name_fields<I, S>(keys: I) -> String
where
    I: Iterator<Item = S>,
    S: AsRef<str>,
{
    let named: Vec<String> = keys.map(|key| format!("\"{}\"", key.as_ref())).collect();
    if named.is_empty() {
        "no fields".to_string()
    } else {
        format!("field(s) {}", named.join(", "))
    }
}

fn name_kinds() -> String {
    STEP_KINDS
        .into_iter()
        .map(|kind| format!("\"{kind}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The named keys a script can press. Arrows are the CSI sequences a real
/// terminal sends; Enter is `\r` (the TTY translates); the ctrl keys are
/// their control bytes, which is what an interrupt actually is on the wire.
const NAMED_KEYS: [(&str, &[u8]); 10] = [
    ("enter", b"\r"),
    ("tab", b"\t"),
    ("esc", b"\x1b"),
    ("space", b" "),
    ("backspace", b"\x7f"),
    ("up", b"\x1b[A"),
    ("down", b"\x1b[B"),
    ("right", b"\x1b[C"),
    ("left", b"\x1b[D"),
    ("ctrl-c", b"\x03"),
];

fn key_bytes(key: &str) -> Option<Vec<u8>> {
    if let Some((_, bytes)) = NAMED_KEYS.iter().find(|(name, _)| *name == key) {
        return Some(bytes.to_vec());
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_graphic() => Some(vec![c as u8]),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The step log.
// ---------------------------------------------------------------------------

struct StepLog {
    out: BufWriter<File>,
    seq: u64,
    t0: Instant,
    records: u64,
}

impl StepLog {
    fn create(path: &Path, t0: Instant) -> std::io::Result<Self> {
        Ok(Self {
            out: BufWriter::new(File::create(path)?),
            seq: 0,
            t0,
            records: 0,
        })
    }

    /// Append one record. Flushed per line: a session that dies mid-run
    /// must leave every completed step on disk, or the failure diagnosis
    /// loses exactly the part it needs.
    fn record(
        &mut self,
        started: Instant,
        took: Duration,
        step: &str,
        fields: &[(&str, serde_json::Value)],
        label: Option<&str>,
        outcome: &str,
    ) -> std::io::Result<()> {
        self.seq += 1;
        let mut line = serde_json::Map::new();
        line.insert("seq".into(), self.seq.into());
        line.insert(
            "t_ns".into(),
            (started.saturating_duration_since(self.t0).as_nanos() as u64).into(),
        );
        line.insert("step".into(), step.into());
        for (key, value) in fields {
            line.insert((*key).into(), value.clone());
        }
        if let Some(label) = label {
            line.insert("label".into(), label.into());
        }
        line.insert("outcome".into(), outcome.into());
        line.insert("took_ms".into(), (took.as_millis() as u64).into());
        serde_json::to_writer(&mut self.out, &serde_json::Value::Object(line))?;
        self.out.write_all(b"\n")?;
        self.out.flush()?;
        self.records += 1;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The step engine, over either launch profile.
// ---------------------------------------------------------------------------

enum Host<'a> {
    Claude {
        session: &'a mut LiveSession,
        /// Index just past the last hook a `wait_hook` matched — each wait
        /// consumes hooks in arrival order, so repeated waits on one name
        /// match distinct events.
        hook_cursor: usize,
    },
    Generic(&'a mut GenericSession),
}

impl Host<'_> {
    fn writer(&self) -> SharedWriter {
        match self {
            Host::Claude { session, .. } => session.writer.clone(),
            Host::Generic(session) => session.writer.clone(),
        }
    }

    fn tracker(&mut self) -> &mut OutputTracker {
        match self {
            Host::Claude { session, .. } => &mut session.tracker,
            Host::Generic(session) => &mut session.tracker,
        }
    }

    /// Wait a slice, draining output while there is any: a pause must not
    /// spin hot against an ended stream, and must not stop the capture from
    /// absorbing what arrives while the driver idles.
    fn idle(&mut self, slice: Duration) -> Result<(), String> {
        let tracker = self.tracker();
        if tracker.stream_ended().is_some() {
            std::thread::sleep(slice);
            return Ok(());
        }
        tracker.pump(slice)
    }

    fn execute(&mut self, kind: &StepKind) -> Result<Duration, String> {
        let started = Instant::now();
        match kind {
            StepKind::TypeLine { text } => {
                self.writer()
                    .type_line(text, TYPE_SETTLE)
                    .map_err(|err| format!("typing the line failed: {err}"))?;
            }
            StepKind::Press { bytes, .. } => {
                self.writer()
                    .send(bytes)
                    .map_err(|err| format!("sending the keystroke failed: {err}"))?;
            }
            StepKind::WaitQuiet {
                quiet_ms,
                timeout_ms,
            } => {
                self.tracker().wait_until_quiet(
                    Duration::from_millis(*quiet_ms),
                    Duration::from_millis(*timeout_ms),
                )?;
            }
            StepKind::WaitText { marker, timeout_ms } => {
                let marker = marker.clone();
                self.tracker().wait_for_text(
                    &format!("text marker '{marker}'"),
                    |text| strip_ansi(text).contains(&marker),
                    Duration::from_millis(*timeout_ms),
                )?;
            }
            StepKind::WaitHook { hook, timeout_ms } => {
                self.wait_hook(hook, Duration::from_millis(*timeout_ms))?;
            }
            StepKind::SetDecision { decision } => match self {
                Host::Claude { session, .. } => session.listener.set_decision(*decision),
                Host::Generic(_) => {
                    return Err("set_decision has no hook listener on this profile".into());
                }
            },
            StepKind::Pause { ms } => {
                let deadline = started + Duration::from_millis(*ms);
                while Instant::now() < deadline {
                    self.idle(
                        deadline
                            .saturating_duration_since(Instant::now())
                            .min(Duration::from_millis(100)),
                    )?;
                }
            }
        }
        Ok(started.elapsed())
    }

    fn wait_hook(&mut self, name: &str, timeout: Duration) -> Result<(), String> {
        let Host::Claude {
            session,
            hook_cursor,
        } = self
        else {
            return Err("wait_hook has no hook channel on this profile".into());
        };
        // Hooks travel over IPC, not the PTY, so a payload can legitimately
        // still be in flight when the output stream ends — but a child that
        // is *gone* will never send another one. An ended stream therefore
        // shrinks the wait to a short grace instead of letting a scripted
        // two-minute timeout burn against a dead session.
        const ENDED_STREAM_GRACE: Duration = Duration::from_secs(2);
        let deadline = Instant::now() + timeout;
        let mut ended_deadline: Option<Instant> = None;
        loop {
            let events = session.hook_events_since(0);
            if let Some(offset) = events[*hook_cursor..]
                .iter()
                .position(|event| event.name == name)
            {
                *hook_cursor += offset + 1;
                return Ok(());
            }
            // Owned, because the borrow on the hook log must end before the
            // tracker is consulted below.
            let seen: Vec<String> = events[*hook_cursor..]
                .iter()
                .map(|event| event.name.clone())
                .collect();
            let ended = session
                .tracker
                .stream_ended()
                .map(|reason| reason.to_string());
            if ended.is_some() && ended_deadline.is_none() {
                ended_deadline = Some(Instant::now() + ENDED_STREAM_GRACE);
            }
            let effective_deadline = ended_deadline.map_or(deadline, |d| d.min(deadline));
            if Instant::now() >= effective_deadline {
                let ended_note = ended.map_or_else(String::new, |reason| {
                    format!(
                        " — the output stream had already ended ({reason}); the process is gone, and only the {}s in-flight grace was waited",
                        ENDED_STREAM_GRACE.as_secs()
                    )
                });
                return Err(format!(
                    "hook {name} not observed within {}s (unconsumed hooks: [{}]){ended_note}; screen tail: '{}'",
                    timeout.as_secs(),
                    seen.join(", "),
                    session.tracker.screen_tail(200),
                ));
            }
            if ended_deadline.is_some() {
                std::thread::sleep(Duration::from_millis(100));
            } else {
                session.tracker.pump(Duration::from_millis(100))?;
            }
        }
    }
}

/// Run every scripted step, logging each with its outcome. The failing
/// step's record is written before the failure propagates, so the log tells
/// the truth about where the session died.
fn execute_steps(host: &mut Host, steps: &[DriverStep], log: &mut StepLog) -> Result<(), Failure> {
    for (index, step) in steps.iter().enumerate() {
        let started = Instant::now();
        let result = host.execute(&step.kind);
        let took = started.elapsed();
        let outcome = match &result {
            Ok(_) => "ok".to_string(),
            Err(detail) => format!("failed: {detail}"),
        };
        log.record(
            started,
            took,
            step.kind.name(),
            &step.kind.log_fields(),
            step.label.as_deref(),
            &outcome,
        )
        .map_err(|err| Failure::new("step", 83, format!("writing the step log failed: {err}")))?;
        match result {
            Ok(_) => print_step(
                "step",
                "pass",
                &format!(
                    "#{} {} ({}ms){}",
                    index + 1,
                    step.kind.name(),
                    took.as_millis(),
                    step.label
                        .as_deref()
                        .map_or_else(String::new, |label| format!(" label={label}")),
                ),
            ),
            Err(detail) => {
                return Err(Failure::new(
                    "step",
                    83,
                    format!("step {} ({}): {detail}", index + 1, step.kind.name()),
                ));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The generic launch profile.
// ---------------------------------------------------------------------------

struct GenericSession {
    writer: SharedWriter,
    tracker: OutputTracker,
    master: Box<dyn portable_pty::MasterPty + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    workdir: PathBuf,
    keep_workdir: bool,
}

impl GenericSession {
    /// Steps `alloc` → `spawn` (exit codes 81–82): the child under a PTY at
    /// the requested size, in a fresh temp workdir, with the same composed
    /// environment the live rig uses, capturing to the fixture directory.
    fn launch(config: &RecordConfig, script: &RecordScript) -> Result<Self, Failure> {
        let binary_arg = config.cli_bin.as_deref().ok_or_else(|| {
            Failure::new(
                "args",
                2,
                "a generic script needs --cli-bin: there is no default binary for it",
            )
        })?;
        let binary =
            resolve_binary(binary_arg).map_err(|detail| Failure::new("spawn", 82, detail))?;

        let (pair, alloc_ms) = alloc_pty(config.cols, config.rows, IO_TIMEOUT)
            .map_err(|detail| Failure::new("alloc", 81, detail))?;
        print_step(
            "alloc",
            "pass",
            &format!(
                "pty allocated at {}x{} in {alloc_ms}ms",
                config.cols, config.rows
            ),
        );
        let PtyPair { master, slave } = pair;

        let nonce = uuid::Uuid::new_v4().to_string();
        let workdir = std::env::temp_dir().join(format!("agent-bridge-record-{}", &nonce[..8]));
        std::fs::create_dir_all(&workdir).map_err(|err| {
            Failure::new(
                "spawn",
                82,
                format!("creating {} failed: {err}", workdir.display()),
            )
        })?;

        let mut command = CommandBuilder::new(&binary);
        for arg in &script.args {
            command.arg(arg);
        }
        command.cwd(&workdir);
        command.env_clear();
        for (key, value) in compose_child_env(config.cols, config.rows, std::env::vars_os()) {
            command.env(key, value);
        }

        let spawned_at = Instant::now();
        let mut child = slave
            .spawn_command(command)
            .map_err(|err| Failure::new("spawn", 82, format!("child spawn failed: {err:#}")))?;
        drop(slave);

        let mut kill_child_on = |detail: String| {
            let killed = force_kill(child.as_mut());
            Failure::new("spawn", 82, format!("{detail}; the child was {killed}"))
        };
        let capture = CaptureWriter::create(&config.out.join(CAPTURE_INTERMEDIATE), spawned_at)
            .map_err(|err| kill_child_on(format!("creating the capture file failed: {err}")))?;
        let reader = master
            .try_clone_reader()
            .map_err(|err| kill_child_on(format!("cloning the reader failed: {err:#}")))?;
        let writer = SharedWriter::new(
            master
                .take_writer()
                .map_err(|err| kill_child_on(format!("taking the writer failed: {err:#}")))?,
        );
        let queries = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let events = spawn_reader(reader, writer.clone(), queries);
        let tracker = OutputTracker::new(events, FirstTokenClock::new(spawned_at), Some(capture));

        print_step(
            "spawn",
            "pass",
            &format!(
                "spawned `{}` pid={} in {}",
                binary.display(),
                child
                    .process_id()
                    .map_or_else(|| "unknown".to_string(), |pid| pid.to_string()),
                workdir.display(),
            ),
        );

        Ok(Self {
            writer,
            tracker,
            master,
            child,
            workdir,
            keep_workdir: config.keep_workdir,
        })
    }

    /// Steps `child_exit` → `capture` → `teardown` (exit code 84): the
    /// scripted session is over, the child must already be exiting on its
    /// own — the script drove it there. Teardown drains the reader to
    /// end-of-stream *recording into the capture*, and only then is the
    /// capture finalized, so the conversion downstream reads a flushed
    /// stream that really ends where the session did.
    fn finish(
        mut self,
        cli_version: &str,
        cols: u16,
        rows: u16,
        scenario: &str,
    ) -> Result<(), Failure> {
        let exit_detail = wait_child(self.child.as_mut(), EPILOGUE_TIMEOUT)
            .map_err(|detail| Failure::new("child_exit", 84, detail))?;
        print_step("child_exit", "pass", &exit_detail);

        let (events, mut capture, end) = self.tracker.into_teardown_parts();
        let teardown_detail = teardown(self.master, &events, end, IO_TIMEOUT, capture.as_mut())
            .map_err(|detail| Failure::new("teardown", 84, detail))?;
        if let Some(capture) = capture {
            let captured_on = utc_date(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since_epoch| since_epoch.as_secs()),
            );
            let (path, chunks, bytes) = capture
                .finish(cli_version, cols, rows, captured_on, scenario)
                .map_err(|err| {
                    Failure::new(
                        "capture",
                        84,
                        format!("finalizing the capture failed: {err}"),
                    )
                })?;
            print_step(
                "capture",
                "pass",
                &format!("{} ({chunks} chunks, {bytes} bytes)", path.display()),
            );
        }
        let removal = if self.keep_workdir {
            None
        } else {
            Some(std::fs::remove_dir_all(&self.workdir))
        };
        let note = match removal {
            None => format!("; workdir kept at {}", self.workdir.display()),
            Some(Ok(())) => "; workdir removed".to_string(),
            Some(Err(err)) => format!(
                "; workdir removal failed, left at {}: {err}",
                self.workdir.display()
            ),
        };
        print_step("teardown", "pass", &format!("{teardown_detail}{note}"));
        Ok(())
    }

    /// The failure path: kill the child, close the PTY, keep whatever the
    /// capture managed to write — diagnostic material for a failed run.
    fn abandon(mut self, cause: &Failure) {
        let killed = force_kill(self.child.as_mut());
        print_step(
            "forced_exit",
            "warn",
            &format!(
                "step {} failed, so the child was killed rather than exited: {killed}",
                cause.step
            ),
        );
        let (events, mut capture, end) = self.tracker.into_teardown_parts();
        // The drain still records into the capture: a failed run's partial
        // recording is diagnostic material, and fuller is better.
        if let Err(detail) = teardown(self.master, &events, end, IO_TIMEOUT, capture.as_mut()) {
            print_step("teardown", "warn", &detail);
        }
        drop(capture); // flushes what it buffered; no meta for a failed run
        if !self.keep_workdir
            && let Err(err) = std::fs::remove_dir_all(&self.workdir)
        {
            print_step(
                "teardown",
                "warn",
                &format!(
                    "workdir removal failed, left at {}: {err}",
                    self.workdir.display()
                ),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Artifact writing.
// ---------------------------------------------------------------------------

/// Convert the rig's capture intermediate into the corpus artifact pair:
/// `input.bytes` (the raw stream) and `input.timing.ndjson` (one
/// `{"offset", "monotonic_ns"}` record per read boundary). The intermediate
/// and its meta side file are removed — committing the same bytes twice in
/// two formats would spend the corpus budget on redundancy.
fn write_input_artifacts(out: &Path) -> Result<(u64, u64), String> {
    let capture_path = out.join(CAPTURE_INTERMEDIATE);
    let chunks =
        read_capture(&capture_path).map_err(|err| format!("reading the capture back: {err}"))?;
    let mut bytes_out = BufWriter::new(
        File::create(out.join("input.bytes")).map_err(|err| format!("input.bytes: {err}"))?,
    );
    let mut timing_out = BufWriter::new(
        File::create(out.join("input.timing.ndjson"))
            .map_err(|err| format!("input.timing.ndjson: {err}"))?,
    );
    let mut offset = 0u64;
    for chunk in &chunks {
        let record = serde_json::json!({
            "offset": offset,
            "monotonic_ns": chunk.t_ns,
        });
        timing_out
            .write_all(format!("{record}\n").as_bytes())
            .map_err(|err| format!("input.timing.ndjson: {err}"))?;
        bytes_out
            .write_all(&chunk.bytes)
            .map_err(|err| format!("input.bytes: {err}"))?;
        offset += chunk.bytes.len() as u64;
    }
    bytes_out
        .flush()
        .map_err(|err| format!("input.bytes: {err}"))?;
    timing_out
        .flush()
        .map_err(|err| format!("input.timing.ndjson: {err}"))?;
    std::fs::remove_file(&capture_path)
        .map_err(|err| format!("removing the capture intermediate: {err}"))?;
    std::fs::remove_file(meta_path_for(&capture_path))
        .map_err(|err| format!("removing the capture meta side file: {err}"))?;
    Ok((offset, chunks.len() as u64))
}

/// Persist the hook stream: payloads verbatim (one JSON line each — the
/// side channel's native shape, exactly what a replay feeds a listener) and
/// a timing sidecar tying each line to the shared spawn-relative clock.
fn write_hook_artifacts(out: &Path, events: &[HookEvent], t0: Instant) -> Result<u64, String> {
    let mut payloads = BufWriter::new(
        File::create(out.join("hook-payloads.ndjson"))
            .map_err(|err| format!("hook-payloads.ndjson: {err}"))?,
    );
    let mut timing = BufWriter::new(
        File::create(out.join("hook-payloads.timing.ndjson"))
            .map_err(|err| format!("hook-payloads.timing.ndjson: {err}"))?,
    );
    for (index, event) in events.iter().enumerate() {
        let line = serde_json::to_string(&event.payload)
            .map_err(|err| format!("serializing hook payload {}: {err}", index + 1))?;
        payloads
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|err| format!("hook-payloads.ndjson: {err}"))?;
        let record = serde_json::json!({
            "line": index + 1,
            "monotonic_ns": event.at.saturating_duration_since(t0).as_nanos() as u64,
        });
        timing
            .write_all(format!("{record}\n").as_bytes())
            .map_err(|err| format!("hook-payloads.timing.ndjson: {err}"))?;
    }
    payloads
        .flush()
        .map_err(|err| format!("hook-payloads.ndjson: {err}"))?;
    timing
        .flush()
        .map_err(|err| format!("hook-payloads.timing.ndjson: {err}"))?;
    Ok(events.len() as u64)
}

/// Double-quote a string for the manifest. Backslash and quote are escaped,
/// and control characters become YAML escapes — `cli_version` comes from a
/// CLI's own `--version` output and `install` from a flag, and a stray
/// newline or ESC in either must not be able to break the manifest's
/// structure or smuggle raw control bytes into a committed file.
fn yaml_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        match c {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            c if c.is_control() => quoted.push_str(&format!("\\u{:04X}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Emit the fixture manifest. Sizes are bytes for every artifact — the
/// number the corpus budget is accounted in. `tier: ci` states how CI
/// consumes the fixture (replay lanes are PR-tier; the capture that made
/// it was not, but the capture is not re-run from the manifest). The
/// install mechanism appears only when the invoker supplied one — an
/// omitted line is honest, a made-up placeholder is not.
fn write_manifest(
    out: &Path,
    script: &RecordScript,
    cli_version: &str,
    install: Option<&str>,
    cols: u16,
    rows: u16,
) -> Result<String, String> {
    let captured_on = utc_date(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since_epoch| since_epoch.as_secs()),
    );
    let mut yaml = String::new();
    yaml.push_str(
        "# Captured-session fixture recorded by `interactive-probe record`.\n\
         # Generated file — re-recording overwrites it; do not hand-edit.\n",
    );
    yaml.push_str(&format!("cli: {}\n", script.cli));
    yaml.push_str(&format!("cli_version: {}\n", yaml_quote(cli_version)));
    if let Some(install) = install {
        yaml.push_str(&format!("install: {}\n", yaml_quote(install)));
    }
    yaml.push_str(&format!("scenario: {}\n", script.name));
    yaml.push_str(&format!(
        "description: {}\n",
        yaml_quote(&script.description)
    ));
    yaml.push_str(&format!("os: {}\n", std::env::consts::OS));
    yaml.push_str(&format!("cols: {cols}\n"));
    yaml.push_str(&format!("rows: {rows}\n"));
    yaml.push_str(&format!("captured_on: {captured_on}\n"));
    yaml.push_str("tier: ci\n");
    yaml.push_str("artifacts:\n");
    let mut summary = Vec::new();
    for name in ARTIFACT_FILES {
        if name == "manifest.yaml" {
            continue;
        }
        let path = out.join(name);
        if let Ok(meta) = std::fs::metadata(&path) {
            yaml.push_str(&format!("  {name}: {}\n", meta.len()));
            summary.push(format!("{name}={}B", meta.len()));
        }
    }
    std::fs::write(out.join("manifest.yaml"), yaml)
        .map_err(|err| format!("manifest.yaml: {err}"))?;
    Ok(summary.join(" "))
}

// ---------------------------------------------------------------------------
// The lane.
// ---------------------------------------------------------------------------

/// Load the script, prepare the fixture directory, run the session through
/// the matching profile, and convert the recording into the committed
/// artifact set. Exit codes: 80 script, 81 alloc/output-dir, 82 generic
/// spawn, 83 step, 84 epilogue/teardown, 85 artifacts; the claude profile's
/// launch and shutdown keep the rig's own codes (30–43).
pub fn run(config: &RecordConfig) -> Result<(), Failure> {
    let text = std::fs::read_to_string(&config.script).map_err(|err| {
        Failure::new(
            "script",
            80,
            format!("reading {} failed: {err}", config.script.display()),
        )
    })?;
    let script_dir = script_dir_of(&config.script);
    let script = parse_script(&text, &script_dir).map_err(|detail| {
        Failure::new(
            "script",
            80,
            format!("{}: {detail}", config.script.display()),
        )
    })?;

    // A misfiled script must fail here, with both names stated — not
    // produce fixtures whose manifest contradicts the directory the
    // invoker filed them under.
    if let Some(expected) = &config.expect_cli
        && script.cli != *expected
    {
        return Err(Failure::new(
            "script",
            80,
            format!(
                "{}: the script records cli \"{}\", but the invoker expects \"{expected}\"",
                config.script.display(),
                script.cli
            ),
        ));
    }
    if let Some(stem) = config
        .script
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".record.json"))
        && stem != script.name
    {
        return Err(Failure::new(
            "script",
            80,
            format!(
                "{}: the file stem \"{stem}\" and the script name \"{}\" disagree — the campaign derives fixture directories from the stem and the manifest from the name, and they must not diverge",
                config.script.display(),
                script.name
            ),
        ));
    }

    if script.is_claude() {
        if config.cli_version.is_some() {
            return Err(Failure::new(
                "args",
                2,
                "--cli-version is not accepted for a claude script: the manifest reports what `claude --version` answers",
            ));
        }
    } else {
        // The version label is provenance in a committed manifest, so
        // presence alone is not enough: an empty or control-charactered
        // value would record meaningless — or structure-breaking — text as
        // what the fixture was captured from.
        if !config
            .cli_version
            .as_deref()
            .is_some_and(is_printable_label)
        {
            return Err(Failure::new(
                "args",
                2,
                "a generic script needs a non-empty, single-line --cli-version: the fixture manifest must name what it recorded",
            ));
        }
        if config.model.is_some() {
            return Err(Failure::new(
                "args",
                2,
                "--model is only meaningful for a claude script",
            ));
        }
    }

    std::fs::create_dir_all(&config.out).map_err(|err| {
        Failure::new(
            "output_dir",
            81,
            format!("creating {} failed: {err}", config.out.display()),
        )
    })?;
    for stale in ARTIFACT_FILES
        .iter()
        .copied()
        .chain([CAPTURE_INTERMEDIATE, "capture-meta.json"])
    {
        let path = config.out.join(stale);
        if let Err(err) = std::fs::remove_file(&path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            return Err(Failure::new(
                "output_dir",
                81,
                format!("removing stale {} failed: {err}", path.display()),
            ));
        }
    }
    print_step(
        "script",
        "pass",
        &format!(
            "{} — cli={} steps={} at {}x{} into {}",
            script.name,
            script.cli,
            script.steps.len(),
            config.cols,
            config.rows,
            config.out.display(),
        ),
    );

    let cli_version = if script.is_claude() {
        run_claude(config, &script)?
    } else {
        run_generic(config, &script)?
    };

    let (input_bytes, chunks) = write_input_artifacts(&config.out)
        .map_err(|detail| Failure::new("artifacts", 85, detail))?;
    let summary = write_manifest(
        &config.out,
        &script,
        &cli_version,
        config.install.as_deref(),
        config.cols,
        config.rows,
    )
    .map_err(|detail| Failure::new("artifacts", 85, detail))?;
    print_step(
        "artifacts",
        "pass",
        &format!("input.bytes={input_bytes}B over {chunks} chunks; {summary}"),
    );
    Ok(())
}

/// A value fit to stand as provenance in the manifest: non-empty once
/// trimmed, and free of control characters.
fn is_printable_label(value: &str) -> bool {
    !value.trim().is_empty() && !value.chars().any(char::is_control)
}

/// The directory `{script_dir}` expands to: the script's parent, absolute
/// and never in Windows' verbatim (`\\?\`) form. Verbatim paths switch off
/// separator normalization, so an arg composed textually as
/// `{script_dir}/name.json` would mix separators into a path the OS
/// rejects outright — and the caller can hand this lane a verbatim script
/// path legitimately, since `canonicalize` produces one on Windows. So the
/// expansion is made absolute without canonicalizing *and* any verbatim
/// prefix already on the input is rebuilt into the normalizing drive/UNC
/// form.
fn script_dir_of(script: &Path) -> PathBuf {
    let dir = script
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    strip_verbatim(std::path::absolute(&dir).unwrap_or(dir))
}

/// Rebuild a verbatim-prefixed Windows path into its normalizing
/// equivalent: `\\?\C:\x` → `C:\x`, `\\?\UNC\srv\share\x` →
/// `\\srv\share\x`. Anything else — including every POSIX path, which
/// never carries a prefix component — passes through untouched. Only the
/// plain disk and UNC forms are rebuilt; exotic prefixes (device
/// namespaces) have no normalizing equivalent to rebuild into.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};

    let mut components = path.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return path;
    };
    let root = match prefix.kind() {
        Prefix::VerbatimDisk(disk) => format!("{}:\\", char::from(disk)),
        Prefix::VerbatimUNC(server, share) => format!(
            "\\\\{}\\{}\\",
            server.to_string_lossy(),
            share.to_string_lossy()
        ),
        _ => return path,
    };
    let mut rebuilt = PathBuf::from(root);
    for component in components {
        if !matches!(component, Component::RootDir) {
            rebuilt.push(component.as_os_str());
        }
    }
    rebuilt
}

/// What the intermediate capture (and the claude rig's capture meta) labels
/// the session with.
fn scenario_label(script: &RecordScript) -> String {
    format!("{}: {}", script.name, script.description)
}

fn run_claude(config: &RecordConfig, script: &RecordScript) -> Result<String, Failure> {
    let probe = ProbeConfig {
        claude_bin: config
            .cli_bin
            .clone()
            .unwrap_or_else(|| "claude".to_string()),
        model: config.model.clone(),
        first_token_ms: config.first_token_ms,
        capture_to: Some(config.out.join(CAPTURE_INTERMEDIATE)),
        keep_workdir: config.keep_workdir,
        cols: config.cols,
        rows: config.rows,
    };
    let label = scenario_label(script);
    let mut session = crate::rig::launch(&probe)?;
    let cli_version = session.cli_version.clone();
    match drive_claude(&mut session, config, script) {
        Ok(()) => {
            session.conclude(&label)?;
            Ok(cli_version)
        }
        Err(failure) => {
            session.abandon(&label, &failure);
            Err(failure)
        }
    }
}

/// Establish, run the steps, type the `/exit` epilogue, and persist the
/// side-channel artifacts — everything that needs the session alive. The
/// child's exit is confirmed here, so the caller's `conclude` finds an
/// already-exited child on the success path.
fn drive_claude(
    session: &mut LiveSession,
    config: &RecordConfig,
    script: &RecordScript,
) -> Result<(), Failure> {
    let info = session.establish()?;
    let t0 = session.tracker.clock.spawn_instant();
    let mut log = StepLog::create(&config.out.join("steps.ndjson"), t0)
        .map_err(|err| Failure::new("output_dir", 81, format!("steps.ndjson: {err}")))?;

    {
        let mut host = Host::Claude {
            session,
            hook_cursor: 0,
        };
        execute_steps(&mut host, &script.steps, &mut log)?;
    }

    // The graceful epilogue, logged like any other driver action: `/exit`
    // typed, SessionEnd as the structured proof it was accepted, then the
    // process's own exit as the separate proof it left.
    let mark = session.hook_mark();
    let started = Instant::now();
    session
        .writer
        .type_line("/exit", TYPE_SETTLE)
        .map_err(|err| Failure::new("exit", 84, format!("typing /exit failed: {err}")))?;
    session
        .wait_for_hook("SessionEnd", mark, EPILOGUE_TIMEOUT)
        .map_err(|detail| Failure::new("exit", 84, detail))?;
    let exit_detail = session
        .await_child_exit(EPILOGUE_TIMEOUT)
        .map_err(|detail| Failure::new("exit", 84, detail))?;
    log.record(
        started,
        started.elapsed(),
        "exit",
        &[("text", "/exit".into())],
        None,
        "ok",
    )
    .map_err(|err| Failure::new("exit", 84, format!("writing the step log failed: {err}")))?;
    print_step("exit", "pass", &format!("/exit accepted; {exit_detail}"));

    // `/clear` starts a *new* transcript file, advertised by a fresh
    // SessionStart over the hook channel — so the file to keep is the one
    // the latest SessionStart names, not the one launch discovered. The
    // earlier path (and the switch itself) stays visible in the recorded
    // hook payloads, which is exactly the evidence a tailer prototype needs.
    let events = session.hook_events_since(0);
    let transcript_path = events
        .iter()
        .rev()
        .find(|event| event.name == "SessionStart")
        .and_then(|event| event.payload.get("transcript_path"))
        .and_then(|value| value.as_str())
        .map_or_else(|| info.transcript_path.clone(), PathBuf::from);
    let hook_count = write_hook_artifacts(&config.out, events, t0)
        .map_err(|detail| Failure::new("artifacts", 85, detail))?;
    std::fs::copy(&transcript_path, config.out.join("transcript.jsonl")).map_err(|err| {
        Failure::new(
            "artifacts",
            85,
            format!(
                "copying the transcript from {} failed: {err} — without it the fixture is missing a primary channel",
                transcript_path.display()
            ),
        )
    })?;
    print_step(
        "side_channels",
        "pass",
        &format!(
            "{hook_count} hook payloads; transcript copied from {}",
            transcript_path.display()
        ),
    );
    Ok(())
}

fn run_generic(config: &RecordConfig, script: &RecordScript) -> Result<String, Failure> {
    // Validated before launch in `run`; the expect is unreachable.
    let cli_version = config
        .cli_version
        .clone()
        .expect("generic profile requires --cli-version");
    let mut session = GenericSession::launch(config, script)?;
    let t0 = session.tracker.clock.spawn_instant();
    let mut log = StepLog::create(&config.out.join("steps.ndjson"), t0)
        .map_err(|err| Failure::new("output_dir", 81, format!("steps.ndjson: {err}")))?;

    let outcome = {
        let mut host = Host::Generic(&mut session);
        execute_steps(&mut host, &script.steps, &mut log)
    };
    match outcome {
        Ok(()) => {
            session.finish(
                &cli_version,
                config.cols,
                config.rows,
                &scenario_label(script),
            )?;
            Ok(cli_version)
        }
        Err(failure) => {
            session.abandon(&failure);
            Err(failure)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(json: &str) -> RecordScript {
        parse_script(json, Path::new("/fixtures")).expect("script must parse")
    }

    fn parse_err(json: &str) -> String {
        parse_script(json, Path::new("/fixtures")).expect_err("script must be rejected")
    }

    fn generic_header() -> &'static str {
        r#""name":"demo","description":"a demo","cli":"fake""#
    }

    #[test]
    fn the_reference_generic_script_parses() {
        let script = parse_ok(&format!(
            r#"{{{},
              "args": ["{{script_dir}}/demo.fake.json"],
              "steps": [
                {{ "wait_text": "ready", "timeout_ms": 5000 }},
                {{ "type_line": "y", "label": "answer-yes" }},
                {{ "press": "ctrl-c" }},
                {{ "pause_ms": 100 }},
                {{ "wait_quiet_ms": 500, "timeout_ms": 10000, "label": "stream-over" }}
              ]
            }}"#,
            generic_header()
        ));
        assert_eq!(script.name, "demo");
        assert!(!script.is_claude());
        assert_eq!(
            script.args,
            vec!["/fixtures/demo.fake.json".to_string()],
            "{{script_dir}} must be substituted"
        );
        assert_eq!(script.steps.len(), 5);
        assert_eq!(script.steps[1].label.as_deref(), Some("answer-yes"));
        match &script.steps[2].kind {
            StepKind::Press { key, bytes } => {
                assert_eq!(key, "ctrl-c");
                assert_eq!(bytes, &vec![0x03]);
            }
            _ => panic!("step 3 must be a press"),
        }
    }

    #[test]
    fn claude_only_steps_are_rejected_in_a_generic_script() {
        for step in [
            r#"{ "wait_hook": "Stop", "timeout_ms": 1000 }"#,
            r#"{ "set_decision": "allow" }"#,
        ] {
            let err = parse_err(&format!(r#"{{{}, "steps": [{step}]}}"#, generic_header()));
            assert!(
                err.contains("claude hook channel"),
                "step {step} must be rejected for a generic cli: {err}"
            );
        }
    }

    #[test]
    fn a_claude_script_accepts_hook_steps_and_rejects_args() {
        let script = parse_ok(
            r#"{"name":"turn","description":"one turn","cli":"claude",
                "steps":[
                  {"set_decision":"deny"},
                  {"type_line":"list files","label":"tool-turn"},
                  {"wait_hook":"Stop","timeout_ms":120000}
                ]}"#,
        );
        assert!(script.is_claude());
        assert_eq!(script.steps.len(), 3);

        let err = parse_err(
            r#"{"name":"turn","description":"one turn","cli":"claude",
                "args":["--whatever"],
                "steps":[{"pause_ms":1}]}"#,
        );
        assert!(err.contains("\"args\""), "unexpected error: {err}");
    }

    #[test]
    fn ambiguous_and_unknown_step_kinds_are_named() {
        let err = parse_err(&format!(
            r#"{{{}, "steps": [{{"type_line":"x","press":"enter"}}]}}"#,
            generic_header()
        ));
        assert!(err.contains("ambiguous"), "unexpected error: {err}");

        let err = parse_err(&format!(
            r#"{{{}, "steps": [{{"explode":true}}]}}"#,
            generic_header()
        ));
        assert!(err.contains("\"explode\""), "unexpected error: {err}");
    }

    #[test]
    fn waits_require_their_timeout() {
        for step in [r#"{"wait_text":"x"}"#, r#"{"wait_quiet_ms":100}"#] {
            let err = parse_err(&format!(r#"{{{}, "steps": [{step}]}}"#, generic_header()));
            assert!(
                err.contains("timeout_ms"),
                "step {step} must demand a timeout: {err}"
            );
        }
    }

    #[test]
    fn unknown_fields_on_a_step_are_rejected() {
        let err = parse_err(&format!(
            r#"{{{}, "steps": [{{"pause_ms":5,"color":"red"}}]}}"#,
            generic_header()
        ));
        assert!(err.contains("\"color\""), "unexpected error: {err}");
    }

    #[test]
    fn names_are_held_to_kebab_case() {
        for bad in ["Demo", "demo scenario", "-demo", "demo-"] {
            let err = parse_err(&format!(
                r#"{{"name":"{bad}","description":"d","cli":"fake","steps":[{{"pause_ms":1}}]}}"#
            ));
            assert!(err.contains("kebab-case"), "{bad} must be rejected: {err}");
        }
    }

    #[test]
    fn descriptions_are_single_line() {
        let err = parse_err(
            r#"{"name":"d","description":"two\nlines","cli":"fake","steps":[{"pause_ms":1}]}"#,
        );
        assert!(err.contains("single line"), "unexpected error: {err}");
    }

    #[test]
    fn key_map_covers_named_keys_and_single_characters() {
        assert_eq!(key_bytes("enter"), Some(b"\r".to_vec()));
        assert_eq!(key_bytes("down"), Some(b"\x1b[B".to_vec()));
        assert_eq!(key_bytes("ctrl-c"), Some(vec![0x03]));
        assert_eq!(key_bytes("2"), Some(b"2".to_vec()));
        assert_eq!(key_bytes("y"), Some(b"y".to_vec()));
        assert_eq!(key_bytes("F1"), None, "multi-char non-names are rejected");
        assert_eq!(key_bytes("é"), None, "non-ASCII keys are rejected");
        assert_eq!(key_bytes(""), None);
    }

    #[test]
    fn unknown_decisions_and_keys_are_rejected_at_parse() {
        let err = parse_err(
            r#"{"name":"d","description":"d","cli":"claude","steps":[{"set_decision":"maybe"}]}"#,
        );
        assert!(err.contains("\"maybe\""), "unexpected error: {err}");

        let err = parse_err(&format!(
            r#"{{{}, "steps": [{{"press":"super-key"}}]}}"#,
            generic_header()
        ));
        assert!(err.contains("\"super-key\""), "unexpected error: {err}");
    }

    #[test]
    fn the_script_dir_is_absolute_but_never_verbatim() {
        // `{script_dir}` is substituted textually and the script's args
        // append `/name.json` to it. A Windows verbatim (\\?\) path — what
        // canonicalize returns there — switches off separator
        // normalization, so that mix names an invalid path. The drive-letter
        // absolute form tolerates it; POSIX absolutes are unaffected.
        let dir = script_dir_of(Path::new(
            "tests/capture-scenarios/fake/roundtrip.record.json",
        ));
        assert!(dir.is_absolute(), "must be absolute: {}", dir.display());
        assert!(
            !dir.to_string_lossy().starts_with(r"\\?\"),
            "must never be a verbatim path: {}",
            dir.display()
        );
        assert!(dir.ends_with("fake"), "unexpected dir: {}", dir.display());

        // A bare file name has no parent directory: the invoker's cwd is
        // the honest expansion.
        let bare = script_dir_of(Path::new("script.json"));
        assert!(bare.is_absolute(), "must be absolute: {}", bare.display());

        // The caller may hand in an already-canonicalized script path —
        // verbatim on Windows. The expansion must strip that form, not
        // merely avoid creating it; this is exactly the case the Windows
        // CI lane hit.
        let canonical = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("Cargo.toml")
            .canonicalize()
            .expect("the package manifest exists");
        let dir = script_dir_of(&canonical);
        assert!(dir.is_absolute(), "must be absolute: {}", dir.display());
        assert!(
            !dir.to_string_lossy().starts_with(r"\\?\"),
            "a canonicalized input must not stay verbatim: {}",
            dir.display()
        );
        assert!(
            dir.ends_with("interactive-probe"),
            "unexpected dir: {}",
            dir.display()
        );
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_prefixes_are_rebuilt_into_normalizing_forms() {
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\C:\work\fixtures")),
            PathBuf::from(r"C:\work\fixtures")
        );
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\?\UNC\srv\share\fixtures")),
            PathBuf::from(r"\\srv\share\fixtures")
        );
        // A device-namespace prefix has no normalizing equivalent — it must
        // pass through rather than be mangled.
        assert_eq!(
            strip_verbatim(PathBuf::from(r"\\.\COM1")),
            PathBuf::from(r"\\.\COM1")
        );
    }

    #[test]
    fn yaml_quoting_escapes_controls_not_just_quotes() {
        assert_eq!(yaml_quote("plain 1.2.3"), "\"plain 1.2.3\"");
        assert_eq!(yaml_quote(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        // A CLI's --version output can carry a trailing newline or worse;
        // none of it may reach the manifest as a raw control byte.
        assert_eq!(yaml_quote("2.1.201\n"), "\"2.1.201\\n\"");
        assert_eq!(yaml_quote("a\tb\r"), "\"a\\tb\\r\"");
        assert_eq!(yaml_quote("esc\u{1b}[31m"), "\"esc\\u001B[31m\"");
    }

    #[test]
    fn provenance_labels_must_be_printable_and_non_empty() {
        assert!(is_printable_label("0.142.5"));
        assert!(is_printable_label("workspace build (cargo)"));
        for bad in ["", "   ", "\t", "1.0\n", "a\u{1b}b"] {
            assert!(!is_printable_label(bad), "{bad:?} must be rejected");
        }
    }

    #[test]
    fn misfiled_scripts_fail_before_any_launch() {
        let dir = std::env::temp_dir().join(format!(
            "agent-bridge-record-test-{}-misfiled",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("roundtrip.record.json");
        std::fs::write(
            &script,
            r#"{"name":"roundtrip","description":"d","cli":"fake","steps":[{"pause_ms":1}]}"#,
        )
        .unwrap();

        // The invoker (the campaign) expects a different CLI: the failure
        // names both sides, before any child is spawned.
        let config = RecordConfig {
            script: script.clone(),
            out: dir.join("out"),
            expect_cli: Some("claude".to_string()),
            ..RecordConfig::default()
        };
        let failure = run(&config).expect_err("a cli mismatch must fail");
        assert_eq!(failure.step, "script");
        assert!(
            failure.detail.contains("\"fake\"") && failure.detail.contains("\"claude\""),
            "both names must be stated: {}",
            failure.detail
        );

        // The file stem and the script name disagree: fixture directories
        // derive from the stem and manifests from the name, so divergence
        // is refused up front.
        let misnamed = dir.join("misnamed.record.json");
        std::fs::copy(&script, &misnamed).unwrap();
        let config = RecordConfig {
            script: misnamed,
            out: dir.join("out"),
            ..RecordConfig::default()
        };
        let failure = run(&config).expect_err("a stem/name mismatch must fail");
        assert_eq!(failure.step, "script");
        assert!(
            failure.detail.contains("misnamed") && failure.detail.contains("roundtrip"),
            "both the stem and the name must be stated: {}",
            failure.detail
        );

        assert!(
            !dir.join("out").exists(),
            "a refused script must not have touched the fixture directory"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn input_artifacts_reproduce_the_capture_bytes_and_boundaries() {
        let out = std::env::temp_dir().join(format!(
            "agent-bridge-record-test-{}-input",
            std::process::id()
        ));
        std::fs::create_dir_all(&out).unwrap();
        let t0 = Instant::now();
        let mut writer = CaptureWriter::create(&out.join(CAPTURE_INTERMEDIATE), t0).unwrap();
        let chunks: &[(u64, &[u8])] = &[
            (1_000, b"\x1b[2Jhello "),
            (2_000_000, "w\u{f6}rld".as_bytes()),
            (3_000_000_000, &[0xFF, 0x00]),
        ];
        for (t_ns, data) in chunks {
            writer
                .record(t0 + Duration::from_nanos(*t_ns), data)
                .unwrap();
        }
        writer
            .finish("test", 80, 24, "2026-07-12".into(), "unit")
            .unwrap();

        let (bytes, count) = write_input_artifacts(&out).expect("conversion must succeed");
        assert_eq!(count, 3);
        let expected: Vec<u8> = chunks.iter().flat_map(|(_, d)| d.iter().copied()).collect();
        assert_eq!(bytes, expected.len() as u64);
        assert_eq!(
            std::fs::read(out.join("input.bytes")).unwrap(),
            expected,
            "input.bytes must be the exact concatenation"
        );

        let timing = std::fs::read_to_string(out.join("input.timing.ndjson")).unwrap();
        let records: Vec<serde_json::Value> = timing
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0]["offset"], 0);
        assert_eq!(records[1]["offset"], chunks[0].1.len() as u64);
        assert_eq!(records[2]["monotonic_ns"], 3_000_000_000u64);

        assert!(
            !out.join(CAPTURE_INTERMEDIATE).exists() && !out.join("capture-meta.json").exists(),
            "the intermediates must not survive into the fixture"
        );
        std::fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn the_manifest_names_the_session_and_sizes_what_exists() {
        let out = std::env::temp_dir().join(format!(
            "agent-bridge-record-test-{}-manifest",
            std::process::id()
        ));
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("input.bytes"), b"12345").unwrap();
        std::fs::write(out.join("steps.ndjson"), b"{}\n").unwrap();

        let script = parse_ok(&format!(
            r#"{{{}, "steps": [{{"pause_ms":1}}]}}"#,
            generic_header()
        ));
        write_manifest(
            &out,
            &script,
            "0.1 \"quoted\"",
            Some("npm some-cli@0.1"),
            120,
            40,
        )
        .unwrap();
        let manifest = std::fs::read_to_string(out.join("manifest.yaml")).unwrap();
        assert!(manifest.contains("cli: fake"), "{manifest}");
        assert!(
            manifest.contains(r#"cli_version: "0.1 \"quoted\"""#),
            "quotes in the version must be escaped: {manifest}"
        );
        assert!(
            manifest.contains(r#"install: "npm some-cli@0.1""#),
            "the install mechanism must be recorded: {manifest}"
        );
        assert!(manifest.contains("scenario: demo"), "{manifest}");
        assert!(manifest.contains("cols: 120"), "{manifest}");
        assert!(manifest.contains("rows: 40"), "{manifest}");
        assert!(manifest.contains("tier: ci"), "{manifest}");
        assert!(manifest.contains("  input.bytes: 5"), "{manifest}");
        assert!(manifest.contains("  steps.ndjson: 3"), "{manifest}");
        assert!(
            !manifest.contains("transcript.jsonl"),
            "absent artifacts must not be listed: {manifest}"
        );

        // No install mechanism supplied: the line is omitted — an honest
        // absence, never a placeholder value.
        write_manifest(&out, &script, "0.1", None, 80, 24).unwrap();
        let manifest = std::fs::read_to_string(out.join("manifest.yaml")).unwrap();
        assert!(!manifest.contains("install:"), "{manifest}");
        std::fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn the_step_log_carries_seq_time_label_and_outcome() {
        let dir = std::env::temp_dir().join(format!(
            "agent-bridge-record-test-{}-steplog",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("steps.ndjson");
        let t0 = Instant::now();
        let mut log = StepLog::create(&path, t0).unwrap();
        log.record(
            t0 + Duration::from_millis(5),
            Duration::from_millis(350),
            "type_line",
            &[("text", "hello".into())],
            Some("prompt"),
            "ok",
        )
        .unwrap();
        log.record(
            t0 + Duration::from_millis(400),
            Duration::from_millis(10),
            "press",
            &[("key", "down".into())],
            None,
            "failed: gone",
        )
        .unwrap();

        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["seq"], 1);
        assert_eq!(lines[0]["step"], "type_line");
        assert_eq!(lines[0]["label"], "prompt");
        assert_eq!(lines[0]["outcome"], "ok");
        assert_eq!(lines[0]["took_ms"], 350);
        assert_eq!(lines[1]["seq"], 2);
        assert!(lines[1].get("label").is_none());
        assert_eq!(lines[1]["outcome"], "failed: gone");
        assert!(
            lines[1]["t_ns"].as_u64().unwrap() > lines[0]["t_ns"].as_u64().unwrap(),
            "timestamps must be monotonic on the shared clock"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
