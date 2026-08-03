//! Terminal-control stripping and line segmentation — the front half of the
//! text-matching pipeline.
//!
//! The segmenter is an incremental state machine, not a regex over a whole
//! buffer, because replay feeds bytes at the recorded PTY-read boundaries and
//! an escape sequence can straddle any of them. The classification a fixture
//! produces must not depend on where the kernel happened to split a read, so
//! the machine carries its state across `feed` calls; a unit test holds the
//! byte-at-a-time replay to the same output as the whole-buffer one.
//!
//! What "stripping" means here is deliberately naive: control sequences are
//! removed, printable text is kept, and a line completes on a literal
//! newline or carriage return. Nothing interprets cursor motion, so a TUI
//! that paints "Do you want to proceed?" with cursor positioning between
//! words comes out as `Doyouwanttoproceed?`, and a full-screen repaint with
//! no trailing newline accumulates into one long line until the next literal
//! line break. That is the fidelity the measurement needs: this pipeline
//! configuration sees exactly what a line-oriented consumer of the stripped
//! stream would see, fragmentation artefacts included.

/// Escape byte (0x1B) — the introducer for every sequence class we strip.
const ESC: u8 = 0x1b;

/// A hard bound on line accumulation so a pathological stream cannot grow a
/// line without limit. Real fixtures stay far below this (the largest whole
/// fixture is ~60 KiB); hitting the cap force-completes the line and is
/// counted by the caller as a measurement artefact, never hidden.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// A line the segmenter completed. `forced` marks a completion caused by the
/// [`MAX_LINE_BYTES`] cap rather than a line break in the stream.
#[derive(Debug, PartialEq, Eq)]
pub struct CompletedLine {
    pub text: String,
    pub forced: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Printable text; the only state that emits bytes into the line.
    Ground,
    /// Saw `\r` in Ground: the line already completed, swallow one `\n`.
    AfterCr,
    /// Saw ESC; the next byte selects the sequence class.
    Escape,
    /// Inside `ESC [` — parameter and intermediate bytes until a final byte.
    Csi,
    /// Inside an `ESC ]` / `ESC P` / `ESC X` / `ESC ^` / `ESC _` string,
    /// consumed until BEL or the ST terminator (`ESC \`).
    StringBody,
    /// Saw ESC inside a string body: `\` closes the string, anything else
    /// aborts it and starts a fresh escape sequence.
    StringEsc,
    /// Inside `ESC (` / `)` / `*` / `+` — one charset byte follows.
    Charset,
}

/// Incremental stripper + segmenter. Feed byte chunks in stream order, then
/// call `finish` once to flush a trailing unterminated line.
pub struct LineSegmenter {
    state: State,
    line: Vec<u8>,
}

impl LineSegmenter {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            line: Vec::new(),
        }
    }

    /// Consume one chunk, appending every line it completes to `out`.
    pub fn feed(&mut self, chunk: &[u8], out: &mut Vec<CompletedLine>) {
        for &byte in chunk {
            // A state transition may hand the byte back for re-dispatch (an
            // ESC that aborts a string still introduces a real sequence), so
            // loop until the byte is consumed.
            let mut consumed = false;
            while !consumed {
                consumed = self.push(byte, out);
            }
        }
    }

    /// Flush the trailing line, if the stream did not end on a line break.
    pub fn finish(self, out: &mut Vec<CompletedLine>) {
        if !self.line.is_empty() {
            let text = String::from_utf8_lossy(&self.line).into_owned();
            out.push(CompletedLine {
                text,
                forced: false,
            });
        }
    }

    fn push(&mut self, byte: u8, out: &mut Vec<CompletedLine>) -> bool {
        match self.state {
            State::Ground => match byte {
                b'\n' => self.complete(out),
                b'\r' => {
                    self.complete(out);
                    self.state = State::AfterCr;
                }
                ESC => self.state = State::Escape,
                b'\t' => self.line.push(byte),
                // Remaining C0 controls and DEL carry no text; BEL and
                // backspace included — nothing edits the line after the
                // fact, per the naive contract above.
                0x00..=0x1f | 0x7f => {}
                _ => {
                    self.line.push(byte);
                    if self.line.len() >= MAX_LINE_BYTES {
                        self.force_complete(out);
                    }
                }
            },
            State::AfterCr => {
                self.state = State::Ground;
                // Swallow the `\n` of a `\r\n` pair; anything else replays
                // against Ground so `\r` alone still terminated the line.
                if byte != b'\n' {
                    return false;
                }
            }
            State::Escape => match byte {
                b'[' => self.state = State::Csi,
                b']' | b'P' | b'X' | b'^' | b'_' => self.state = State::StringBody,
                b'(' | b')' | b'*' | b'+' => self.state = State::Charset,
                ESC => {}
                // Two-byte escape (ESC M, ESC 7, ESC =, ...): consumed.
                _ => self.state = State::Ground,
            },
            State::Csi => match byte {
                // Parameter (0x30–0x3F) and intermediate (0x20–0x2F) bytes.
                0x20..=0x3f => {}
                // Final byte ends the sequence.
                0x40..=0x7e => self.state = State::Ground,
                ESC => self.state = State::Escape,
                // Stray controls inside a sequence: dropped, sequence
                // continues — the tolerant reading keeps one malformed
                // paint from desynchronizing the rest of the stream.
                _ => {}
            },
            State::StringBody => match byte {
                0x07 => self.state = State::Ground,
                ESC => self.state = State::StringEsc,
                _ => {}
            },
            State::StringEsc => {
                if byte == b'\\' {
                    self.state = State::Ground;
                } else {
                    self.state = State::Escape;
                    return false;
                }
            }
            State::Charset => self.state = State::Ground,
        }
        true
    }

    fn complete(&mut self, out: &mut Vec<CompletedLine>) {
        let text = String::from_utf8_lossy(&self.line).into_owned();
        self.line.clear();
        out.push(CompletedLine {
            text,
            forced: false,
        });
    }

    fn force_complete(&mut self, out: &mut Vec<CompletedLine>) {
        let text = String::from_utf8_lossy(&self.line).into_owned();
        self.line.clear();
        out.push(CompletedLine { text, forced: true });
    }
}

impl Default for LineSegmenter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment_all(bytes: &[u8]) -> Vec<CompletedLine> {
        let mut segmenter = LineSegmenter::new();
        let mut out = Vec::new();
        segmenter.feed(bytes, &mut out);
        segmenter.finish(&mut out);
        out
    }

    fn texts(lines: &[CompletedLine]) -> Vec<&str> {
        lines.iter().map(|line| line.text.as_str()).collect()
    }

    #[test]
    fn plain_lines_split_on_lf_cr_and_crlf() {
        let lines = segment_all(b"one\ntwo\rthree\r\nfour");
        assert_eq!(texts(&lines), ["one", "two", "three", "four"]);
    }

    #[test]
    fn csi_sequences_are_stripped_including_private_params() {
        let lines = segment_all(b"\x1b[2J\x1b[1;32mgreen\x1b[0m \x1b[?25lhidden\x1b[>4;0m\n");
        assert_eq!(texts(&lines), ["green hidden"]);
    }

    #[test]
    fn osc_title_sequences_are_stripped_with_both_terminators() {
        let bel = segment_all(b"\x1b]0;title\x07text\n");
        assert_eq!(texts(&bel), ["text"]);
        let st = segment_all(b"\x1b]0;title\x1b\\text\n");
        assert_eq!(texts(&st), ["text"]);
    }

    #[test]
    fn esc_inside_a_string_aborts_it_and_starts_a_new_sequence() {
        // The OSC never terminates; the ESC that follows introduces a CSI
        // whose final byte returns to ground — the trailing text survives.
        let lines = segment_all(b"\x1b]0;title\x1b[2Kkept\n");
        assert_eq!(texts(&lines), ["kept"]);
    }

    #[test]
    fn charset_and_two_byte_escapes_are_stripped() {
        let lines = segment_all(b"\x1b(Bascii\x1bM\x1b7\x1b=text\n");
        assert_eq!(texts(&lines), ["asciitext"]);
    }

    #[test]
    fn control_bytes_are_dropped_without_editing_the_line() {
        // Backspace does not erase: the naive contract keeps what was
        // painted and drops only the control byte itself.
        let lines = segment_all(b"ab\x08c\x07d\n");
        assert_eq!(texts(&lines), ["abcd"]);
    }

    #[test]
    fn multibyte_utf8_survives_stripping() {
        let lines = segment_all("⎿  $ echo ⏺ ❯\n".as_bytes());
        assert_eq!(texts(&lines), ["⎿  $ echo ⏺ ❯"]);
    }

    #[test]
    fn chunk_boundaries_never_change_the_output() {
        // A CSI split mid-sequence, a CRLF split between chunks, and a
        // multibyte character split mid-encoding: byte-at-a-time replay must
        // equal the whole-buffer replay.
        let bytes = "before\x1b[1;32m mid⏺dle\r\nafter\x1b]0;t\x07end\n".as_bytes();
        let whole = segment_all(bytes);

        let mut segmenter = LineSegmenter::new();
        let mut split = Vec::new();
        for byte in bytes {
            segmenter.feed(std::slice::from_ref(byte), &mut split);
        }
        segmenter.finish(&mut split);

        assert_eq!(whole, split);
    }

    #[test]
    fn trailing_unterminated_line_is_flushed_by_finish() {
        let lines = segment_all(b"complete\npartial");
        assert_eq!(texts(&lines), ["complete", "partial"]);
        assert!(!lines[1].forced);
    }

    #[test]
    fn line_cap_forces_completion_and_marks_it() {
        let mut bytes = vec![b'x'; MAX_LINE_BYTES + 10];
        bytes.push(b'\n');
        let lines = segment_all(&bytes);
        assert_eq!(lines.len(), 2, "cap splits the run into two lines");
        assert!(lines[0].forced);
        assert_eq!(lines[0].text.len(), MAX_LINE_BYTES);
        assert!(!lines[1].forced);
        assert_eq!(lines[1].text.len(), 10);
    }
}
