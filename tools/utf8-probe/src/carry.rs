//! `Utf8Carry` — the decode-carry layer between a PTY's byte chunks and
//! text. Chunks split at arbitrary offsets, so a multi-byte codepoint can
//! straddle reads; complete codepoints decode as each chunk arrives, an
//! incomplete trailing sequence is carried into the next push, and bytes
//! that can never become valid UTF-8 are reported as located
//! [`InvalidSpan`]s while decoding continues past them. Surrounding valid
//! content always survives, and no byte ever disappears: each one is
//! decoded, reported inside a span, or still observably pending.
//!
//! This is the decode contract the production stream reader will adopt.
//! The probes' existing `Reassembler` stops at the first invalid byte —
//! right for lanes where any invalid byte is outright failure — but a
//! runtime decoding a live stream must keep going and *report* the
//! corruption instead. The exhaustive split-position suite below is the
//! evidence the carry logic holds at every offset, not just the lucky ones.
//!
//! Span boundaries follow `str::from_utf8` (substitution of maximal
//! subparts): a broken sequence is reported as the longest prefix that
//! could still have become valid, so one corrupted codepoint is one span,
//! and a run of lone junk bytes is one span each. Chunking never changes
//! the spans — a boundary mid-junk yields the same report as the junk
//! arriving whole, which the unit suite pins down.

/// A run of bytes that can never become valid UTF-8, in stream
/// coordinates: `offset` counts from the first byte ever pushed, so a span
/// points into the raw capture no matter how the chunks fell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidSpan {
    pub offset: u64,
    pub len: usize,
}

/// What one push handed back: the text that became decodable, and any
/// invalid spans encountered along the way.
pub struct Decoded {
    pub text: String,
    pub invalid: Vec<InvalidSpan>,
}

#[derive(Default)]
pub struct Utf8Carry {
    /// An incomplete trailing sequence, held for the next push.
    carry: Vec<u8>,
    /// Stream offset of `carry[0]` — every byte before it has been decoded
    /// or reported.
    base: u64,
}

impl Utf8Carry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode `chunk` in stream order: complete codepoints extend the
    /// text, invalid sequences become spans, and an incomplete trailing
    /// sequence is carried for the next push.
    pub fn push(&mut self, chunk: &[u8]) -> Decoded {
        self.carry.extend_from_slice(chunk);
        let mut text = String::new();
        let mut invalid = Vec::new();
        // Index into the carry buffer of the first unprocessed byte.
        let mut at = 0;
        loop {
            let rest = &self.carry[at..];
            match std::str::from_utf8(rest) {
                Ok(tail) => {
                    text.push_str(tail);
                    at = self.carry.len();
                    break;
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    // Unreachable panic: `valid_up_to` guarantees the
                    // prefix is valid UTF-8.
                    text.push_str(std::str::from_utf8(&rest[..valid]).unwrap());
                    match err.error_len() {
                        Some(len) => {
                            invalid.push(InvalidSpan {
                                offset: self.base + (at + valid) as u64,
                                len,
                            });
                            at += valid + len;
                        }
                        // Not wrong, just unfinished — carry it.
                        None => {
                            at += valid;
                            break;
                        }
                    }
                }
            }
        }
        self.carry.drain(..at);
        self.base += at as u64;
        Decoded { text, invalid }
    }

    /// Bytes held back waiting for the rest of their codepoint.
    pub fn pending(&self) -> usize {
        self.carry.len()
    }

    /// End-of-stream: a still-carried sequence can never complete now, so
    /// it surfaces as one final span — a truncated codepoint is an error
    /// to report, not a few bytes to quietly forget.
    pub fn finish(&mut self) -> Option<InvalidSpan> {
        if self.carry.is_empty() {
            return None;
        }
        let span = InvalidSpan {
            offset: self.base,
            len: self.carry.len(),
        };
        self.base += self.carry.len() as u64;
        self.carry.clear();
        Some(span)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridge_probe_child::corpus::{JUNK_BEFORE, UTF8_CORPUS, junk_decoded, junk_payload};

    fn span(offset: u64, len: usize) -> InvalidSpan {
        InvalidSpan { offset, len }
    }

    /// Push `chunks` in order, then finish; returns everything observed.
    fn feed(chunks: &[&[u8]]) -> (String, Vec<InvalidSpan>) {
        let mut carry = Utf8Carry::new();
        let mut text = String::new();
        let mut invalid = Vec::new();
        for chunk in chunks {
            let decoded = carry.push(chunk);
            text.push_str(&decoded.text);
            invalid.extend(decoded.invalid);
        }
        invalid.extend(carry.finish());
        (text, invalid)
    }

    #[test]
    fn the_full_corpus_survives_a_split_at_every_offset() {
        // The acceptance bar for the carry logic: no byte offset in the
        // real corpus may exist at which a chunk boundary changes the
        // decoded text — this is every offset, not a sample.
        let expected = UTF8_CORPUS.concat();
        let full = expected.as_bytes();
        for at in 0..=full.len() {
            let (text, invalid) = feed(&[&full[..at], &full[at..]]);
            assert!(invalid.is_empty(), "split at {at} fabricated a span");
            assert_eq!(text, expected, "split at {at} corrupted the corpus");
        }
    }

    #[test]
    fn the_full_corpus_survives_every_tiny_chunk_size() {
        // The same sweep the probe runs against the live PTY, in miniature:
        // reading N bytes at a time must decode to the same text for every N.
        let expected = UTF8_CORPUS.concat();
        let full = expected.as_bytes();
        for size in 1..=8 {
            let chunks: Vec<&[u8]> = full.chunks(size).collect();
            let (text, invalid) = feed(&chunks);
            assert!(invalid.is_empty(), "chunk size {size} fabricated a span");
            assert_eq!(text, expected, "chunk size {size} corrupted the corpus");
        }
    }

    #[test]
    fn junk_between_valid_neighbors_is_located_exactly() {
        // Three junk bytes, none of which any continuation could repair:
        // three one-byte spans at consecutive stream offsets, with both
        // neighbors decoded intact around them.
        let payload = junk_payload();
        let (text, invalid) = feed(&[&payload]);
        assert_eq!(text, junk_decoded());
        let at = JUNK_BEFORE.len() as u64;
        assert_eq!(invalid, vec![span(at, 1), span(at + 1, 1), span(at + 2, 1)]);
    }

    #[test]
    fn junk_spans_are_identical_wherever_the_chunk_boundary_falls() {
        // The junk line split at every offset — including inside the junk
        // itself and inside both multi-byte neighbors. The report must be a
        // property of the stream, not of the chunking.
        let payload = junk_payload();
        let at = JUNK_BEFORE.len() as u64;
        let expected_spans = vec![span(at, 1), span(at + 1, 1), span(at + 2, 1)];
        for cut in 0..=payload.len() {
            let (text, invalid) = feed(&[&payload[..cut], &payload[cut..]]);
            assert_eq!(text, junk_decoded(), "split at {cut} corrupted a neighbor");
            assert_eq!(invalid, expected_spans, "split at {cut} moved the spans");
        }
    }

    #[test]
    fn offsets_count_the_whole_stream_not_the_current_chunk() {
        assert_eq!(
            feed(&[b"ab", &[0x80], b"c"]),
            ("abc".to_string(), vec![span(2, 1)]),
            "a span must locate the byte in stream coordinates"
        );
    }

    #[test]
    fn a_carried_prefix_that_can_no_longer_complete_is_reported_whole() {
        // E2 82 starts a 3-byte sequence; '(' proves it will never finish.
        // Maximal-subpart semantics: the pair is one 2-byte span, then the
        // '(' decodes normally — nothing is dropped, nothing re-ordered.
        assert_eq!(
            feed(&[&[0xE2, 0x82], b"("]),
            ("(".to_string(), vec![span(0, 2)])
        );
    }

    #[test]
    fn a_stream_ending_mid_codepoint_surfaces_as_a_span() {
        let full = "🌍".as_bytes();
        let mut carry = Utf8Carry::new();
        let decoded = carry.push(&full[..2]);
        assert_eq!(decoded.text, "");
        assert!(
            decoded.invalid.is_empty(),
            "mid-stream this is not an error"
        );
        assert_eq!(carry.pending(), 2);
        assert_eq!(carry.finish(), Some(span(0, 2)));
        assert_eq!(carry.pending(), 0);
        assert_eq!(carry.finish(), None, "a second finish reports nothing new");
    }

    #[test]
    fn decoding_resumes_cleanly_after_a_finish() {
        // Not a shape the probe uses (one stream, one finish), but the
        // accounting must stay coherent if a caller reuses the carry.
        let mut carry = Utf8Carry::new();
        carry.push(&"é".as_bytes()[..1]);
        carry.finish();
        let decoded = carry.push("ok".as_bytes());
        assert_eq!(decoded.text, "ok");
        assert!(decoded.invalid.is_empty());
    }
}
