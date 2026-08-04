//! The generated stream — the shape a `generate` step emits, and everything
//! a reader needs to check that what arrived is what was sent.
//!
//! A scripted `emit` carries its bytes in the scenario file, which is fine
//! for a handful of lines and impossible for the millions a half-hour of
//! continuous streaming needs. A `generate` step instead derives its content
//! from the line number, so a scenario asks for thirty minutes of traffic in
//! one step and still emits a stream whose every byte is known in advance.
//!
//! Two line shapes, both plain ASCII:
//!
//! - **payload** — `L<seq> <payload>`, where `<payload>` is `line_bytes`
//!   characters derived from `<seq>`. Content varies per line on purpose: a
//!   stream of identical lines cannot tell a reader that it lost some.
//! - **checksum** — `C<covered> <digest>`, emitted every `checksum_every`
//!   payload lines. `<covered>` is how many payload lines the digest spans
//!   and `<digest>` is the FNV-1a 64 of their text, in order, terminators
//!   excluded.
//!
//! Terminators are excluded from the digest deliberately. A terminal is
//! entitled to rewrite them — a POSIX PTY turns each `\n` into `\r\n` on the
//! way out — so a digest that covered them would report every platform's
//! normal behaviour as corruption. Everything the digest does cover crosses
//! the terminal untouched or the stream is broken, which is the claim under
//! test.
//!
//! This module is the shared definition, not a convenience: the writer and
//! the reader live in different processes, and a reader with its own copy of
//! "what line 900 000 should say" would eventually disagree with the writer
//! about it.

/// Payload characters per line when a scenario does not say. Short enough
/// that a whole line clears any terminal width a probe is likely to allocate
/// — a terminal that reflows (ConPTY does) would otherwise hard-wrap the
/// payload and no reader could put it back together.
pub const DEFAULT_LINE_BYTES: usize = 64;

/// The payload alphabet: lowercase letters and digits, so every generated
/// byte survives any encoding, any locale, and any log viewer.
const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz012345";

/// Write `L<seq> <payload>` into `out` (no terminator). The payload is a
/// pure function of `seq`, which is what lets a reader regenerate any line
/// it wants to check without keeping the stream around.
pub fn write_payload_line(seq: u64, line_bytes: usize, out: &mut String) {
    out.clear();
    out.push('L');
    push_u64(out, seq);
    out.push(' ');
    // A 64-bit LCG seeded by the line number: cheap enough to run at a
    // million lines a minute, and mixing enough that neighbouring lines
    // share no visible structure. Nothing here needs to be unpredictable —
    // it needs to be different per line and identical across processes.
    let mut state = mix(seq);
    for _ in 0..line_bytes {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        // The high bits of an LCG are the well-mixed ones.
        out.push(ALPHABET[(state >> 58) as usize & 31] as char);
    }
}

/// `L<seq> <payload>` as an owned string — the allocating form, for callers
/// that check one line rather than a stream of them.
pub fn payload_line(seq: u64, line_bytes: usize) -> String {
    let mut out = String::with_capacity(line_bytes + 24);
    write_payload_line(seq, line_bytes, &mut out);
    out
}

/// `C<covered> <digest>` (no terminator).
pub fn checksum_line(covered: u64, digest: u64) -> String {
    format!("C{covered} {digest:016x}")
}

/// The rolling digest over payload-line text. FNV-1a 64: a byte at a time,
/// no tables, no allocation — the digest must cost less than the stream it
/// covers or the pacing it is measuring becomes the pacing of the checksum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rolling(u64);

impl Default for Rolling {
    fn default() -> Self {
        Self::new()
    }
}

impl Rolling {
    pub const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    /// Fold one payload line's text in, terminator excluded.
    pub fn feed(&mut self, line: &str) {
        for byte in line.as_bytes() {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    pub const fn value(self) -> u64 {
        self.0
    }
}

/// One line of a generated stream, as a reader recognises it.
#[derive(Debug, PartialEq, Eq)]
pub enum Line<'a> {
    Payload { seq: u64, payload: &'a str },
    Checksum { covered: u64, digest: u64 },
}

/// Recognise a generated line. Returns `None` for anything else — a reader
/// of a real terminal sees banners, repaints, and escape residue alongside
/// the stream, and none of that is this module's business.
pub fn parse_line(text: &str) -> Option<Line<'_>> {
    let (tag, rest) = text.split_at_checked(1)?;
    let (head, tail) = rest.split_once(' ')?;
    match tag {
        "L" => Some(Line::Payload {
            seq: head.parse().ok()?,
            payload: tail,
        }),
        "C" => Some(Line::Checksum {
            covered: head.parse().ok()?,
            digest: u64::from_str_radix(tail, 16).ok()?,
        }),
        _ => None,
    }
}

/// Decimal `u64` without going through the formatting machinery: the
/// generator writes one of these per line, and `format!` at a million lines
/// a minute is a measurable share of the pacing budget.
fn push_u64(out: &mut String, mut value: u64) {
    if value == 0 {
        out.push('0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut len = 0;
    while value > 0 {
        digits[len] = b'0' + (value % 10) as u8;
        value /= 10;
        len += 1;
    }
    for index in (0..len).rev() {
        out.push(digits[index] as char);
    }
}

/// Spread a line number across all 64 bits before it seeds the LCG, so
/// consecutive lines start from unrelated states rather than adjacent ones.
const fn mix(seq: u64) -> u64 {
    let mut x = seq ^ 0x9e37_79b9_7f4a_7c15;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_lines_are_a_pure_function_of_the_line_number() {
        assert_eq!(payload_line(0, 16), payload_line(0, 16));
        assert_eq!(payload_line(1_800_000, 64), payload_line(1_800_000, 64));
    }

    #[test]
    fn neighbouring_lines_differ() {
        // The point of generated content: a reader that lost a line must be
        // able to tell, which it cannot if every line reads the same.
        let mut seen = std::collections::HashSet::new();
        for seq in 0..500 {
            assert!(
                seen.insert(payload_line(seq, 32)),
                "line {seq} repeated an earlier line's content"
            );
        }
    }

    #[test]
    fn payload_lines_carry_the_requested_length_and_alphabet() {
        let line = payload_line(42, 64);
        let (head, payload) = line.split_once(' ').expect("a line has a header");
        assert_eq!(head, "L42");
        assert_eq!(payload.len(), 64);
        assert!(
            payload.bytes().all(|b| ALPHABET.contains(&b)),
            "payload left the alphabet: {payload}"
        );
    }

    #[test]
    fn zero_length_payloads_are_still_well_formed() {
        assert_eq!(payload_line(7, 0), "L7 ");
    }

    #[test]
    fn parse_round_trips_both_line_shapes() {
        let line = payload_line(12_345, 8);
        let Some(Line::Payload { seq, payload }) = parse_line(&line) else {
            panic!("a payload line must parse: {line}");
        };
        assert_eq!(seq, 12_345);
        assert_eq!(payload, &line["L12345 ".len()..]);

        let line = checksum_line(64, 0x0123_4567_89ab_cdef);
        assert_eq!(
            parse_line(&line),
            Some(Line::Checksum {
                covered: 64,
                digest: 0x0123_4567_89ab_cdef,
            })
        );
    }

    #[test]
    fn non_generated_text_does_not_parse_as_a_line() {
        for text in ["", "L", "Lx y", "C12 nothex", "banner text", "\u{1b}[2J"] {
            assert_eq!(parse_line(text), None, "{text:?} must not parse");
        }
    }

    #[test]
    fn the_digest_depends_on_content_and_order() {
        let digest = |lines: &[&str]| {
            let mut rolling = Rolling::new();
            for line in lines {
                rolling.feed(line);
            }
            rolling.value()
        };
        assert_eq!(digest(&["L0 aa", "L1 bb"]), digest(&["L0 aa", "L1 bb"]));
        assert_ne!(digest(&["L0 aa", "L1 bb"]), digest(&["L1 bb", "L0 aa"]));
        assert_ne!(digest(&["L0 aa"]), digest(&["L0 ab"]));
        // A dropped line must change the digest — that is the whole job.
        assert_ne!(digest(&["L0 aa", "L1 bb"]), digest(&["L0 aa"]));
    }

    #[test]
    fn checksum_lines_render_the_digest_at_full_width() {
        assert_eq!(checksum_line(500, 0xff), "C500 00000000000000ff");
    }
}
