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
//! - A line longer than the cap keeps its head and sheds the rest. The
//!   head is the honest part to keep — a line-start anchor still means
//!   what it says — and an unbounded buffer on input an adversary shapes
//!   is not an option. Prompts, the lines this engine exists to catch, are
//!   short.
//!
//! What has not completed stays pending, readable at any time: a real
//! prompt typically *never* ends its line — it is waiting for input — so
//! the pending tail at a quiet moment is exactly the text the degradation
//! path evaluates.

/// The most of one line a matcher will ever see. Generous next to any
/// prompt and tiny next to the buffer a pathological no-newline stream
/// would otherwise grow.
pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// Assembles completed lines from stripped-text chunks.
#[derive(Default)]
pub struct LineAssembler {
    buffer: String,
    /// A carriage return was seen and not yet resolved into CRLF (line
    /// end) or overwrite (line restart).
    pending_cr: bool,
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
                    completed.push(std::mem::take(&mut self.buffer));
                }
                '\r' => self.pending_cr = true,
                _ => {
                    if self.pending_cr {
                        self.pending_cr = false;
                        self.buffer.clear();
                    }
                    if self.buffer.len() + ch.len_utf8() <= MAX_LINE_BYTES {
                        self.buffer.push(ch);
                    }
                }
            }
        }
        completed
    }

    /// The line under construction — the unterminated tail a prompt
    /// usually is.
    pub fn pending(&self) -> &str {
        &self.buffer
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
    fn an_overlong_line_keeps_its_head() {
        let mut assembler = LineAssembler::new();
        let long = "x".repeat(MAX_LINE_BYTES + 100);
        assert!(assembler.push(&long).is_empty());
        assert_eq!(assembler.pending().len(), MAX_LINE_BYTES);
        // The cap respects character boundaries: a multi-byte scalar that
        // would straddle it is dropped whole.
        let mut nearly_full = LineAssembler::new();
        nearly_full.push(&"y".repeat(MAX_LINE_BYTES - 1));
        nearly_full.push("é");
        assert_eq!(nearly_full.pending().len(), MAX_LINE_BYTES - 1);
        // Completion still fires, with the retained head.
        let completed = assembler.push("\n");
        assert_eq!(completed[0].len(), MAX_LINE_BYTES);
    }
}
