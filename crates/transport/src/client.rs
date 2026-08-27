//! A framed JSON-RPC client over a byte-stream pair.
//!
//! One framing implementation in the repository, spoken from both sides: this
//! is the client half the transport's own round-trip tests drive `serve` with,
//! and it is written to be the same client the conformance harness reuses when
//! it captures traces from this wire — so the bytes a scenario asserts on are
//! produced and consumed by the exact reader and writer the runtime uses.
//!
//! It is deliberately unopinionated about interleaving: `next` returns each
//! inbound message in arrival order — responses and notifications alike — and
//! `call` layers a request/await-response convenience on top, buffering the
//! notifications it steps over so a caller can inspect them afterwards.

use std::collections::VecDeque;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::framing::{FrameError, FrameReader, encode};

/// One inbound message: a response to a request, or a server notification.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    /// A response, carrying the id it answers and exactly one of a result or
    /// an error — the disjointness JSON-RPC guarantees, surfaced as two
    /// options a caller matches on.
    Response {
        /// The id this answers.
        id: Value,
        /// The success value, when the call succeeded.
        result: Option<Value>,
        /// The error object, when it failed.
        error: Option<Value>,
    },
    /// A server-to-client notification: a method and its params, no id.
    Notification {
        /// The notification method, e.g. `session.event`.
        method: String,
        /// The notification params.
        params: Value,
    },
}

/// A framed client over a read half and a write half.
pub struct Client<R, W> {
    reader: FrameReader<R>,
    writer: W,
    /// Notifications seen while `call` waited for a response, kept in arrival
    /// order for the caller to drain.
    buffered: VecDeque<Message>,
}

impl<R, W> Client<R, W>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    /// A client reading from `reader` and writing to `writer`, refusing any
    /// inbound frame larger than `max_frame_bytes`.
    pub fn new(reader: R, writer: W, max_frame_bytes: usize) -> Self {
        Self {
            reader: FrameReader::new(reader, max_frame_bytes),
            writer,
            buffered: VecDeque::new(),
        }
    }

    /// Send one request frame. `id` is echoed back on the matching response;
    /// `params` may be `Value::Null` for the parameterless methods.
    pub async fn send(&mut self, id: Value, method: &str, params: Value) -> std::io::Result<()> {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let frame = encode(&serde_json::to_vec(&request).expect("a request value serializes"));
        self.writer.write_all(&frame).await?;
        self.writer.flush().await
    }

    /// Write raw bytes straight to the wire, bypassing framing — the hook a
    /// test needs to hand the server a deliberately malformed or oversized
    /// frame and watch how it answers.
    pub async fn send_raw(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.writer.write_all(bytes).await?;
        self.writer.flush().await
    }

    /// The next inbound message in arrival order, or `None` at end of stream.
    /// Buffered notifications from a prior `call` are returned first.
    pub async fn next(&mut self) -> Result<Option<Message>, FrameError> {
        if let Some(message) = self.buffered.pop_front() {
            return Ok(Some(message));
        }
        self.read_message().await
    }

    /// Send a request and return its response result or error, buffering any
    /// notifications that arrive first. The returned `Result` is the JSON-RPC
    /// outcome — `Ok(result)` or `Err(error object)` — not a transport error;
    /// a stream that ends before the response is a framing error.
    pub async fn call(
        &mut self,
        id: Value,
        method: &str,
        params: Value,
    ) -> Result<Result<Value, Value>, FrameError> {
        self.send(id.clone(), method, params)
            .await
            .map_err(FrameError::Io)?;
        loop {
            match self.read_message().await? {
                Some(Message::Response {
                    id: got,
                    result,
                    error,
                }) if got == id => {
                    return Ok(match (result, error) {
                        (_, Some(error)) => Err(error),
                        (Some(result), None) => Ok(result),
                        (None, None) => Ok(Value::Null),
                    });
                }
                Some(other) => self.buffered.push_back(other),
                None => {
                    return Err(FrameError::Malformed("stream ended before the response"));
                }
            }
        }
    }

    /// The notifications `call` stepped over, drained in arrival order.
    pub fn take_buffered(&mut self) -> VecDeque<Message> {
        std::mem::take(&mut self.buffered)
    }

    /// Read and classify one frame straight off the wire.
    async fn read_message(&mut self) -> Result<Option<Message>, FrameError> {
        let Some(frame) = self.reader.next_frame().await? else {
            return Ok(None);
        };
        let value: Value = serde_json::from_slice(&frame)
            .map_err(|_| FrameError::Malformed("server frame is not JSON"))?;
        // A response carries an id; a notification carries a method and none.
        if value.get("method").is_some() && value.get("id").is_none() {
            Ok(Some(Message::Notification {
                method: value
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                params: value.get("params").cloned().unwrap_or(Value::Null),
            }))
        } else {
            Ok(Some(Message::Response {
                id: value.get("id").cloned().unwrap_or(Value::Null),
                result: value.get("result").cloned(),
                error: value.get("error").cloned(),
            }))
        }
    }
}
