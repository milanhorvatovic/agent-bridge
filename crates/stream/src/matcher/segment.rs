//! Line segmentation: the stripped feed becomes the completed lines the
//! matchers read.
//!
//! Nothing upstream deals in lines. The reader hands text on whenever it
//! decodes, the stripper is chunk-in chunk-out, and chunk boundaries carry
//! no meaning — so the line structure the matcher contract is written
//! against is assembled here, from scratch, with three rules:
//!
//! - A line feed completes a line; a carriage return immediately before it
//!   is the Windows spelling of the same thing and is dropped.
//! - A bare carriage return *restarts* the line. That is what it does on a
//!   terminal — the cursor returns to column zero and what follows
//!   overwrites — and it is how progress bars repaint in place. Keeping
//!   the last writing is what makes the eventual completed line the one a
//!   human saw. The overwrite is whole-line, not per column: tracking
//!   partial overwrites is the reconstructed screen's job, and a matcher
//!   that needs that fidelity is a screen matcher.
//! - A line longer than the cap is withheld from matching entirely — not
//!   truncated into something matchable. Any cut produces an artificial
//!   end boundary, and an end-anchored pattern with an unbounded middle
//!   (`Allow .+\?` is the canonical approval shape) will happily span
//!   sixteen kibibytes to reach a suffix an adversary parked exactly at
//!   the cap. A line this long is not a prompt; nothing sound can be
//!   asked of a piece of one.
//!
//! What has not completed stays pending, readable at any time: a real
//! prompt typically *never* ends its line — it is waiting for input — so
//! the pending tail at a quiet moment is exactly the text the degradation
//! path evaluates.

/// The most of one line a matcher will ever see. Generous next to any
/// prompt and tiny next to the buffer a pathological no-newline stream
/// would otherwise grow.
///
/// Past it, the line is out of the conversation: the pending view is
/// withheld and the eventual completed line is discarded, counted, not
/// evaluated. Both cuts would otherwise present an artificial end
/// boundary, and anchoring does not save the completed side — a
/// start-anchored approval pattern with an unbounded middle matches a
/// crafted head that opens like a prompt and parks the choice suffix at
/// the cap. Discarding is the only reading that cannot be steered.
pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// Assembles completed lines from stripped-text chunks.
#[derive(Default)]
pub struct LineAssembler {
    buffer: String,
    /// A carriage return was seen and not yet resolved into CRLF (line
    /// end) or overwrite (line restart).
    pending_cr: bool,
    /// The line hit its cap: everything until the next terminator or
    /// restart is shed, not just the character that overflowed — keeping
    /// later, smaller characters would splice text that was never
    /// adjacent, and a matcher must never fire on a line nobody saw.
    overflowed: bool,
    /// Lines discarded for outgrowing the cap — the visible ledger for
    /// input that never reached a matcher.
    discarded: u64,
}

impl LineAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one stripped chunk; returns the lines it completed, in
    /// order, without their terminators.
    pub fn push(&mut self, text: &str) -> Vec<String> {
        let mut completed = Vec::new();
        for ch in text.chars() {
            match ch {
                '\n' => {
                    self.pending_cr = false;
                    if self.overflowed {
                        // The whole line goes, head included: any cut
                        // edge is a boundary an end-anchored pattern
                        // could be steered onto.
                        self.overflowed = false;
                        self.discarded += 1;
                        tracing::debug!(
                            retained_bytes = self.buffer.len(),
                            "overlong line discarded unevaluated"
                        );
                        self.buffer.clear();
                    } else {
                        completed.push(std::mem::take(&mut self.buffer));
                    }
                }
                '\r' => self.pending_cr = true,
                _ => {
                    if self.pending_cr {
                        self.pending_cr = false;
                        self.overflowed = false;
                        self.buffer.clear();
                    }
                    if self.overflowed {
                        continue;
                    }
                    if self.buffer.len() + ch.len_utf8() <= MAX_LINE_BYTES {
                        self.buffer.push(ch);
                    } else {
                        self.overflowed = true;
                    }
                }
            }
        }
        completed
    }

    /// The line under construction — the unterminated tail a prompt
    /// usually is.
    ///
    /// Empty while the line is overflowed: a shed tail has lost its true
    /// end, so evaluating the retained head as "what the session is
    /// waiting on" would hand end-anchored patterns a boundary the stream
    /// never produced. No sound question can be asked of it until the
    /// line restarts or terminates — and a line this long is not a
    /// prompt.
    pub fn pending(&self) -> &str {
        if self.overflowed { "" } else { &self.buffer }
    }

    /// Lines discarded for outgrowing the cap. Diagnostics, not events: a
    /// sixteen-kibibyte line is not a prompt, and repeating one back as
    /// unrecognized output would hand an adversary an amplifier.
    pub fn discarded_lines(&self) -> u64 {
        self.discarded
    }
}

impl std::fmt::Debug for LineAssembler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LineAssembler({} pending bytes)", self.buffer.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_complete_on_lf_and_crlf_alike() {
        let mut assembler = LineAssembler::new();
        assert_eq!(assembler.push("one\ntwo\r\nthr"), vec!["one", "two"]);
        assert_eq!(assembler.pending(), "thr");
        assert_eq!(assembler.push("ee\n"), vec!["three"]);
        assert_eq!(assembler.pending(), "");
    }

    #[test]
    fn chunk_boundaries_carry_no_meaning() {
        let mut assembler = LineAssembler::new();
        let mut lines = Vec::new();
        // One logical stream, cut at awkward places — including between
        // the CR and LF of one terminator.
        for chunk in ["Allow filesystem", " write? [y/N]\r", "\nnext"] {
            lines.extend(assembler.push(chunk));
        }
        assert_eq!(lines, vec!["Allow filesystem write? [y/N]"]);
        assert_eq!(assembler.pending(), "next");
    }

    #[test]
    fn a_bare_carriage_return_restarts_the_line() {
        let mut assembler = LineAssembler::new();
        assert_eq!(
            assembler.push("10%\r55%\r100%\rdone\n"),
            vec!["done"],
            "last writing wins, as it would on the terminal"
        );
        // A restart that has not completed yet is the pending text.
        assembler.push("waiting\rready> ");
        assert_eq!(assembler.pending(), "ready> ");
    }

    #[test]
    fn an_overlong_line_is_withheld_and_then_discarded() {
        let mut assembler = LineAssembler::new();
        let long = "x".repeat(MAX_LINE_BYTES + 100);
        assert!(assembler.push(&long).is_empty());
        // While overflowed, the tail is withheld: its true end is gone,
        // so no evaluation may treat the head as what the session waits on.
        assert_eq!(assembler.pending(), "");
        // Completion discards rather than emitting a matchable piece.
        assert!(assembler.push("\n").is_empty());
        assert_eq!(assembler.discarded_lines(), 1);
        assert_eq!(assembler.pending(), "", "and the next line starts clean");
        assert_eq!(assembler.push("short\n"), vec!["short"]);
    }

    /// The steering attack the discard exists for: a crafted line that
    /// opens like a prompt and parks the choice suffix exactly at the
    /// cap must not surface as a completed line at all — a start-anchored
    /// approval pattern with an unbounded middle would match the cut.
    #[test]
    fn a_crafted_overlong_prompt_never_reaches_matching() {
        let mut assembler = LineAssembler::new();
        let mut crafted = String::from("Allow filesystem write");
        let suffix = "? [y/N]";
        crafted.push_str(&"x".repeat(MAX_LINE_BYTES - crafted.len() - suffix.len()));
        crafted.push_str(suffix);
        assert_eq!(crafted.len(), MAX_LINE_BYTES);
        crafted.push_str("and the bytes the cap would have cut away");
        crafted.push('\n');
        assert!(
            assembler.push(&crafted).is_empty(),
            "discarded, not truncated"
        );
        assert_eq!(assembler.discarded_lines(), 1);
    }

    /// The retained head must be a *prefix* of the real line: once one
    /// character is shed, everything after it is shed too, or characters
    /// that were never adjacent would sit next to each other and a matcher
    /// could fire on text nobody saw.
    #[test]
    fn overflow_sheds_the_whole_tail_never_splicing() {
        let mut assembler = LineAssembler::new();
        let head = "y".repeat(MAX_LINE_BYTES - 1);
        assembler.push(&head);
        // The 2-byte scalar does not fit; the 1-byte one after it would —
        // and must not be taken.
        assembler.push("éz");
        assert_eq!(
            assembler.pending(),
            "",
            "an overflowed tail has lost its end; nothing sound can be asked of it"
        );

        // A restart ends the shedding; the overwriting line is ordinary.
        assert_eq!(assembler.push("\rfresh\n"), vec!["fresh"]);
        assert_eq!(
            assembler.discarded_lines(),
            0,
            "a restarted line was never completed"
        );
        assembler.push(&head);
        assembler.push("é");
        assert!(
            assembler.push("\nnext").is_empty(),
            "overflow discards on completion"
        );
        assert_eq!(assembler.discarded_lines(), 1);
        assert_eq!(assembler.pending(), "next");
    }
}
