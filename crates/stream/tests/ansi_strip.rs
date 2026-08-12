//! The stripper under adversarial input, at volume.
//!
//! The unit suite proves each sequence family behaves; this one holds the
//! module's three standing properties against input designed to break
//! them, generated rather than curated so the cases nobody thought to
//! write are the cases that run:
//!
//! - **Never a panic.** Every case that runs to completion is this
//!   assertion; there is no other form it could take.
//! - **No ESC and no C1 in the output**, whatever arrives — including
//!   sequences the stripper abandoned and re-emitted.
//! - **Where the chunk boundaries fall does not change the answer.** The
//!   same stream cut anywhere — mid-parameter, mid-payload, between an ESC
//!   and its terminator — yields the same text and the same removals.
//!
//! Everything here is deterministic: the generator runs on a seeded LCG,
//! so a failing case names its seed and replays exactly. The default sweep
//! is sized for the pull-request tier; the `#[ignore]`d extended sweep is
//! the same generator at nightly volume, which the nightly lane runs via
//! `cargo xtask soak-nightly`.

use std::borrow::Cow;

use agent_bridge_stream::{SeqClass, Stripper};

/// A 64-bit LCG, high bits out — deterministic, seedable, and enough: the
/// generator needs variety, not statistical quality.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        // Spread small seeds across the state before the first draw.
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

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }
}

/// One generated stretch of stream: text, valid sequences, truncated ones,
/// cancellations, floods — the full repertoire, adversarially mixed.
fn token(rng: &mut Lcg, into: &mut String) {
    const WORDS: &[&str] = &[
        "ok", "done 42%", "naïve", "漢字", "🚀", "e\u{301}", " ", "…",
    ];
    const FINALS: &[char] = &[
        'A', 'H', 'J', 'K', 'm', 'h', 'l', 'X', 'S', 'r', 's', 'u', 't', 'p',
    ];
    const KEPT: &[char] = &['\u{8}', '\t', '\n', '\u{b}', '\u{c}', '\r'];
    const BANNED: &[char] = &['\0', '\u{1}', '\u{7}', '\u{e}', '\u{f}', '\u{11}', '\u{7f}'];
    const C1: &[char] = &[
        '\u{80}', '\u{84}', '\u{85}', '\u{8e}', '\u{8f}', '\u{90}', '\u{98}', '\u{9b}', '\u{9c}',
        '\u{9d}', '\u{9e}', '\u{9f}',
    ];
    match rng.below(20) {
        0..=5 => {
            for _ in 0..rng.below(8) + 1 {
                into.push_str(rng.pick(WORDS));
            }
        }
        6..=8 => {
            into.push_str("\u{1b}[");
            if rng.below(4) == 0 {
                into.push(*rng.pick(&['?', '<', '=', '>']));
            }
            for _ in 0..rng.below(4) {
                into.push_str(&rng.below(1100).to_string());
                into.push(*rng.pick(&[';', ':']));
            }
            if rng.below(5) == 0 {
                into.push(*rng.pick(&[' ', '!', '$']));
            }
            into.push(*rng.pick(FINALS));
        }
        9 => {
            into.push_str("\u{1b}]");
            into.push_str(&rng.below(60).to_string());
            into.push(';');
            for _ in 0..rng.below(12) {
                into.push_str(rng.pick(WORDS));
            }
            into.push_str(rng.pick(&["\u{7}", "\u{1b}\\", "\u{9c}"]));
        }
        10 => {
            into.push_str(rng.pick(&["\u{1b}P", "\u{1b}X", "\u{1b}^", "\u{1b}_"]));
            for _ in 0..rng.below(10) {
                into.push_str(rng.pick(WORDS));
            }
            into.push_str(rng.pick(&["\u{1b}\\", "\u{9c}"]));
        }
        11 => into.push(*rng.pick(C1)),
        12 => into.push(*rng.pick(BANNED)),
        13 => into.push(*rng.pick(KEPT)),
        14 => {
            // A truncated sequence: an introducer whose ending never comes
            // from this token — whatever follows collides with it.
            into.push_str(rng.pick(&[
                "\u{1b}",
                "\u{1b}[",
                "\u{1b}[12;",
                "\u{1b}]52;c;aG",
                "\u{1b}P1|half",
                "\u{1b}[38;2;10;",
            ]));
        }
        15 => {
            into.push_str(rng.pick(&["\u{1b}N", "\u{1b}O", "\u{8e}", "\u{8f}"]));
            if rng.below(2) == 0 {
                into.push_str(rng.pick(WORDS));
            }
        }
        16 => into.push(*rng.pick(&['\u{18}', '\u{1a}'])),
        17 => {
            // Deep parameter runs, occasionally past the control budget.
            let run = 64 + rng.below(40) * 40;
            into.push_str("\u{1b}[");
            for _ in 0..run {
                into.push_str("9;");
            }
            into.push('m');
        }
        18 => {
            into.push_str("\u{1b}(");
            into.push(*rng.pick(&['B', '0']));
        }
        _ => {
            // An ESC landing on another sequence's body.
            into.push_str("\u{1b}[3\u{1b}");
        }
    }
}

fn case(seed: u64) -> String {
    let mut rng = Lcg::new(seed);
    let mut input = String::new();
    for _ in 0..rng.below(38) + 3 {
        token(&mut rng, &mut input);
    }
    input
}

/// The whole-stream answer: text and classes, one feed plus the flush.
fn reference(input: &str) -> (String, Vec<SeqClass>) {
    let mut stripper = Stripper::new();
    let chunk = stripper.feed(input);
    let mut text = chunk.text.into_owned();
    let mut classes: Vec<SeqClass> = chunk.stripped.iter().map(|(class, _)| *class).collect();
    let tail = stripper.finish();
    text.push_str(&tail.text);
    classes.extend(tail.stripped.iter().map(|(class, _)| *class));
    (text, classes)
}

fn assert_postcondition(text: &str, context: &dyn std::fmt::Display) {
    if let Some(leaked) = text
        .chars()
        .find(|&c| c == '\u{1b}' || ('\u{80}'..='\u{9f}').contains(&c))
    {
        panic!(
            "{context}: {leaked:?} (U+{:04X}) reached the output",
            leaked as u32
        );
    }
}

/// One adversarial case, cut into random pieces: same text, same classes,
/// well-formed spans, nothing banned in the output.
fn exercise(seed: u64) {
    let input = case(seed);
    let (whole_text, whole_classes) = reference(&input);
    assert_postcondition(&whole_text, &format_args!("seed {seed} whole"));

    let mut rng = Lcg::new(seed ^ 0x5eed_c0de);
    for round in 0..3 {
        let mut cuts: Vec<usize> = (0..rng.below(7) + 1)
            .map(|_| rng.below(input.len()))
            .filter(|&at| input.is_char_boundary(at))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        let mut stripper = Stripper::new();
        let mut text = String::new();
        let mut classes = Vec::new();
        let mut from = 0;
        for at in cuts.into_iter().chain([input.len()]) {
            let piece = &input[from..at];
            from = at;
            let chunk = stripper.feed(piece);
            assert_postcondition(&chunk.text, &format_args!("seed {seed} round {round}"));
            let mut last_end = 0;
            for (_, span) in &chunk.stripped {
                assert!(
                    last_end <= span.start && span.start <= span.end && span.end <= piece.len(),
                    "seed {seed} round {round}: span {span:?} is out of order or out of \
                     bounds in a piece of {} bytes",
                    piece.len()
                );
                last_end = span.end;
            }
            text.push_str(&chunk.text);
            classes.extend(chunk.stripped.iter().map(|(class, _)| *class));
        }
        let tail = stripper.finish();
        text.push_str(&tail.text);
        classes.extend(tail.stripped.iter().map(|(class, _)| *class));
        assert_eq!(
            text, whole_text,
            "seed {seed} round {round}: the cuts changed the text"
        );
        assert_eq!(
            classes, whole_classes,
            "seed {seed} round {round}: the cuts changed the removals"
        );
    }

    // Stripping is a projection: its own output has nothing left to
    // remove, so a second pass is the identity, borrowed.
    let mut stripper = Stripper::new();
    let again = stripper.feed(&whole_text);
    assert!(
        matches!(again.text, Cow::Borrowed(_)) && again.stripped.is_empty(),
        "seed {seed}: re-stripping the output was not the identity"
    );
    assert_eq!(again.text, whole_text, "seed {seed}");
}

#[test]
fn adversarial_streams_strip_the_same_however_they_are_cut() {
    for seed in 0..300 {
        exercise(seed);
    }
}

/// The same sweep at nightly volume. Ignored on the pull-request tier;
/// `cargo xtask soak-nightly` runs it with `--ignored`.
#[test]
#[ignore = "nightly-volume sweep; the pull-request tier runs the 300-seed version"]
fn extended_adversarial_sweep() {
    for seed in 300..10_300 {
        exercise(seed);
    }
}

#[test]
fn the_budget_boundaries_hold_to_the_byte() {
    // A sequence exactly at its budget survives; one byte more abandons.
    // Sized from the constants rather than literals, so a retuned budget
    // moves the test with it.
    use agent_bridge_stream::ansi::{MAX_CONTROL_SEQUENCE_BYTES, MAX_STRING_SEQUENCE_BYTES};

    // "\u{1b}[" + digits + "m" == budget → the last push lands exactly on
    // the limit and the sequence completes.
    let digits = "4".repeat(MAX_CONTROL_SEQUENCE_BYTES - 3);
    let (text, classes) = reference(&format!("a\u{1b}[{digits}mb"));
    assert_eq!(text, "ab");
    assert_eq!(classes, vec![SeqClass::Sgr]);

    let digits = "4".repeat(MAX_CONTROL_SEQUENCE_BYTES - 2);
    let (text, classes) = reference(&format!("a\u{1b}[{digits}mb"));
    assert_eq!(classes[0], SeqClass::Abandoned);
    assert!(text.starts_with("a[44"));

    let payload = "A".repeat(MAX_STRING_SEQUENCE_BYTES - 3);
    let (text, classes) = reference(&format!("a\u{1b}]{payload}\u{7}b"));
    assert_eq!(text, "ab");
    assert_eq!(classes, vec![SeqClass::OscOther]);

    let payload = "A".repeat(MAX_STRING_SEQUENCE_BYTES - 2);
    let (_, classes) = reference(&format!("a\u{1b}]{payload}\u{7}b"));
    assert_eq!(classes[0], SeqClass::Abandoned);
}

#[test]
fn adversarial_volume_finishes_in_bounded_time() {
    // The generosity is deliberate: two orders of magnitude above what the
    // work takes on any machine this runs on, so the assertion can only
    // fail on a genuine complexity blowup — quadratic buffering, a stuck
    // state — and never on a busy runner.
    let mut input = String::new();
    let mut rng = Lcg::new(0x71_4d_e0);
    while input.len() < 2 * 1024 * 1024 {
        token(&mut rng, &mut input);
    }
    let started = std::time::Instant::now();
    let (text, _) = reference(&input);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "2 MiB of adversarial input took {:?}",
        started.elapsed()
    );
    assert_postcondition(&text, &"bounded-time sweep");
}
