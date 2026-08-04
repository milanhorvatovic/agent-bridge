//! Turning arriving chunks back into lines.
//!
//! A PTY read boundary falls wherever the kernel felt like putting it, so a
//! line arrives split across chunks as readily as three lines arrive in one.
//! This reassembles them, strips the escape sequences a terminal wraps its
//! output in, and hands over one complete line at a time.
//!
//! The terminator is treated as an equivalence class — `\r\n` and `\n` are
//! one line ending — because rewriting it is exactly what a terminal is for.
//! A bare carriage return *inside* a line is also a boundary: it is how a
//! re-rendering terminal overwrites a row in place, so `partial\rfull` is
//! two things the terminal said about that row, and the verifier wants to
//! judge each of them, not their concatenation. Everything else is left
//! alone: this is the layer that must not "fix" anything, since what it
//! would be fixing is the corruption under test.

use agent_bridge_interactive_probe::pty::strip_ansi;

/// A line longer than this is emitted as-is rather than buffered further.
/// Nothing the lanes generate comes close; the cap exists so a terminal that
/// stops sending newlines costs a bounded amount of memory instead of a
/// half-hour of accumulation.
const MAX_LINE_BYTES: usize = 1 << 20;

#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
}

impl LineSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one arrived chunk, calling `on_line` for every complete line it
    /// completes. Lines are ANSI-stripped and carry no terminator.
    pub fn push(&mut self, chunk: &[u8], mut on_line: impl FnMut(&str)) {
        self.buf.extend_from_slice(chunk);
        let mut start = 0;
        while let Some(offset) = memchr(b'\n', &self.buf[start..]) {
            let end = start + offset;
            emit(&self.buf[start..end], &mut on_line);
            start = end + 1;
        }
        if start > 0 {
            self.buf.drain(..start);
        }
        if self.buf.len() > MAX_LINE_BYTES {
            let held = std::mem::take(&mut self.buf);
            emit(&held, &mut on_line);
        }
    }

    /// Whatever is left when the stream ends — a final line that never got
    /// its terminator is still a line that arrived.
    pub fn finish(&mut self, mut on_line: impl FnMut(&str)) {
        if !self.buf.is_empty() {
            let held = std::mem::take(&mut self.buf);
            emit(&held, &mut on_line);
        }
    }
}

fn emit(raw: &[u8], on_line: &mut impl FnMut(&str)) {
    // Lossy on purpose: a byte that cannot be UTF-8 is not a reason to stop
    // reading a stream whose integrity is the thing being measured. It will
    // fail its line's content check, which is the report the reader wants.
    let text = String::from_utf8_lossy(raw);
    let stripped = strip_ansi(&text);
    // Carriage returns split the line into what the terminal said about the
    // row, in order. Empty pieces are terminator residue (the CR half of a
    // CRLF), not content — nothing to judge.
    for piece in stripped.split('\r') {
        if !piece.is_empty() {
            on_line(piece);
        }
    }
}

fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|byte| *byte == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(chunks: &[&[u8]]) -> Vec<String> {
        let mut splitter = LineSplitter::new();
        let mut lines = Vec::new();
        for chunk in chunks {
            splitter.push(chunk, |line| lines.push(line.to_string()));
        }
        splitter.finish(|line| lines.push(line.to_string()));
        lines
    }

    #[test]
    fn a_line_split_across_chunks_arrives_whole() {
        assert_eq!(
            collect(&[b"L1 ab", b"cd\nL2 ", b"efgh\n"]),
            ["L1 abcd", "L2 efgh"]
        );
    }

    #[test]
    fn several_lines_in_one_chunk_all_arrive() {
        assert_eq!(collect(&[b"a\nb\nc\n"]), ["a", "b", "c"]);
    }

    #[test]
    fn both_terminator_forms_are_one_line_ending() {
        assert_eq!(collect(&[b"crlf\r\nlf\n"]), ["crlf", "lf"]);
    }

    #[test]
    fn an_in_place_overwrite_yields_each_version_of_the_row() {
        // A re-rendering terminal completes a partial row by returning to
        // column 0 and writing it again; both versions must reach the
        // verifier separately, not glued into one unparseable line.
        assert_eq!(
            collect(&[b"L5 abc\rL5 abcdef\r\n"]),
            ["L5 abc", "L5 abcdef"]
        );
    }

    #[test]
    fn escape_sequences_are_stripped_from_the_line() {
        assert_eq!(
            collect(&[b"\x1b[2J\x1b[1;1HL7 payload\x1b[0m\r\n"]),
            ["L7 payload"]
        );
    }

    #[test]
    fn a_final_line_without_a_terminator_still_arrives() {
        assert_eq!(
            collect(&[b"complete\nunterminated"]),
            ["complete", "unterminated"]
        );
    }

    #[test]
    fn an_empty_stream_yields_nothing() {
        assert!(collect(&[]).is_empty());
        assert!(collect(&[b""]).is_empty());
    }

    #[test]
    fn a_terminal_that_stops_sending_newlines_costs_bounded_memory() {
        let mut splitter = LineSplitter::new();
        let mut emitted = 0usize;
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..32 {
            splitter.push(&chunk, |_| emitted += 1);
        }
        assert!(emitted >= 1, "the cap must flush rather than accumulate");
        assert!(
            splitter.buf.len() <= MAX_LINE_BYTES,
            "held {} bytes past the cap",
            splitter.buf.len()
        );
    }
}
