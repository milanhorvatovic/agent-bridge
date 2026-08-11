//! The stream with the terminal's instructions removed.
//!
//! Raw CLI output is not text: it is text braided with instructions to a
//! terminal — reposition, restyle, retitle, write the clipboard, report the
//! mouse. Handing that braid to event consumers would make every one of
//! them an escape parser, and the ones that skipped the work would carry
//! the hazards instead: content that was never shown on screen, hyperlinks
//! whose targets say nothing their text says, sequences that act on the
//! terminal of whoever views them later. So the runtime strips once, here,
//! and consumers of the text path receive words.
//!
//! The same bytes feed two consumers — this stripper for the text path and
//! the reconstructed screen for the visual one — and both must agree on
//! where a sequence begins and ends. They agree by construction: the
//! stripper drives the *same parser* the screen's emulator runs, and this
//! module owns only policy — what is text, what is a removal, what an
//! overgrown sequence degrades to, and what each removal is called. An
//! independent implementation of the same parsing model checks that split
//! in the test suite, over the recorded corpus.
//!
//! Three properties hold under any input at all, and the adversarial suite
//! exists to keep them: stripping never panics; the output text carries no
//! ESC and no C1 control; and nothing is dropped silently — a sequence the
//! stripper gives up on is re-emitted as visible text, tagged, where a
//! matcher can still route it.
//!
//! One honest limit, so nobody reads this layer as more than it is: on a
//! redrawing interface, stripping alone yields the same words repeated —
//! that is what a repaint *is* — and the reconstructed screen, not this
//! module, is the answer for those. The corpus test over real recordings
//! documents that division of labor rather than pretending strip suffices.

mod classify;

pub use classify::SeqClass;

use std::borrow::Cow;
use std::ops::Range;

use avt::parser::{Parser, State};

use classify::classify;

/// How much of a CSI or escape sequence the stripper will buffer before
/// abandoning it, in bytes.
///
/// Real control sequences are tens of bytes; the longest legitimate ones —
/// graphic-rendition chains naming several true colors — stay under two
/// hundred. A kilobyte is headroom past anything a terminal emits, while
/// keeping what an adversarial stream can force the stripper to hold, and
/// to re-emit on abandonment, small.
pub const MAX_CONTROL_SEQUENCE_BYTES: usize = 1024;

/// How much of an OSC, DCS, or other string sequence the stripper will
/// buffer before abandoning it, in bytes.
///
/// String sequences legitimately carry payloads — a clipboard write is a
/// document, a hyperlink target is a URL — so the control-sequence budget
/// would abandon well-formed traffic. 128 KiB clears the clipboard payloads
/// real multiplexers forward while still bounding what one unterminated
/// sequence can hold in memory.
pub const MAX_STRING_SEQUENCE_BYTES: usize = 128 * 1024;

/// One feed's worth of stripped output.
///
/// `text` upholds the module's standing postcondition: it contains no ESC
/// (U+001B) and no C1 control (U+0080–U+009F), whatever was fed. The other
/// C0 controls survive only as far as they are visible structure — [`Stripper`]
/// documents the exact set.
///
/// Derived `Debug` prints the text, deliberately: this is a payload type,
/// and formatting one is a decision its holder makes. The [`Stripper`]
/// itself is the type that guards its contents.
#[derive(Debug, PartialEq)]
pub struct StrippedChunk<'a> {
    /// The input with every control sequence removed. Borrowed whenever the
    /// input contained nothing to remove and no sequence was carried in —
    /// the common case on line-oriented output, and the reason feeding
    /// clean text costs a scan rather than a copy.
    pub text: Cow<'a, str>,
    /// What was removed, in stream order: a class and the byte range, in
    /// this feed's input, from the removal's introducer to its last
    /// character. A sequence entered or left open across feeds contributes
    /// the portion that lies in this one — an empty range at 0 when none of
    /// it does — and a C0 control a sequence let through to `text` sits
    /// inside its range. Classes are the durable coordinates; ranges locate
    /// removals within a chunk, no more.
    pub stripped: Vec<(SeqClass, Range<usize>)>,
}

/// A streaming escape-sequence stripper for the text path.
///
/// Pure computation, stateful only in parse position: a sequence split
/// across [`Stripper::feed`] calls — anywhere, including mid-parameter —
/// strips identically to the unsplit stream. One instance per session;
/// cloning one carries its parse position.
///
/// What survives to the output text: every character that is not a control,
/// plus the C0 controls that are visible structure — backspace, tab, line
/// feed, vertical tab, form feed, carriage return. Control sequences, lone
/// C1 controls, and the remaining C0 controls (a bell, a shift-in, a
/// stray NUL — things a terminal shows nothing for) are removed and
/// recorded in [`StrippedChunk::stripped`], classified.
///
/// Sequences are bounded: past [`MAX_CONTROL_SEQUENCE_BYTES`] (or
/// [`MAX_STRING_SEQUENCE_BYTES`] for the string kinds) the stripper stops
/// believing the input is a sequence and re-emits what it buffered as
/// visible text, tagged [`SeqClass::Abandoned`]. Degradation is content a
/// matcher can still see and route — never silence, and never a panic.
#[derive(Default)]
pub struct Stripper {
    /// The grammar: the same DEC/xterm parsing model, from the same crate,
    /// that the reconstructed screen's emulator runs. This module never
    /// decides where a sequence begins or ends — it asks.
    parser: Parser,
    /// The raw characters of the sequence currently open, introducer first,
    /// minus any C0 the sequence let through to text. What abandonment
    /// re-emits, what classification reads, and what a clone replays to
    /// reconstruct the parser's position.
    pending: String,
    /// A single shift (SS2/SS3) is waiting for the one character it shifts.
    /// The parser has no state for the shifted character — it treats the
    /// shift as complete — so the wait is this module's to carry.
    single_shift: bool,
    /// The open string sequence ends in an ESC that may be the first half
    /// of its terminator. Which it is depends on the next character, so the
    /// answer waits for one — across a feed boundary if it must.
    st_pending: bool,
}

/// The working state of one `feed` call: the output being built, and where
/// in this chunk the open sequence and its deferred ESC sit — positions
/// that mean nothing outside the call, which is why they live here and not
/// on the stripper.
struct Pass {
    text: String,
    stripped: Vec<(SeqClass, Range<usize>)>,
    /// Where the open sequence began in this feed's input; `None` while the
    /// open sequence began in an earlier feed.
    seq_start: Option<usize>,
    /// Where the deferred maybe-terminator ESC sits in this feed's input;
    /// `None` when the deferral crossed a feed boundary.
    esc_pos: Option<usize>,
}

impl Stripper {
    /// A stripper at the start of a stream.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one decoded chunk and returns it stripped.
    ///
    /// The input is already valid UTF-8 by construction — the reader
    /// decodes and substitutes before anything downstream sees text — so
    /// this layer never meets a broken byte, only characters.
    pub fn feed<'a>(&mut self, input: &'a str) -> StrippedChunk<'a> {
        debug_assert!(
            !self.pending.is_empty() || matches!(self.parser.state, State::Ground),
            "an empty pending buffer means the parser is at ground"
        );
        debug_assert!(
            !self.single_shift || !self.pending.is_empty(),
            "a waiting single shift keeps its sequence open"
        );
        if self.pending.is_empty() && !input.chars().any(sequence_bearing) {
            return StrippedChunk {
                text: Cow::Borrowed(input),
                stripped: Vec::new(),
            };
        }
        let mut pass = Pass {
            text: String::with_capacity(input.len()),
            stripped: Vec::new(),
            seq_start: None,
            esc_pos: None,
        };
        for (position, ch) in input.char_indices() {
            self.step(ch, position, position + ch.len_utf8(), &mut pass);
        }
        StrippedChunk {
            text: Cow::Owned(pass.text),
            stripped: pass.stripped,
        }
    }

    /// Ends the stream: a sequence still open degrades to visible text,
    /// tagged [`SeqClass::Abandoned`], exactly as one that outgrew its
    /// budget would — an unterminated sequence at end-of-stream is the same
    /// event with a different clock. The removal's range is empty, there
    /// being no input for it to point into. The stripper is ready for a
    /// fresh stream afterwards.
    pub fn finish(&mut self) -> StrippedChunk<'static> {
        if self.pending.is_empty() {
            return StrippedChunk {
                text: Cow::Owned(String::new()),
                stripped: Vec::new(),
            };
        }
        let text: String = self
            .pending
            .chars()
            .filter(|ch| !sequence_bearing(*ch))
            .collect();
        self.reset();
        StrippedChunk {
            text: Cow::Owned(text),
            stripped: vec![(SeqClass::Abandoned, 0..0)],
        }
    }

    /// Routes one character to the shift wait, the open sequence, or
    /// ground.
    fn step(&mut self, ch: char, position: usize, end: usize, pass: &mut Pass) {
        if self.single_shift {
            self.single_shift = false;
            if is_graphic(ch) {
                // The character was addressed to an alternate character
                // set; what it displays as is not what it says, so it
                // leaves with the shift that claimed it.
                self.pending.push(ch);
                self.close(end, pass);
                return;
            }
            // A control cannot be shifted: the shift closes empty and the
            // character is processed as itself.
            self.close(position, pass);
        }
        if self.pending.is_empty() {
            self.ground(ch, position, end, pass);
        } else {
            self.open_sequence(ch, position, end, pass);
        }
    }

    /// One character with no sequence open.
    fn ground(&mut self, ch: char, position: usize, end: usize, pass: &mut Pass) {
        match ch {
            // ESC, and the C1 single-byte spellings of CSI, OSC, DCS, SOS,
            // PM, and APC: a sequence opens.
            '\u{1b}' | '\u{9b}' | '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => {
                self.parser.feed(ch);
                self.pending.push(ch);
                pass.seq_start = Some(position);
            }
            // The C1 single shifts. The parser treats them as already
            // complete, so the character they shift is this module's wait.
            '\u{8e}' | '\u{8f}' => {
                self.pending.push(ch);
                pass.seq_start = Some(position);
                self.single_shift = true;
            }
            ch if sequence_bearing(ch) => {
                // A control with no sequence to belong to — a bell, a lone
                // C1, a shift-in. A terminal displays nothing for it, so
                // the text keeps nothing of it.
                pass.stripped.push((SeqClass::Other, position..end));
            }
            ch => pass.text.push(ch),
        }
    }

    /// One character while a sequence is open.
    fn open_sequence(&mut self, ch: char, position: usize, end: usize, pass: &mut Pass) {
        if self.st_pending {
            self.st_pending = false;
            if ch == '\\' {
                // ESC \ — the string terminator; the sequence is whole.
                // The backslash itself is never buffered: the sequence is
                // closing on this very statement and nothing reads it
                // afterwards, and skipping the push is what keeps the
                // budget a true bound — a sequence sitting exactly at its
                // limit would otherwise overshoot by its own terminator,
                // or worse, be abandoned for ending well-formed.
                self.parser.feed(ch);
                pass.esc_pos = None;
                self.close(end, pass);
                return;
            }
            // Anything else means the deferred ESC was a restart, not a
            // terminator: the string closes at that ESC — classified by
            // what it was — and the ESC stays open as the start of
            // whatever this character begins.
            let esc_position = pass.esc_pos.take();
            let start = pass.seq_start.take().unwrap_or(0);
            let split = self.pending.len() - 1;
            let class = classify(&self.pending[..split]);
            pass.stripped
                .push((class, start..esc_position.unwrap_or(0)));
            self.pending.drain(..split);
            pass.seq_start = esc_position;
        }
        let state = self.parser.state;
        if is_kept_control(ch) && executes_controls(state) {
            // A C0 the model executes mid-sequence is visible structure —
            // a newline inside a CSI still moved the cursor — so it passes
            // to text and the sequence stays open around it.
            pass.text.push(ch);
            return;
        }
        if ch == '\u{1b}' {
            if in_string(state) {
                // Inside a string sequence an ESC may be the first half of
                // the terminator; the next character decides.
                self.parser.feed(ch);
                if self.push_or_abandon(ch, end, pass) {
                    return;
                }
                self.st_pending = true;
                pass.esc_pos = Some(position);
            } else {
                // Anywhere else an ESC restarts: the open sequence is dead
                // where it stands, and it never completed, so its class is
                // whatever its fragment says.
                self.close(position, pass);
                self.parser.feed(ch);
                self.pending.push(ch);
                pass.seq_start = Some(position);
            }
            return;
        }
        if matches!(ch, '\u{8e}' | '\u{8f}') {
            // A C1 single shift cancels whatever is open and starts its
            // own wait.
            self.parser.feed(ch);
            self.close(position, pass);
            self.pending.push(ch);
            pass.seq_start = Some(position);
            self.single_shift = true;
            return;
        }
        if matches!(
            ch,
            '\u{9b}' | '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}'
        ) {
            // A C1 introducer restarts, the single-byte way.
            self.close(position, pass);
            self.parser.feed(ch);
            self.pending.push(ch);
            pass.seq_start = Some(position);
            return;
        }
        let state_before = state;
        self.parser.feed(ch);
        if self.push_or_abandon(ch, end, pass) {
            return;
        }
        if matches!(self.parser.state, State::Ground) {
            if state_before == State::Escape && matches!(ch, 'N' | 'O') {
                // ESC N / ESC O — a single shift. The parser is done with
                // it; this module still owes it the character it shifts.
                self.single_shift = true;
            } else {
                self.close(end, pass);
            }
        }
    }

    /// Appends `ch` to the open sequence, abandoning the sequence instead
    /// if that would grow it past its budget. Returns whether it abandoned.
    fn push_or_abandon(&mut self, ch: char, end: usize, pass: &mut Pass) -> bool {
        self.pending.push(ch);
        if self.pending.len() > self.budget() {
            self.abandon(end, pass);
            return true;
        }
        false
    }

    /// The bounded lookahead for the open sequence: string sequences carry
    /// payloads and get the larger budget; everything else gets the small
    /// one.
    fn budget(&self) -> usize {
        let mut chars = self.pending.chars();
        let string_sequence = match chars.next() {
            Some('\u{1b}') => matches!(chars.next(), Some(']' | 'P' | 'X' | '^' | '_')),
            Some('\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}') => true,
            _ => false,
        };
        if string_sequence {
            MAX_STRING_SEQUENCE_BYTES
        } else {
            MAX_CONTROL_SEQUENCE_BYTES
        }
    }

    /// The budget ran out: what was buffered is re-emitted as visible text
    /// — minus the characters the postcondition bans, which are exactly
    /// the ones a terminal would not have displayed either — and the
    /// stripper returns to ground.
    fn abandon(&mut self, end: usize, pass: &mut Pass) {
        pass.text
            .extend(self.pending.chars().filter(|ch| !sequence_bearing(*ch)));
        pass.stripped
            .push((SeqClass::Abandoned, pass.seq_start.take().unwrap_or(0)..end));
        self.reset();
    }

    /// Closes the open sequence: classified, recorded with the portion of
    /// it that lies in this chunk, and cleared.
    fn close(&mut self, end: usize, pass: &mut Pass) {
        let class = classify(&self.pending);
        let start = pass.seq_start.take().unwrap_or(0);
        pass.stripped.push((class, start..end));
        self.drain_pending();
    }

    /// Back to ground with nothing open — the state a fresh stripper is in.
    fn reset(&mut self) {
        self.drain_pending();
        self.parser = Parser::new();
        self.single_shift = false;
        self.st_pending = false;
    }

    /// Empties the sequence buffer without keeping a payload-sized
    /// allocation for the rest of the session: one budget-sized string
    /// sequence would otherwise pin its kilobytes until the stripper is
    /// dropped. Shrinking to the control budget is free for ordinary
    /// traffic — every sequence a terminal actually emits fits it many
    /// times over, so the buffer never reallocates on the hot path.
    fn drain_pending(&mut self) {
        self.pending.clear();
        self.pending.shrink_to(MAX_CONTROL_SEQUENCE_BYTES);
    }
}

impl Clone for Stripper {
    /// The parser underneath does not clone, but it never needs to: every
    /// character of the open sequence is in `pending`, and replaying them
    /// through a fresh parser reconstructs the parse position exactly.
    fn clone(&self) -> Self {
        let mut parser = Parser::new();
        for ch in self.pending.chars() {
            parser.feed(ch);
        }
        Self {
            parser,
            pending: self.pending.clone(),
            single_shift: self.single_shift,
            st_pending: self.st_pending,
        }
    }
}

impl std::fmt::Debug for Stripper {
    /// Parse position and how much is buffered, never the buffered
    /// characters: an open OSC 52 is holding clipboard content, and
    /// content does not go into a log line unless somebody asked for it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Stripper")
            .field("state", &self.parser.state)
            .field("pending_bytes", &self.pending.len())
            .field("single_shift", &self.single_shift)
            .field("st_pending", &self.st_pending)
            .finish()
    }
}

/// Whether the parser executes C0 controls in this state — the escape and
/// CSI family — rather than swallowing them into a string or ignoring them.
fn executes_controls(state: State) -> bool {
    matches!(
        state,
        State::Escape
            | State::EscapeIntermediate
            | State::CsiEntry
            | State::CsiParam
            | State::CsiIntermediate
            | State::CsiIgnore
    )
}

/// Whether this state is inside a string sequence, where an ESC may be the
/// first half of the ST terminator rather than a restart.
fn in_string(state: State) -> bool {
    matches!(
        state,
        State::OscString
            | State::DcsEntry
            | State::DcsParam
            | State::DcsIntermediate
            | State::DcsPassthrough
            | State::DcsIgnore
            | State::SosPmApcString
    )
}

/// Whether a character can open or belong to a control sequence — the
/// complement of what [`Stripper`] lets through to text.
fn sequence_bearing(ch: char) -> bool {
    match ch {
        // The C0 controls that are visible structure.
        '\u{08}' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' => false,
        // The rest of C0 (ESC among them), DEL, and all of C1.
        c if c < ' ' => true,
        '\u{7f}' => true,
        '\u{80}'..='\u{9f}' => true,
        _ => false,
    }
}

/// The C0 controls the text keeps: backspace, tab, line feed, vertical
/// tab, form feed, carriage return.
fn is_kept_control(ch: char) -> bool {
    matches!(ch, '\u{08}' | '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r')
}

/// Whether a character can be claimed by a single shift: anything the text
/// would otherwise keep, controls excepted.
fn is_graphic(ch: char) -> bool {
    !sequence_bearing(ch) && !is_kept_control(ch)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::{
        MAX_CONTROL_SEQUENCE_BYTES, MAX_STRING_SEQUENCE_BYTES, SeqClass, StrippedChunk, Stripper,
    };

    /// Text and classes for one whole input, fed in one piece.
    fn strip(input: &str) -> (String, Vec<SeqClass>) {
        let mut stripper = Stripper::new();
        let chunk = stripper.feed(input);
        let mut text = chunk.text.into_owned();
        let mut classes: Vec<SeqClass> = chunk.stripped.iter().map(|(class, _)| *class).collect();
        let tail = stripper.finish();
        text.push_str(&tail.text);
        classes.extend(tail.stripped.iter().map(|(class, _)| *class));
        (text, classes)
    }

    #[test]
    fn plain_text_passes_through_borrowed() {
        let mut stripper = Stripper::new();
        let chunk = stripper.feed("plain words, tabs\tand\r\nline breaks");
        assert!(
            matches!(chunk.text, Cow::Borrowed(_)),
            "clean input must not be copied"
        );
        assert_eq!(chunk.text, "plain words, tabs\tand\r\nline breaks");
        assert_eq!(chunk.stripped, Vec::new());
    }

    #[test]
    fn csi_sequences_strip_and_the_words_remain() {
        let (text, classes) =
            strip("\u{1b}[2J\u{1b}[1;1HDo\u{1b}[1;4Hyou\u{1b}[0;32mwant\u{1b}[0m?");
        assert_eq!(text, "Doyouwant?");
        assert_eq!(
            classes,
            vec![
                SeqClass::EraseClear,
                SeqClass::CursorMovement,
                SeqClass::CursorMovement,
                SeqClass::Sgr,
                SeqClass::Sgr,
            ]
        );
    }

    #[test]
    fn osc_terminates_on_st_and_on_bel_alike() {
        for input in [
            "before\u{1b}]0;a title\u{7}after",
            "before\u{1b}]0;a title\u{1b}\\after",
            "before\u{9d}0;a title\u{9c}after",
        ] {
            let (text, classes) = strip(input);
            assert_eq!(text, "beforeafter", "{input:?}");
            assert_eq!(classes, vec![SeqClass::OscOther], "{input:?}");
        }
    }

    #[test]
    fn dcs_payload_never_reaches_the_text() {
        let (text, classes) = strip("a\u{1b}P1;2|secret payload\u{1b}\\b");
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::DcsPassthrough]);
    }

    #[test]
    fn sos_pm_apc_strings_are_consumed_whole() {
        for intro in ['X', '^', '_'] {
            let (text, classes) = strip(&format!("a\u{1b}{intro}hidden words\u{1b}\\b"));
            assert_eq!(text, "ab", "{intro:?}");
            assert_eq!(classes, vec![SeqClass::Other], "{intro:?}");
        }
    }

    #[test]
    fn the_c1_introducers_behave_like_their_escape_spellings() {
        let (text, classes) = strip("a\u{9b}5Ab\u{9d}0;t\u{9c}c\u{90}data\u{9c}d");
        assert_eq!(text, "abcd");
        assert_eq!(
            classes,
            vec![
                SeqClass::CursorMovement,
                SeqClass::OscOther,
                SeqClass::DcsPassthrough,
            ]
        );
    }

    #[test]
    fn lone_controls_strip_as_themselves() {
        // A bell, a C1 with no sequence role, a DEL, a shift-in: invisible
        // on a terminal, so stripped — while the structural C0 survive.
        let (text, classes) = strip("a\u{7}b\u{85}c\u{7f}d\u{0e}e\tf\ng");
        assert_eq!(text, "abcde\tf\ng");
        assert_eq!(classes, vec![SeqClass::Other; 4]);
    }

    #[test]
    fn single_shifts_consume_exactly_one_graphic_character() {
        for input in ["a\u{1b}NZb", "a\u{1b}OZb", "a\u{8e}Zb", "a\u{8f}Zb"] {
            let (text, classes) = strip(input);
            assert_eq!(text, "ab", "{input:?}: the shifted character leaves too");
            assert_eq!(classes, vec![SeqClass::Other], "{input:?}");
        }
    }

    #[test]
    fn a_control_after_a_single_shift_is_not_consumed_by_it() {
        // The shift closes empty and the control is processed as itself: a
        // newline stays, an ESC opens the sequence it introduces.
        let (text, classes) = strip("a\u{1b}N\nb");
        assert_eq!(text, "a\nb");
        assert_eq!(classes, vec![SeqClass::Other]);

        let (text, classes) = strip("a\u{8e}\u{1b}[2Jb");
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::Other, SeqClass::EraseClear]);
    }

    #[test]
    fn can_and_sub_cancel_the_sequence_they_land_in() {
        let (text, classes) = strip("a\u{1b}[12;3\u{18}b");
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::Other]);

        // A cancelled clipboard write is still named as one — the consumer
        // deciding whether to worry wants the class, not a shrug.
        let (text, classes) = strip("a\u{1b}]52;c;aGtl\u{1a}b");
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::OscClipboard]);
    }

    #[test]
    fn a_c1_control_mid_sequence_cancels_it_too() {
        let (text, classes) = strip("a\u{1b}[12\u{85}b");
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::Other]);
    }

    #[test]
    fn a_c1_introducer_mid_sequence_restarts_cleanly() {
        // The interrupted OSC keeps its name; the CSI that interrupted it
        // gets its own.
        let (text, classes) = strip("a\u{1b}]52;c;abc\u{9b}?1000hb");
        assert_eq!(text, "ab");
        assert_eq!(
            classes,
            vec![SeqClass::OscClipboard, SeqClass::MouseTracking]
        );
    }

    #[test]
    fn mid_sequence_structural_controls_stay_visible() {
        // A newline inside a CSI still moved the cursor; the sequence
        // strips and the newline stays where it fell.
        let (text, classes) = strip("a\u{1b}[1\n2mb");
        assert_eq!(text, "a\nb");
        assert_eq!(classes, vec![SeqClass::Sgr]);
    }

    #[test]
    fn a_control_swallowed_by_a_string_sequence_stays_swallowed() {
        // Inside an OSC the model ignores C0 controls rather than
        // executing them, so a newline there is part of the removal — the
        // screen never showed it and the text must not either.
        let (text, classes) = strip("a\u{1b}]0;ti\ntle\u{7}b");
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::OscOther]);
    }

    #[test]
    fn an_esc_restart_closes_the_fragment_and_opens_the_sequence() {
        let (text, classes) = strip("a\u{1b}[12\u{1b}[0mb");
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::Other, SeqClass::Sgr]);
    }

    #[test]
    fn an_aborted_string_sequence_still_names_its_class() {
        // The ESC turned out to start a CSI, not the terminator: the
        // clipboard write closes as what it was, and the mouse-tracking
        // set that aborted it is detected rather than folded in.
        let (text, classes) = strip("a\u{1b}]52;c;abc\u{1b}[?1002hb");
        assert_eq!(text, "ab");
        assert_eq!(
            classes,
            vec![SeqClass::OscClipboard, SeqClass::MouseTracking]
        );
    }

    #[test]
    fn an_embedded_control_does_not_hide_the_mouse_from_the_class() {
        // Through the whole feed path, not just the classifier: the NUL
        // rides inside the removed sequence — the model executes it without
        // ending the parameter's digit run, so the terminal enables mode
        // 1000 — and the removal must still carry the unsafe name.
        let (text, classes) = strip("a\u{1b}[?10\u{0}00hb");
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::MouseTracking]);
    }

    #[test]
    fn a_high_character_inside_a_sequence_ends_it_the_way_the_emulator_reads_it() {
        // The parser underneath maps every character above U+00A0 to a
        // dispatch while a control sequence is open, so `é` mid-CSI ends
        // the sequence and is consumed with it, classified as nothing
        // nameable. Pinned because the screen path reads the same bytes
        // through the same table — that agreement is this module's reason
        // for riding the emulator's parser — and a parser swap or upgrade
        // that changed the reading must surface here, not as a silent
        // divergence between the stripped text and the reconstructed
        // screen.
        let (text, classes) = strip("a\u{1b}[12\u{e9}b");
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::Other]);
    }

    #[test]
    fn an_esc_restart_chain_never_loses_ground() {
        let (text, classes) = strip("a\u{1b}\u{1b}\u{1b}[1mb");
        assert_eq!(text, "ab");
        assert_eq!(
            classes,
            vec![SeqClass::Other, SeqClass::Other, SeqClass::Sgr]
        );
    }

    #[test]
    fn the_st_decision_survives_a_feed_boundary() {
        // The deferred ESC resolves as a terminator in the next feed…
        let mut stripper = Stripper::new();
        let first = stripper.feed("a\u{1b}]0;title\u{1b}");
        let second = stripper.feed("\\b");
        assert_eq!(first.text, "a");
        assert_eq!(second.text, "b");
        assert_eq!(second.stripped.len(), 1);
        assert_eq!(second.stripped[0].0, SeqClass::OscOther);

        // …and as a restart in the next feed.
        let mut stripper = Stripper::new();
        stripper.feed("a\u{1b}]0;title\u{1b}");
        let second = stripper.feed("[2Jb");
        assert_eq!(second.text, "b");
        let classes: Vec<SeqClass> = second.stripped.iter().map(|(class, _)| *class).collect();
        assert_eq!(classes, vec![SeqClass::OscOther, SeqClass::EraseClear]);
    }

    #[test]
    fn a_sequence_split_at_every_boundary_strips_identically() {
        // Exhaustive, not sampled: every split point of every input, with
        // the whole-stream result as the reference. The randomized sweep
        // in the integration suite covers longer inputs; this covers every
        // state the small ones can reach.
        for input in [
            "a\u{1b}[1;32mgreen\u{1b}[0m plain",
            "x\u{1b}]52;c;aGVsbG8=\u{7}y",
            "x\u{1b}]8;;https://e.com\u{1b}\\link\u{1b}]8;;\u{1b}\\y",
            "\u{1b}P0;1|payload\u{1b}\\text",
            "a\u{9b}2Jb\u{8e}Qc",
            "a\u{1b}N\u{e9}b",
            "\u{1b}[?1002h\u{1b}[?1002l",
            "a\u{1b}[12\u{18}b\u{1b}]0;t\u{1a}c",
            "tail ends open\u{1b}[12;",
            "shift waits\u{1b}N",
            "st waits\u{1b}]0;t\u{1b}",
        ] {
            let reference = strip(input);
            for split in 1..input.len() {
                if !input.is_char_boundary(split) {
                    continue;
                }
                let (head, tail) = input.split_at(split);
                let mut stripper = Stripper::new();
                let first = stripper.feed(head);
                let second = stripper.feed(tail);
                let last = stripper.finish();
                let text = format!("{}{}{}", first.text, second.text, last.text);
                let classes: Vec<SeqClass> = first
                    .stripped
                    .iter()
                    .chain(second.stripped.iter())
                    .chain(last.stripped.iter())
                    .map(|(class, _)| *class)
                    .collect();
                assert_eq!(
                    (text, classes),
                    reference.clone(),
                    "{input:?} split at {split}"
                );
            }
        }
    }

    #[test]
    fn a_control_sequence_past_its_budget_degrades_to_visible_text() {
        let flood = "9;".repeat(MAX_CONTROL_SEQUENCE_BYTES);
        let input = format!("a\u{1b}[{flood}mb");
        let (text, classes) = strip(&input);
        let head: String = text.chars().take(12).collect();
        assert!(
            text.starts_with("a[9;9;"),
            "the buffered characters must become visible content: {head:?}…"
        );
        assert!(text.ends_with('b'));
        assert!(
            !text.contains('\u{1b}'),
            "the introducer itself stays banned"
        );
        assert_eq!(classes[0], SeqClass::Abandoned);
    }

    #[test]
    fn a_string_sequence_gets_the_larger_budget_and_then_the_same_degradation() {
        // Longer than the control budget: survives, because payloads are
        // what string sequences are for.
        let survivable = format!(
            "a\u{1b}]52;c;{}\u{7}b",
            "A".repeat(4 * MAX_CONTROL_SEQUENCE_BYTES)
        );
        let (text, classes) = strip(&survivable);
        assert_eq!(text, "ab");
        assert_eq!(classes, vec![SeqClass::OscClipboard]);

        // Longer than the string budget: degrades visibly, like anything
        // the stripper stops believing in.
        let flood = "A".repeat(MAX_STRING_SEQUENCE_BYTES + 16);
        let (text, classes) = strip(&format!("a\u{1b}]52;c;{flood}\u{7}b"));
        assert!(text.starts_with("a]52;c;AAAA"));
        assert_eq!(classes[0], SeqClass::Abandoned);
        // Whatever the sequence held past the abandonment point flows on
        // as ordinary text — including its never-used terminator, which is
        // a lone bell by the time it arrives.
        assert!(text.ends_with('b'));
        assert_eq!(classes[1], SeqClass::Other);
    }

    #[test]
    fn a_string_sequence_at_its_budget_still_closes_on_its_terminator() {
        // The deferred ESC lands the buffer exactly on the limit, and the
        // terminator's backslash is never buffered at all — so a stream one
        // byte from the cap still strips cleanly instead of degrading on
        // its own well-formed ending, and the budget stays a true bound.
        let payload = "A".repeat(MAX_STRING_SEQUENCE_BYTES - 3);
        let (text, classes) = strip(&format!("x\u{1b}]{payload}\u{1b}\\y"));
        assert_eq!(text, "xy");
        assert_eq!(classes, vec![SeqClass::OscOther]);
    }

    #[test]
    fn an_esc_arriving_exactly_over_budget_abandons_with_the_rest() {
        // The one push that can overflow a string sequence from inside the
        // terminator question: the sequence sits exactly at its budget and
        // the maybe-ST ESC itself is the byte that crosses it.
        let payload = "A".repeat(MAX_STRING_SEQUENCE_BYTES - 2);
        let (text, classes) = strip(&format!("x\u{1b}]{payload}\u{1b}\\y"));
        assert_eq!(classes, vec![SeqClass::Abandoned]);
        assert!(text.starts_with("x]AAA"));
        // The ESC went down with the sequence that buffered it, so the
        // terminator's second half arrives at ground as what it now is:
        // a printable backslash.
        assert!(text.ends_with("\\y"));
    }

    #[test]
    fn finish_flushes_an_unterminated_sequence_as_text() {
        let mut stripper = Stripper::new();
        let chunk = stripper.feed("done\u{1b}]0;half a title");
        assert_eq!(chunk.text, "done");
        let tail = stripper.finish();
        assert_eq!(tail.text, "]0;half a title");
        assert_eq!(tail.stripped, vec![(SeqClass::Abandoned, 0..0)]);
        // And the stripper is ready for a fresh stream.
        assert_eq!(stripper.feed("clean").text, "clean");
    }

    #[test]
    fn finish_on_a_clean_stream_carries_nothing() {
        let mut stripper = Stripper::new();
        stripper.feed("all text\u{1b}[0m all closed");
        let tail = stripper.finish();
        assert_eq!(tail.text, "");
        assert_eq!(tail.stripped, Vec::new());
    }

    #[test]
    fn spans_locate_removals_within_the_chunk() {
        let mut stripper = Stripper::new();
        let chunk = stripper.feed("ab\u{1b}[2Jcd\u{7}e");
        assert_eq!(chunk.text, "abcde");
        assert_eq!(
            chunk.stripped,
            vec![(SeqClass::EraseClear, 2..6), (SeqClass::Other, 8..9)]
        );

        // A sequence carried in from an earlier feed contributes the
        // portion in this one: here, only its terminator.
        let mut stripper = Stripper::new();
        stripper.feed("x\u{1b}[2");
        let chunk = stripper.feed("Jy");
        assert_eq!(chunk.stripped, vec![(SeqClass::EraseClear, 0..1)]);
    }

    #[test]
    fn clone_carries_the_parse_position() {
        let mut original = Stripper::new();
        original.feed("a\u{1b}]52;c;aG");
        let mut cloned = original.clone();
        let rest = "Vsbg==\u{7}b";
        let from_original = original.feed(rest);
        let from_clone = cloned.feed(rest);
        assert_eq!(from_original, from_clone);
    }

    #[test]
    fn debug_names_no_stream_content() {
        let mut stripper = Stripper::new();
        stripper.feed("\u{1b}]52;c;hunter2");
        let printed = format!("{stripper:?}");
        assert!(
            !printed.contains("hunter2"),
            "a stripper holds stream content and must not print it: {printed}"
        );
        assert!(printed.contains("OscString"), "{printed}");
    }

    #[test]
    fn the_output_never_carries_esc_or_c1_whatever_arrives() {
        // The unit-sized sweep; the adversarial suite does this at scale.
        for input in [
            "\u{1b}",
            "\u{1b}\u{1b}\u{1b}",
            "\u{9b}\u{9d}\u{90}\u{98}\u{9c}\u{8e}\u{8f}\u{85}",
            "\u{1b}[\u{1b}]\u{1b}P\u{1b}X\u{1b}N\u{1b}O",
            "text\u{1b}[999999999999999999999999m more",
            "\u{1b}]52;\u{1b}]52;\u{7}\u{7}",
        ] {
            let (text, _) = strip(input);
            assert!(
                !text
                    .chars()
                    .any(|c| c == '\u{1b}' || ('\u{80}'..='\u{9f}').contains(&c)),
                "{input:?} leaked into {text:?}"
            );
        }
    }

    #[test]
    fn a_fresh_stripper_reports_itself_at_ground() {
        let stripper = Stripper::new();
        let printed = format!("{stripper:?}");
        assert!(printed.contains("Ground"), "{printed}");
        assert!(printed.contains("pending_bytes: 0"), "{printed}");
    }

    #[test]
    fn stripped_chunks_compare_by_value() {
        // `StrippedChunk` derives its equality; hold it to meaning what a
        // test needs it to mean — text and removals, not borrow status.
        let owned = StrippedChunk {
            text: Cow::Owned("a".to_string()),
            stripped: Vec::new(),
        };
        let borrowed = StrippedChunk {
            text: Cow::Borrowed("a"),
            stripped: Vec::new(),
        };
        assert_eq!(owned, borrowed);
    }
}
