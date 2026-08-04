//! Turning arriving chunks back into rows — by modelling the one row the
//! cursor is on.
//!
//! A PTY read boundary falls wherever the kernel felt like putting it, and a
//! re-rendering terminal does not speak in lines at all: it homes the cursor
//! with a carriage return, skips already-painted cells with cursor-forward,
//! re-asserts positions mid-row after a pause, and overwrites in place. Two
//! generations of this module tried to translate that into line splitting —
//! first treating cursor motion as decoration (which glued different rows'
//! texts together), then as boundaries (which cut resumed rows into
//! unparseable fragments). Both failed on real ConPTY traffic, each with a
//! CI run to its name, because no linear rule can tell "restart this row"
//! from "continue this row" — that distinction lives in cursor arithmetic.
//!
//! So this does what the terminal does: keep the current row as a cell
//! buffer with a cursor, apply writes and motions to it, and hand the row
//! over only when the terminal *leaves* it — a line feed, a row-changing
//! motion, a clear, or a reposition to column 1. Overwrites collapse to the
//! row's settled content; a pause-and-resume lands the resumed text exactly
//! where the cursor arithmetic says it belongs. What this deliberately does
//! not model is the rest of the screen: the lanes verify a scrolling stream,
//! and one row of state is the whole difference between parsing it and
//! misparsing it.

/// A row longer than this is handed over early rather than grown further.
/// Nothing the lanes generate comes close; the cap exists so a terminal that
/// never leaves its row costs a bounded amount of memory instead of a
/// half-hour of accumulation.
const MAX_LINE_BYTES: usize = 1 << 20;

pub struct LineSplitter {
    /// Undecoded holdover: an escape sequence or UTF-8 character split
    /// across read boundaries waits here for its remainder.
    raw: Vec<u8>,
    /// The row the cursor is on, as written so far.
    row: Vec<char>,
    cursor: usize,
}

impl Default for LineSplitter {
    fn default() -> Self {
        Self::new()
    }
}

/// What one complete escape sequence means to the row.
enum Action {
    /// Decoration — colors, titles, modes. The row is untouched.
    None,
    /// The cursor left the row without homing — cursor up/down, row
    /// addressing, display clear. The row is done; the column carries over.
    LeaveRow,
    /// The cursor left the row for the start of another — next/previous
    /// line, or a reposition to column 1.
    LeaveRowToStart,
    /// Reposition within the row (columns are 1-based on the wire).
    SetColumn(usize),
    Forward(usize),
    Back(usize),
    /// Erase in line: 0 = cursor to end, 1 = start to cursor, 2 = all.
    EraseLine(u8),
}

enum Escape {
    /// The buffer ends mid-sequence — wait for more bytes.
    Incomplete,
    /// A complete sequence: how many bytes it spans, and what it does.
    Done(usize, Action),
}

impl LineSplitter {
    pub fn new() -> Self {
        Self {
            raw: Vec::new(),
            row: Vec::new(),
            cursor: 0,
        }
    }

    /// Feed one arrived chunk, calling `on_line` for every row the terminal
    /// finished saying. Rows arrive as their settled text, escape-free, with
    /// trailing blanks trimmed.
    pub fn push(&mut self, chunk: &[u8], mut on_line: impl FnMut(&str)) {
        self.raw.extend_from_slice(chunk);
        let mut consumed = 0;
        while consumed < self.raw.len() {
            match self.raw[consumed] {
                b'\n' => {
                    self.flush(&mut on_line);
                    self.cursor = 0;
                    consumed += 1;
                }
                b'\r' => {
                    // Homing is not leaving: the terminal may be about to
                    // overwrite this row, or to skip forward and resume it.
                    self.cursor = 0;
                    consumed += 1;
                }
                0x08 => {
                    self.cursor = self.cursor.saturating_sub(1);
                    consumed += 1;
                }
                0x1b => match parse_escape(&self.raw[consumed..]) {
                    Escape::Incomplete => break,
                    Escape::Done(len, action) => {
                        self.apply(action, &mut on_line);
                        consumed += len;
                    }
                },
                _ => {
                    let (advanced, chars) = decode_text_run(&self.raw[consumed..]);
                    if advanced == 0 {
                        break; // an incomplete UTF-8 tail — wait for the rest
                    }
                    for ch in chars {
                        self.write(ch, &mut on_line);
                    }
                    consumed += advanced;
                }
            }
        }
        self.raw.drain(..consumed);
        // A pathological holdover (an OSC that never terminates) must not
        // accumulate forever either: past the cap, treat it as text.
        if self.raw.len() > MAX_LINE_BYTES {
            let held = std::mem::take(&mut self.raw);
            for ch in String::from_utf8_lossy(&held).chars() {
                self.write(ch, &mut on_line);
            }
            self.flush(&mut on_line);
            self.cursor = 0;
        }
    }

    /// Whatever the row holds when the stream ends — a final row the
    /// terminal never left is still something it said.
    pub fn finish(&mut self, mut on_line: impl FnMut(&str)) {
        self.flush(&mut on_line);
        self.cursor = 0;
    }

    fn write(&mut self, ch: char, on_line: &mut impl FnMut(&str)) {
        match ch {
            '\t' => self.cursor = (self.cursor / 8 + 1) * 8,
            ch if ch.is_control() => {}
            ch => {
                while self.row.len() < self.cursor {
                    self.row.push(' ');
                }
                if self.cursor < self.row.len() {
                    self.row[self.cursor] = ch;
                } else {
                    self.row.push(ch);
                }
                self.cursor += 1;
            }
        }
        if self.row.len() > MAX_LINE_BYTES {
            self.flush(on_line);
            self.cursor = 0;
        }
    }

    fn apply(&mut self, action: Action, on_line: &mut impl FnMut(&str)) {
        match action {
            Action::None => {}
            Action::LeaveRow => self.flush(on_line),
            Action::LeaveRowToStart => {
                self.flush(on_line);
                self.cursor = 0;
            }
            Action::SetColumn(column) => {
                self.cursor = column.saturating_sub(1).min(MAX_LINE_BYTES);
            }
            Action::Forward(cells) => {
                self.cursor = (self.cursor + cells).min(MAX_LINE_BYTES);
            }
            Action::Back(cells) => self.cursor = self.cursor.saturating_sub(cells),
            Action::EraseLine(mode) => match mode {
                0 => self.row.truncate(self.cursor),
                1 => {
                    for cell in self.row.iter_mut().take(self.cursor) {
                        *cell = ' ';
                    }
                }
                _ => self.row.clear(),
            },
        }
    }

    fn flush(&mut self, on_line: &mut impl FnMut(&str)) {
        let text: String = self.row.iter().collect();
        let settled = text.trim_end();
        if !settled.is_empty() {
            on_line(settled);
        }
        self.row.clear();
    }
}

/// Decode the printable run at the front of `bytes`: how many bytes it
/// spans and its characters. Stops at the next byte the row model handles
/// itself. Returns `(0, ..)` when the run is nothing but an incomplete
/// UTF-8 tail that should wait for its remainder.
fn decode_text_run(bytes: &[u8]) -> (usize, Vec<char>) {
    let end = bytes
        .iter()
        .position(|byte| matches!(byte, b'\n' | b'\r' | 0x08 | 0x1b))
        .unwrap_or(bytes.len());
    let run = &bytes[..end];
    match std::str::from_utf8(run) {
        Ok(text) => (end, text.chars().collect()),
        Err(err) => {
            let valid = err.valid_up_to();
            match err.error_len() {
                // Invalid bytes mid-run: replace them and carry on — a byte
                // that cannot be UTF-8 is content the verifier should see
                // fail, not a reason to stop reading.
                Some(bad) => {
                    let mut chars: Vec<char> = std::str::from_utf8(&run[..valid])
                        .expect("prefix is valid")
                        .chars()
                        .collect();
                    chars.push(char::REPLACEMENT_CHARACTER);
                    (valid + bad, chars)
                }
                // A character split across reads: decode up to it, hold the
                // tail.
                None => (
                    valid,
                    std::str::from_utf8(&run[..valid])
                        .expect("prefix is valid")
                        .chars()
                        .collect(),
                ),
            }
        }
    }
}

/// Classify the escape sequence at the front of `bytes` (`bytes[0]` is ESC).
fn parse_escape(bytes: &[u8]) -> Escape {
    match bytes.get(1) {
        None => Escape::Incomplete,
        // CSI: parameter and intermediate bytes, then one final in 0x40..=0x7E.
        Some(b'[') => {
            for (offset, byte) in bytes.iter().enumerate().skip(2) {
                if (0x40..=0x7e).contains(byte) {
                    return Escape::Done(offset + 1, classify_csi(*byte, &bytes[2..offset]));
                }
            }
            Escape::Incomplete
        }
        // OSC: payload until BEL or ESC-backslash.
        Some(b']') => {
            let mut offset = 2;
            while offset < bytes.len() {
                match bytes[offset] {
                    0x07 => return Escape::Done(offset + 1, Action::None),
                    0x1b if bytes.get(offset + 1) == Some(&b'\\') => {
                        return Escape::Done(offset + 2, Action::None);
                    }
                    _ => offset += 1,
                }
            }
            Escape::Incomplete
        }
        // Any other two-character escape.
        Some(_) => Escape::Done(2, Action::None),
    }
}

/// What one complete CSI sequence does to the row. The load-bearing
/// distinction: **a position sequence targeting column 1 restarts a row;
/// one targeting any other column repositions within it** — a terminal
/// resuming a row after a pause re-asserts a mid-row position (or homes and
/// skips forward), and only cursor arithmetic tells that from a row switch.
fn classify_csi(final_byte: u8, params: &[u8]) -> Action {
    // Motion parameters default to 1 on the wire; erase modes default to 0.
    let first = |default| csi_number(params.split(|b| *b == b';').next().unwrap_or(&[]), default);
    match final_byte {
        b'A' | b'B' | b'd' | b'J' => Action::LeaveRow,
        b'E' | b'F' => Action::LeaveRowToStart,
        // CUP/HVP: `row;col`, both defaulting to 1.
        b'H' | b'f' => {
            let column = csi_number(params.split(|b| *b == b';').nth(1).unwrap_or(&[]), 1);
            if column <= 1 {
                Action::LeaveRowToStart
            } else {
                Action::SetColumn(column)
            }
        }
        // CHA/HPA: a bare column.
        b'G' | b'`' => {
            let column = first(1);
            if column <= 1 {
                Action::LeaveRowToStart
            } else {
                Action::SetColumn(column)
            }
        }
        b'C' => Action::Forward(first(1)),
        b'D' => Action::Back(first(1)),
        b'K' => Action::EraseLine(first(0).min(2) as u8),
        _ => Action::None,
    }
}

/// A CSI numeric parameter; empty or malformed takes the sequence's wire
/// default.
fn csi_number(bytes: &[u8], default: usize) -> usize {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return default;
    }
    std::str::from_utf8(bytes)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(default)
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
    fn an_in_place_overwrite_settles_to_the_rows_final_content() {
        // A terminal that homes and rewrites a row is revising it, not
        // saying two things: the row hands over what it settled to.
        assert_eq!(collect(&[b"L5 abc\rL5 abcdef\r\n"]), ["L5 abcdef"]);
    }

    #[test]
    fn decoration_is_stripped_without_disturbing_the_row() {
        assert_eq!(
            collect(&[b"\x1b[1mL7 pay\x1b[0mload\x1b[K\r\n"]),
            ["L7 payload"]
        );
    }

    /// The shape that broke the first Windows replay run: rows separated by
    /// repositions to column 1 rather than newlines.
    #[test]
    fn cursor_positioning_separates_row_statements() {
        assert_eq!(
            collect(&[b"\x1b[2J\x1b[1;1HL1 abcd\x1b[2;1HL2 efgh\r\n"]),
            ["L1 abcd", "L2 efgh"]
        );
    }

    /// The shape that broke the second run: a pause mid-row, then a mid-row
    /// reposition to resume it. The resumed text lands where the cursor
    /// arithmetic puts it — in the same row.
    #[test]
    fn a_mid_row_reposition_continues_the_row() {
        assert_eq!(collect(&[b"L8", b"\x1b[5;3H7 abcd\r\n"]), ["L87 abcd"]);
    }

    /// The shape that broke the third run — identical under both prior
    /// rule-sets: home, skip the already-painted cells with cursor-forward,
    /// resume. Only a row model resolves it, because the resumed text
    /// overlays the very cells the fragment already holds.
    #[test]
    fn a_home_and_skip_resume_continues_the_row() {
        assert_eq!(collect(&[b"L8", b"\r\x1b[2C7 abcd\r\n"]), ["L87 abcd"]);
    }

    #[test]
    fn a_positioning_move_with_defaulted_column_restarts_a_row() {
        // `ESC[H` and `ESC[5H` both mean column 1.
        assert_eq!(collect(&[b"L1 ab\x1b[HL2 cd\r\n"]), ["L1 ab", "L2 cd"]);
        assert_eq!(collect(&[b"L3 ef\x1b[7HL4 gh\r\n"]), ["L3 ef", "L4 gh"]);
    }

    #[test]
    fn an_escape_split_across_chunks_is_not_misread() {
        assert_eq!(collect(&[b"L8", b"\x1b[5;", b"3H7 abcd\r\n"]), ["L87 abcd"]);
    }

    #[test]
    fn a_character_split_across_chunks_is_not_misread() {
        // "€" is three bytes; the read boundary lands inside it.
        assert_eq!(collect(&[b"a\xe2\x82", b"\xacb\r\n"]), ["a€b"]);
    }

    #[test]
    fn erase_to_end_truncates_at_the_cursor() {
        // The terminal rewrites a row shorter than it was: the leftover tail
        // is erased, and the row settles to the shorter say.
        assert_eq!(collect(&[b"L5 abcdef\rL5 abc\x1b[K\r\n"]), ["L5 abc"]);
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
            splitter.row.len() <= MAX_LINE_BYTES && splitter.raw.len() <= MAX_LINE_BYTES,
            "held {} row / {} raw past the cap",
            splitter.row.len(),
            splitter.raw.len()
        );
    }

    #[test]
    fn an_unterminated_osc_title_costs_bounded_memory_too() {
        let mut splitter = LineSplitter::new();
        splitter.push(b"\x1b]0;title without a terminator ", |_| {});
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..32 {
            splitter.push(&chunk, |_| {});
        }
        assert!(
            splitter.row.len() <= MAX_LINE_BYTES && splitter.raw.len() <= MAX_LINE_BYTES,
            "held {} row / {} raw past the cap",
            splitter.row.len(),
            splitter.raw.len()
        );
    }
}
