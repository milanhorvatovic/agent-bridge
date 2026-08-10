//! What it costs to keep a screen, measured on recorded sessions.
//!
//! Feeding runs on every byte of every session that keeps a screen.
//! Examining runs at evaluation points, and is where the grid gets walked.
//! Which of the two dominates is the question this measures rather than
//! assumes — and the answer is not the one it was built expecting: a session
//! examining its screen at a live-ish cadence pays several times what feeding
//! costs it. Feeding is the whole story only for a session that keeps a
//! screen and never looks at it, which is not what asking for one buys.
//!
//! Recorded, not gated. The throughput a session must sustain is the SLO
//! harness's contract to hold, and a second threshold defended here would
//! either duplicate that one or quietly disagree with it. What this produces
//! is the input to that decision.
//!
//! It reports three figures, each a claim the component makes: a session that
//! keeps a screen, one that does not — which is supposed to cost nothing at
//! all — and one whose screen is also examined on a cadence close to a live
//! session's, which is what a `tui_aware` session actually pays and the only
//! one of the three that tests "the render is amortized" rather than assuming
//! it.
//!
//! Examining means both halves of what a matcher does, and the second half is
//! the expensive one. `evaluate` walks the rows written to since the last
//! point and costs about what was written; `render` walks the entire grid and
//! builds an owned snapshot, which is the cost the amortization claim is
//! about. A figure that sampled only the first would be an evaluation cost
//! wearing an examination's name, and would come out low by however much the
//! grid walk costs — so the run asserts it rendered rather than trusting that
//! it did.

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

    let (kept, kept_renders) = measure(&recordings, true, false);
    report("kept", kept, total_bytes);
    let (unkept, unkept_renders) = measure(&recordings, false, false);
    report("not kept", unkept, total_bytes);
    // What a session that asked for a screen actually pays: feeding, plus
    // examining at a cadence close to a live one. It comes out several times
    // the feed-only figure rather than a little above it, which is a
    // measurement and not a threshold — the number a streaming budget has to
    // be set against, and the reason it is worth printing beside the other
    // two rather than inferring from them.
    let (examined, examined_renders) = measure(&recordings, true, true);
    report("examined", examined, total_bytes);

    // Each figure asserts the thing its label claims, because none of it is
    // visible in a duration. The two feed-only runs must have materialized
    // nothing — that is the separation the design rests on — and the examined
    // run must have materialized something, or it is a fourth measurement of
    // feeding printed under a third name.
    assert_eq!(kept_renders, 0, "a feed-only run rendered");
    assert_eq!(unkept_renders, 0, "a session keeping no screen rendered");
    assert!(examined_renders > 0, "the examined run never rendered");
    println!(
        "screen_feed: examined materialized {examined_renders} snapshots over {ROUNDS} rounds"
    );
}

/// Replays every recording `ROUNDS` times and returns how long it took,
/// along with how many snapshots were materialized while it ran.
///
/// The count is what keeps the third figure honest. A run that examined
/// nothing and a run that examined everything take different amounts of time
/// and print the same label, and the difference between them is invisible in
/// the number itself — so the caller checks the count rather than the shape
/// of the code that was supposed to produce it.
fn measure(recordings: &[Recording], keeps_a_screen: bool, examine: bool) -> (Duration, u64) {
    // One warm round outside the clock: the first pass through pays for page
    // faults on freshly read files and for the branch predictor learning the
    // parser, neither of which a session in its second second still pays.
    replay(recordings, keeps_a_screen, examine);
    let start = Instant::now();
    let mut renders = 0;
    for _ in 0..ROUNDS {
        renders += replay(recordings, keeps_a_screen, examine);
    }
    (start.elapsed(), renders)
}

fn replay(recordings: &[Recording], keeps_a_screen: bool, examine: bool) -> u64 {
    let mut renders = 0;
    for recording in recordings {
        let mut screen = ScreenState::new(recording.cols, recording.rows, keeps_a_screen);
        let mut offset = 0;
        for (index, &next) in recording.reads.iter().enumerate() {
            screen.feed(&recording.bytes[offset..next]);
            offset = next;
            if examine && index % READS_PER_EVALUATION == 0 {
                examine_once(&mut screen);
            }
        }
        screen.feed(&recording.bytes[offset..]);
        if examine {
            examine_once(&mut screen);
        }
        // The screen itself, not a number read off it: the render count is
        // only moved by rendering, which the feed-only run never does, so
        // hiding *that* from the optimizer would be hiding a constant zero
        // and would leave the reconstruction eliminable.
        std::hint::black_box(&screen);
        renders += screen.renders();
    }
    renders
}

/// One evaluation point, as a matcher spends it.
///
/// Evaluating says which rows were written to; a matcher then has to read
/// them, and reading them is [`ScreenState::render`] — there is no partial
/// render, so the whole grid walk is what looking at one changed row costs.
/// Rendering only when something was written is the live behaviour rather
/// than a worst case: an evaluation point that arrives on a quiet screen
/// finds nothing to look at and pays for the walk it did not do.
fn examine_once(screen: &mut ScreenState) {
    let evaluation = screen.evaluate();
    let looking = !evaluation.damaged.is_empty();
    std::hint::black_box(evaluation);
    if looking {
        std::hint::black_box(screen.render());
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
