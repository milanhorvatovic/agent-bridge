//! The reconstructed screen, replayed against real recorded sessions.
//!
//! Synthetic escape sequences prove the emulator does what the standard says.
//! They cannot prove the thing this component exists for, which is that a
//! real interactive CLI's output — an Ink interface repainting regions,
//! animating a spinner, and rewrapping itself at two widths — comes back as
//! the screen a person would have been looking at. Only a recording of one
//! can say that, so these replay the committed captures byte for byte.
//!
//! Every fixture is replayed twice: once as the recording arrived, and once
//! re-cut at boundaries the recording never had. The two must produce the
//! same screen. That is the property the whole component rests on — reads
//! from a terminal fall wherever the kernel puts them, and a screen that
//! depended on where they fell would be a different screen on every run.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_bridge_events::ScreenSnapshot;
use agent_bridge_stream::{EvalPointScheduler, ScreenState};
use unicode_width::UnicodeWidthChar;

/// One recorded session: its dimensions, its bytes, and when each read
/// arrived.
struct Fixture {
    id: String,
    cols: u16,
    rows: u16,
    bytes: Vec<u8>,
    /// Where each recorded read began, and how long after the capture
    /// started it arrived.
    reads: Vec<Read>,
}

struct Read {
    monotonic_ns: u64,
    offset: usize,
}

impl Fixture {
    /// The recorded reads as byte ranges, in order.
    fn reads(&self) -> impl Iterator<Item = (u64, &[u8])> {
        self.reads.iter().enumerate().map(|(index, read)| {
            let end = self
                .reads
                .get(index + 1)
                .map_or(self.bytes.len(), |next| next.offset);
            (read.monotonic_ns, &self.bytes[read.offset..end])
        })
    }
}

/// Every replayable capture under `tests/corpus`, in a fixed order.
///
/// A missing corpus is a failure rather than an empty run: a suite that
/// silently tests nothing is worse than one that fails, because it reports
/// success.
fn corpus() -> Vec<Fixture> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus");
    let mut fixtures = Vec::new();
    for cli in sorted_dirs(&root) {
        for version in sorted_dirs(&cli) {
            for scenario in sorted_dirs(&version) {
                if let Some(fixture) = load(&scenario) {
                    fixtures.push(fixture);
                }
            }
        }
    }
    assert!(
        fixtures.len() >= 50,
        "the corpus holds {} replayable captures, which is too few to have found it",
        fixtures.len()
    );
    fixtures
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

/// Loads a capture directory, or `None` for one that is not a byte-stream
/// recording — the scripted-CLI scenarios keep a different artifact set.
fn load(dir: &Path) -> Option<Fixture> {
    let bytes = std::fs::read(dir.join("input.bytes")).ok()?;
    let timing = std::fs::read_to_string(dir.join("input.timing.ndjson")).ok()?;
    let name = dir.file_name()?.to_str()?;
    let (_, dims) = name.rsplit_once('-')?;
    let (cols, rows) = dims.split_once('x')?;
    let reads = timing
        .lines()
        .map(|line| {
            let record: serde_json::Value =
                serde_json::from_str(line).expect("the capture rig writes valid JSON");
            Read {
                monotonic_ns: record["monotonic_ns"].as_u64().expect("a recorded arrival"),
                offset: usize::try_from(record["offset"].as_u64().expect("a recorded offset"))
                    .expect("an offset within the recording"),
            }
        })
        .collect();
    Some(Fixture {
        id: dir
            .components()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .iter()
            .rev()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        cols: cols.parse().ok()?,
        rows: rows.parse().ok()?,
        bytes,
        reads,
    })
}

/// Replays a fixture as the recording arrived, and returns the screen at
/// every evaluation point the recorded timing produces.
fn screens_at_evaluation_points(fixture: &Fixture) -> Vec<ScreenSnapshot> {
    let mut screen = ScreenState::new(fixture.cols, fixture.rows, true);
    let mut scheduler = EvalPointScheduler::new();
    // The recording's own clock, offset from an arbitrary origin. Nothing
    // here sleeps: the arrival times come from the file, so the replay runs
    // at whatever speed the machine manages and still sees the gaps the live
    // session had.
    let origin = Instant::now();
    let mut screens = Vec::new();
    for (monotonic_ns, chunk) in fixture.reads() {
        let now = origin + Duration::from_nanos(monotonic_ns);
        if scheduler.poll(now).is_some() {
            screens.push(screen.render().expect("a kept screen renders"));
        }
        screen.feed(chunk);
        scheduler.on_feed(now, chunk.len());
    }
    if scheduler.on_quiescent().is_some() {
        screens.push(screen.render().expect("a kept screen renders"));
    }
    screens
}

/// Everything visible on a screen, one row per line.
///
/// Zero-width cells are skipped, which is how a row is meant to be read:
/// the covered half of a double-width glyph is a space carrying `width` 0,
/// so concatenating every cell would turn `漢x` into `漢 x`. No recorded
/// fixture holds a wide glyph today, so this changes nothing now and stops
/// the assertions quietly drifting the first time one is re-recorded with
/// one in it.
fn text(snapshot: &ScreenSnapshot) -> String {
    snapshot
        .cells
        .iter()
        .map(|row| {
            row.iter()
                .filter(|cell| cell.width != 0)
                .map(|cell| cell.ch)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Feeds a whole recording in one call and renders the final screen.
fn final_screen(fixture: &Fixture) -> ScreenSnapshot {
    let mut screen = ScreenState::new(fixture.cols, fixture.rows, true);
    screen.feed(&fixture.bytes);
    screen.render().expect("a kept screen renders")
}

/// Feeds a whole recording re-cut at pseudo-random boundaries and renders
/// the final screen.
///
/// The generator is a fixed-seed integer recurrence rather than a random
/// number crate: the cuts have to be the same cuts on every machine and
/// every run, or a failure could not be reproduced from the report of it.
fn final_screen_rechunked(fixture: &Fixture, seed: u64) -> ScreenSnapshot {
    let mut screen = ScreenState::new(fixture.cols, fixture.rows, true);
    let mut state = seed | 1;
    let mut rest = fixture.bytes.as_slice();
    while !rest.is_empty() {
        // Xorshift64. Cheap, deterministic, and more than uniform enough to
        // land cuts inside characters and inside escape sequences, which is
        // the point of the exercise.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let take = (state as usize % 64 + 1).min(rest.len());
        let (chunk, remainder) = rest.split_at(take);
        screen.feed(chunk);
        rest = remainder;
    }
    screen.render().expect("a kept screen renders")
}

#[test]
fn where_the_chunk_boundaries_fall_does_not_change_the_screen() {
    // Reads from a terminal are cut wherever the kernel cuts them — through
    // the middle of a character, through the middle of an escape sequence.
    // Every fixture, three different chunkings, against the whole-stream
    // replay.
    for fixture in corpus() {
        let expected = final_screen(&fixture);
        for seed in [0x2545_F491_4F6C_DD1D, 0x9E37_79B9_7F4A_7C15, 7] {
            assert_eq!(
                final_screen_rechunked(&fixture, seed),
                expected,
                "{}: re-cutting the recording at seed {seed:#x} changed the screen",
                fixture.id
            );
        }
    }
}

#[test]
fn the_recorded_reads_reconstruct_the_same_screen_as_the_whole_stream() {
    for fixture in corpus() {
        let mut screen = ScreenState::new(fixture.cols, fixture.rows, true);
        for (_, chunk) in fixture.reads() {
            screen.feed(chunk);
        }
        assert_eq!(
            screen.render().expect("a kept screen renders"),
            final_screen(&fixture),
            "{}: replaying the recorded reads differed from replaying the whole stream",
            fixture.id
        );
    }
}

#[test]
fn every_fixture_reconstructs_at_the_size_it_was_recorded_at() {
    for fixture in corpus() {
        let snapshot = final_screen(&fixture);
        assert_eq!(
            (snapshot.cols, snapshot.rows),
            (u32::from(fixture.cols), u32::from(fixture.rows)),
            "{}",
            fixture.id
        );
        assert_eq!(
            snapshot.cells.len(),
            usize::from(fixture.rows),
            "{}",
            fixture.id
        );
    }
}

/// The claude captures of the arrow-key approval scenario, at both recorded
/// widths.
///
/// Claude's, specifically: the assertions below quote the dialogs Claude Code
/// draws, and the same scenario recorded against another CLI is a recording
/// of that CLI's wording.
fn approval_fixtures() -> Vec<Fixture> {
    let fixtures: Vec<Fixture> = corpus()
        .into_iter()
        .filter(|fixture| fixture.id.starts_with("claude/"))
        .filter(|fixture| fixture.id.contains("approval-arrow-key"))
        .collect();
    assert!(
        fixtures.len() >= 6,
        "expected the arrow-key approval capture at both widths across the recorded versions, \
         found {}",
        fixtures.len()
    );
    fixtures
}

#[test]
fn a_menu_rendered_dialog_appears_on_the_screen_at_both_widths() {
    // The surface that motivates the whole component. Claude Code draws its
    // permission prompt as a menu, positioned on the screen — a matcher
    // reading the stripped stream sees the fragments of it that each repaint
    // wrote, in the order they were written, and never a prompt.
    let mut widths_seen = BTreeSet::new();
    for fixture in approval_fixtures() {
        let screens = screens_at_evaluation_points(&fixture);
        let found = screens.iter().any(|snapshot| {
            let text = text(snapshot);
            text.contains("Do you want to proceed?")
                && text.contains("1. Yes")
                && text.contains("2. No")
        });
        assert!(
            found,
            "{}: no evaluation point showed the permission dialog, across {} screens",
            fixture.id,
            screens.len()
        );
        widths_seen.insert(fixture.cols);
    }
    assert_eq!(
        widths_seen,
        BTreeSet::from([80, 120]),
        "the dialog has to reconstruct at both recorded widths, not just one"
    );
}

#[test]
fn the_first_run_trust_dialog_appears_on_the_screen() {
    // One of the things the structured channels cannot see: the trust prompt
    // is drawn before the session that would report it has started.
    for fixture in approval_fixtures() {
        let found = screens_at_evaluation_points(&fixture)
            .iter()
            .any(|snapshot| text(snapshot).contains("1. Yes, I trust this folder"));
        assert!(
            found,
            "{}: no evaluation point showed the trust dialog",
            fixture.id
        );
    }
}

#[test]
fn which_menu_entry_is_selected_is_visible_on_the_screen() {
    // Arrow-key navigation moves a marker between two lines that are
    // otherwise unchanged. In the stripped stream that is indistinguishable
    // from the menu being redrawn; on the screen it is the answer the caller
    // is about to give.
    for fixture in approval_fixtures() {
        let screens = screens_at_evaluation_points(&fixture);
        let rendered: Vec<String> = screens.iter().map(text).collect();
        assert!(
            rendered.iter().any(|screen| screen.contains("❯ 1. Yes")),
            "{}: the selection never rested on the first entry",
            fixture.id
        );
        assert!(
            rendered.iter().any(|screen| screen.contains("❯ 2. No")),
            "{}: the selection never moved to the second entry",
            fixture.id
        );
    }
}

#[test]
fn no_recorded_session_reports_the_same_line_twice() {
    // The property the repaint filter exists for, asserted on real output
    // rather than on a synthetic redraw: across a whole recorded session,
    // nothing it passes through as new content is a line it has already
    // passed through.
    //
    // This is the test that caught the design being wrong. A filter that
    // remembered what each *row* last said caught a region redrawn in place
    // and missed the scroll, where every row's text moves up one and every
    // row therefore differs from what that row last said — 40 % of its
    // output was text already emitted, and 63 % on the narrowest recording.
    // Nothing in the emulator reports it: this interface spends its session
    // on the alternate screen, where lines leaving the top produce no signal
    // at all.
    for fixture in corpus() {
        let mut screen = ScreenState::new(fixture.cols, fixture.rows, true);
        let mut scheduler = EvalPointScheduler::new();
        let origin = Instant::now();
        let mut reported: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (monotonic_ns, chunk) in fixture.reads() {
            let now = origin + Duration::from_nanos(monotonic_ns);
            if scheduler.poll(now).is_some() {
                for span in screen.evaluate().novel {
                    assert!(
                        seen.insert(span.text.clone()),
                        "{}: reported {:?} again, having already reported it",
                        fixture.id,
                        span.text,
                    );
                    reported.push(span.text);
                }
            }
            screen.feed(chunk);
            scheduler.on_feed(now, chunk.len());
        }
        // The recording ends mid-burst, so without draining the scheduler the
        // last thing every session painted would sit outside the assertion —
        // and the tail is where a session signs off, which is exactly where a
        // repeat of something said earlier would be likely.
        if scheduler.on_quiescent().is_some() {
            for span in screen.evaluate().novel {
                assert!(
                    seen.insert(span.text.clone()),
                    "{}: reported {:?} again in the session tail, having already reported it",
                    fixture.id,
                    span.text,
                );
                reported.push(span.text);
            }
        }
        assert!(
            !reported.is_empty(),
            "{}: the replay reported no content at all, so it proves nothing",
            fixture.id,
        );
        assert!(
            reported.iter().all(|text| !text.is_empty()),
            "{}: emptiness is not content and must not be reported as it",
            fixture.id,
        );
    }
}

#[test]
fn a_repainting_session_writes_far_more_rows_than_it_says() {
    // The other half: the filter has to be suppressing a lot, not merely
    // avoiding repeats by reporting nothing. A TUI rewrites its frame
    // constantly, and the great majority of those rewrites put back what was
    // already there or move it up a row.
    let mut damaged_rows = 0_usize;
    let mut novel_rows = 0_usize;
    for fixture in approval_fixtures() {
        let mut screen = ScreenState::new(fixture.cols, fixture.rows, true);
        let mut scheduler = EvalPointScheduler::new();
        let origin = Instant::now();
        for (monotonic_ns, chunk) in fixture.reads() {
            let now = origin + Duration::from_nanos(monotonic_ns);
            if scheduler.poll(now).is_some() {
                let evaluation = screen.evaluate();
                damaged_rows += evaluation.damaged.len();
                novel_rows += evaluation.novel.len();
            }
            screen.feed(chunk);
            scheduler.on_feed(now, chunk.len());
        }
        // The trailing burst counts too, or the ratio is taken over a
        // session with its last paint cut off.
        if scheduler.on_quiescent().is_some() {
            let evaluation = screen.evaluate();
            damaged_rows += evaluation.damaged.len();
            novel_rows += evaluation.novel.len();
        }
    }
    assert!(damaged_rows > 0, "the replay wrote to no rows at all");
    assert!(novel_rows > 0, "the replay reported nothing at all");
    let suppressed = damaged_rows - novel_rows;
    assert!(
        suppressed * 2 >= damaged_rows,
        "the filter suppressed {suppressed} of {damaged_rows} written rows, well under the \
         share a repainting interface produces — either it stopped comparing or the \
         recordings stopped repainting, and the two need telling apart"
    );
}

#[test]
fn replaying_the_whole_corpus_materializes_no_snapshots() {
    // The feed/render split, held to on the largest input available. A
    // snapshot built anywhere on the feed path shows up as a count here.
    for fixture in corpus() {
        let mut screen = ScreenState::new(fixture.cols, fixture.rows, true);
        let mut scheduler = EvalPointScheduler::new();
        let origin = Instant::now();
        for (monotonic_ns, chunk) in fixture.reads() {
            let now = origin + Duration::from_nanos(monotonic_ns);
            scheduler.poll(now);
            screen.feed(chunk);
            scheduler.on_feed(now, chunk.len());
            screen.evaluate();
        }
        assert_eq!(
            screen.renders(),
            0,
            "{}: feeding and evaluating built {} snapshots",
            fixture.id,
            screen.renders()
        );
    }
}

#[test]
fn a_session_that_keeps_no_screen_reconstructs_nothing_from_the_same_bytes() {
    for fixture in approval_fixtures() {
        let mut screen = ScreenState::new(fixture.cols, fixture.rows, false);
        screen.feed(&fixture.bytes);
        assert_eq!(screen.render(), None, "{}", fixture.id);
    }
}

#[test]
fn a_snapshot_of_a_real_screen_survives_the_wire() {
    // The golden-shape test pins a four-cell screen. This one takes the
    // screens the recordings actually produce — thousands of styled cells,
    // box drawing, wide glyphs — serializes each, reads it back, and
    // requires the two to be the same value. A snapshot that cannot survive
    // its own encoding is worse than no snapshot, because the caller has no
    // way to tell.
    for fixture in approval_fixtures() {
        let snapshot = final_screen(&fixture);
        let json = serde_json::to_string(&snapshot).expect("a snapshot serializes");
        let back: ScreenSnapshot =
            serde_json::from_str(&json).expect("and reads back as a snapshot");
        assert_eq!(back, snapshot, "{}", fixture.id);
    }
}

#[test]
fn a_real_screen_is_drawn_from_very_few_styles() {
    // The property the style table is worth having for, and the one that
    // would quietly stop being true if a cell ever started carrying
    // something per-cell in its style — a cursor position, a dirty flag.
    // Then the table would hold one entry per cell and cost more than it
    // saves, with nothing else failing to say so.
    for fixture in corpus() {
        let snapshot = final_screen(&fixture);
        let cells: usize = snapshot.cells.iter().map(Vec::len).sum();
        if cells < 200 {
            continue; // too empty a screen to say anything about
        }
        assert!(
            snapshot.styles.len() * 20 < cells,
            "{}: {} styles over {cells} cells — a table that big is not saving anything",
            fixture.id,
            snapshot.styles.len(),
        );
        assert!(
            snapshot
                .cells
                .iter()
                .flatten()
                .all(|cell| (cell.style as usize) < snapshot.styles.len()),
            "{}: a cell named a style the snapshot does not carry",
            fixture.id,
        );
    }
}

#[test]
fn a_screen_painted_in_true_colour_still_renders_promptly() {
    // Every cell a colour of its own — an image viewer, a gradient banner, a
    // dashboard. Nothing exotic, and the case that punishes any scan over
    // the styles found so far: with one style per cell, a scan compares
    // every cell against every style before it. Rendering the largest screen
    // a caller may ask for measured at 152 ms that way, on a path reached by
    // reconnecting.
    //
    // The bound is wall-clock and deliberately loose. The work here is a few
    // milliseconds and a return to scanning is seconds even in a release
    // build and far worse in a debug one, so two orders of magnitude of
    // headroom separates the two without leaving room to flake on a busy
    // machine.
    let (cols, rows) = (200_u16, 100_u16);
    let mut paint = String::new();
    for row in 0..rows {
        paint.push_str(&format!("\u{1b}[{};1H", row + 1));
        for col in 0..cols {
            let value = u32::from(row) * u32::from(cols) + u32::from(col);
            paint.push_str(&format!(
                "\u{1b}[38;2;{};{};{}m#",
                (value >> 16) & 0xff,
                (value >> 8) & 0xff,
                value & 0xff
            ));
        }
    }
    let mut screen = ScreenState::new(cols, rows, true);
    screen.feed(paint.as_bytes());

    let started = Instant::now();
    let snapshot = screen.render().expect("a kept screen renders");
    let took = started.elapsed();

    let cells: usize = snapshot.cells.iter().map(Vec::len).sum();
    assert_eq!(cells, usize::from(cols) * usize::from(rows));
    assert_eq!(
        snapshot.styles.len(),
        cells + 1,
        "every cell has its own colour, plus the default"
    );
    assert!(
        snapshot
            .cells
            .iter()
            .flatten()
            .all(|cell| (cell.style as usize) < snapshot.styles.len()),
        "a cell named a style the snapshot does not carry"
    );
    assert!(
        took < Duration::from_secs(5),
        "rendering a true-colour screen took {took:?}, which is the shape of a scan rather \
         than a lookup"
    );
}

#[test]
fn a_tall_narrow_screen_does_not_stall_evaluation() {
    // The window of recently reported lines is sized from the screen's
    // height, and the height comes from a caller. A fifteen-column terminal
    // twelve thousand rows tall sits just inside the memory bound this
    // component enforces — tiny by area, enormous by height — and gives a
    // window of forty-eight thousand digests to check every damaged row
    // against.
    //
    // Asking that by walking the window takes 3.5 s here; asking a set takes
    // 90 ms. Both figures are from the unoptimized build the check sequence
    // actually runs, which is the correction that matters: an earlier
    // version set its bound from release timings, left no headroom in debug,
    // and failed on Linux and Windows while passing on the machine the
    // number came from. The bound leaves roughly twenty times the fast path
    // here and six times on a CI runner, and the slow path overruns it by as
    // much again.
    let rows = 12_000_u16;
    let mut screen = ScreenState::new(15, rows, true);
    assert!(screen.is_kept(), "this shape is inside the memory bound");

    let started = Instant::now();
    for round in 0..3 {
        let mut paint = String::new();
        for row in 0..rows {
            paint.push_str(&format!("\u{1b}[{};1Hr{round}c{row}\r\n", row + 1));
        }
        screen.feed(paint.as_bytes());
        // Every line is distinct, so nothing is suppressed and the window
        // stays saturated — the state that makes a scan worst.
        assert!(!screen.evaluate().novel.is_empty());
    }
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(2),
        "three repaints of a 15×{rows} screen took {took:?}, which is the shape of a scan \
         over the recent-line window rather than a lookup into it"
    );
}

#[test]
fn the_quiet_window_never_catches_a_dialog_half_drawn() {
    // The window this component samples on is the security floor, not a
    // measured "the paint has finished" boundary — recorded sessions show
    // gaps up to 400 ms inside a burst of painting. So the question worth
    // asking of real output is whether sampling that often actually catches
    // a dialog mid-draw: a matcher shown the question without its answers
    // could act on half a prompt.
    //
    // It does not, on any recorded approval session. That is an observation
    // rather than a guarantee — a matcher must still tolerate a partial
    // paint — but it is the evidence for preferring the shorter window, and
    // it would be the first thing to break if the cadence stopped suiting
    // the interface being recorded.
    let mut complete = 0_usize;
    for fixture in approval_fixtures() {
        for snapshot in screens_at_evaluation_points(&fixture) {
            let screen = text(&snapshot);
            if !screen.contains("Do you want to proceed?") {
                continue;
            }
            assert!(
                screen.contains("1. Yes") && screen.contains("2. No"),
                "{}: an evaluation point saw the question without its answers",
                fixture.id,
            );
            complete += 1;
        }
    }
    assert!(
        complete > 0,
        "no evaluation point saw the dialog at all, so this proves nothing"
    );
}

#[test]
fn no_recorded_session_emits_a_scalar_the_emulator_would_misplace() {
    // A tripwire on a known, deferred limitation rather than a property of
    // this crate.
    //
    // The emulator gives every non-double-width scalar a column of its own,
    // having no way to represent zero width at all, so a combining mark or a
    // joining character takes a column a real terminal would not give it and
    // shifts everything after it. That is documented on `ScreenCell::ch` and
    // is not fixable from this side.
    //
    // It has never mattered, because no recorded CLI emits such a scalar.
    // This is what notices when that stops being true: re-record a fixture
    // against a version that prints decomposed text or joined emoji, and the
    // suite says so here rather than leaving column-anchored matching to
    // discover it against a screen that is quietly one column out.
    let mut offenders: Vec<String> = Vec::new();
    for fixture in corpus() {
        let text = String::from_utf8_lossy(&fixture.bytes);
        let found: BTreeSet<char> = text
            .chars()
            // Asked of Unicode rather than of a list written by hand. The
            // first version of this enumerated a few ranges and missed whole
            // blocks — later combining marks, the variation selectors, the
            // supplementary ones — so it would have passed while the thing
            // it watches was present.
            .filter(|ch| !ch.is_control() && ch.width().unwrap_or(1) == 0)
            .collect();
        if !found.is_empty() {
            offenders.push(format!("{} carries {:?}", fixture.id, found));
        }
    }
    assert!(
        offenders.is_empty(),
        "a recorded session now emits scalars this emulator cannot place correctly, so the \
         deferred limitation on `ScreenCell::ch` has become real and needs deciding rather \
         than documenting: {offenders:#?}"
    );
}

/// How many times `text` asks a terminal to conceal what follows.
///
/// Conceal is SGR parameter 8, and a parameter is not a byte sequence — the
/// same request has many spellings. `ESC[8m` is the shortest, but `ESC[0;8m`
/// resets and conceals, `ESC[8;31m` conceals in red, `ESC[08m` pads the
/// parameter, and `\u{9b}8m` writes the introducer as one character. A
/// tripwire matching one of those and claiming to cover conceal is worse
/// than none, because it reads as an answer.
///
/// Scanned as characters rather than bytes, because that is what the
/// emulator is given: the byte `0x9b` inside a UTF-8 sequence is a
/// continuation byte and not an introducer, and a byte-level scan would call
/// it one.
///
/// Extended colour is the trap in the other direction. `ESC[38;5;8m` selects
/// colour 8 and conceals nothing, and it is common enough in recorded output
/// that a scan counting every `8` would cry wolf on nearly every fixture.
/// The arguments of 38, 48 and 58 are therefore consumed rather than read as
/// parameters of their own. Their colon-delimited form needs no special case:
/// `38:5:8` is one parameter whose value is 38.
fn conceal_requests(text: &str) -> usize {
    let mut found = 0;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        // Either spelling of the introducer.
        match ch {
            '\u{9b}' => {}
            '\u{1b}' if chars.peek() == Some(&'[') => {
                chars.next();
            }
            _ => continue,
        }
        // Parameter bytes, then intermediates, then one final byte that says
        // which control this was.
        let mut params = String::new();
        while let Some(&next) = chars.peek() {
            if ('\u{30}'..='\u{3f}').contains(&next) {
                params.push(next);
                chars.next();
            } else {
                break;
            }
        }
        while chars
            .peek()
            .is_some_and(|&c| ('\u{20}'..='\u{2f}').contains(&c))
        {
            chars.next();
        }
        let Some(final_byte) = chars.next() else {
            break;
        };
        if final_byte != 'm' {
            continue;
        }
        // Kept as written rather than as numbers, because whether a
        // selector carried its arguments as sub-parameters decides whether
        // the parameters after it are its arguments or somebody else's.
        let mut fields = params.split(';');
        while let Some(field) = fields.next() {
            // A sub-parameter list belongs to the parameter it hangs off, and
            // an omitted parameter means zero.
            let value = field
                .split(':')
                .next()
                .unwrap_or_default()
                .parse::<u32>()
                .unwrap_or(0);
            match value {
                8 => found += 1,
                // Extended colour. Its arguments follow as parameters only
                // when they were not given as sub-parameters — `38:5:8` is
                // already complete, and consuming what comes after it would
                // eat the next parameter. `ESC[38:5:8;8m` is exactly that
                // shape: a colon-delimited colour followed by a conceal,
                // which an earlier version of this read as a colour whose
                // mode was 8, and reported nothing.
                38 | 48 | 58 if !field.contains(':') => match fields.next() {
                    Some("5") => {
                        fields.next();
                    }
                    Some("2") => {
                        fields.nth(2);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    found
}

#[test]
fn the_conceal_scan_recognizes_conceal_however_it_is_spelled() {
    // The tripwire below is a security claim, and a scan that quietly missed
    // a spelling would keep making it while being wrong. So the scan is
    // tested against the spellings it must catch and, just as importantly,
    // against the ones it must not: colour 8 is not concealed text, and a
    // tripwire that fired on it would be turned off by whoever it woke.
    for conceals in [
        "\x1b[8m",
        "\x1b[0;8m",
        "\x1b[8;31m",
        "\x1b[08m",
        "\x1b[1;8;4m",
        "\u{9b}8m",
        "\x1b[;8m",
        // A colon-delimited colour standing in front of a conceal: the
        // colour is complete in its own parameter, so the 8 after it is a
        // parameter of its own and conceals.
        "\x1b[38:5:8;8m",
        "\x1b[48:2::1:2:3;8m",
        "\x1b[38;5;8;8m",
    ] {
        assert_eq!(
            conceal_requests(conceals),
            1,
            "missed conceal in {conceals:?}"
        );
    }
    for innocent in [
        "\x1b[38;5;8m",
        "\x1b[48;5;8m",
        "\x1b[38;2;8;8;8m",
        "\x1b[38:5:8m",
        "\x1b[48:2::8:8:8m",
        "\x1b[18m",
        "\x1b[80m",
        "\x1b[8A",
        "plain text with an 8 in it",
    ] {
        assert_eq!(conceal_requests(innocent), 0, "false alarm on {innocent:?}");
    }
    assert_eq!(
        conceal_requests("\x1b[8m\x1b[0;8m"),
        2,
        "counts each request"
    );
}

#[test]
fn no_recorded_session_conceals_anything() {
    // A tripwire on a limitation that cannot be closed from this side.
    //
    // SGR 8 tells a terminal to stop showing what follows, and it is what a
    // CLI reaches for to keep something off the screen. This emulator has no
    // conceal state, so the text is stored and read back like any other —
    // concealed output reaches the snapshot, and reaches the content that
    // becomes tokens, as though it had been displayed. Nothing here can tell
    // those cells apart afterwards.
    //
    // No recorded CLI uses it, which is why this has never mattered. If one
    // starts, the failure belongs here rather than in a log with somebody's
    // secret in it.
    let mut offenders = Vec::new();
    for fixture in corpus() {
        // Lossy on purpose: a recording is what a terminal was sent, and a
        // scan that gave up on the first undecodable byte would stop looking
        // exactly where the output got strange.
        let found = conceal_requests(&String::from_utf8_lossy(&fixture.bytes));
        if found > 0 {
            offenders.push(format!("{} uses conceal {found} time(s)", fixture.id));
        }
    }
    assert!(
        offenders.is_empty(),
        "a recorded session conceals output, which this emulator cannot represent and this \
         component therefore reports as visible: {offenders:#?}"
    );
}
