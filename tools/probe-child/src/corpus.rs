//! The UTF-8 corpus and its wire format — shared by the `utf8-child`
//! fixture (which emits it through a PTY) and the UTF-8 probe (which
//! reassembles and verifies it), so the two sides can never disagree about
//! a byte, a split offset, or a checksum.
//!
//! The corpus covers every multi-byte shape UTF-8 has: 2-byte Latin, 3-byte
//! CJK, 4-byte emoji and mathematical alphanumerics, ZWJ sequences (one
//! grapheme, many codepoints), regional-indicator flag pairs, a variation
//! selector, and combining diacritics in decomposed form — that last one
//! also catches a terminal that quietly normalizes to precomposed
//! characters, which preserves the *look* of the text while changing its
//! bytes.
//!
//! Wire format: each item travels as one line, `u8line <seq> <payload>`,
//! and the run ends with a [`EVENT_UTF8_END`] report carrying item, byte,
//! and codepoint totals plus an FNV-1a 64 checksum over the concatenated
//! payload bytes. Line framing (rather than one blob) keeps the corpus
//! recoverable under terminal decoration: ConPTY brackets output with
//! escape sequences and can repaint a line, so the probe extracts payloads
//! per line, matches them by `seq`, and tolerates a repaint-truncated
//! prefix alongside the complete copy it requires.

/// Every corpus payload. Items must carry no leading/trailing whitespace
/// and no control characters (a unit test enforces both): the payload is
/// framed by a space on the left and the line end on the right, and the
/// probe trims terminal artifacts (`\r`, ConPTY's end-of-line padding)
/// that would otherwise be indistinguishable from corpus bytes.
pub const UTF8_CORPUS: &[&str] = &[
    // 1-byte ASCII, a 2-byte é, a 4-byte emoji, and an interior space.
    "héllo 🌍",
    // One grapheme, seven codepoints, 25 bytes: a ZWJ family sequence. A
    // terminal that stores graphemes instead of passing codepoints through
    // could corrupt this without changing what a human sees.
    "👨\u{200d}👩\u{200d}👧\u{200d}👦",
    // A ZWJ flag with a variation selector: white flag, VS16, ZWJ, rainbow.
    "🏳\u{fe0f}\u{200d}🌈",
    // Regional-indicator pairs: two flags, four 4-byte codepoints.
    "🇨🇿🇯🇵",
    // 3-byte sequences across two scripts: hiragana and CJK ideographs.
    "こんにちは世界",
    "中文字符集",
    // Combining diacritics in decomposed form: é/à/ö each as base + mark.
    // Quiet NFC normalization would keep the look and change the bytes.
    "e\u{301}a\u{300}o\u{308}",
    // 4-byte codepoints outside the emoji blocks: mathematical alphanumerics.
    "𝕬𝖌𝟠",
];

/// Bytes that can never appear in valid UTF-8, whatever arrives next: a
/// lone continuation byte, then the classic overlong pair (0xC0 0xAF — an
/// overlong `/`, the encoding rejected precisely because it could smuggle
/// a path separator past a validator).
pub const JUNK_BYTES: &[u8] = &[0x80, 0xC0, 0xAF];

/// Valid neighbors hugging the junk: a 2-byte sequence ends right before
/// it and a 4-byte one starts right after, so recovering from the junk
/// means recovering straight into multi-byte sequences on both sides.
pub const JUNK_BEFORE: &str = "aé";
pub const JUNK_AFTER: &str = "b🌍";

/// The extra line the fixture's invalid mode appends after the corpus:
/// [`JUNK_BEFORE`] ++ [`JUNK_BYTES`] ++ [`JUNK_AFTER`], as raw bytes.
pub fn junk_payload() -> Vec<u8> {
    [JUNK_BEFORE.as_bytes(), JUNK_BYTES, JUNK_AFTER.as_bytes()].concat()
}

/// The junk line's `seq` — one past the last corpus item.
pub fn junk_seq() -> usize {
    UTF8_CORPUS.len()
}

/// What a decode layer that reports junk as spans (rather than text) hands
/// back for the junk line: both neighbors, nothing in between.
pub fn junk_decoded() -> String {
    format!("{JUNK_BEFORE}{JUNK_AFTER}")
}

/// Where the fixture places write boundaries inside the junk payload:
/// mid-way through the é before the junk, between the two bytes of the
/// overlong pair, and inside the 4-byte emoji after it — so the junk and
/// both of its neighbors all cross a write boundary.
pub fn junk_splits() -> Vec<usize> {
    let before = JUNK_BEFORE.len();
    vec![before - 1, before + 2, before + JUNK_BYTES.len() + 2]
}

/// Byte offsets strictly inside one of `payload`'s multi-byte sequences —
/// the first and the last such offset, so a write boundary lands
/// mid-codepoint near both ends of the item. Empty only for pure-ASCII
/// payloads, which the corpus deliberately has none of.
pub fn mid_sequence_splits(payload: &str) -> Vec<usize> {
    let mut inside = (1..payload.len()).filter(|&at| !payload.is_char_boundary(at));
    let first = inside.next();
    let last = inside.next_back();
    first.into_iter().chain(last).collect()
}

/// Cut `payload` at the ascending, in-range `splits`, yielding the slices
/// the fixture writes (and thereby flushes) one at a time.
pub fn split_at_offsets<'a>(payload: &'a [u8], splits: &[usize]) -> Vec<&'a [u8]> {
    let mut slices = Vec::with_capacity(splits.len() + 1);
    let mut from = 0;
    for &at in splits {
        slices.push(&payload[from..at]);
        from = at;
    }
    slices.push(&payload[from..]);
    slices
}

/// One line of the emission: its `seq`, the payload bytes, where the write
/// boundaries go, and how many codepoints of valid text it carries (junk
/// bytes are not codepoints and count zero).
pub struct CorpusLine {
    pub seq: usize,
    pub payload: Vec<u8>,
    pub splits: Vec<usize>,
    pub chars: usize,
}

/// The full emission plan for one fixture mode, in `seq` order.
pub fn corpus_lines(include_junk: bool) -> Vec<CorpusLine> {
    let mut lines: Vec<CorpusLine> = UTF8_CORPUS
        .iter()
        .enumerate()
        .map(|(seq, item)| CorpusLine {
            seq,
            payload: item.as_bytes().to_vec(),
            splits: mid_sequence_splits(item),
            chars: item.chars().count(),
        })
        .collect();
    if include_junk {
        lines.push(CorpusLine {
            seq: junk_seq(),
            payload: junk_payload(),
            splits: junk_splits(),
            chars: JUNK_BEFORE.chars().count() + JUNK_AFTER.chars().count(),
        });
    }
    lines
}

/// Everything the trailer report states about one emission — the fixture
/// computes it from the bytes it actually wrote, the probe from these
/// shared constants, and the two must agree field for field.
pub struct CorpusSummary {
    pub items: usize,
    pub bytes: usize,
    pub chars: usize,
    pub fnv: u64,
}

pub fn corpus_summary(include_junk: bool) -> CorpusSummary {
    let lines = corpus_lines(include_junk);
    CorpusSummary {
        items: lines.len(),
        bytes: lines.iter().map(|line| line.payload.len()).sum(),
        chars: lines.iter().map(|line| line.chars).sum(),
        fnv: fnv1a64(lines.iter().map(|line| line.payload.as_slice())),
    }
}

/// The corpus emission is complete; carries the [`CorpusSummary`] fields
/// (`items`, `bytes`, `chars`, `fnv`) so the probe can hold the reassembled
/// stream to what the fixture actually wrote.
pub const EVENT_UTF8_END: &str = "utf8-end";

/// The fixture's two emission modes, as its CLI spells them.
pub const UTF8_MODE_VALID: &str = "valid";
pub const UTF8_MODE_INVALID: &str = "invalid";

/// Every corpus line starts with this token — distinct from the report
/// prefix so a corpus payload (which may contain spaces) never has to obey
/// the report protocol's no-whitespace-in-values rule.
pub const CORPUS_LINE_PREFIX: &str = "u8line";

/// The lead-in the fixture writes before a payload: prefix, seq, one space.
pub fn corpus_line_lead(seq: usize) -> String {
    format!("{CORPUS_LINE_PREFIX} {seq} ")
}

/// Parse one (ANSI-stripped) terminal line as a corpus line; `None` for
/// anything else. Leading whitespace and trailing `\r`/spaces are terminal
/// artifacts (repaint indentation, `\r\n` endings, end-of-line padding)
/// and are trimmed — safe because no corpus payload starts or ends with
/// whitespace.
pub fn parse_corpus_line(line: &str) -> Option<(usize, &str)> {
    let rest = line
        .trim_start()
        .strip_prefix(CORPUS_LINE_PREFIX)?
        .strip_prefix(' ')?;
    let (seq, payload) = rest.split_once(' ')?;
    Some((seq.parse().ok()?, payload.trim_end_matches(['\r', ' '])))
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Streaming FNV-1a 64: the fixture folds in each slice as it writes, the
/// probe folds in whole payloads — same bytes, same hash. FNV because the
/// checksum guards against corruption, not an adversary, and it is a few
/// lines of arithmetic instead of a dependency.
#[derive(Clone, Copy)]
pub struct Fnv1a64(u64);

impl Fnv1a64 {
    pub fn new() -> Self {
        Self(FNV_OFFSET_BASIS)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    pub fn finish(self) -> u64 {
        self.0
    }
}

impl Default for Fnv1a64 {
    fn default() -> Self {
        Self::new()
    }
}

/// FNV-1a 64 over a sequence of chunks, as one stream of bytes.
pub fn fnv1a64<'a>(chunks: impl IntoIterator<Item = &'a [u8]>) -> u64 {
    let mut hash = Fnv1a64::new();
    for chunk in chunks {
        hash.update(chunk);
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a64_matches_the_published_vectors() {
        // From the FNV reference test suite (Noll's fnv64a list).
        assert_eq!(fnv1a64([b"".as_slice()]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64([b"a".as_slice()]), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64([b"foobar".as_slice()]), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn chunking_never_changes_the_hash() {
        // The fixture hashes slice by slice, the probe hashes whole
        // payloads; the checksum must be a property of the byte stream, not
        // of how it was fed.
        let whole = fnv1a64([b"hello world".as_slice()]);
        let split = fnv1a64([b"hel".as_slice(), b"lo wor".as_slice(), b"ld".as_slice()]);
        assert_eq!(whole, split);
    }

    #[test]
    fn corpus_items_are_wire_safe() {
        for item in UTF8_CORPUS {
            assert!(!item.is_empty(), "an empty item asserts nothing");
            assert!(
                !item.starts_with(char::is_whitespace) && !item.ends_with(char::is_whitespace),
                "the probe trims terminal artifacts, so edge whitespace \
                 would be indistinguishable from them: {item:?}"
            );
            assert!(
                !item.chars().any(char::is_control),
                "a control character would break the line framing: {item:?}"
            );
            assert!(
                !item.contains('\u{fffd}'),
                "U+FFFD is how the probe detects terminal substitution and \
                 must never be corpus content: {item:?}"
            );
            assert!(
                item.chars().any(|c| c.len_utf8() > 1),
                "every item must put at least one multi-byte sequence on \
                 the wire: {item:?}"
            );
        }
    }

    #[test]
    fn the_corpus_covers_every_sequence_length() {
        let lengths: std::collections::HashSet<usize> = UTF8_CORPUS
            .iter()
            .flat_map(|item| item.chars())
            .map(char::len_utf8)
            .collect();
        assert!(
            lengths.is_superset(&[1, 2, 3, 4].into()),
            "the corpus must exercise 1- through 4-byte sequences, got {lengths:?}"
        );
    }

    #[test]
    fn mid_sequence_splits_land_strictly_inside_sequences() {
        for item in UTF8_CORPUS {
            let splits = mid_sequence_splits(item);
            assert!(
                !splits.is_empty(),
                "every corpus item must offer a mid-sequence boundary: {item:?}"
            );
            for at in splits {
                assert!(
                    !item.is_char_boundary(at),
                    "split at {at} is a char boundary in {item:?} — no split forced"
                );
            }
        }
    }

    #[test]
    fn junk_splits_cross_the_junk_and_both_neighbors() {
        let payload = junk_payload();
        let splits = junk_splits();
        // Mid-é: inside the 2-byte sequence that ends JUNK_BEFORE.
        assert!(!JUNK_BEFORE.is_char_boundary(splits[0]));
        // Between the two bytes of the overlong pair.
        assert_eq!(splits[1], JUNK_BEFORE.len() + 2);
        assert_eq!(payload[splits[1] - 1], 0xC0);
        assert_eq!(payload[splits[1]], 0xAF);
        // Inside the 4-byte emoji that follows the junk.
        let after_at = splits[2] - JUNK_BEFORE.len() - JUNK_BYTES.len();
        assert!(!JUNK_AFTER.is_char_boundary(after_at));
    }

    #[test]
    fn junk_is_genuinely_invalid_next_to_its_neighbors() {
        // The whole point of the junk: no decoder may ever accept it.
        assert!(std::str::from_utf8(&junk_payload()).is_err());
        // And the neighbors alone are exactly the decoded remainder.
        assert_eq!(junk_decoded(), "aéb🌍");
    }

    #[test]
    fn split_slices_reassemble_into_the_payload() {
        for line in corpus_lines(true) {
            let slices = split_at_offsets(&line.payload, &line.splits);
            assert!(
                slices.iter().all(|slice| !slice.is_empty()),
                "an empty slice is a write that forces nothing (seq {})",
                line.seq
            );
            assert_eq!(
                slices.concat(),
                line.payload,
                "slicing must cover every payload byte exactly once (seq {})",
                line.seq
            );
        }
    }

    #[test]
    fn corpus_lines_round_trip_through_the_wire_format() {
        for (seq, item) in UTF8_CORPUS.iter().enumerate() {
            let line = format!("{}{item}\r", corpus_line_lead(seq));
            assert_eq!(parse_corpus_line(&line), Some((seq, *item)));
        }
    }

    #[test]
    fn parse_tolerates_terminal_artifacts_and_ignores_noise() {
        assert_eq!(
            parse_corpus_line("  u8line 3 héllo 🌍  \r"),
            Some((3, "héllo 🌍")),
            "repaint indentation, padding, and \\r are terminal artifacts"
        );
        assert_eq!(parse_corpus_line(""), None);
        assert_eq!(parse_corpus_line("terminal noise"), None);
        assert_eq!(parse_corpus_line("u8liner 3 x"), None);
        assert_eq!(parse_corpus_line("u8line notanumber x"), None);
        assert_eq!(
            parse_corpus_line("probe-child event=utf8-end items=9"),
            None
        );
    }

    #[test]
    fn the_summary_agrees_with_a_slice_wise_emission() {
        // The fixture hashes and counts what it writes, slice by slice; the
        // probe trusts this summary. The two computations must be one.
        for include_junk in [false, true] {
            let summary = corpus_summary(include_junk);
            let mut hash = Fnv1a64::new();
            let mut bytes = 0;
            for line in corpus_lines(include_junk) {
                for slice in split_at_offsets(&line.payload, &line.splits) {
                    hash.update(slice);
                    bytes += slice.len();
                }
            }
            assert_eq!(summary.fnv, hash.finish());
            assert_eq!(summary.bytes, bytes);
        }
    }

    #[test]
    fn the_junk_line_is_the_only_difference_between_the_modes() {
        let valid = corpus_lines(false);
        let invalid = corpus_lines(true);
        assert_eq!(invalid.len(), valid.len() + 1);
        assert_eq!(invalid.last().unwrap().seq, junk_seq());
        assert_eq!(invalid.last().unwrap().payload, junk_payload());
    }

    #[test]
    fn corpus_lines_fit_a_terminal_row_with_margin() {
        // The probe spawns the fixture at 200 columns; ConPTY reflows
        // output to the PTY width, and a hard-wrapped corpus line would
        // never reassemble. Byte length over-counts display width for
        // multi-byte text, so holding it under 120 leaves real margin even
        // if a terminal measures every emoji at its widest.
        for line in corpus_lines(true) {
            let total = corpus_line_lead(line.seq).len() + line.payload.len();
            assert!(
                total <= 120,
                "corpus line {} is {total} bytes — too close to the terminal width",
                line.seq
            );
        }
    }
}
