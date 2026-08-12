//! What stripping costs, measured on recorded real sessions.
//!
//! Two figures, because the stripper is two machines wearing one API. Input
//! that carries no sequences takes the fast path — one scan, no copy — and
//! that is what most line-oriented output is. Input from a redrawing
//! interface is wall-to-wall escape traffic and pays for the character
//! walk. A single blended number would describe neither workload, so the
//! recorded corpus supplies the second figure and its own stripped output,
//! re-fed, supplies the first: same volume, same read boundaries, no
//! sequences left in it.
//!
//! Recorded, not gated. The budgets this number must fit under — the
//! matcher chain's, the pipeline's — belong to the layers that own them;
//! a threshold defended here would either duplicate those or quietly
//! disagree with them. This prints the input to that decision.

#![allow(
    clippy::disallowed_macros,
    reason = "a benchmark's report is its output, and it is run by hand or by the bench lane \
              rather than by the runtime — nothing is reading a protocol on this stdout"
)]

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_bridge_stream::{StrippedChunk, Stripper};

/// How many times the whole corpus is replayed: enough to measure, short
/// enough for the bench lane.
const ROUNDS: u32 = 20;

/// One recorded session, decoded, with its read boundaries mapped onto the
/// decoded text.
struct Recording {
    text: String,
    /// Where each feed begins, in `text` — the recorded read boundaries,
    /// nudged forward to character boundaries where a read split one.
    reads: Vec<usize>,
}

fn main() {
    let escape_heavy = corpus();
    let total_bytes: usize = escape_heavy.iter().map(|one| one.text.len()).sum();
    let feeds: usize = escape_heavy.iter().map(|one| one.reads.len() + 1).sum();
    println!(
        "ansi_strip: {} recordings, {total_bytes} decoded bytes in {feeds} feeds, {ROUNDS} rounds",
        escape_heavy.len()
    );

    // The recorded interfaces, as captured: repaint traffic, so nearly
    // every feed strips something.
    let (elapsed, outcome) = measure(&escape_heavy);
    assert!(
        outcome.removals > 0,
        "the recorded corpus stripped nothing — this measured a copy, not the stripper"
    );
    report("recorded", elapsed, total_bytes);
    println!(
        "ansi_strip: recorded: {} of {} bytes removed as {} sequences",
        total_bytes - outcome.text_bytes,
        total_bytes,
        outcome.removals / ROUNDS as usize,
    );

    // The same volume with nothing to strip: the corpus's own output,
    // re-cut and re-fed, which is what the fast path exists for.
    let clean: Vec<Recording> = escape_heavy.iter().map(restripped).collect();
    let clean_bytes: usize = clean.iter().map(|one| one.text.len()).sum();
    let clean_feeds: usize = clean.iter().map(|one| one.reads.len() + 1).sum();
    let (elapsed, outcome) = measure(&clean);
    assert_eq!(
        outcome.borrowed,
        clean_feeds * ROUNDS as usize,
        "sequence-free input must ride the borrow path on every feed"
    );
    report("clean", elapsed, clean_bytes);
}

/// What one measured run did, so each label can be held to what it claims.
struct Outcome {
    /// Stripped sequences recorded, across all rounds.
    removals: usize,
    /// Feeds whose text came back borrowed, across all rounds.
    borrowed: usize,
    /// Output text produced in one round.
    text_bytes: usize,
}

fn measure(recordings: &[Recording]) -> (Duration, Outcome) {
    // One warm round outside the clock, for the page faults and the branch
    // predictor — costs a first read pays and a live session's second
    // second does not.
    replay(recordings);
    let mut outcome = Outcome {
        removals: 0,
        borrowed: 0,
        text_bytes: 0,
    };
    let start = Instant::now();
    for _ in 0..ROUNDS {
        let round = replay(recordings);
        outcome.removals += round.removals;
        outcome.borrowed += round.borrowed;
        outcome.text_bytes = round.text_bytes;
    }
    (start.elapsed(), outcome)
}

fn replay(recordings: &[Recording]) -> Outcome {
    let mut outcome = Outcome {
        removals: 0,
        borrowed: 0,
        text_bytes: 0,
    };
    for recording in recordings {
        let mut stripper = Stripper::new();
        let mut offset = 0;
        for &next in &recording.reads {
            let chunk = stripper.feed(&recording.text[offset..next]);
            offset = next;
            observe(&chunk, &mut outcome);
        }
        let chunk = stripper.feed(&recording.text[offset..]);
        observe(&chunk, &mut outcome);
        let tail = stripper.finish();
        observe(&tail, &mut outcome);
    }
    outcome
}

fn observe(chunk: &StrippedChunk<'_>, outcome: &mut Outcome) {
    outcome.text_bytes += chunk.text.len();
    outcome.removals += chunk.stripped.len();
    if matches!(chunk.text, Cow::Borrowed(_)) {
        outcome.borrowed += 1;
    }
    std::hint::black_box(chunk);
}

fn report(label: &str, elapsed: Duration, bytes_per_round: usize) {
    let bytes = bytes_per_round as f64 * f64::from(ROUNDS);
    let seconds = elapsed.as_secs_f64();
    println!(
        "ansi_strip: {label:>8}: {:>7.1} ms total, {:>6.1} MiB/s, {:>6.2} ns/byte",
        seconds * 1_000.0,
        bytes / seconds / (1024.0 * 1024.0),
        seconds * 1e9 / bytes,
    );
}

/// A recording's own stripped output, cut where each recorded feed's
/// output ends — the sequence-free workload at the recorded volume and the
/// recorded cadence, empty feeds included: a read that was all sequence
/// traffic strips to nothing, and nothing is exactly what the fast path is
/// handed at that point in a session.
fn restripped(recording: &Recording) -> Recording {
    let mut stripper = Stripper::new();
    let mut text = String::new();
    let mut reads = Vec::with_capacity(recording.reads.len());
    let mut offset = 0;
    for &next in &recording.reads {
        text.push_str(&stripper.feed(&recording.text[offset..next]).text);
        offset = next;
        reads.push(text.len());
    }
    text.push_str(&stripper.feed(&recording.text[offset..]).text);
    text.push_str(&stripper.finish().text);
    Recording { text, reads }
}

/// Every recorded byte stream under `tests/corpus`, decoded.
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
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("{}: the corpus cannot be walked: {error}", dir.display()));
    let mut dirs: Vec<PathBuf> = entries
        .map(|entry| {
            entry.unwrap_or_else(|error| {
                panic!("{}: an entry cannot be read: {error}", dir.display())
            })
        })
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Loads a capture directory, or `None` for one that is not a byte-stream
/// recording.
///
/// **The absence of `input.bytes` is the only thing that skips a
/// directory.** Anything else wrong with one fails by name: a throughput
/// figure is a number over an amount of data, and a loader that quietly
/// dropped a capture or its read boundaries would print a plausible rate
/// over less work than the label claims.
fn load(dir: &Path) -> Option<Recording> {
    let at = dir.display();
    let bytes = match std::fs::read(dir.join("input.bytes")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => panic!("{at}: a recording that cannot be read: {error}"),
    };
    let timing = std::fs::read_to_string(dir.join("input.timing.ndjson"))
        .unwrap_or_else(|error| panic!("{at}: a recording with no readable timing: {error}"));
    // The stripper takes decoded text — the reader upstream owns byte-level
    // decoding — so the recording is decoded whole here, and the recorded
    // byte offsets are nudged forward to the nearest character boundary. On
    // a clean capture the two coordinate systems are identical; on one with
    // substitutions they drift by the replacements, which moves a feed
    // boundary a few bytes and costs the measurement nothing.
    let text: String = String::from_utf8_lossy(&bytes).into_owned();
    let reads = timing
        .lines()
        .map(|line| {
            let record: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("{at}: a timing record that is not JSON: {error}"));
            let offset = record["offset"]
                .as_u64()
                .unwrap_or_else(|| panic!("{at}: a timing record with no offset"));
            usize::try_from(offset)
                .unwrap_or_else(|_| panic!("{at}: an offset past what this machine can index"))
        })
        .map(|offset| char_boundary_at(&text, offset.min(text.len())))
        // Not every recorded offset is a boundary to feed at: the first is
        // where the recording starts, and a capture may name one past its
        // own end. Those are the record's shape rather than a fault in it.
        .filter(|&offset| offset > 0 && offset < text.len())
        .collect();
    Some(Recording { text, reads })
}

/// The nearest character boundary at or after `offset`.
fn char_boundary_at(text: &str, offset: usize) -> usize {
    let mut at = offset;
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}
