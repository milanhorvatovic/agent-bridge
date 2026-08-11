//! What a consumer sees in place of bytes that were never text.
//!
//! The layer below has already done the cutting: an output chunk never
//! begins or ends part-way through a character, and a run no continuation
//! could repair arrives separately, located in stream coordinates. What is
//! decided *here* is the encoding-error policy.
//! Undecodable bytes are replaced with U+FFFD in the text feed and
//! reported with their offset and length, so the substitution is visible in
//! the record even though the bytes themselves never leak into an event.
//! And when replacements arrive in a burst — a CLI emitting something that
//! is not text at all — the reports degrade to a single coalesced incident,
//! because a thousand identical alarms carry less information than one that
//! says a thousand things happened.
//!
//! Both halves serve the same principle: the runtime never silently drops
//! content. Either the bytes round-trip cleanly, or an incident records the
//! lossy substitution.

use std::time::{Duration, Instant};

use agent_bridge_events::{PtyErrorCode, PtyErrorPayload};

/// One piece of a decoded chunk, in order.
#[derive(Debug, PartialEq, Eq)]
pub enum DecodeItem<'a> {
    /// Valid text, borrowed from the input.
    Text(&'a str),
    /// A span that is not UTF-8 and cannot become it. The text feed carries
    /// one U+FFFD in its place; the span's position and size are what an
    /// `encoding_replacement` incident reports.
    Invalid {
        /// Position of the span's first byte, counted from the first byte
        /// the child ever wrote.
        offset: u64,
        /// How many bytes were replaced.
        len: u32,
    },
}

/// Split one chunk into its valid text and its undecodable spans.
///
/// `stream_offset` is the chunk's position in the whole stream, so the spans
/// come out in stream coordinates — the same coordinates the layer below
/// uses for the runs it pre-locates, and the ones a session recording can be
/// indexed by.
///
/// The chunk is treated as complete: a trailing sequence that stops part-way
/// is reported as invalid rather than held, because carrying an incomplete
/// suffix to the next read is the terminal layer's job and a chunk that ends
/// mid-character has therefore already broken that contract. Holding it here
/// would turn a broken promise below into silence above, which is the one
/// outcome this crate must never produce.
pub fn decode(bytes: &[u8], stream_offset: u64) -> Decode<'_> {
    Decode {
        bytes,
        at: 0,
        stream_offset,
    }
}

/// Iterator behind [`decode`].
pub struct Decode<'a> {
    bytes: &'a [u8],
    /// Index of the first byte not yet accounted for.
    at: usize,
    stream_offset: u64,
}

impl<'a> Iterator for Decode<'a> {
    type Item = DecodeItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // Copied out of `self` so the returned borrow is the input's
        // lifetime, not this call's.
        let bytes: &'a [u8] = self.bytes;
        let rest = &bytes[self.at..];
        if rest.is_empty() {
            return None;
        }
        match std::str::from_utf8(rest) {
            Ok(text) => {
                self.at = bytes.len();
                Some(DecodeItem::Text(text))
            }
            Err(err) if err.valid_up_to() > 0 => {
                let valid = err.valid_up_to();
                let text = std::str::from_utf8(&rest[..valid])
                    .expect("valid_up_to promises this prefix decodes");
                self.at += valid;
                Some(DecodeItem::Text(text))
            }
            Err(err) => {
                // `error_len` distinguishes a sequence no continuation could
                // repair (`Some`) from one that stops at the end of the
                // chunk (`None`). Both are invalid here — see [`decode`] on
                // why an unfinished tail is not carried.
                let len = err.error_len().unwrap_or(rest.len());
                let item = DecodeItem::Invalid {
                    offset: self.stream_offset + self.at as u64,
                    len: len as u32,
                };
                self.at += len;
                Some(item)
            }
        }
    }
}

/// One reportable encoding event, typed rather than serialized so the tests
/// that pin the policy compare values, not strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodingIncident {
    /// One undecodable span was replaced with U+FFFD.
    Replacement {
        /// Position of the replaced span's first byte in the stream.
        offset: u64,
        /// How many bytes the one replacement character stands in for.
        len: u32,
    },
    /// Replacements arrived faster than they are worth reporting one by one.
    /// Covers the replacements *beyond* the ones already reported
    /// individually, so a consumer summing incidents counts each replaced
    /// span exactly once.
    Burst {
        /// How many replacements this burst stands in for.
        count: u32,
        /// How long the coalescing window actually ran, in milliseconds.
        window_ms: u32,
    },
}

impl EncodingIncident {
    /// The `pty.error` payload this incident publishes, on the codes the
    /// event taxonomy already names. Bus publication and `seq` assignment
    /// belong to the core; this is only the mapping.
    pub fn to_payload(&self) -> PtyErrorPayload {
        let mut detail = serde_json::Map::new();
        let (code, message) = match *self {
            EncodingIncident::Replacement { offset, len } => {
                detail.insert("offset".into(), offset.into());
                detail.insert("length".into(), len.into());
                (
                    PtyErrorCode::EncodingReplacement,
                    format!("{len} undecodable byte(s) at offset {offset} replaced with U+FFFD"),
                )
            }
            EncodingIncident::Burst { count, window_ms } => {
                detail.insert("count".into(), count.into());
                detail.insert("window_ms".into(), window_ms.into());
                (
                    PtyErrorCode::EncodingBurst,
                    format!(
                        "{count} further replacement(s) within {window_ms} ms coalesced into \
                         this event"
                    ),
                )
            }
        };
        PtyErrorPayload {
            code,
            message,
            detail,
        }
    }
}

/// The burst window: replacements this close together coalesce.
pub const BURST_WINDOW: Duration = Duration::from_secs(1);

/// How many replacements a window reports individually before coalescing.
///
/// The policy says three within a second degrade to a burst; the
/// first two have already been reported by the time anything qualifies —
/// holding them back to see whether a burst forms would delay every isolated
/// replacement by a full window — so "degrade" means the third and everything
/// after it fold into one event.
const REPORTED_INDIVIDUALLY: u32 = 2;

/// Turns a stream of replacements into at most a few incidents per second.
///
/// A pure state machine over instants the caller supplies — the same shape
/// as [`crate::screen::EvalPointScheduler`], and for the same reason: with no
/// clock of its own it is deterministic under test, and [`deadline`] lets
/// whoever owns it arm a real timer for the moment the pending burst comes
/// due.
///
/// [`deadline`]: BurstCoalescer::deadline
#[derive(Debug, Default)]
pub struct BurstCoalescer {
    /// When the current window opened: the instant of its first replacement.
    window_start: Option<Instant>,
    /// Replacements this window reported individually, at most
    /// [`REPORTED_INDIVIDUALLY`].
    reported: u32,
    /// Replacements swallowed into the pending burst.
    swallowed: u32,
}

impl BurstCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    /// A replacement happened at `now`. Returns what to emit right away — a
    /// burst closing an expired window, the replacement itself, both, or
    /// neither when the open window has swallowed it.
    pub fn on_replacement(&mut self, now: Instant, offset: u64, len: u32) -> Vec<EncodingIncident> {
        let mut out = Vec::new();
        if let Some(burst) = self.poll(now) {
            out.push(burst);
        }
        self.window_start.get_or_insert(now);
        if self.reported < REPORTED_INDIVIDUALLY {
            self.reported += 1;
            out.push(EncodingIncident::Replacement { offset, len });
        } else {
            if self.swallowed == 0 {
                tracing::debug!(
                    "replacement rate crossed the burst threshold; coalescing further reports"
                );
            }
            self.swallowed += 1;
        }
        out
    }

    /// Close the window if `now` is past its end, emitting the pending burst
    /// when there is one.
    ///
    /// Also how a window with nothing pending is retired: two isolated
    /// replacements a minute apart must not count toward one burst, and it
    /// is this reset that keeps them apart.
    pub fn poll(&mut self, now: Instant) -> Option<EncodingIncident> {
        let start = self.window_start?;
        if now.duration_since(start) < BURST_WINDOW {
            return None;
        }
        self.reset();
        let swallowed = std::mem::take(&mut self.swallowed);
        (swallowed > 0).then_some(EncodingIncident::Burst {
            count: swallowed,
            window_ms: BURST_WINDOW.as_millis() as u32,
        })
    }

    /// When the pending burst comes due, if one is pending at all.
    ///
    /// `None` while nothing has been swallowed: a window that closes empty
    /// needs no timer, because closing it late costs nothing.
    pub fn deadline(&self) -> Option<Instant> {
        self.window_start
            .filter(|_| self.swallowed > 0)
            .map(|start| start + BURST_WINDOW)
    }

    /// End of stream: whatever is pending will not grow further, so it is
    /// reported now, with the window sized to what actually elapsed.
    pub fn finish(&mut self, now: Instant) -> Option<EncodingIncident> {
        let start = self.window_start?;
        self.reset();
        let swallowed = std::mem::take(&mut self.swallowed);
        (swallowed > 0).then_some(EncodingIncident::Burst {
            count: swallowed,
            window_ms: now.duration_since(start).min(BURST_WINDOW).as_millis() as u32,
        })
    }

    fn reset(&mut self) {
        self.window_start = None;
        self.reported = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(items: &[DecodeItem<'_>]) -> String {
        items
            .iter()
            .map(|item| match item {
                DecodeItem::Text(text) => (*text).to_string(),
                DecodeItem::Invalid { .. } => "\u{FFFD}".to_string(),
            })
            .collect()
    }

    #[test]
    fn a_valid_chunk_decodes_whole() {
        let items: Vec<_> = decode("héllo 🌍".as_bytes(), 0).collect();
        assert_eq!(items, vec![DecodeItem::Text("héllo 🌍")]);
    }

    #[test]
    fn an_invalid_span_is_located_in_stream_coordinates() {
        // The chunk starts at stream offset 100, so the 0xFF at chunk
        // position 2 is at stream position 102 — the coordinates a session
        // recording is indexed by, not the coordinates of one read.
        let items: Vec<_> = decode(b"ok\xFFgo", 100).collect();
        assert_eq!(
            items,
            vec![
                DecodeItem::Text("ok"),
                DecodeItem::Invalid {
                    offset: 102,
                    len: 1
                },
                DecodeItem::Text("go"),
            ]
        );
        assert_eq!(text(&items), "ok\u{FFFD}go");
    }

    #[test]
    fn a_chunk_ending_mid_character_reports_the_tail_as_invalid() {
        // Two of the world emoji's four bytes. Within one chunk nothing can
        // complete them — carrying a suffix across chunks is the terminal
        // layer's contract, and it promised not to produce this chunk.
        let emoji = "🌍".as_bytes();
        let items: Vec<_> = decode(&emoji[..2], 7).collect();
        assert_eq!(items, vec![DecodeItem::Invalid { offset: 7, len: 2 }]);
    }

    #[test]
    fn every_split_of_a_valid_corpus_decodes_to_the_corpus() {
        // The contract chunks arrive under: cut only at character
        // boundaries. For every such cut, the two chunks must decode to the
        // corpus with no invalid span invented — exhaustive, not sampled.
        let corpus = "héllo 🌍 — ascii, 2-byte é, 3-byte —, 4-byte 🌍";
        for (at, _) in corpus.char_indices().chain([(corpus.len(), ' ')]) {
            let bytes = corpus.as_bytes();
            let mut items: Vec<_> = decode(&bytes[..at], 0).collect();
            items.extend(decode(&bytes[at..], at as u64));
            assert!(
                items.iter().all(|item| matches!(item, DecodeItem::Text(_))),
                "a split at {at} invented an invalid span"
            );
            assert_eq!(text(&items), corpus, "a split at {at} corrupted the text");
        }
    }

    #[test]
    fn consecutive_invalid_bytes_come_out_as_reported_runs() {
        // `from_utf8` reports invalid input in maximal-subpart runs; what
        // matters here is that every byte is covered by exactly one span and
        // the valid neighbours survive.
        let items: Vec<_> = decode(b"a\xFF\xFE\xFDz", 0).collect();
        let replaced: u32 = items
            .iter()
            .filter_map(|item| match item {
                DecodeItem::Invalid { len, .. } => Some(*len),
                DecodeItem::Text(_) => None,
            })
            .sum();
        assert_eq!(replaced, 3, "three bytes went in, three must be reported");
        assert_eq!(text(&items).matches('\u{FFFD}').count(), 3);
        assert!(text(&items).starts_with('a') && text(&items).ends_with('z'));
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn two_replacements_in_a_window_stay_individual() {
        let base = Instant::now();
        let mut coalescer = BurstCoalescer::new();
        assert_eq!(
            coalescer.on_replacement(at(base, 0), 5, 1),
            vec![EncodingIncident::Replacement { offset: 5, len: 1 }]
        );
        assert_eq!(
            coalescer.on_replacement(at(base, 500), 9, 2),
            vec![EncodingIncident::Replacement { offset: 9, len: 2 }]
        );
        assert_eq!(coalescer.deadline(), None, "nothing pending, no timer");
        assert_eq!(coalescer.finish(at(base, 600)), None);
    }

    #[test]
    fn the_third_and_fourth_replacements_coalesce_into_one_burst() {
        // Four undecodable spans inside one second: the first two report
        // individually, the third and fourth fold into exactly one burst
        // when the window closes — not four events.
        let base = Instant::now();
        let mut coalescer = BurstCoalescer::new();
        let mut emitted = Vec::new();
        for step in 0..4u64 {
            emitted.extend(coalescer.on_replacement(at(base, step * 100), step, 1));
        }
        assert_eq!(emitted.len(), 2, "only the first two report individually");
        assert_eq!(
            coalescer.deadline(),
            Some(at(base, 1000)),
            "a pending burst arms the window timer"
        );
        assert_eq!(
            coalescer.poll(at(base, 999)),
            None,
            "the window is still open"
        );
        assert_eq!(
            coalescer.poll(at(base, 1000)),
            Some(EncodingIncident::Burst {
                count: 2,
                window_ms: 1000,
            })
        );
        assert_eq!(
            coalescer.poll(at(base, 1001)),
            None,
            "emitted once, not twice"
        );
    }

    #[test]
    fn the_window_resets_after_it_closes() {
        let base = Instant::now();
        let mut coalescer = BurstCoalescer::new();
        for step in 0..3u64 {
            coalescer.on_replacement(at(base, step * 10), step, 1);
        }
        coalescer.poll(at(base, 1500));
        // A fresh window after the close: the next replacement is the first
        // of its own second, reported individually again.
        assert_eq!(
            coalescer.on_replacement(at(base, 1600), 40, 1),
            vec![EncodingIncident::Replacement { offset: 40, len: 1 }]
        );
    }

    #[test]
    fn a_replacement_after_an_expired_window_flushes_the_burst_first() {
        // Nothing polled the coalescer while the stream was quiet; the next
        // replacement must not fold into a window that is over. Both events
        // come out together, oldest first.
        let base = Instant::now();
        let mut coalescer = BurstCoalescer::new();
        for step in 0..3u64 {
            coalescer.on_replacement(at(base, step), step, 1);
        }
        let emitted = coalescer.on_replacement(at(base, 5000), 90, 1);
        assert_eq!(
            emitted,
            vec![
                EncodingIncident::Burst {
                    count: 1,
                    window_ms: 1000,
                },
                EncodingIncident::Replacement { offset: 90, len: 1 },
            ]
        );
    }

    #[test]
    fn finish_reports_the_window_that_actually_elapsed() {
        // End of stream 400 ms into the window: the burst says 400, not
        // 1000, because claiming a window that never ran would misstate the
        // rate.
        let base = Instant::now();
        let mut coalescer = BurstCoalescer::new();
        for step in 0..4u64 {
            coalescer.on_replacement(at(base, step * 100), step, 1);
        }
        assert_eq!(
            coalescer.finish(at(base, 400)),
            Some(EncodingIncident::Burst {
                count: 2,
                window_ms: 400,
            })
        );
        assert_eq!(
            coalescer.finish(at(base, 500)),
            None,
            "nothing left to flush"
        );
    }

    #[test]
    fn incident_payloads_carry_the_documented_codes_and_detail() {
        let replacement = EncodingIncident::Replacement { offset: 12, len: 3 }.to_payload();
        assert_eq!(replacement.code, PtyErrorCode::EncodingReplacement);
        assert_eq!(replacement.detail["offset"], 12);
        assert_eq!(replacement.detail["length"], 3);

        let burst = EncodingIncident::Burst {
            count: 7,
            window_ms: 1000,
        }
        .to_payload();
        assert_eq!(burst.code, PtyErrorCode::EncodingBurst);
        assert_eq!(burst.detail["count"], 7);
        assert_eq!(burst.detail["window_ms"], 1000);
    }
}
