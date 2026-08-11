//! The stripper against real recorded sessions.
//!
//! Synthetic input proves the machine handles each sequence family; only a
//! recording proves the thing this layer exists for — that an interactive
//! CLI's actual output comes back as readable words. These replay the
//! committed captures four ways:
//!
//! - **Cut-invariance on real traffic**: each recording stripped whole, at
//!   its recorded read boundaries, and at random cuts, all yielding one
//!   answer.
//! - **A golden pin**: a committed digest of every recording's stripped
//!   text and removals. It is the cross-platform determinism assertion —
//!   the suite runs on three operating systems against one file — and the
//!   regression tripwire for any change to strip policy, deliberate or
//!   not.
//! - **A second engine**: the same strip policy implemented over `vte`,
//!   an independent implementation of the same parsing model, must produce
//!   byte-identical text over the whole corpus.
//! - **The seam upstream**: the decoder's output fed chunk-by-chunk into
//!   the stripper equals stripping the decoded whole — the composition a
//!   live session actually runs.
//!
//! One expectation is deliberately absent: readable does not mean
//! deduplicated. A redrawing interface repaints the same words many times
//! and the strip path faithfully yields every repaint; saying which
//! repaint is news is the reconstructed screen's job, not this one's.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_bridge_stream::{DecodeItem, SeqClass, Stripper, decode};

/// One recorded session: its identity, bytes, and read boundaries.
struct Fixture {
    /// The corpus-relative path, in forward slashes on every OS — the key
    /// the golden file pins.
    id: String,
    bytes: Vec<u8>,
    /// Read boundaries as recorded, as offsets into `bytes`.
    reads: Vec<usize>,
}

/// The one whole-stream answer for a decoded recording.
fn reference(text: &str) -> (String, Vec<SeqClass>) {
    let mut stripper = Stripper::new();
    let chunk = stripper.feed(text);
    let mut out = chunk.text.into_owned();
    let mut classes: Vec<SeqClass> = chunk.stripped.iter().map(|(class, _)| *class).collect();
    let tail = stripper.finish();
    out.push_str(&tail.text);
    classes.extend(tail.stripped.iter().map(|(class, _)| *class));
    (out, classes)
}

#[test]
fn every_recording_strips_the_same_however_it_is_cut() {
    for fixture in corpus() {
        let text = String::from_utf8_lossy(&fixture.bytes).into_owned();
        let expected = reference(&text);

        // At the recorded read boundaries — the cuts a live session dealt.
        let boundaries: Vec<usize> = fixture
            .reads
            .iter()
            .map(|&offset| char_boundary_at(&text, offset.min(text.len())))
            .filter(|&offset| offset > 0 && offset < text.len())
            .collect();
        assert_cuts_agree(&fixture.id, &text, &boundaries, &expected);

        // And at cuts the recording never had.
        let mut rng = Lcg::new(fixture.bytes.len() as u64);
        for _ in 0..2 {
            let mut cuts: Vec<usize> = (0..rng.below(40) + 10)
                .map(|_| rng.below(text.len()))
                .filter(|&at| text.is_char_boundary(at))
                .collect();
            cuts.sort_unstable();
            cuts.dedup();
            assert_cuts_agree(&fixture.id, &text, &cuts, &expected);
        }
    }
}

fn assert_cuts_agree(id: &str, text: &str, cuts: &[usize], expected: &(String, Vec<SeqClass>)) {
    let mut stripper = Stripper::new();
    let mut out = String::new();
    let mut classes = Vec::new();
    let mut from = 0;
    for &at in cuts.iter().chain([text.len()].iter()) {
        let chunk = stripper.feed(&text[from..at]);
        from = at;
        out.push_str(&chunk.text);
        classes.extend(chunk.stripped.iter().map(|(class, _)| *class));
    }
    let tail = stripper.finish();
    out.push_str(&tail.text);
    classes.extend(tail.stripped.iter().map(|(class, _)| *class));
    assert_eq!(&out, &expected.0, "{id}: the cuts changed the text");
    assert_eq!(&classes, &expected.1, "{id}: the cuts changed the removals");
}

#[test]
fn the_corpus_output_carries_no_esc_and_no_c1() {
    for fixture in corpus() {
        let text = String::from_utf8_lossy(&fixture.bytes).into_owned();
        let (stripped, classes) = reference(&text);
        if let Some(leaked) = stripped
            .chars()
            .find(|&c| c == '\u{1b}' || ('\u{80}'..='\u{9f}').contains(&c))
        {
            panic!(
                "{}: {leaked:?} (U+{:04X}) reached the output",
                fixture.id, leaked as u32
            );
        }
        // The interactive recordings are what this layer exists for; a
        // capture that stripped to nothing, or removed nothing, would mean
        // the corpus stopped holding what these assertions think it holds.
        if fixture.id.starts_with("claude/") || fixture.id.starts_with("codex/") {
            assert!(!stripped.is_empty(), "{}: stripped to nothing", fixture.id);
            assert!(!classes.is_empty(), "{}: nothing was removed", fixture.id);
        }
    }
}

/// The committed digest of every recording's stripped output.
///
/// Three operating systems run this suite against the one committed file,
/// which is the "byte-identical everywhere" assertion in executable form.
/// On an intended strip-policy change, regenerate with
/// `UPDATE_ANSI_GOLDEN=1 cargo test -p agent-bridge-stream --test
/// ansi_corpus golden` and commit the diff — the diff review *is* the
/// policy review.
#[test]
fn golden_digests_pin_the_corpus_output() {
    let golden_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/ansi_strip_corpus.txt");
    let mut lines = String::new();
    for fixture in corpus() {
        let text = String::from_utf8_lossy(&fixture.bytes).into_owned();
        let (stripped, classes) = reference(&text);
        let mut class_names = String::new();
        for class in &classes {
            let _ = write!(class_names, "{class:?};");
        }
        let _ = writeln!(
            lines,
            "{} {:016x} {:016x} {}",
            fixture.id,
            fnv64(stripped.as_bytes()),
            fnv64(class_names.as_bytes()),
            classes.len(),
        );
    }
    if std::env::var_os("UPDATE_ANSI_GOLDEN").is_some() {
        std::fs::write(&golden_path, &lines)
            .unwrap_or_else(|error| panic!("cannot write {}: {error}", golden_path.display()));
        return;
    }
    let committed = std::fs::read_to_string(&golden_path).unwrap_or_else(|error| {
        panic!(
            "{}: no golden digests ({error}); generate them with UPDATE_ANSI_GOLDEN=1",
            golden_path.display()
        )
    });
    // Line-by-line, so a mismatch names the recording rather than "the
    // file differs".
    for (have, want) in lines.lines().zip(committed.lines()) {
        assert_eq!(
            have, want,
            "a recording's stripped output changed; if the change is intended, \
             regenerate with UPDATE_ANSI_GOLDEN=1 and commit the diff"
        );
    }
    assert_eq!(
        lines.lines().count(),
        committed.lines().count(),
        "the corpus and the golden file hold different recording sets; regenerate \
         with UPDATE_ANSI_GOLDEN=1 and commit the diff"
    );
}

/// The same strip policy over `vte` — an independent implementation of the
/// same DEC/xterm parsing model. What this pins is the division of labor:
/// if the policy layer is honest, the grammar underneath is exchangeable
/// and the text cannot tell.
struct Oracle {
    text: String,
    /// A single shift waiting for the character it claims — the policy
    /// mirrored from the stripper, because `vte` treats a shift as already
    /// complete just as `avt` does.
    shift: bool,
}

impl vte::Perform for Oracle {
    fn print(&mut self, c: char) {
        if self.shift {
            self.shift = false;
            return;
        }
        self.text.push(c);
    }

    fn execute(&mut self, byte: u8) {
        self.shift = false;
        if matches!(byte, 0x08..=0x0d) {
            self.text.push(char::from(byte));
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.shift = intermediates.is_empty() && matches!(byte, b'N' | b'O');
    }

    fn hook(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {
        // A device-control string arriving while a shift waits ends the
        // wait like every other sequence does — without this, the flag
        // would survive the DCS and eat the next printed character.
        self.shift = false;
    }

    fn csi_dispatch(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {
        self.shift = false;
    }

    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {
        self.shift = false;
    }
}

#[test]
fn a_vte_driven_strip_agrees_on_the_whole_corpus() {
    for fixture in corpus() {
        let text = String::from_utf8_lossy(&fixture.bytes).into_owned();
        // The two engines meet C1 controls differently — `vte` reads bytes
        // and sees encoded C1 as printable characters, `avt` reads
        // characters and sees them as controls — so equality is only
        // promised where no encoded C1 occurs. The corpus holds none; if a
        // future capture carries one, fail by name here rather than as an
        // unexplained text diff below.
        assert!(
            !text.chars().any(|c| ('\u{80}'..='\u{9f}').contains(&c)),
            "{}: the recording decodes to a C1 control, which the two engines \
             read differently — this comparison no longer covers it",
            fixture.id
        );
        let (ours, _) = reference(&text);
        let mut oracle = Oracle {
            text: String::new(),
            shift: false,
        };
        let mut parser = vte::Parser::new();
        parser.advance(&mut oracle, text.as_bytes());
        assert_eq!(
            ours, oracle.text,
            "{}: the avt-driven and vte-driven strips disagree",
            fixture.id
        );
    }
}

#[test]
fn the_decoded_feed_composes_with_the_stripper() {
    // What a live session runs: the decoder's chunk stream, stripped as it
    // arrives. Cuts fall only where the layer below promises them — never
    // inside a character — and the answer must equal stripping the decoded
    // whole.
    for fixture in corpus() {
        let text = String::from_utf8_lossy(&fixture.bytes).into_owned();
        let expected = reference(&text);
        let mut rng = Lcg::new(fixture.bytes.len() as u64 ^ 0xdec0de);
        let mut cuts: Vec<usize> = (0..rng.below(40) + 10)
            .map(|_| rng.below(fixture.bytes.len()))
            .filter(|&at| fixture.bytes[at] & 0xc0 != 0x80)
            .collect();
        cuts.sort_unstable();
        cuts.dedup();

        let mut stripper = Stripper::new();
        let mut out = String::new();
        let mut classes = Vec::new();
        let mut from = 0;
        for at in cuts.into_iter().chain([fixture.bytes.len()]) {
            for item in decode(&fixture.bytes[from..at], from as u64) {
                let piece = match item {
                    DecodeItem::Text(piece) => Cow::Borrowed(piece),
                    // The reader replaces what cannot decode; the seam
                    // here mirrors it.
                    DecodeItem::Invalid { .. } => Cow::Borrowed("\u{fffd}"),
                };
                let chunk = stripper.feed(&piece);
                out.push_str(&chunk.text);
                classes.extend(chunk.stripped.iter().map(|(class, _)| *class));
            }
            from = at;
        }
        let tail = stripper.finish();
        out.push_str(&tail.text);
        classes.extend(tail.stripped.iter().map(|(class, _)| *class));
        assert_eq!(
            out, expected.0,
            "{}: decode→strip changed the text",
            fixture.id
        );
        assert_eq!(
            classes, expected.1,
            "{}: decode→strip changed the removals",
            fixture.id
        );
    }
}

/// FNV-1a, 64-bit: a digest the golden file can hold in one short line,
/// with no dependency bought for it. Determinism is the property in use;
/// collision resistance is not, and a colliding regression would still
/// have to collide on both digests and the removal count at once.
fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// The nearest character boundary at or after `offset`.
fn char_boundary_at(text: &str, offset: usize) -> usize {
    let mut at = offset;
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// A 64-bit LCG, high bits out — the same deterministic generator the
/// adversarial suite uses, so a failure names a reproducible cut set.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(
            seed.wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(0x2545_f491_4f6c_dd1d),
        )
    }

    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() as usize) % bound
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
                if let Some(fixture) = load(&root, &scenario) {
                    fixtures.push(fixture);
                }
            }
        }
    }
    // A floor, not a count: a corpus emptied or trimmed to a handful would
    // make every assertion in this file pass by having nothing to check.
    assert!(
        fixtures.len() >= 50,
        "the corpus holds {} replayable captures, which is too few to have found it",
        fixtures.len()
    );
    fixtures
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
/// recording — the scripted-CLI scenarios keep a different artifact set.
///
/// **The absence of `input.bytes` is the only thing that skips a
/// directory.** Past that point the directory is a recording, and anything
/// else wrong with it fails by name rather than quietly reducing what the
/// golden file pins.
fn load(root: &Path, dir: &Path) -> Option<Fixture> {
    let at = dir.display();
    let bytes = match std::fs::read(dir.join("input.bytes")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => panic!("{at}: a recording that cannot be read: {error}"),
    };
    let timing = std::fs::read_to_string(dir.join("input.timing.ndjson"))
        .unwrap_or_else(|error| panic!("{at}: a recording with no readable timing: {error}"));
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
        .filter(|&offset| offset > 0 && offset < bytes.len())
        .collect();
    let id = dir
        .strip_prefix(root)
        .unwrap_or_else(|_| panic!("{at}: a scenario outside the corpus root"))
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    Some(Fixture { id, bytes, reads })
}
