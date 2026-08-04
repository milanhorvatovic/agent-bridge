//! The recorded-workload lane: real CLI traffic, replayed at its captured
//! pacing.
//!
//! The synthetic lanes stream evenly, and even load is the load a terminal
//! layer finds easiest: buffers drain as fast as they fill, nothing idles
//! long enough to be reclaimed, nothing bursts hard enough to queue. Real
//! CLI traffic does the opposite — it bursts while tokens stream, goes quiet
//! while the model thinks, and slams out a screenful when a diff paints. A
//! path can pass every even-load check and still fail the real shape, which
//! is why this lane replays recordings of real sessions rather than turning
//! the synthetic rate knob up and down.
//!
//! The recordings are the committed capture corpus: byte streams read from a
//! real PTY hosting a real CLI, with the boundary and arrival time of every
//! read. A replay run compiles a **plan** from one or more of them — every
//! chunk's size and every inter-chunk gap, looped end to end until the plan
//! covers the requested duration — and a child binary performs it against
//! the shared monotonic clock. Idle gaps above a threshold can be shortened
//! by a stated divisor so a lane fits its budget; the shortening is recorded
//! in the report, and it never touches the gaps *below* the threshold — the
//! burst structure — because that structure is the point of the lane.
//!
//! What fills the chunks is a choice between two honest options:
//!
//! - **recorded** — the captured bytes themselves. The strongest form: the
//!   probe knows the exact byte sequence that went in, so verification is
//!   byte-for-byte and a divergence is reported at its offset. It requires
//!   the terminal to be a transparent pipe, so the replay child turns off
//!   output post-processing on its side; on a terminal that re-renders
//!   rather than pipes (ConPTY), byte identity is not a property even an
//!   uncorrupted stream has, and the lane refuses the combination rather
//!   than reporting rendering as corruption.
//! - **generated** — the recording's chunk sizes and gaps, filled with the
//!   generator's line stream. Content becomes verifiable on every terminal
//!   (the line verifier already speaks repaint), at the cost of not being
//!   the captured bytes; the pacing, which is what makes the workload
//!   bimodal, is untouched.
//!
//! Both modes ship in the report with their names attached, so a reader
//! always knows which claim a green run is making.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_bridge_fake_cli::generator::{
    Line, Rolling, checksum_line, parse_line, write_payload_line,
};
use serde::{Deserialize, Serialize};

use crate::clock::{Anchor, monotonic_ns};
use crate::lines::LineSplitter;
use crate::monitor::{self, Monitor};
use crate::report::{Budget, Measurement, Report};
use crate::session::{self, ScenarioFile, Session, sibling_binary};
use crate::verify::Verifier;
use crate::{human_bytes, human_ns, print_step};

/// Longest the lane waits with nothing arriving. Idle periods in real
/// recordings run to a minute or more, so this is well above any gap a plan
/// can schedule after compression — see `plan_max_gap` where the bound is
/// enforced.
const STALL: Duration = Duration::from_secs(120);

/// How long past its scheduled end the child gets before the lane stops
/// waiting.
const OVERRUN_GRACE: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Recorded,
    Generated,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Recorded => "recorded",
            Mode::Generated => "generated",
        }
    }
}

/// One captured fixture's pacing and bytes, loaded from a corpus directory
/// (`input.bytes` + `input.timing.ndjson`, dimensions from the directory
/// name).
pub struct Fixture {
    pub name: String,
    pub cols: u16,
    pub rows: u16,
    /// (gap from the previous chunk in ns, chunk length). The first entry's
    /// gap is the recording's lead-in — spawn to first output.
    pub chunks: Vec<(u64, u32)>,
    pub bytes: Vec<u8>,
}

impl Fixture {
    pub fn load(dir: &Path) -> Result<Self, String> {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("{}: non-UTF-8 fixture name", dir.display()))?
            .to_string();
        let (_, dims) = name
            .rsplit_once('-')
            .ok_or_else(|| format!("{name}: fixture name lacks a -<cols>x<rows> suffix"))?;
        let (cols, rows) = dims
            .split_once('x')
            .and_then(|(c, r)| Some((c.parse().ok()?, r.parse().ok()?)))
            .ok_or_else(|| format!("{name}: cannot parse dimensions from {dims:?}"))?;

        let bytes_path = dir.join("input.bytes");
        let bytes =
            std::fs::read(&bytes_path).map_err(|err| format!("{}: {err}", bytes_path.display()))?;
        let timing_path = dir.join("input.timing.ndjson");
        let timing = std::fs::read_to_string(&timing_path)
            .map_err(|err| format!("{}: {err}", timing_path.display()))?;

        #[derive(Deserialize)]
        struct TimingRecord {
            offset: u64,
            monotonic_ns: u64,
        }
        let mut records: Vec<TimingRecord> = Vec::new();
        for (index, line) in timing.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            records.push(
                serde_json::from_str(line)
                    .map_err(|err| format!("{}:{}: {err}", timing_path.display(), index + 1))?,
            );
        }
        if records.is_empty() || records[0].offset != 0 {
            return Err(format!(
                "{}: timing must start at offset 0 and cover the stream",
                timing_path.display()
            ));
        }

        let mut chunks = Vec::with_capacity(records.len());
        let mut previous_t = 0u64;
        for (index, record) in records.iter().enumerate() {
            let start = record.offset as usize;
            let end = records
                .get(index + 1)
                .map_or(bytes.len(), |next| next.offset as usize);
            if end <= start || end > bytes.len() {
                return Err(format!(
                    "{}: record {} spans {start}..{end} outside 0..{} or is empty",
                    timing_path.display(),
                    index + 1,
                    bytes.len()
                ));
            }
            if record.monotonic_ns < previous_t {
                return Err(format!(
                    "{}: record {} goes backwards in time",
                    timing_path.display(),
                    index + 1
                ));
            }
            chunks.push((record.monotonic_ns - previous_t, (end - start) as u32));
            previous_t = record.monotonic_ns;
        }
        Ok(Self {
            name,
            cols,
            rows,
            chunks,
            bytes,
        })
    }
}

/// A compiled replay: what the child performs and the probe verifies. The
/// two read it from the same file, which is what entitles the probe to call
/// a divergence corruption rather than a disagreement.
#[derive(Debug)]
pub struct Plan {
    pub mode: Mode,
    pub line_bytes: usize,
    pub checksum_every: u64,
    /// (gap since previous chunk in ns, chunk length).
    pub entries: Vec<(u64, u32)>,
    /// The chunk contents, concatenated — recorded mode only.
    pub bytes: Vec<u8>,
    /// Fixture names, in playlist order, for the report.
    pub sources: Vec<String>,
    /// Terminal dimensions the recorded bytes were painted for.
    pub cols: u16,
    pub rows: u16,
    /// Idle handling, for the report.
    pub idle_threshold: Duration,
    pub idle_divisor: u64,
}

pub struct BuildOptions {
    pub mode: Mode,
    pub duration: Duration,
    /// Gaps above the threshold are shortened; gaps below it — the burst
    /// structure — are never touched.
    pub idle_threshold: Duration,
    /// What a long gap's excess is divided by. 1 replays idle in full.
    pub idle_divisor: u64,
    pub line_bytes: usize,
    pub checksum_every: u64,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            mode: Mode::Generated,
            duration: Duration::from_secs(30 * 60),
            idle_threshold: Duration::from_secs(2),
            idle_divisor: 1,
            line_bytes: agent_bridge_fake_cli::generator::DEFAULT_LINE_BYTES,
            checksum_every: 1000,
        }
    }
}

/// Compile a plan: loop the fixtures end to end until the scheduled time
/// covers the duration, compressing idle gaps as configured.
pub fn build_plan(fixtures: &[Fixture], options: &BuildOptions) -> Result<Plan, String> {
    if fixtures.is_empty() {
        return Err("a replay needs at least one fixture".to_string());
    }
    if options.idle_divisor == 0 {
        return Err("the idle divisor must be at least 1".to_string());
    }
    let threshold_ns = options.idle_threshold.as_nanos() as u64;
    let target_ns = options.duration.as_nanos() as u64;

    let mut entries = Vec::new();
    let mut bytes = Vec::new();
    let mut scheduled_ns = 0u64;
    'fill: loop {
        let before = scheduled_ns;
        for fixture in fixtures {
            let mut offset = 0usize;
            for (gap_ns, len) in &fixture.chunks {
                let gap_ns = compress_gap(*gap_ns, threshold_ns, options.idle_divisor);
                entries.push((gap_ns, *len));
                scheduled_ns += gap_ns;
                if options.mode == Mode::Recorded {
                    bytes.extend_from_slice(&fixture.bytes[offset..offset + *len as usize]);
                }
                offset += *len as usize;
                if scheduled_ns >= target_ns {
                    break 'fill;
                }
            }
        }
        if scheduled_ns == before {
            return Err(
                "the fixtures schedule no time at all — a plan of instantaneous chunks would \
                 never reach its duration"
                    .to_string(),
            );
        }
    }

    let plan = Plan {
        mode: options.mode,
        line_bytes: options.line_bytes,
        checksum_every: options.checksum_every,
        entries,
        bytes,
        sources: fixtures.iter().map(|f| f.name.clone()).collect(),
        cols: fixtures.iter().map(|f| f.cols).max().unwrap_or(80),
        rows: fixtures.iter().map(|f| f.rows).max().unwrap_or(24),
        idle_threshold: options.idle_threshold,
        idle_divisor: options.idle_divisor,
    };
    // The stall detector must never fire on a gap the plan itself schedules.
    let max_gap = plan.entries.iter().map(|(gap, _)| *gap).max().unwrap_or(0);
    if max_gap >= STALL.as_nanos() as u64 {
        return Err(format!(
            "the plan schedules a {} gap, above the {} stall bound — raise the idle divisor \
             or lower the threshold",
            human_ns(max_gap),
            STALL.as_secs(),
        ));
    }
    Ok(plan)
}

/// Shorten a gap's excess over the threshold; the part under it survives
/// whole.
fn compress_gap(gap_ns: u64, threshold_ns: u64, divisor: u64) -> u64 {
    if gap_ns <= threshold_ns {
        gap_ns
    } else {
        threshold_ns + (gap_ns - threshold_ns) / divisor
    }
}

impl Plan {
    pub fn scheduled_ns(&self) -> u64 {
        self.entries.iter().map(|(gap, _)| *gap).sum()
    }

    pub fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|(_, len)| u64::from(*len)).sum()
    }

    /// The byte stream the child will write, exactly — derived here,
    /// identically on both sides of the plan, which is what lets the probe
    /// know what the run should have said. For recorded mode it is the
    /// captured bytes. For generated mode it is the generator stream, ending
    /// at the last *complete* line that fits the plan's volume: a stream cut
    /// mid-line would deliver a final fragment indistinguishable from a
    /// truncation fault, and a plan must not schedule its own corruption.
    /// The last chunk of the plan absorbs the shortfall (at most one line).
    pub fn expected_bytes(&self) -> Vec<u8> {
        match self.mode {
            Mode::Recorded => self.bytes.clone(),
            Mode::Generated => {
                let total = self.total_bytes() as usize;
                let mut source = GeneratedStream::new(self.line_bytes, self.checksum_every);
                let mut out = Vec::with_capacity(total);
                loop {
                    let line = source.next_line();
                    if out.len() + line.len() > total {
                        break;
                    }
                    out.extend_from_slice(line.as_bytes());
                }
                out
            }
        }
    }

    /// How many payload and checkpoint lines the generated stream delivers
    /// — the two halves of the completion expectation, counted in one pass
    /// over one derivation. Complete lines by construction, so every one of
    /// them is owed, and a run must not end before its final checkpoint has
    /// been judged.
    pub fn expected_line_counts(&self) -> (u64, u64) {
        debug_assert_eq!(self.mode, Mode::Generated);
        let bytes = self.expected_bytes();
        let mut payloads = 0;
        let mut checkpoints = 0;
        for segment in bytes.split(|byte| *byte == b'\n') {
            match parse_line(&String::from_utf8_lossy(segment)) {
                Some(Line::Payload { .. }) => payloads += 1,
                Some(Line::Checksum { .. }) => checkpoints += 1,
                None => {}
            }
        }
        (payloads, checkpoints)
    }

    /// The payload half of [`Plan::expected_line_counts`], for callers that
    /// need only it.
    pub fn expected_payload_lines(&self) -> u64 {
        self.expected_line_counts().0
    }

    /// The chunk boundaries as delivered: consecutive slices of the
    /// expected stream, each entry's length until `delivered` bytes run
    /// out, so only the tail chunks feel the whole-line rounding. The
    /// caller passes the length of the stream it already derived —
    /// re-deriving it here just to measure it would double the generation
    /// cost for long plans.
    pub fn chunk_ranges(&self, delivered: usize) -> Vec<(u64, std::ops::Range<usize>)> {
        let mut ranges = Vec::with_capacity(self.entries.len());
        let mut offset = 0usize;
        for (gap_ns, len) in &self.entries {
            let end = (offset + *len as usize).min(delivered);
            ranges.push((*gap_ns, offset..end));
            offset = end;
        }
        ranges
    }

    /// Serialise for the child: a JSON header line, then one line per chunk.
    /// Recorded bytes travel in a sibling file — they are opaque binary, and
    /// a probe debugging a plan greps the NDJSON without wading through them.
    pub fn write(&self, plan_path: &Path, bytes_path: &Path) -> Result<(), String> {
        let mut out = String::new();
        let header = PlanHeader {
            mode: self.mode,
            line_bytes: self.line_bytes,
            checksum_every: self.checksum_every,
            chunks: self.entries.len() as u64,
        };
        out.push_str(&serde_json::to_string(&header).expect("the header serialises"));
        out.push('\n');
        for (gap_ns, len) in &self.entries {
            out.push_str(&format!("{{\"gap_ns\":{gap_ns},\"len\":{len}}}\n"));
        }
        std::fs::write(plan_path, out).map_err(|err| format!("{}: {err}", plan_path.display()))?;
        if self.mode == Mode::Recorded {
            std::fs::write(bytes_path, &self.bytes)
                .map_err(|err| format!("{}: {err}", bytes_path.display()))?;
        }
        Ok(())
    }

    /// The child's half of the round trip: everything it needs to perform
    /// the plan, read back from the files.
    pub fn read(plan_path: &Path, bytes_path: Option<&Path>) -> Result<Self, String> {
        let text = std::fs::read_to_string(plan_path)
            .map_err(|err| format!("{}: {err}", plan_path.display()))?;
        let mut lines = text.lines();
        let header: PlanHeader = serde_json::from_str(
            lines
                .next()
                .ok_or_else(|| format!("{}: empty plan", plan_path.display()))?,
        )
        .map_err(|err| format!("{}: header: {err}", plan_path.display()))?;

        #[derive(Deserialize)]
        struct Entry {
            gap_ns: u64,
            len: u32,
        }
        let mut entries = Vec::with_capacity(header.chunks as usize);
        for (index, line) in lines.enumerate() {
            if line.is_empty() {
                continue;
            }
            let entry: Entry = serde_json::from_str(line)
                .map_err(|err| format!("{}:{}: {err}", plan_path.display(), index + 2))?;
            entries.push((entry.gap_ns, entry.len));
        }
        if entries.len() as u64 != header.chunks {
            return Err(format!(
                "{}: header promises {} chunks, file carries {}",
                plan_path.display(),
                header.chunks,
                entries.len()
            ));
        }
        let bytes = match (header.mode, bytes_path) {
            (Mode::Recorded, Some(path)) => {
                let bytes =
                    std::fs::read(path).map_err(|err| format!("{}: {err}", path.display()))?;
                let expected: u64 = entries.iter().map(|(_, len)| u64::from(*len)).sum();
                if bytes.len() as u64 != expected {
                    return Err(format!(
                        "{}: {} bytes for a plan of {expected}",
                        path.display(),
                        bytes.len()
                    ));
                }
                bytes
            }
            (Mode::Recorded, None) => {
                return Err("a recorded plan needs its bytes file".to_string());
            }
            (Mode::Generated, _) => Vec::new(),
        };
        Ok(Self {
            mode: header.mode,
            line_bytes: header.line_bytes,
            checksum_every: header.checksum_every,
            entries,
            bytes,
            sources: Vec::new(),
            cols: 0,
            rows: 0,
            idle_threshold: Duration::ZERO,
            idle_divisor: 1,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct PlanHeader {
    mode: Mode,
    line_bytes: usize,
    checksum_every: u64,
    chunks: u64,
}

/// The generator's line stream, one terminated line at a time — the shared
/// definition of what fills a generated-mode plan. It must be exactly the
/// stream a `generate` step produces (payload lines, a checksum line every
/// so many), because the verifier that checks it is the same one.
pub struct GeneratedStream {
    line_bytes: usize,
    checksum_every: u64,
    seq: u64,
    rolling: Rolling,
    queued_checksum: Option<String>,
    scratch: String,
}

impl GeneratedStream {
    pub fn new(line_bytes: usize, checksum_every: u64) -> Self {
        Self {
            line_bytes,
            checksum_every,
            seq: 0,
            rolling: Rolling::new(),
            queued_checksum: None,
            scratch: String::with_capacity(line_bytes + 32),
        }
    }

    /// The next line of the stream, terminator included.
    pub fn next_line(&mut self) -> &str {
        if let Some(checksum) = self.queued_checksum.take() {
            self.scratch = checksum;
            return &self.scratch;
        }
        write_payload_line(self.seq, self.line_bytes, &mut self.scratch);
        self.rolling.feed(&self.scratch);
        self.scratch.push('\n');
        self.seq += 1;
        if self.checksum_every > 0 && self.seq.is_multiple_of(self.checksum_every) {
            let mut line = checksum_line(self.seq, self.rolling.value());
            line.push('\n');
            self.queued_checksum = Some(line);
        }
        &self.scratch
    }
}

/// Byte-for-byte verification against the plan, for recorded mode. Reports
/// the first divergence at its offset and stops comparing there — past a
/// divergence there is no alignment to compare against — while still
/// counting how much arrived.
pub struct RecordedVerifier {
    expected: Vec<u8>,
    matched: usize,
    arrived: u64,
    divergence: Option<String>,
}

impl RecordedVerifier {
    pub fn new(expected: Vec<u8>) -> Self {
        Self {
            expected,
            matched: 0,
            arrived: 0,
            divergence: None,
        }
    }

    /// Whether the whole plan has arrived intact — the lane's completion
    /// condition. A diverged run never completes; the lane's exited-child
    /// fallback ends it instead.
    pub fn complete(&self) -> bool {
        self.divergence.is_none() && self.matched == self.expected.len()
    }

    pub fn feed(&mut self, chunk: &[u8], at_ns: u64) {
        self.arrived += chunk.len() as u64;
        if self.divergence.is_some() {
            return;
        }
        for byte in chunk {
            if self.matched >= self.expected.len() {
                self.divergence = Some(format!(
                    "{} bytes arrived beyond the {}-byte plan at {} ms into the run",
                    self.arrived - self.expected.len() as u64,
                    self.expected.len(),
                    at_ns / 1_000_000,
                ));
                return;
            }
            if *byte != self.expected[self.matched] {
                self.divergence = Some(format!(
                    "byte {} differs at {} ms into the run: expected 0x{:02x}, got 0x{byte:02x}",
                    self.matched,
                    at_ns / 1_000_000,
                    self.expected[self.matched],
                ));
                return;
            }
            self.matched += 1;
        }
    }

    pub fn finish(self) -> RecordedFindings {
        RecordedFindings {
            expected: self.expected.len() as u64,
            matched: self.matched as u64,
            arrived: self.arrived,
            divergence: self.divergence,
        }
    }
}

pub struct RecordedFindings {
    pub expected: u64,
    pub matched: u64,
    pub arrived: u64,
    pub divergence: Option<String>,
}

impl RecordedFindings {
    pub fn clean(&self) -> bool {
        self.divergence.is_none() && self.matched == self.expected
    }
}

pub struct Options {
    pub fixture_dirs: Vec<PathBuf>,
    pub build: BuildOptions,
    pub monitor_out: Option<PathBuf>,
    pub monitor_interval: Duration,
    pub warmup: Duration,
}

pub struct Outcome {
    pub bytes_read: u64,
    pub elapsed_ns: u64,
    pub scheduled_ns: u64,
    pub faults: u64,
    pub monitor: Option<monitor::Assessment>,
}

/// Run the lane: compile the plan, perform it, verify what arrives, and
/// fold the result into a report.
pub fn run(options: &Options) -> Result<(Report, Outcome), String> {
    if options.build.mode == Mode::Recorded && cfg!(windows) {
        return Err(
            "recorded-content replay needs a terminal that pipes rather than re-renders; \
             on this platform run the generated-content mode"
                .to_string(),
        );
    }
    let fixtures = options
        .fixture_dirs
        .iter()
        .map(|dir| Fixture::load(dir))
        .collect::<Result<Vec<_>, _>>()?;
    let plan = build_plan(&fixtures, &options.build)?;
    print_step(
        "plan",
        "pass",
        &format!(
            "{} chunks, {} over {} scheduled, sources {:?}, mode {}, idle gaps over {} divided by {}",
            plan.entries.len(),
            human_bytes(plan.total_bytes()),
            human_ns(plan.scheduled_ns()),
            plan.sources,
            plan.mode.name(),
            human_ns(plan.idle_threshold.as_nanos() as u64),
            plan.idle_divisor,
        ),
    );

    let (plan_file, bytes_file) = write_plan_files(&plan)?;
    let monitor = Monitor::start(options.monitor_interval, options.monitor_out.clone())?;
    let performed = perform(
        &plan,
        plan_file.path(),
        bytes_file.as_ref().map(ScenarioFile::path),
    );
    // The lane's own failure stays the headline over any sampler-stop
    // failure, as in the soak lane: the monitor is ancillary, and a replay
    // error surfacing as a sampler error would misdirect the diagnosis.
    let samples = monitor.stop();
    let (mut outcome, integrity_notes, integrity_measurements) =
        performed.map_err(|lane| match &samples {
            Err(sampler) => {
                format!("{lane} (stopping the resource sampler also failed: {sampler})")
            }
            Ok(_) => lane,
        })?;
    let samples = samples?;
    outcome.monitor = monitor::assess(&samples, options.warmup);
    outcome.scheduled_ns = plan.scheduled_ns();

    let mut report = Report::new("replay", &workload_name(&plan));
    report.note(format!(
        "sources {:?} looped to {}; idle gaps over {} divided by {}; content {}",
        plan.sources,
        human_ns(plan.scheduled_ns()),
        human_ns(plan.idle_threshold.as_nanos() as u64),
        plan.idle_divisor,
        plan.mode.name(),
    ));
    for note in integrity_notes {
        report.note(note);
    }
    for measurement in integrity_measurements {
        report.add(measurement);
    }
    report.add(Measurement::scalar(
        "bytes_read",
        "bytes",
        outcome.bytes_read,
        None,
    ));
    report.add(Measurement::scalar(
        "elapsed",
        "ns",
        outcome.elapsed_ns,
        None,
    ));
    report.add(
        Measurement::scalar("scheduled", "ns", outcome.scheduled_ns, None).with_note(
            "how far elapsed runs past scheduled is the cost of performing the pacing, \
             not a property of the workload",
        ),
    );

    match &outcome.monitor {
        Some(assessment) => {
            report.add(
                Measurement::scalar(
                    "descriptor_growth",
                    "descriptors",
                    assessment.descriptor_delta.max(0) as u64,
                    Some(Budget::AtMost(0)),
                )
                .with_note(format!(
                    "{} went from {} to {} over {} samples (net delta {})",
                    monitor::DESCRIPTOR_NOUN,
                    assessment.baseline_descriptors,
                    assessment.final_descriptors,
                    assessment.samples,
                    assessment.descriptor_delta,
                )),
            );
            report.add(
                Measurement::scalar(
                    "rss_growth",
                    "bytes",
                    assessment.rss_growth_bytes.max(0) as u64,
                    Some(Budget::AtMost(monitor::RSS_GROWTH_BUDGET_BYTES)),
                )
                .with_note(format!(
                    "resident memory went from {} to {}, peaking at {}",
                    human_bytes(assessment.baseline_rss_bytes),
                    human_bytes(assessment.final_rss_bytes),
                    human_bytes(assessment.peak_rss_bytes),
                )),
            );
        }
        None => report.note(
            "the run was too short to have a steady state, so no resource growth was assessed"
                .to_string(),
        ),
    }
    Ok((report, outcome))
}

fn workload_name(plan: &Plan) -> String {
    format!("bimodal-{}", plan.mode.name())
}

/// Serialise the plan into per-run temp files the child reads back.
fn write_plan_files(plan: &Plan) -> Result<(ScenarioFile, Option<ScenarioFile>), String> {
    let plan_file = ScenarioFile::write("replay-plan", "")?;
    let bytes_file = if plan.mode == Mode::Recorded {
        Some(ScenarioFile::write("replay-bytes", "")?)
    } else {
        None
    };
    plan.write(
        plan_file.path(),
        bytes_file
            .as_ref()
            .map_or(Path::new(""), |file| file.path()),
    )?;
    Ok((plan_file, bytes_file))
}

type Performed = (Outcome, Vec<String>, Vec<Measurement>);

fn perform(plan: &Plan, plan_path: &Path, bytes_path: Option<&Path>) -> Result<Performed, String> {
    let mut argv: Vec<OsString> = vec![
        sibling_binary("replay-child")?.into_os_string(),
        "--plan".into(),
        plan_path.as_os_str().to_os_string(),
    ];
    if let Some(bytes) = bytes_path {
        argv.push("--bytes".into());
        argv.push(bytes.as_os_str().to_os_string());
    }

    let anchor = Anchor::take();
    let mut session = Session::spawn(&argv, plan.cols, plan.rows)?;
    let started_ns = monotonic_ns();
    let deadline_ns = started_ns + plan.scheduled_ns() + OVERRUN_GRACE.as_nanos() as u64;

    enum Checker {
        Recorded(RecordedVerifier),
        Generated(Box<Verifier>, LineSplitter),
    }
    let mut checker = match plan.mode {
        Mode::Recorded => Checker::Recorded(RecordedVerifier::new(plan.expected_bytes())),
        Mode::Generated => Checker::Generated(
            Box::new(Verifier::for_this_platform(plan.line_bytes)),
            LineSplitter::new(),
        ),
    };
    // Complete on the plan's own expectation — every byte matched, or every
    // line and checkpoint accounted — never on end-of-stream, which a
    // re-rendering terminal only reports once the master closes.
    let (expected_lines, expected_checkpoints) = match plan.mode {
        Mode::Recorded => (0, 0),
        Mode::Generated => plan.expected_line_counts(),
    };
    let mut watch = session::EndWatch::new();
    let mut bytes_read = 0u64;

    loop {
        match session.pump(session::PUMP_TICK)? {
            session::Pump::Data { at, bytes } => {
                watch.data();
                bytes_read += bytes.len() as u64;
                let at_ns = anchor.ns_at(at).saturating_sub(started_ns);
                let done = match &mut checker {
                    Checker::Recorded(verifier) => {
                        verifier.feed(&bytes, at_ns);
                        verifier.complete()
                    }
                    Checker::Generated(verifier, splitter) => {
                        splitter.push(&bytes, |line| verifier.feed(line, at_ns));
                        let findings = verifier.findings();
                        verifier.accounted() >= expected_lines
                            && findings.checksums_verified + findings.checksum_faults
                                >= expected_checkpoints
                    }
                };
                if done {
                    break;
                }
            }
            session::Pump::Ended => break,
            session::Pump::Quiet => {
                if watch.ended(&mut session) {
                    break;
                }
                if watch.since_data() >= STALL {
                    return Err(format!(
                        "nothing arrived for {} s, {} into a {} plan — the replay stalled",
                        STALL.as_secs(),
                        human_ns(monotonic_ns() - started_ns),
                        human_ns(plan.scheduled_ns()),
                    ));
                }
            }
        }
        if monotonic_ns() > deadline_ns {
            return Err(format!(
                "the replay was still running {} past its schedule",
                human_ns(monotonic_ns() - started_ns - plan.scheduled_ns()),
            ));
        }
    }
    let elapsed_ns = monotonic_ns() - started_ns;
    let teardown = session.finish()?;
    print_step("teardown", "pass", &teardown);

    let mut notes = Vec::new();
    let mut measurements = Vec::new();
    let faults = match checker {
        Checker::Recorded(verifier) => {
            let findings = verifier.finish();
            notes.push(format!(
                "{} of {} bytes matched byte-for-byte",
                findings.matched, findings.expected
            ));
            if let Some(divergence) = &findings.divergence {
                notes.push(divergence.clone());
            }
            let faults = if findings.clean() { 0 } else { 1 };
            measurements.push(
                Measurement::scalar(
                    "byte_divergences",
                    "divergences",
                    faults,
                    Some(Budget::AtMost(0)),
                )
                .with_note("byte-for-byte comparison against the captured stream"),
            );
            measurements.push(Measurement::scalar(
                "bytes_matched",
                "bytes",
                findings.matched,
                None,
            ));
            faults
        }
        Checker::Generated(verifier, mut splitter) => {
            let mut verifier = *verifier;
            splitter.finish(|line| verifier.feed(line, elapsed_ns));
            let findings = verifier.finish(expected_lines, expected_checkpoints);
            notes.push(findings.summary());
            notes.extend(findings.detail.iter().cloned());
            measurements.push(Measurement::scalar(
                "lines_lost",
                "lines",
                findings.lines_lost,
                Some(Budget::AtMost(0)),
            ));
            measurements.push(Measurement::scalar(
                "content_faults",
                "lines",
                findings.content_faults,
                Some(Budget::AtMost(0)),
            ));
            measurements.push(Measurement::scalar(
                "checksum_faults",
                "checkpoints",
                findings.checksum_faults,
                Some(Budget::AtMost(0)),
            ));
            findings.faults()
        }
    };

    Ok((
        Outcome {
            bytes_read,
            elapsed_ns,
            scheduled_ns: 0, // the caller fills this from the plan
            faults,
            monitor: None,
        },
        notes,
        measurements,
    ))
}

/// A replay session running as background load while another lane measures:
/// the workload under which "under bimodal load" numbers are taken. The plan
/// is built to outlast the measuring lane, which kills the load when it is
/// done measuring.
///
/// Nothing reads the load's output while it runs — it queues on the reader
/// channel and is drained at teardown. That is a deliberate size call, not
/// an oversight: recorded CLI traffic runs to kilobytes a second, so even a
/// long benchmark queues megabytes at most, and a drain thread would buy
/// nothing but a lifetime to manage.
pub struct BackgroundLoad {
    session: Session,
    _plan_file: ScenarioFile,
}

impl BackgroundLoad {
    pub fn start(fixture_dirs: &[PathBuf], outlast: Duration) -> Result<Self, String> {
        let fixtures = fixture_dirs
            .iter()
            .map(|dir| Fixture::load(dir))
            .collect::<Result<Vec<_>, _>>()?;
        let plan = build_plan(
            &fixtures,
            &BuildOptions {
                mode: Mode::Generated,
                duration: outlast,
                ..BuildOptions::default()
            },
        )?;
        let (plan_file, _) = write_plan_files(&plan)?;
        let argv: Vec<OsString> = vec![
            sibling_binary("replay-child")?.into_os_string(),
            "--plan".into(),
            plan_file.path().as_os_str().to_os_string(),
        ];
        let session = Session::spawn(&argv, plan.cols, plan.rows)?;
        Ok(Self {
            session,
            _plan_file: plan_file,
        })
    }

    /// Kill the load and tear its terminal down. The measuring lane is done;
    /// a load left running would pollute the next lane's numbers.
    pub fn stop(self) -> Result<String, String> {
        self.session.stop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(chunks: &[(u64, u32)]) -> Fixture {
        let total: u32 = chunks.iter().map(|(_, len)| len).sum();
        Fixture {
            name: "test-80x24".to_string(),
            cols: 80,
            rows: 24,
            chunks: chunks.to_vec(),
            bytes: (0..total).map(|i| (i % 251) as u8).collect(),
        }
    }

    #[test]
    fn idle_gaps_are_compressed_and_bursts_are_not() {
        let threshold = 1_000_000_000;
        assert_eq!(compress_gap(500, threshold, 10), 500);
        assert_eq!(compress_gap(threshold, threshold, 10), threshold);
        // 61 s of idle at divisor 10: the second past the threshold survives,
        // the rest shrinks tenfold.
        assert_eq!(
            compress_gap(61_000_000_000, threshold, 10),
            threshold + 6_000_000_000
        );
    }

    #[test]
    fn a_plan_loops_its_fixtures_until_it_covers_the_duration() {
        let fixture = fixture(&[(100_000_000, 10), (200_000_000, 20)]);
        let plan = build_plan(
            &[fixture],
            &BuildOptions {
                duration: Duration::from_secs(3),
                ..BuildOptions::default()
            },
        )
        .expect("the plan must build");
        // 300 ms per loop → ten loops for three seconds.
        assert_eq!(plan.entries.len(), 20);
        assert_eq!(plan.scheduled_ns(), 3_000_000_000);
        assert_eq!(plan.total_bytes(), 300);
    }

    #[test]
    fn a_zero_time_playlist_is_refused_not_looped_forever() {
        let err = build_plan(
            &[fixture(&[(0, 10)])],
            &BuildOptions {
                duration: Duration::from_secs(1),
                ..BuildOptions::default()
            },
        )
        .expect_err("a plan of instantaneous chunks must be refused");
        assert!(err.contains("no time"), "unexpected error: {err}");
    }

    #[test]
    fn a_plan_whose_gap_would_outlast_the_stall_detector_is_refused() {
        let err = build_plan(
            &[fixture(&[(200_000_000_000, 10)])],
            &BuildOptions {
                duration: Duration::from_secs(1),
                idle_divisor: 1,
                ..BuildOptions::default()
            },
        )
        .expect_err("a gap past the stall bound must be refused at build time");
        assert!(err.contains("stall"), "unexpected error: {err}");
    }

    #[test]
    fn recorded_plans_carry_the_bytes_in_chunk_order() {
        let fixture = fixture(&[(100_000_000, 3), (100_000_000, 2)]);
        let expected = fixture.bytes.clone();
        let plan = build_plan(
            &[fixture],
            &BuildOptions {
                mode: Mode::Recorded,
                duration: Duration::from_millis(200),
                ..BuildOptions::default()
            },
        )
        .expect("the plan must build");
        assert_eq!(plan.bytes, expected);
        assert_eq!(plan.expected_bytes(), expected);
    }

    #[test]
    fn the_generated_stream_is_the_generate_steps_stream() {
        // Same shape the fake CLI emits: payload lines with a checksum line
        // every N — so the same verifier accepts both.
        let mut source = GeneratedStream::new(16, 5);
        let mut payloads = 0u64;
        let mut checksums = 0u64;
        let mut rolling = Rolling::new();
        for _ in 0..120 {
            let line = source.next_line().trim_end().to_string();
            match parse_line(&line) {
                Some(Line::Payload { seq, .. }) => {
                    assert_eq!(seq, payloads);
                    rolling.feed(&line);
                    payloads += 1;
                }
                Some(Line::Checksum { covered, digest }) => {
                    assert_eq!(covered, payloads);
                    assert_eq!(digest, rolling.value());
                    checksums += 1;
                }
                None => panic!("unrecognized generated line: {line}"),
            }
        }
        assert_eq!(payloads, 100, "one checksum line per five payload lines");
        assert_eq!(checksums, 20);
    }

    #[test]
    fn a_generated_plan_never_schedules_its_own_truncation() {
        // 50 bytes of plan volume against 20-byte lines: two complete lines,
        // and the 10-byte shortfall is absorbed rather than delivered as a
        // fragment a verifier would report as a fault.
        let fixture = fixture(&[(100_000_000, 50)]);
        let plan = build_plan(
            &[fixture],
            &BuildOptions {
                mode: Mode::Generated,
                line_bytes: 16,
                checksum_every: 0,
                duration: Duration::from_millis(100),
                ..BuildOptions::default()
            },
        )
        .expect("the plan must build");
        let bytes = plan.expected_bytes();
        assert_eq!(bytes.len(), 40, "two 20-byte lines fit in 50 bytes");
        assert_eq!(*bytes.last().expect("non-empty"), b'\n');
        assert_eq!(plan.expected_payload_lines(), 2);
        // The delivered chunks are the same stream, cut at the plan's
        // boundary with the shortfall on the tail chunk.
        let ranges = plan.chunk_ranges(bytes.len());
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0].1, 0..40);
    }

    #[test]
    fn a_plan_round_trips_through_its_files() {
        let dir = std::env::temp_dir();
        let plan_path = dir.join(format!(
            "agent-bridge-replay-plan-test-{}",
            std::process::id()
        ));
        let bytes_path = dir.join(format!(
            "agent-bridge-replay-bytes-test-{}",
            std::process::id()
        ));
        let fixture = fixture(&[(100_000_000, 3), (100_000_000, 2)]);
        let plan = build_plan(
            &[fixture],
            &BuildOptions {
                mode: Mode::Recorded,
                duration: Duration::from_millis(200),
                ..BuildOptions::default()
            },
        )
        .expect("the plan must build");
        plan.write(&plan_path, &bytes_path)
            .expect("write must succeed");

        let read = Plan::read(&plan_path, Some(&bytes_path)).expect("read must succeed");
        assert_eq!(read.mode, Mode::Recorded);
        assert_eq!(read.entries, plan.entries);
        assert_eq!(read.bytes, plan.bytes);

        std::fs::remove_file(&plan_path).expect("cleanup");
        std::fs::remove_file(&bytes_path).expect("cleanup");
    }

    #[test]
    fn a_byte_divergence_is_located_not_just_detected() {
        let mut verifier = RecordedVerifier::new(vec![1, 2, 3, 4, 5]);
        verifier.feed(&[1, 2], 5_000_000);
        verifier.feed(&[3, 9, 5], 12_000_000);
        let findings = verifier.finish();
        assert!(!findings.clean());
        assert_eq!(findings.matched, 3);
        let divergence = findings.divergence.expect("a divergence is reported");
        assert!(divergence.contains("byte 3"), "{divergence}");
        assert!(divergence.contains("12 ms"), "{divergence}");
        assert!(divergence.contains("0x04"), "{divergence}");
        assert!(divergence.contains("0x09"), "{divergence}");
    }

    #[test]
    fn a_short_delivery_is_unclean_even_with_no_divergence() {
        let mut verifier = RecordedVerifier::new(vec![1, 2, 3]);
        verifier.feed(&[1, 2], 0);
        let findings = verifier.finish();
        assert!(findings.divergence.is_none());
        assert!(!findings.clean(), "two of three bytes is not a clean run");
    }

    #[test]
    fn extra_bytes_after_the_plan_are_a_divergence() {
        let mut verifier = RecordedVerifier::new(vec![1, 2]);
        verifier.feed(&[1, 2, 3], 0);
        let findings = verifier.finish();
        assert!(!findings.clean());
        assert!(
            findings.divergence.expect("reported").contains("beyond"),
            "extra bytes must be named as such"
        );
    }
}
