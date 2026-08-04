//! Turning arriving chunks back into the things the terminal said, one row
//! statement at a time.
//!
//! A PTY read boundary falls wherever the kernel felt like putting it, so a
//! line arrives split across chunks as readily as three lines arrive in one.
//! This reassembles them, strips the escape decoration a terminal wraps its
//! output in, and hands over one segment at a time.
//!
//! What ends a segment is the crux, and it is more than the newline:
//!
//! - `\n` and `\r\n` are one line ending — rewriting terminators is what a
//!   terminal is for.
//! - A bare carriage return is how a terminal overwrites a row in place, so
//!   `partial\rfull` is two statements about that row, judged separately.
//! - **A cursor-motion or clear sequence is a boundary too.** A re-rendering
//!   terminal (ConPTY) separates and repaints rows by *positioning the
//!   cursor*, not by sending newlines — and it skips unchanged spans with
//!   cursor-forward moves. Stripping those sequences as decoration would
//!   glue two rows' texts into one unparseable line and turn honest repaint
//!   traffic into fault reports; this is not hypothetical, it is how the
//!   first Windows run of the replay lane failed. Colors, erases, and
//!   titles remain decoration: they say how text looks, not where it goes.
//!
//! Everything else is left alone: this is the layer that must not "fix"
//! anything, since what it would be fixing is the corruption under test.

use agent_bridge_interactive_probe::pty::strip_ansi;

/// A segment longer than this is emitted as-is rather than buffered further.
/// Nothing the lanes generate comes close; the cap exists so a terminal that
/// stops sending boundaries costs a bounded amount of memory instead of a
/// half-hour of accumulation.
const MAX_LINE_BYTES: usize = 1 << 20;

/// CSI final bytes that always end a row statement: cursor up/down/forward/
/// back (`A B C D`), next/previous line (`E F`), row addressing (`d`), and
/// display clear (`J`). Position (`H f`) and column (`G`) sequences are
/// classified by their *target column* — see `classify_csi`.
const BOUNDARY_FINALS: &[u8] = b"ABCDEFdJ";

/// What an escape sequence starting at the front of a byte slice turned out
/// to be.
enum Escape {
    /// The buffer ends mid-sequence — wait for more bytes.
    Incomplete,
    /// Decoration, `len` bytes long — skipped over, stripped at emit time.
    Plain(usize),
    /// A row boundary, `len` bytes long — ends the current segment.
    Boundary(usize),
}

#[derive(Default)]
pub struct LineSplitter {
    buf: Vec<u8>,
    /// How far into `buf` scanning has already looked without finding a
    /// boundary, so a segment arriving in many chunks is not rescanned per
    /// chunk.
    scanned: usize,
}

impl LineSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one arrived chunk, calling `on_line` for every segment it
    /// completes. Segments are ANSI-stripped and carry no terminator.
    pub fn push(&mut self, chunk: &[u8], mut on_line: impl FnMut(&str)) {
        self.buf.extend_from_slice(chunk);
        let mut consumed = 0;
        let mut cursor = self.scanned;
        while cursor < self.buf.len() {
            match self.buf[cursor] {
                b'\n' | b'\r' => {
                    emit(&self.buf[consumed..cursor], &mut on_line);
                    cursor += 1;
                    consumed = cursor;
                }
                0x1b => match parse_escape(&self.buf[cursor..]) {
                    Escape::Incomplete => break,
                    Escape::Boundary(len) => {
                        emit(&self.buf[consumed..cursor], &mut on_line);
                        cursor += len;
                        consumed = cursor;
                    }
                    Escape::Plain(len) => cursor += len,
                },
                _ => cursor += 1,
            }
        }
        self.scanned = cursor - consumed;
        if consumed > 0 {
            self.buf.drain(..consumed);
        }
        if self.buf.len() > MAX_LINE_BYTES {
            let held = std::mem::take(&mut self.buf);
            self.scanned = 0;
            emit(&held, &mut on_line);
        }
    }

    /// Whatever is left when the stream ends — a final segment that never
    /// got its boundary is still something that arrived.
    pub fn finish(&mut self, mut on_line: impl FnMut(&str)) {
        if !self.buf.is_empty() {
            let held = std::mem::take(&mut self.buf);
            self.scanned = 0;
            emit(&held, &mut on_line);
        }
    }
}

/// Classify the escape sequence at the front of `bytes` (`bytes[0]` is ESC).
/// The length covers the whole sequence, so the scanner steps over exactly
/// what a stripper would remove.
fn parse_escape(bytes: &[u8]) -> Escape {
    match bytes.get(1) {
        None => Escape::Incomplete,
        // CSI: parameter and intermediate bytes, then one final in 0x40..=0x7E.
        Some(b'[') => {
            for (offset, byte) in bytes.iter().enumerate().skip(2) {
                if (0x40..=0x7e).contains(byte) {
                    return classify_csi(*byte, &bytes[2..offset], offset + 1);
                }
            }
            Escape::Incomplete
        }
        // OSC: payload until BEL or ESC-backslash.
        Some(b']') => {
            let mut offset = 2;
            while offset < bytes.len() {
                match bytes[offset] {
                    0x07 => return Escape::Plain(offset + 1),
                    0x1b if bytes.get(offset + 1) == Some(&b'\\') => {
                        return Escape::Plain(offset + 2);
                    }
                    _ => offset += 1,
                }
            }
            Escape::Incomplete
        }
        // Any other two-character escape.
        Some(_) => Escape::Plain(2),
    }
}

/// The row-boundary judgement for one complete CSI sequence.
///
/// Position (`H`/`f`) and column-absolute (`G`/`` ` ``) moves carry the
/// distinction in their parameters: **a move to column 1 restarts a row, a
/// move to any other column continues one.** A re-rendering terminal that
/// pauses mid-row re-asserts the cursor position before resuming — a
/// mid-column move whose following text belongs to the row already in
/// progress; splitting there is how the second Windows run lost a line into
/// two unparseable fragments. Everything in `BOUNDARY_FINALS` ends a row
/// unconditionally; everything else is decoration.
fn classify_csi(final_byte: u8, params: &[u8], len: usize) -> Escape {
    match final_byte {
        _ if BOUNDARY_FINALS.contains(&final_byte) => Escape::Boundary(len),
        // CUP/HVP: `row;col`, both defaulting to 1.
        b'H' | b'f' => {
            let column = params
                .split(|byte| *byte == b';')
                .nth(1)
                .map_or(1, parse_csi_number);
            if column <= 1 {
                Escape::Boundary(len)
            } else {
                Escape::Plain(len)
            }
        }
        // CHA/HPA: a bare column, defaulting to 1.
        b'G' | b'`' => {
            if parse_csi_number(params) <= 1 {
                Escape::Boundary(len)
            } else {
                Escape::Plain(len)
            }
        }
        _ => Escape::Plain(len),
    }
}

/// A CSI numeric parameter; empty or malformed defaults to 1, the CSI
/// convention.
fn parse_csi_number(bytes: &[u8]) -> u64 {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return 1;
    }
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(1)
}

fn emit(raw: &[u8], on_line: &mut impl FnMut(&str)) {
    if raw.is_empty() {
        // Boundary residue — the LF half of a CRLF, back-to-back cursor
        // moves. Nothing to judge.
        return;
    }
    // Lossy on purpose: a byte that cannot be UTF-8 is not a reason to stop
    // reading a stream whose integrity is the thing being measured. It will
    // fail its line's content check, which is the report the reader wants.
    let text = String::from_utf8_lossy(raw);
    let stripped = strip_ansi(&text);
    if !stripped.is_empty() {
        on_line(&stripped);
    }
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
    fn decoration_is_stripped_without_splitting_the_line() {
        // Colors and erase-to-end say how the text looks, not where it goes:
        // one row statement, decorated.
        assert_eq!(
            collect(&[b"\x1b[1mL7 pay\x1b[0mload\x1b[K\r\n"]),
            ["L7 payload"]
        );
    }

    /// The ConPTY shape that broke the first Windows replay run: rows
    /// separated by cursor positioning rather than newlines. A move to
    /// column 1 ends a row statement; stripping it as decoration would glue
    /// two rows into one unparseable line.
    #[test]
    fn cursor_positioning_separates_row_statements() {
        assert_eq!(
            collect(&[b"\x1b[2J\x1b[1;1HL1 abcd\x1b[2;1HL2 efgh\r\n"]),
            ["L1 abcd", "L2 efgh"]
        );
    }

    /// The ConPTY shape that broke the *second* Windows replay run: a pause
    /// mid-row, then the terminal re-asserting the cursor position before
    /// resuming the same row. A move to a mid-row column is a continuation,
    /// not a boundary — splitting there loses the row into two unparseable
    /// fragments.
    #[test]
    fn a_mid_row_reposition_continues_the_row() {
        assert_eq!(collect(&[b"L8", b"\x1b[5;3H7 abcd\r\n"]), ["L87 abcd"]);
    }

    #[test]
    fn a_positioning_move_with_defaulted_column_restarts_a_row() {
        // `ESC[H` and `ESC[5H` both mean column 1.
        assert_eq!(collect(&[b"L1 ab\x1b[HL2 cd\r\n"]), ["L1 ab", "L2 cd"]);
        assert_eq!(collect(&[b"L3 ef\x1b[7HL4 gh\r\n"]), ["L3 ef", "L4 gh"]);
    }

    #[test]
    fn cursor_forward_skips_split_a_repainted_row() {
        // A diffing re-renderer skips unchanged cells with cursor-forward;
        // the fragments must arrive separately (as repaint noise the
        // verifier tolerates), never glued into a fake row.
        assert_eq!(collect(&[b"L5 ab\x1b[10Cxy\r\n"]), ["L5 ab", "xy"]);
    }

    #[test]
    fn an_escape_split_across_chunks_is_not_misread() {
        // The boundary sequence arrives in two reads; the scanner must wait
        // for its completion rather than treating half an escape as text.
        assert_eq!(
            collect(&[b"L1 abcd\x1b[2", b";1HL2 efgh\r\n"]),
            ["L1 abcd", "L2 efgh"]
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

    #[test]
    fn an_unterminated_osc_title_costs_bounded_memory_too() {
        // An OSC with no terminator parks the scanner at Incomplete; the cap
        // must still flush rather than accumulate forever. (What the flush
        // emits is stripped as the OSC's payload — the guarantee here is the
        // memory bound, not salvage of a malformed sequence.)
        let mut splitter = LineSplitter::new();
        splitter.push(b"\x1b]0;title without a terminator ", |_| {});
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..32 {
            splitter.push(&chunk, |_| {});
        }
        assert!(
            splitter.buf.len() <= MAX_LINE_BYTES,
            "held {} bytes past the cap",
            splitter.buf.len()
        );
    }
}
