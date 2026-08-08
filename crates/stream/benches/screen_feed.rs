//! What it costs to keep a screen, measured on recorded sessions.
//!
//! Feeding is the steady-state work of the read pipeline: it runs on every
//! byte of every session that keeps a screen, where rendering runs when
//! somebody asks. At the concurrency this runtime is sized for, feeding is
//! therefore the dominant cost of the whole path, and this is the number that
//! says how much of the budget it takes.
//!
//! Recorded, not gated. The throughput a session must sustain is the SLO
//! harness's contract to hold, and a second threshold defended here would
//! either duplicate that one or quietly disagree with it. What this produces
//! is the input to that decision.
//!
//! It reports two figures because both are claims the component makes: the
//! cost of a session that keeps a screen, and the cost of one that does not,
//! which is supposed to be nothing at all.

#![allow(
    clippy::disallowed_macros,
    reason = "a benchmark's report is its output, and it is run by hand or by the bench lane \
              rather than by the runtime — nothing is reading a protocol on this stdout"
)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_bridge_stream::ScreenState;

/// How many times the whole corpus is replayed. Enough that a run takes long
/// enough to measure and short enough to sit in a benchmark lane.
const ROUNDS: u32 = 20;

/// One recorded session, reduced to what feeding needs.
struct Recording {
    cols: u16,
    rows: u16,
    bytes: Vec<u8>,
    /// The read boundaries as recorded, so the feed is called the number of
    /// times a live session would call it. Feeding one 20 KiB slab measures
    /// something no session ever does.
    reads: Vec<usize>,
}

/// How often the screen is examined, expressed as a number of reads.
///
/// The real cadence is a quiet window in wall-clock time, which a replay
/// running as fast as it can does not have — so it is approximated by read
/// count. One in eight puts about thirty evaluations on the average
/// recording, against the forty-five to sixty-five the recorded timing
/// actually produces for one. Close enough that the figure below is roughly
/// what a live session pays, rather than a worst case with headroom in it.
const READS_PER_EVALUATION: usize = 8;

fn main() {
    let recordings = corpus();
    let total_bytes: usize = recordings.iter().map(|one| one.bytes.len()).sum();
    let reads: usize = recordings.iter().map(|one| one.reads.len()).sum();
    println!(
        "screen_feed: {} recordings, {total_bytes} bytes in {reads} reads, {ROUNDS} rounds",
        recordings.len()
    );

    report("kept", measure(&recordings, true, false), total_bytes);
    report("not kept", measure(&recordings, false, false), total_bytes);
    // The claim this one checks is that steady-state cost is the feed. If
    // examining the screen ever approaches it, the repaint filter has stopped
    // being bounded work over what was written and the difference belongs in
    // the record before someone budgets against the wrong number.
    report("examined", measure(&recordings, true, true), total_bytes);
}

/// Replays every recording `ROUNDS` times and returns how long it took.
fn measure(recordings: &[Recording], keeps_a_screen: bool, examine: bool) -> Duration {
    // One warm round outside the clock: the first pass through pays for page
    // faults on freshly read files and for the branch predictor learning the
    // parser, neither of which a session in its second second still pays.
    replay(recordings, keeps_a_screen, examine);
    let start = Instant::now();
    for _ in 0..ROUNDS {
        replay(recordings, keeps_a_screen, examine);
    }
    start.elapsed()
}

fn replay(recordings: &[Recording], keeps_a_screen: bool, examine: bool) {
    for recording in recordings {
        let mut screen = ScreenState::new(recording.cols, recording.rows, keeps_a_screen);
        let mut offset = 0;
        for (index, &next) in recording.reads.iter().enumerate() {
            screen.feed(&recording.bytes[offset..next]);
            offset = next;
            if examine && index % READS_PER_EVALUATION == 0 {
                std::hint::black_box(screen.evaluate());
            }
        }
        screen.feed(&recording.bytes[offset..]);
        if examine {
            std::hint::black_box(screen.evaluate());
        }
        // Read something back, so nothing above can be optimized away on the
        // grounds that the screen is never looked at.
        std::hint::black_box(screen.renders());
    }
}

fn report(label: &str, elapsed: Duration, bytes_per_round: usize) {
    let bytes = bytes_per_round as f64 * f64::from(ROUNDS);
    let seconds = elapsed.as_secs_f64();
    println!(
        "screen_feed: {label:>8}: {:>7.1} ms total, {:>6.1} MiB/s, {:>6.1} ns/byte",
        seconds * 1_000.0,
        bytes / seconds / (1024.0 * 1024.0),
        seconds * 1e9 / bytes,
    );
}

/// Every recorded byte stream under `tests/corpus`.
fn corpus() -> Vec<Recording> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let mut recordings = Vec::new();
    for cli in sorted_dirs(&root) {
        for version in sorted_dirs(&cli) {
            for scenario in sorted_dirs(&version) {
                if let Some(recording) = load(&scenario) {
                    recordings.push(recording);
                }
            }
        }
    }
    assert!(
        !recordings.is_empty(),
        "no recordings found under tests/corpus — a benchmark over nothing reports nothing"
    );
    recordings
}

fn sorted_dirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn load(dir: &Path) -> Option<Recording> {
    let bytes = std::fs::read(dir.join("input.bytes")).ok()?;
    let timing = std::fs::read_to_string(dir.join("input.timing.ndjson")).ok()?;
    let (_, dims) = dir.file_name()?.to_str()?.rsplit_once('-')?;
    let (cols, rows) = dims.split_once('x')?;
    let reads = timing
        .lines()
        .filter_map(|line| {
            let record: serde_json::Value = serde_json::from_str(line).ok()?;
            usize::try_from(record["offset"].as_u64()?).ok()
        })
        .filter(|&offset| offset > 0 && offset < bytes.len())
        .collect();
    Some(Recording {
        cols: cols.parse().ok()?,
        rows: rows.parse().ok()?,
        bytes,
        reads,
    })
}
