//! Bytes to text, across whatever boundaries the reads happened to fall on.
//!
//! The emulator behind the reconstructed screen consumes characters, not
//! bytes, so something has to carry a character split across two reads from
//! the first to the second. The layer that hosts the process already does
//! that for its own consumers — but this one cannot inherit the guarantee,
//! because the fixture replays deliberately re-cut the same recording at
//! arbitrary offsets to prove the screen does not depend on where the cuts
//! fell. A decoder that only works on well-cut input could not be tested for
//! the property it exists to provide.
//!
//! Undecodable bytes become `U+FFFD`, one per maximal invalid run, which is
//! what a terminal does with them. Replacing here rather than upstream is
//! deliberate: the byte pipe carries such runs through with their position so
//! that a diagnosis can name them, and choosing what the *screen* shows in
//! their place is a question about display, which is this layer's to answer.

/// Holds the tail of a character that a read cut in half.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    /// The incomplete trailing sequence, at most three bytes — the longest
    /// prefix of a UTF-8 character that is not yet a character.
    carry: Vec<u8>,
}

impl Decoder {
    /// Decodes what `bytes` completes and appends it to `out`, keeping any
    /// unfinished character for the next call.
    ///
    /// An unfinished character is not yet anything to show, so holding it is
    /// the whole of the policy — a stream that stops mid-character leaves a
    /// screen without it, which is what the terminal it is reconstructing
    /// would also show.
    pub(crate) fn push(&mut self, bytes: &[u8], out: &mut String) {
        // Nothing held back from last time, which is the state a stream
        // spends nearly all of its life in. Both outcomes that do not
        // involve undecodable bytes are answered here, without the read ever
        // being copied into the carry — including the one where it ends
        // mid-character, which is not rare at all on output full of
        // multi-byte glyphs and is exactly where a carry-everything path
        // would start copying every read it saw.
        if self.carry.is_empty() {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    out.push_str(text);
                    return;
                }
                Err(error) if error.error_len().is_none() => {
                    let valid = error.valid_up_to();
                    out.push_str(
                        std::str::from_utf8(&bytes[..valid]).expect("the prefix decoded already"),
                    );
                    self.carry.extend_from_slice(&bytes[valid..]);
                    return;
                }
                // Undecodable bytes somewhere in the middle: the general
                // path below reports each run and carries on past it.
                Err(_) => {}
            }
        }

        self.carry.extend_from_slice(bytes);
        let mut consumed = 0;
        loop {
            let rest = &self.carry[consumed..];
            match std::str::from_utf8(rest) {
                Ok(text) => {
                    out.push_str(text);
                    consumed = self.carry.len();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // Safe by construction: `valid_up_to` is where the
                    // decode succeeded up to.
                    out.push_str(
                        std::str::from_utf8(&rest[..valid]).expect("the prefix decoded already"),
                    );
                    match error.error_len() {
                        // A run that no continuation can rescue. One
                        // replacement character stands for the whole run,
                        // matching how a terminal — and `String::from_utf8_lossy`
                        // — account for it.
                        Some(run) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            consumed += valid + run;
                        }
                        // The read ended part-way through a character. Hold
                        // the fragment for the bytes that finish it.
                        None => {
                            consumed += valid;
                            break;
                        }
                    }
                }
            }
        }
        self.carry.drain(..consumed);
    }

    /// How many bytes of an unfinished character are being held.
    ///
    /// Only the tests ask, and they ask because "held rather than shown" is
    /// otherwise invisible from the outside: a screen missing a character it
    /// never received looks the same as one that dropped it.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.carry.len()
    }
}

#[cfg(test)]
mod tests {
    use super::Decoder;

    /// Decode `chunks` in order and return everything the screen would see.
    fn decode(chunks: &[&[u8]]) -> String {
        let mut decoder = Decoder::default();
        let mut out = String::new();
        for chunk in chunks {
            decoder.push(chunk, &mut out);
        }
        out
    }

    #[test]
    fn a_character_split_across_reads_arrives_whole() {
        let arrow = "❯ 1. Yes".as_bytes();
        assert_eq!(decode(&[&arrow[..2], &arrow[2..]]), "❯ 1. Yes");
    }

    #[test]
    fn a_character_split_one_byte_at_a_time_arrives_whole() {
        // The worst cut a four-byte character can suffer, and the one a
        // randomized chunking will eventually produce.
        let emoji = "🭬".as_bytes();
        let one_at_a_time: Vec<&[u8]> = emoji.chunks(1).collect();
        assert_eq!(decode(&one_at_a_time), "🭬");
    }

    #[test]
    fn undecodable_bytes_become_one_replacement_each_and_do_not_stop_the_stream() {
        // The property that matters is the second half: a screen that stopped
        // at the first bad byte would be a screen missing everything the CLI
        // drew afterwards.
        assert_eq!(
            decode(&[b"ok", &[0xFF, 0xFE], b"after"]),
            "ok\u{fffd}\u{fffd}after"
        );
    }

    #[test]
    fn a_truncated_character_at_the_end_is_held_not_shown() {
        let mut decoder = Decoder::default();
        let mut out = String::new();
        let euro = "€".as_bytes();
        decoder.push(b"ok ", &mut out);
        decoder.push(&euro[..2], &mut out);
        assert_eq!(out, "ok ");
        assert_eq!(decoder.pending(), 2);
        decoder.push(&euro[2..], &mut out);
        assert_eq!(out, "ok €");
        assert_eq!(decoder.pending(), 0);
    }

    #[test]
    fn where_the_chunk_boundaries_fall_does_not_change_the_text() {
        let source = "❯ 1. Yes\r\n  2. No — \u{1b}[1mbold\u{1b}[0m 漢字 🭬";
        let bytes = source.as_bytes();
        let whole = decode(&[bytes]);
        for width in 1..=bytes.len() {
            let cut: Vec<&[u8]> = bytes.chunks(width).collect();
            assert_eq!(
                decode(&cut),
                whole,
                "cutting every {width} bytes changed the text"
            );
        }
    }
}
