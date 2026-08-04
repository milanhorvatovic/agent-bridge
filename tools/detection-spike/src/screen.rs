//! Headless virtual-terminal replay: feed the recorded byte stream into a
//! screen buffer and materialize the viewport at evaluation points.
//!
//! The screen-state pipeline never evaluates per byte. It samples the screen
//! at **evaluation points**: quiet-period boundaries — a gap in the recorded
//! arrival times long enough to mean the CLI finished painting — and feed
//! quiescence, the end of the stream. Replay derives both from the recorded
//! timing sidecar, so the same fixture always yields the same evaluation
//! points and the replay stays deterministic; nothing here reads a clock.
//!
//! The virtual terminal interprets the escape stream instead of stripping
//! it: cursor-positioned paints land in their addressed cells, so text the
//! stripped stream sees mashed (`Doyouwanttoproceed?`) is spaced out on the
//! screen, and a repaint overwrites cells instead of duplicating lines. The
//! viewport is bounded to the recorded terminal size with no scrollback —
//! the same shape as the screen-state side buffer the planned runtime keeps
//! — and follows a CLI that switches to the alternate screen.
//!
//! A stream the terminal cannot fully decode is an error, never a shorter
//! replay: emissions measured over a silently truncated screen would flatter
//! the pipeline.

use crate::pacing::PacedInput;
use crate::utf8::Reassembler;

/// Recorded-time gap that closes a paint burst and becomes an evaluation
/// point. Chosen from the corpus itself: inter-chunk gaps cluster below
/// 400 ms (paint bursts, key echo, spinner frames) or above 500 ms (the
/// driver thinking between steps), with almost nothing in between — and the
/// capture scripts' own quiet waits use the same 500 ms, so a boundary here
/// is a boundary the live session actually settled at.
pub const QUIET_GAP_NS: u64 = 500_000_000;

/// Why an evaluation point fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalCause {
    /// The gap to the next recorded chunk exceeded [`QUIET_GAP_NS`].
    QuietGap,
    /// The stream ended — the feed went quiescent.
    FeedEnd,
}

/// One materialized screen: the full viewport at an evaluation point, one
/// string per row with trailing blank cells trimmed. Blank rows stay in
/// place so row positions remain meaningful to region-anchored detection.
#[derive(Debug)]
pub struct EvalPoint {
    /// 1-based position of this evaluation point in the replay.
    pub ordinal: u64,
    /// Recorded arrival time of the last chunk fed before this point.
    pub monotonic_ns: u64,
    pub cause: EvalCause,
    pub rows: Vec<String>,
}

/// Replay one fixture's byte stream into a virtual terminal of the recorded
/// size and return the screen at every evaluation point.
pub fn eval_points(input: &PacedInput, cols: u16, rows: u16) -> Result<Vec<EvalPoint>, String> {
    let mut vt = avt::Vt::builder()
        .size(cols as usize, rows as usize)
        .scrollback_limit(0)
        .build();
    let mut reassembler = Reassembler::new();
    let mut points = Vec::new();

    for (index, (chunk, monotonic_ns)) in input.iter_chunks().enumerate() {
        if reassembler.push(chunk).is_err() {
            return Err(format!(
                "chunk {} of {} holds bytes that can never be valid UTF-8 — the \
                 virtual terminal takes decoded text, so this stream cannot replay \
                 at all, and a partial replay would report a screen the session \
                 never showed",
                index + 1,
                input.chunks.len(),
            ));
        }
        let decoded = reassembler.take_decoded();
        if !decoded.is_empty() {
            vt.feed_str(&decoded);
        }

        let boundary = match input.chunks.get(index + 1) {
            Some(next) => {
                if next.monotonic_ns - monotonic_ns >= QUIET_GAP_NS {
                    Some(EvalCause::QuietGap)
                } else {
                    None
                }
            }
            None => Some(EvalCause::FeedEnd),
        };
        if let Some(cause) = boundary {
            points.push(EvalPoint {
                ordinal: points.len() as u64 + 1,
                monotonic_ns,
                cause,
                rows: snapshot(&vt),
            });
        }
    }

    if reassembler.pending() != 0 {
        return Err(format!(
            "the stream ends mid-codepoint with {} undecoded trailing byte(s) — \
             the final screen would silently omit them",
            reassembler.pending(),
        ));
    }
    Ok(points)
}

fn snapshot(vt: &avt::Vt) -> Vec<String> {
    vt.view()
        .map(|line| line.text().trim_end().to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pacing::ChunkBoundary;

    /// Build a paced input from (bytes, arrival-time) pairs.
    fn paced(chunks: &[(&[u8], u64)]) -> PacedInput {
        let mut bytes = Vec::new();
        let mut boundaries = Vec::new();
        for (chunk, monotonic_ns) in chunks {
            boundaries.push(ChunkBoundary {
                offset: bytes.len(),
                len: chunk.len(),
                monotonic_ns: *monotonic_ns,
            });
            bytes.extend_from_slice(chunk);
        }
        PacedInput {
            bytes,
            chunks: boundaries,
        }
    }

    #[test]
    fn quiet_gaps_and_feed_end_are_the_only_evaluation_points() {
        let input = paced(&[
            (b"one", 0),
            (b" two", QUIET_GAP_NS / 4),   // busy: no point
            (b" three", QUIET_GAP_NS / 2), // quiet gap after this
            (b"\r\nfour", QUIET_GAP_NS / 2 + QUIET_GAP_NS), // feed end after this
        ]);
        let points = eval_points(&input, 80, 24).expect("valid stream replays");

        assert_eq!(points.len(), 2);
        assert_eq!(points[0].cause, EvalCause::QuietGap);
        assert_eq!(points[0].ordinal, 1);
        assert_eq!(points[0].monotonic_ns, QUIET_GAP_NS / 2);
        assert_eq!(points[0].rows[0], "one two three");
        assert_eq!(points[1].cause, EvalCause::FeedEnd);
        assert_eq!(points[1].ordinal, 2);
        assert_eq!(points[1].rows[1], "four");
    }

    #[test]
    fn cursor_positioned_paint_is_spaced_on_the_screen() {
        // The exact surface that defeats the stripped stream: a dialog title
        // painted word-by-word with cursor addressing between the words. The
        // stripper sees `Doyouwant`; the screen shows the addressed columns.
        let input = paced(&[(b"\x1b[1;1HDo\x1b[1;4Hyou\x1b[1;8Hwant", 0)]);
        let points = eval_points(&input, 80, 24).expect("valid stream replays");
        assert_eq!(points[0].rows[0], "Do you want");
    }

    #[test]
    fn a_repaint_overwrites_instead_of_duplicating() {
        // Home the cursor and rewrite the same row: the stripped stream sees
        // the text twice, the screen holds it once.
        let input = paced(&[(b"\x1b[1;1Hstatus line\x1b[1;1Hstatus line", 0)]);
        let points = eval_points(&input, 80, 24).expect("valid stream replays");
        assert_eq!(points[0].rows[0], "status line");
        assert_eq!(
            points[0].rows[1..]
                .iter()
                .filter(|row| !row.is_empty())
                .count(),
            0,
            "the repaint must not spill onto other rows"
        );
    }

    #[test]
    fn viewport_has_the_recorded_dimensions_and_trimmed_rows() {
        let input = paced(&[(b"x", 0)]);
        let small = eval_points(&input, 80, 24).expect("valid stream replays");
        assert_eq!(small[0].rows.len(), 24);
        let large = eval_points(&input, 120, 40).expect("valid stream replays");
        assert_eq!(large[0].rows.len(), 40);
        assert_eq!(large[0].rows[0], "x", "trailing blank cells are trimmed");
    }

    #[test]
    fn a_codepoint_split_across_chunks_is_reassembled() {
        let dialog = "❯ 1. Yes".as_bytes();
        let input = paced(&[(&dialog[..2], 0), (&dialog[2..], 1)]);
        let points = eval_points(&input, 80, 24).expect("split codepoint replays");
        assert_eq!(points.len(), 1, "mid-codepoint split is not a boundary");
        assert_eq!(points[0].rows[0], "❯ 1. Yes");
    }

    #[test]
    fn undecodable_bytes_abort_the_replay_with_position() {
        let input = paced(&[(b"fine", 0), (&[0xFF, 0xFE], 1), (b"never", 2)]);
        let err = eval_points(&input, 80, 24).expect_err("invalid UTF-8 must abort");
        assert!(
            err.contains("chunk 2 of 3"),
            "must say how far it got: {err}"
        );
    }

    #[test]
    fn a_stream_ending_mid_codepoint_aborts_the_replay() {
        let euro = "€".as_bytes(); // 3 bytes: E2 82 AC
        let input = paced(&[(b"ok ", 0), (&euro[..2], 1)]);
        let err = eval_points(&input, 80, 24).expect_err("truncated tail must abort");
        assert!(err.contains("mid-codepoint"), "must name the cause: {err}");
    }

    #[test]
    fn an_empty_stream_yields_no_evaluation_points() {
        let input = paced(&[]);
        assert!(
            eval_points(&input, 80, 24)
                .expect("empty is fine")
                .is_empty()
        );
    }
}
