//! LSP-style `Content-Length` framing over a byte stream.
//!
//! One header block, a blank line, then exactly `Content-Length` bytes of
//! body and **nothing after it** — the next frame's headers begin on the very
//! next byte. The trailing-terminator form is a real bug this project has had
//! to name: a reader that expected a `\r\n` after the payload would read the
//! following frame's first two header bytes as that terminator and corrupt
//! the stream on the second message, so the reader here consumes the payload
//! and stops, and [`encode`] writes no terminator.
//!
//! The reader is a small state machine over an [`AsyncRead`], resilient to a
//! frame arriving in any number of chunks: it accumulates into an internal
//! buffer, hands back one frame per `next_frame`, and keeps the remainder for
//! the next call. Two bounds keep a hostile or broken peer from turning the
//! buffer into unbounded memory — the frame-body cap the caller sets, and a
//! defensive ceiling on the header block itself.

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt};

/// What framing can refuse. Both are terminal for the stream they occur on:
/// once a `Content-Length` is unparseable or a body exceeds the cap, the
/// reader no longer knows where the next frame begins, so the transport
/// reports the condition and closes rather than trying to resynchronize on a
/// stream whose structure it has lost.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    /// A header line was not `Name: Value`, `Content-Length` was absent or
    /// not a non-negative integer, or the header block grew past its ceiling
    /// without a blank line — anything that makes the frame's structure
    /// unreadable. Maps to `transport.error { malformed_frame }`.
    #[error("malformed frame: {0}")]
    Malformed(&'static str),
    /// The declared `Content-Length` exceeds the configured cap. Bounds a
    /// denial-of-service via an enormous length before a byte of the body is
    /// read. Maps to `transport.error { frame_too_large }` / `-32010`.
    #[error("frame body of {declared} bytes exceeds the {cap}-byte cap")]
    TooLarge {
        /// The `Content-Length` the peer declared.
        declared: usize,
        /// The configured maximum.
        cap: usize,
    },
    /// The underlying stream failed mid-frame.
    #[error("transport read failed: {0}")]
    Io(#[from] std::io::Error),
}

/// The header block may not exceed this before the blank line, defensively:
/// the spec bounds the body with `Content-Length` but says nothing about the
/// headers, so a peer dribbling header bytes forever would otherwise grow the
/// buffer without limit. Far above any legitimate header block (a
/// `Content-Length` and an optional `Content-Type`), so it can only be hit by
/// a peer that is not framing at all.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// The end-of-headers marker: a blank line closing the header block.
const HEADER_TERMINATOR: &[u8] = b"\r\n\r\n";

/// Encode one payload as a frame: `Content-Length: N\r\n\r\n` then the N
/// payload bytes, and no terminator after them. The single place a frame is
/// written, so the no-trailing-terminator rule lives in exactly one spot.
#[must_use]
pub fn encode(payload: &[u8]) -> Bytes {
    let header = format!("Content-Length: {}\r\n\r\n", payload.len());
    let mut frame = Vec::with_capacity(header.len() + payload.len());
    frame.extend_from_slice(header.as_bytes());
    frame.extend_from_slice(payload);
    Bytes::from(frame)
}

/// The incremental reader over one inbound byte stream.
///
/// Owns the read half and a buffer of bytes seen but not yet delivered as a
/// frame. `next_frame` is the only way to advance it.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
    buffer: Vec<u8>,
    /// How many leading bytes of `buffer` belong to a frame already returned;
    /// dropped lazily so a returned frame does not force a shift of the tail
    /// every call.
    consumed: usize,
    max_frame_bytes: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// A reader over `inner` that refuses any body larger than
    /// `max_frame_bytes`.
    pub fn new(inner: R, max_frame_bytes: usize) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            consumed: 0,
            max_frame_bytes,
        }
    }

    /// The next complete frame's payload, or `None` at a clean end of stream
    /// (the peer closed its side between frames).
    ///
    /// A stream that ends *mid-frame* — bytes were buffered toward a frame
    /// that never completed — is [`FrameError::Malformed`], not a clean end:
    /// silently dropping a truncated frame would hide a peer that crashed
    /// while writing.
    pub async fn next_frame(&mut self) -> Result<Option<Bytes>, FrameError> {
        loop {
            if let Some(frame) = self.take_buffered_frame()? {
                return Ok(Some(frame));
            }
            // The active region is what has arrived and not yet been
            // delivered; a header block that fills it without a blank line is
            // a peer that is not speaking the protocol. Reserve room for a
            // terminator split across reads: the last few bytes may be the
            // start of the blank line, so only a buffer past the ceiling *plus*
            // that prefix proves the header itself exceeds the bound before the
            // terminator has fully arrived — without the reserve, a header of
            // exactly the ceiling whose terminator straddles a read boundary
            // would be rejected here, though the post-terminator check accepts
            // that same boundary once the blank line completes.
            if self.buffer.len() - self.consumed > MAX_HEADER_BYTES + HEADER_TERMINATOR.len() - 1
                && find(&self.buffer[self.consumed..], HEADER_TERMINATOR).is_none()
            {
                return Err(FrameError::Malformed("header block exceeded its ceiling"));
            }
            let mut chunk = [0u8; 8 * 1024];
            let read = self.inner.read(&mut chunk).await?;
            if read == 0 {
                if self.buffer.len() == self.consumed {
                    return Ok(None);
                }
                return Err(FrameError::Malformed("stream ended mid-frame"));
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }

    /// Try to carve one frame out of what is already buffered. `Ok(None)`
    /// means "need more bytes", never a protocol end — that distinction is
    /// [`Self::next_frame`]'s to make once it knows the stream is at EOF.
    fn take_buffered_frame(&mut self) -> Result<Option<Bytes>, FrameError> {
        let active = &self.buffer[self.consumed..];
        let Some(header_end) = find(active, HEADER_TERMINATOR) else {
            return Ok(None);
        };
        // Bound the header on its own length, not only on a terminator-less
        // overflow: a peer that does eventually send the blank line, but only
        // after an enormous header, would otherwise have that whole block
        // parsed. The ceiling holds whether or not the terminator is present.
        if header_end > MAX_HEADER_BYTES {
            return Err(FrameError::Malformed("header block exceeded its ceiling"));
        }
        let content_length = parse_content_length(&active[..header_end])?;
        if content_length > self.max_frame_bytes {
            return Err(FrameError::TooLarge {
                declared: content_length,
                cap: self.max_frame_bytes,
            });
        }
        let body_start = header_end + HEADER_TERMINATOR.len();
        if active.len() - body_start < content_length {
            return Ok(None);
        }
        let payload = Bytes::copy_from_slice(&active[body_start..body_start + content_length]);
        self.consumed += body_start + content_length;
        // Reclaim the delivered prefix once it has grown past what any single
        // frame's headers could be, so a long-lived connection does not carry
        // every byte it has ever seen.
        if self.consumed > MAX_HEADER_BYTES {
            self.buffer.drain(..self.consumed);
            self.consumed = 0;
        }
        Ok(Some(payload))
    }
}

/// Parse a header block into the one field framing needs.
///
/// `Content-Length` is required and must be a non-negative integer;
/// `Content-Type` and any other header are accepted and ignored (the base
/// protocol's `Content-Type` default is the only other header defined, and it
/// changes nothing here). A duplicated `Content-Length` is a malformed frame
/// rather than a silent last-wins, because the two values disagree about
/// where the next frame starts.
fn parse_content_length(headers: &[u8]) -> Result<usize, FrameError> {
    let text =
        std::str::from_utf8(headers).map_err(|_| FrameError::Malformed("headers are not UTF-8"))?;
    let mut content_length = None;
    for line in text.split("\r\n") {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or(FrameError::Malformed("header line is not Name: Value"))?;
        // A colon alone is not a `Name: Value` line: an empty or whitespace-only
        // name would otherwise slip through as a skipped non-Content-Length
        // header, contradicting the block's own definition of a malformed line.
        if name.trim().is_empty() {
            return Err(FrameError::Malformed("header line has an empty name"));
        }
        if name.trim().eq_ignore_ascii_case("content-length") {
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| FrameError::Malformed("Content-Length is not an integer"))?;
            if content_length.replace(parsed).is_some() {
                return Err(FrameError::Malformed("duplicate Content-Length"));
            }
        }
    }
    content_length.ok_or(FrameError::Malformed("no Content-Length header"))
}

/// The first index of `needle` in `haystack`, or `None`. A plain scan: the
/// needle is four bytes and the haystack is one bounded header block, so
/// nothing more elaborate earns its place.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// Drive a reader over a duplex whose write half we feed by hand, so a
    /// test can deliver bytes in exactly the chunking it wants to exercise.
    async fn reader_over(bytes: &[u8]) -> FrameReader<tokio::io::DuplexStream> {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        client.write_all(bytes).await.unwrap();
        client.shutdown().await.unwrap();
        FrameReader::new(server, 16 * 1024 * 1024)
    }

    #[tokio::test]
    async fn a_payload_with_newlines_and_utf8_survives_framing() {
        // The reason NDJSON was rejected: a payload can legitimately hold a
        // newline (a token's content) and multi-byte UTF-8, and framing must
        // carry those bytes intact.
        let payload = "line one\nlíne twö\r\nend".as_bytes();
        let mut reader = reader_over(&encode(payload)).await;
        let frame = reader.next_frame().await.unwrap().unwrap();
        assert_eq!(&frame[..], payload);
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn back_to_back_frames_parse_with_no_trailing_terminator() {
        // The regression that names this project's framing bug: two messages
        // written end to end, the second's `Content-Length` header beginning
        // at the first payload's last byte plus one. A reader expecting a
        // trailing CRLF corrupts here.
        let mut stream = encode(b"first");
        let mut both = Vec::from(&stream[..]);
        stream = encode(b"second-message");
        both.extend_from_slice(&stream);
        let mut reader = reader_over(&both).await;
        assert_eq!(&reader.next_frame().await.unwrap().unwrap()[..], b"first");
        assert_eq!(
            &reader.next_frame().await.unwrap().unwrap()[..],
            b"second-message"
        );
        assert!(reader.next_frame().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_frame_split_across_reads_reassembles() {
        // The header, then the body, arriving in separate writes with the
        // reader waiting between them: the state machine must hold the
        // partial frame rather than treating the first chunk as complete.
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let mut reader = FrameReader::new(server, 16 * 1024 * 1024);
        client
            .write_all(b"Content-Length: 5\r\n\r\nhel")
            .await
            .unwrap();
        client.flush().await.unwrap();
        let pending = tokio::spawn(async move { reader.next_frame().await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        client.write_all(b"lo").await.unwrap();
        client.shutdown().await.unwrap();
        let frame = pending.await.unwrap().unwrap().unwrap();
        assert_eq!(&frame[..], b"hello");
    }

    #[tokio::test]
    async fn a_tolerated_content_type_header_is_ignored() {
        let framed = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\nhi";
        let mut reader = reader_over(framed).await;
        assert_eq!(&reader.next_frame().await.unwrap().unwrap()[..], b"hi");
    }

    #[tokio::test]
    async fn an_oversized_length_is_refused_before_the_body() {
        let mut reader =
            FrameReader::new(reader_over(b"Content-Length: 100\r\n\r\n").await.inner, 8);
        // The cap is 8; the declared 100 is refused on the header alone, with
        // no body ever read.
        match reader.next_frame().await {
            Err(FrameError::TooLarge { declared, cap }) => {
                assert_eq!((declared, cap), (100, 8));
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_maximal_header_survives_a_terminator_split_across_reads() {
        // A header of exactly the ceiling is legal, and the blank line that
        // ends it may straddle a read boundary. The pre-terminator bound must
        // not reject it while only the first bytes of `\r\n\r\n` have arrived —
        // the reserve for a partial terminator is what keeps this valid frame.
        let mut header = Vec::from(&b"Content-Length: 5\r\nX-Pad: "[..]);
        header.extend(std::iter::repeat_n(b'a', MAX_HEADER_BYTES - header.len()));
        assert_eq!(header.len(), MAX_HEADER_BYTES);

        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let mut reader = FrameReader::new(server, 16 * 1024 * 1024);
        // The header and the first three terminator bytes; the fourth, and the
        // body, arrive only after the reader has processed this and reached the
        // pre-terminator bound with no complete terminator in sight.
        let mut first = header;
        first.extend_from_slice(b"\r\n\r");
        client.write_all(&first).await.unwrap();
        client.flush().await.unwrap();
        let pending = tokio::spawn(async move { reader.next_frame().await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        client.write_all(b"\nhello").await.unwrap();
        client.shutdown().await.unwrap();
        let frame = pending.await.unwrap().unwrap().unwrap();
        assert_eq!(&frame[..], b"hello");
    }

    #[tokio::test]
    async fn an_oversized_header_is_refused_even_with_a_terminator() {
        // The ceiling binds on the header's own length, not only on a
        // terminator-less overflow: a peer that sends an enormous but otherwise
        // well-formed header block, then the blank line, must still be refused
        // rather than have the whole block parsed.
        let mut input = Vec::new();
        input.extend_from_slice(b"Content-Type: ");
        input.extend(std::iter::repeat_n(b'x', MAX_HEADER_BYTES + 1024));
        input.extend_from_slice(b"\r\nContent-Length: 5\r\n\r\nhello");
        let mut reader = reader_over(&input).await;
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn a_missing_content_length_is_malformed() {
        let mut reader = reader_over(b"Content-Type: text/plain\r\n\r\nhi").await;
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn a_header_line_with_an_empty_name_is_malformed() {
        // A colon with nothing before it is not a `Name: Value` header. A
        // lenient scan would skip it as a non-Content-Length line and frame the
        // body regardless; the parser rejects it to hold its own contract.
        let mut reader = reader_over(b": x\r\nContent-Length: 2\r\n\r\nhi").await;
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::Malformed(_))
        ));
    }

    #[tokio::test]
    async fn a_stream_that_ends_mid_frame_is_malformed_not_a_clean_end() {
        // Ten bytes promised, three delivered, then EOF: a truncated frame is
        // a broken peer, and reporting a clean end would hide that.
        let mut reader = reader_over(b"Content-Length: 10\r\n\r\nabc").await;
        assert!(matches!(
            reader.next_frame().await,
            Err(FrameError::Malformed(_))
        ));
    }

    #[test]
    fn encode_writes_the_header_and_body_with_no_terminator() {
        let frame = encode(b"xy");
        assert_eq!(&frame[..], b"Content-Length: 2\r\n\r\nxy");
    }

    #[tokio::test]
    async fn the_framer_never_panics_on_adversarial_input() {
        // The framer parses untrusted wire bytes, so it is fuzzed: many
        // pseudo-random streams — pure garbage, structured-but-hostile frames,
        // and random header blocks — must each yield a typed `Result`, never a
        // panic and never a non-terminating loop. Deterministic (fixed seed) so
        // a failure reproduces; this is the bounded PR pass, with the deep
        // unbounded run left to a nightly `cargo-fuzz` lane.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            // xorshift64 — a dependency-free, reproducible byte source.
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..5_000 {
            let mut bytes = Vec::new();
            match next() % 3 {
                // Pure garbage.
                0 => {
                    for _ in 0..(next() % 512) {
                        bytes.push((next() & 0xff) as u8);
                    }
                }
                // A real header, then a body of an unrelated random length —
                // exercises the "declared length vs delivered bytes" edges.
                1 => {
                    let declared = next() % 2048;
                    bytes.extend_from_slice(
                        format!("Content-Length: {declared}\r\n\r\n").as_bytes(),
                    );
                    for _ in 0..(next() % 512) {
                        bytes.push((next() & 0xff) as u8);
                    }
                }
                // Random header lines then a blank line then a body — hostile
                // header shapes, duplicate or missing Content-Length, junk
                // values.
                _ => {
                    for _ in 0..(next() % 6) {
                        for _ in 0..(next() % 20) {
                            bytes.push(b'a' + (next() % 26) as u8);
                        }
                        bytes.extend_from_slice(b": ");
                        for _ in 0..(next() % 20) {
                            bytes.push((next() & 0xff) as u8);
                        }
                        bytes.extend_from_slice(b"\r\n");
                    }
                    bytes.extend_from_slice(b"\r\n");
                    for _ in 0..(next() % 256) {
                        bytes.push((next() & 0xff) as u8);
                    }
                }
            }
            // Drain to a definite end: every call is a typed Result, and the
            // loop reaching `Ok(None)`/`Err` (rather than panicking or spinning)
            // is the property. The iteration cap is a backstop against a reader
            // that ever failed to make progress — it never should.
            let mut reader = reader_over(&bytes).await;
            for _ in 0..2_000 {
                match reader.next_frame().await {
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
        }
    }
}
