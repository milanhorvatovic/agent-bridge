//! Incremental UTF-8 reassembly for a PTY read loop: chunks arrive split at
//! arbitrary byte boundaries, so a multi-byte codepoint may straddle two
//! reads. Complete codepoints are decoded as each chunk arrives, an
//! incomplete trailing codepoint is carried into the next push, and
//! genuinely invalid bytes — including a stream that ends mid-codepoint —
//! are surfaced as an error, never silently dropped.
//!
//! Copied from the PTY allocation probe (`tools/pty-probe`), whose
//! spawn-read-teardown skeleton this probe extends.

/// The byte stream contained (or ended inside) a sequence that can never
/// become valid UTF-8, which must be reported rather than silently dropped.
#[derive(Debug, PartialEq)]
pub struct InvalidUtf8;

#[derive(Default)]
pub struct Reassembler {
    decoded: String,
    carry: Vec<u8>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode `chunk`, appending complete codepoints to the decoded text and
    /// carrying an incomplete trailing codepoint into the next push.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), InvalidUtf8> {
        self.carry.extend_from_slice(chunk);
        match std::str::from_utf8(&self.carry) {
            Ok(text) => {
                self.decoded.push_str(text);
                self.carry.clear();
                Ok(())
            }
            Err(err) => {
                let valid = err.valid_up_to();
                // Unreachable panic: `valid_up_to` guarantees the prefix is
                // valid UTF-8.
                self.decoded
                    .push_str(std::str::from_utf8(&self.carry[..valid]).unwrap());
                // Drain the decoded prefix in both branches: on the error
                // path too, or a retried push would decode it a second time.
                self.carry.drain(..valid);
                match err.error_len() {
                    // The suffix is not wrong, just not complete yet — carry
                    // it into the next chunk.
                    None => Ok(()),
                    // No continuation could ever repair these bytes.
                    Some(_) => Err(InvalidUtf8),
                }
            }
        }
    }

    /// The text decoded so far (complete codepoints only).
    pub fn decoded(&self) -> &str {
        &self.decoded
    }

    /// Take the decoded text, leaving the reassembler empty but keeping any
    /// carried partial codepoint. A caller that drains after every push keeps
    /// this buffer from growing with the length of the session; one that
    /// never drains gets the accumulating `decoded` above.
    pub fn take_decoded(&mut self) -> String {
        std::mem::take(&mut self.decoded)
    }

    /// Bytes held back waiting for the rest of their codepoint. Non-zero at
    /// end-of-stream means the stream ended mid-codepoint — truncated output
    /// that must be reported, never swallowed.
    pub fn pending(&self) -> usize {
        self.carry.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_survives_mid_codepoint_chunk_split() {
        // "héllo 🌍" split inside both the 2-byte 'é' and the 4-byte '🌍'.
        let full = "héllo 🌍".as_bytes();
        let mut reassembler = Reassembler::new();
        for chunk in [&full[..2], &full[2..9], &full[9..]] {
            reassembler.push(chunk).unwrap();
        }
        assert_eq!(reassembler.decoded(), "héllo 🌍");
        assert_eq!(reassembler.pending(), 0);
    }

    #[test]
    fn genuinely_invalid_utf8_is_detected_not_dropped() {
        // 0xFF can never appear in UTF-8; the reassembler must surface the
        // error rather than silently dropping the bytes.
        let mut reassembler = Reassembler::new();
        reassembler.push(b"ok ").unwrap();
        assert_eq!(reassembler.push(&[0xFF, 0xFE]), Err(InvalidUtf8));
    }

    #[test]
    fn error_path_does_not_duplicate_the_decoded_prefix_on_retry() {
        // The valid prefix decoded alongside an invalid byte must be drained
        // from the carry, or a retried push would decode it twice.
        let mut reassembler = Reassembler::new();
        assert_eq!(reassembler.push(b"ok \xFF"), Err(InvalidUtf8));
        assert_eq!(reassembler.decoded(), "ok ");
        assert_eq!(reassembler.push(b"more"), Err(InvalidUtf8));
        assert_eq!(reassembler.decoded(), "ok ", "prefix must not re-decode");
    }

    #[test]
    fn taking_the_decoded_text_leaves_the_carried_codepoint_alone() {
        // Draining must not disturb the partial codepoint waiting for its
        // continuation bytes, or the next push would decode garbage.
        let full = "é".as_bytes();
        let mut reassembler = Reassembler::new();
        reassembler.push(b"ok").unwrap();
        reassembler.push(&full[..1]).unwrap();
        assert_eq!(reassembler.take_decoded(), "ok");
        assert_eq!(reassembler.decoded(), "");
        assert_eq!(reassembler.pending(), 1);
        reassembler.push(&full[1..]).unwrap();
        assert_eq!(reassembler.decoded(), "é");
    }

    #[test]
    fn truncated_final_codepoint_is_left_pending_not_dropped() {
        // A stream ending mid-codepoint must not silently lose the carried
        // suffix: at end-of-stream, pending bytes mean truncated output.
        let full = "héllo 🌍".as_bytes();
        let mut reassembler = Reassembler::new();
        reassembler.push(&full[..2]).unwrap();
        reassembler.push(&full[2..full.len() - 1]).unwrap();
        assert!(
            reassembler.pending() > 0,
            "a truncated final codepoint must stay observable as pending bytes"
        );
    }
}
