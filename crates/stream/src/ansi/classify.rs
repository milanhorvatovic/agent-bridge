//! What kind of sequence was removed, said in one word.
//!
//! Every stripped span carries a class, and the classes exist for the
//! consumers downstream of stripping rather than for stripping itself: a
//! heuristic weighing whether output is a repaint wants to know how much of
//! it was cursor traffic, a log mirror re-emitting content elsewhere must
//! know which sequences are hostile outside a live terminal, and a decision
//! about unrecognized output reads differently when what was removed was one
//! reset against a wall of erase-and-redraw. One classifier serves them all,
//! so the next consumer extends this table instead of growing a second one
//! that quietly disagrees with it.
//!
//! Classification reads the removed characters, not the parser: the parser
//! decides where a sequence begins and ends, and this module answers only
//! "which one was it" from the raw text of the removal. That split is what
//! lets a sequence the stream cancelled halfway still be classified by what
//! it was trying to be — a clipboard write aborted mid-payload was still a
//! clipboard write, and the consumer deciding whether to worry wants it
//! named as one.

/// The class of one removed control sequence.
///
/// The three classes [`SeqClass::is_unsafe`] names are the reason this enum
/// is more than a debugging aid; the rest exist because a consumer weighing
/// stripped output wants "cursor churn" and "styling" distinguished from
/// "something unrecognized".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeqClass {
    /// Cursor positioning, scrolling, and cursor save/restore — the traffic
    /// a redrawing interface emits constantly and a line-oriented one almost
    /// never.
    CursorMovement,
    /// Character styling (`CSI … m`): colors, intensity, underline, and the
    /// rest of the graphic-rendition family.
    Sgr,
    /// Erasing and editing: clearing regions of the display or line, and
    /// inserting or deleting characters and lines in place. A full terminal
    /// reset is classified here too — it is the largest erase there is.
    EraseClear,
    /// `OSC 8` — a hyperlink wrapped around visible text. Unsafe: the target
    /// need not resemble what the text shows, which makes it a spoofing
    /// vector anywhere the sequence is re-emitted for a person to click.
    OscHyperlink,
    /// `OSC 52` — a write to (or query of) the system clipboard. Unsafe: a
    /// stream that can place chosen bytes on the clipboard of whoever views
    /// it is one paste away from running them.
    OscClipboard,
    /// The xterm mouse-reporting modes, set or reset via DEC private mode
    /// sequences. Unsafe: enabled in a viewer's terminal, they turn that
    /// terminal into an input channel reporting where the viewer clicks.
    MouseTracking,
    /// Any other operating-system command — window titles, color palette
    /// programming, working-directory reports, and every selector this table
    /// does not name.
    OscOther,
    /// A device-control string. The payload is consumed with the sequence
    /// and never surfaced: nothing downstream of a stripper has a use for
    /// terminal-to-terminal protocol data.
    DcsPassthrough,
    /// A sequence that outgrew the stripper's bounded lookahead, or was
    /// still open when the stream ended. Its characters were re-emitted as
    /// visible text rather than dropped — degradation is content a matcher
    /// can still route, never silence.
    Abandoned,
    /// Everything else: single-shifts, charset designations, lone C1
    /// controls, stripped C0 controls, cancelled fragments, and escape
    /// sequences this table has no name for.
    Other,
}

impl SeqClass {
    /// Whether this class is hostile anywhere the content might be shown in
    /// a terminal again.
    ///
    /// Stripping already neutralizes every class for the text path; this
    /// flag exists for consumers that re-emit content elsewhere — a log
    /// mirror, a transcript — so the classes that can act on a *viewer's*
    /// terminal (write their clipboard, spoof a link target, capture their
    /// mouse) are one call away instead of a second parse.
    pub fn is_unsafe(self) -> bool {
        matches!(
            self,
            Self::OscHyperlink | Self::OscClipboard | Self::MouseTracking
        )
    }
}

/// Names the sequence held in `raw` — the removed characters exactly as they
/// appeared, introducer first.
pub(crate) fn classify(raw: &str) -> SeqClass {
    let mut chars = raw.chars();
    match chars.next() {
        Some('\u{1b}') => {
            let designator = chars.next();
            let body = chars.as_str();
            match designator {
                Some('[') => classify_csi(body),
                Some(']') => classify_osc(body),
                Some('P') => SeqClass::DcsPassthrough,
                // Cursor save and restore, index, next-line, and reverse
                // index — the two-character spellings of cursor movement.
                Some('7' | '8' | 'D' | 'E' | 'M') => SeqClass::CursorMovement,
                // RIS resets the terminal outright, which erases everything.
                Some('c') => SeqClass::EraseClear,
                _ => SeqClass::Other,
            }
        }
        // The C1 single-byte spellings of the same introducers.
        Some('\u{9b}') => classify_csi(chars.as_str()),
        Some('\u{9d}') => classify_osc(chars.as_str()),
        Some('\u{90}') => SeqClass::DcsPassthrough,
        _ => SeqClass::Other,
    }
}

/// The DEC private modes that enable or disable mouse reporting: X10 (9),
/// the button/drag/motion family with its focus and UTF-8 extensions
/// (1000–1006), and the two extended-coordinate encodings (1015 urxvt,
/// 1016 SGR-pixels). One mode more than the threat table's shorthand names:
/// mode 9 predates the family but terminals still honor it, and a class
/// that says "mouse tracking" must not wave the oldest spelling through.
fn is_mouse_mode(parameter: &str) -> bool {
    matches!(parameter.parse::<u32>(), Ok(9 | 1000..=1006 | 1015 | 1016))
}

/// Classifies a control-sequence body: parameters and intermediates, then a
/// final character — absent when the sequence was cancelled before one.
fn classify_csi(body: &str) -> SeqClass {
    let Some(final_char) = body.chars().last().filter(|c| ('@'..='~').contains(c)) else {
        // No final byte means the sequence never completed; there is nothing
        // to name it by.
        return SeqClass::Other;
    };
    if body.starts_with('?') {
        // DEC private modes. Only the mouse family gets a name; the rest —
        // cursor visibility, the alternate screen, autowrap — are stripped
        // like anything else and classified as unremarkable.
        return match final_char {
            'h' | 'l' if body[1..body.len() - 1].split([';', ':']).any(is_mouse_mode) => {
                SeqClass::MouseTracking
            }
            _ => SeqClass::Other,
        };
    }
    if body.starts_with(['<', '=', '>']) {
        // The other private-parameter markers introduce vendor sequences
        // this table has no names for.
        return SeqClass::Other;
    }
    match final_char {
        'm' => SeqClass::Sgr,
        // Absolute and relative cursor motion, tabulation, scrolling, the
        // scroll-region setter, and the SCO save/restore pair.
        'A'..='I' | 'Z' | '`' | 'a' | 'd' | 'e' | 'f' | 'r' | 's' | 'u' | 'S' | 'T' => {
            SeqClass::CursorMovement
        }
        // Erase display/line, erase characters, and the in-place editing
        // quartet (insert/delete characters and lines).
        'J' | 'K' | 'X' | 'P' | 'M' | 'L' | '@' => SeqClass::EraseClear,
        _ => SeqClass::Other,
    }
}

/// Classifies an operating-system command by its numeric selector — the
/// digits before the first `;`. Read numerically, the way terminals read it,
/// so `052` is the clipboard selector and not a novel one.
fn classify_osc(body: &str) -> SeqClass {
    let digits_end = body
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(body.len());
    match body[..digits_end].parse::<u32>() {
        Ok(8) => SeqClass::OscHyperlink,
        Ok(52) => SeqClass::OscClipboard,
        _ => SeqClass::OscOther,
    }
}

#[cfg(test)]
mod tests {
    use super::{SeqClass, classify};

    #[test]
    fn the_csi_families_classify_by_final_byte() {
        for (raw, class) in [
            ("\u{1b}[2;7H", SeqClass::CursorMovement),
            ("\u{1b}[10A", SeqClass::CursorMovement),
            ("\u{1b}[3S", SeqClass::CursorMovement),
            ("\u{1b}[1;24r", SeqClass::CursorMovement),
            ("\u{1b}[s", SeqClass::CursorMovement),
            ("\u{1b}[u", SeqClass::CursorMovement),
            ("\u{1b}[1;38;5;9m", SeqClass::Sgr),
            ("\u{1b}[m", SeqClass::Sgr),
            ("\u{1b}[2J", SeqClass::EraseClear),
            ("\u{1b}[K", SeqClass::EraseClear),
            ("\u{1b}[5X", SeqClass::EraseClear),
            ("\u{1b}[2L", SeqClass::EraseClear),
            ("\u{1b}[b", SeqClass::Other),
            ("\u{1b}[8;40;120t", SeqClass::Other),
        ] {
            assert_eq!(classify(raw), class, "{raw:?}");
        }
    }

    #[test]
    fn the_two_character_escapes_classify_without_a_csi_body() {
        for (raw, class) in [
            ("\u{1b}7", SeqClass::CursorMovement),
            ("\u{1b}8", SeqClass::CursorMovement),
            ("\u{1b}M", SeqClass::CursorMovement),
            ("\u{1b}c", SeqClass::EraseClear),
            ("\u{1b}(B", SeqClass::Other),
            ("\u{1b}=", SeqClass::Other),
        ] {
            assert_eq!(classify(raw), class, "{raw:?}");
        }
    }

    #[test]
    fn every_mouse_mode_spelling_is_tracking_and_other_private_modes_are_not() {
        for mode in [9, 1000, 1001, 1002, 1003, 1004, 1005, 1006, 1015, 1016] {
            for final_byte in ['h', 'l'] {
                let raw = format!("\u{1b}[?{mode}{final_byte}");
                assert_eq!(classify(&raw), SeqClass::MouseTracking, "{raw:?}");
            }
        }
        // A mouse mode buried in a longer parameter list still names the
        // sequence: setting five modes, one of which captures the mouse,
        // captures the mouse.
        assert_eq!(classify("\u{1b}[?25;1002;1049h"), SeqClass::MouseTracking);
        for raw in [
            "\u{1b}[?25l",
            "\u{1b}[?1049h",
            "\u{1b}[?7l",
            "\u{1b}[?2004h",
        ] {
            assert_eq!(classify(raw), SeqClass::Other, "{raw:?}");
        }
    }

    #[test]
    fn osc_selectors_name_the_two_unsafe_commands_and_pool_the_rest() {
        for (raw, class) in [
            ("\u{1b}]8;;https://example.com\u{7}", SeqClass::OscHyperlink),
            ("\u{1b}]52;c;aGk=\u{7}", SeqClass::OscClipboard),
            // Numeric, not textual: a zero-padded selector is the same
            // selector.
            ("\u{1b}]052;c;aGk=\u{7}", SeqClass::OscClipboard),
            ("\u{1b}]0;a title\u{7}", SeqClass::OscOther),
            ("\u{1b}]10;?\u{7}", SeqClass::OscOther),
            ("\u{1b}]no-digits\u{7}", SeqClass::OscOther),
        ] {
            assert_eq!(classify(raw), class, "{raw:?}");
        }
    }

    #[test]
    fn a_cancelled_sequence_is_classified_by_what_it_was_trying_to_be() {
        // The CSI never got a final byte, so there is nothing to name it by;
        // the OSC never got its terminator, but its selector already said
        // "clipboard" — and the consumer deciding whether to worry wants
        // that name, not a shrug.
        assert_eq!(classify("\u{1b}[12;3\u{18}"), SeqClass::Other);
        assert_eq!(classify("\u{1b}]52;c;aGtld\u{18}"), SeqClass::OscClipboard);
    }

    #[test]
    fn the_c1_spellings_classify_like_their_escape_forms() {
        for (raw, class) in [
            ("\u{9b}5A", SeqClass::CursorMovement),
            ("\u{9b}?1000h", SeqClass::MouseTracking),
            ("\u{9d}52;c;aGk=\u{9c}", SeqClass::OscClipboard),
            ("\u{90}0;1|payload\u{9c}", SeqClass::DcsPassthrough),
            ("\u{85}", SeqClass::Other),
        ] {
            assert_eq!(classify(raw), class, "{raw:?}");
        }
    }

    #[test]
    fn the_unsafe_set_is_exactly_the_three_viewer_facing_classes() {
        for class in [
            SeqClass::OscHyperlink,
            SeqClass::OscClipboard,
            SeqClass::MouseTracking,
        ] {
            assert!(class.is_unsafe(), "{class:?}");
        }
        for class in [
            SeqClass::CursorMovement,
            SeqClass::Sgr,
            SeqClass::EraseClear,
            SeqClass::OscOther,
            SeqClass::DcsPassthrough,
            SeqClass::Abandoned,
            SeqClass::Other,
        ] {
            assert!(!class.is_unsafe(), "{class:?}");
        }
    }
}
