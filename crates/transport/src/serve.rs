//! The serve loop: read frames, dispatch them, write responses, and end the
//! way the runtime's lifecycle contract requires.
//!
//! One loop selects over three futures — the next inbound frame, the shutdown
//! signal (flipped by `runtime.shutdown` or by the binary's signal handlers),
//! and the writer's fatal signal — and every way the loop can end maps to one
//! of a small set of outcomes the binary turns into an exit code. The one
//! rule the shape exists to keep is the lockfile's: on an **operator**
//! shutdown (the caller closed stdin, or asked to shut down) the operator
//! intent is recorded *before* any session drains, so a kill between the
//! signal and the exit still tells a supervisor the runtime meant to stop.

use std::time::Duration;

use agent_bridge_core::{BoundedWriter, WriterConfig};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use crate::dispatch::{Dispatcher, RuntimeContext};
use crate::framing::{FrameError, FrameReader};
use crate::notify::transport_error_frame;
use crate::outbound::Outbound;

/// How the serve loop ended, for the binary to turn into an exit code and a
/// lockfile decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeOutcome {
    /// An operator path ended it — stdin EOF or `runtime.shutdown`. Sessions
    /// drained, the tail flushed; a clean exit (code 0), and the operator
    /// intent was recorded before the drain.
    Drained,
    /// The parent stopped reading stdout and die-loudly fired. Sessions were
    /// still cleaned up, but no events could go out and no operator intent was
    /// recorded — a crash-class exit a supervisor may restart.
    StdoutBlocked,
    /// The inbound stream ended abnormally: the peer sent a frame that could
    /// not be parsed or exceeded the size cap, or the read itself failed. The
    /// transport closed and cleaned up sessions; a parse or size violation is
    /// reported to the peer first, a read failure is not — the connection is
    /// already gone. No operator intent recorded.
    ProtocolClosed,
}

/// What the serve loop needs from its host beyond the streams themselves.
pub struct ServeControl {
    /// The shutdown channel. The loop watches a receiver derived from it; the
    /// dispatcher flips it for `runtime.shutdown`, and the binary holds its
    /// own clone to flip it from a signal handler.
    pub shutdown: watch::Sender<bool>,
    /// How long the session drain may take before its remainder is forced.
    pub drain_grace: Duration,
    /// The bounded writer's zero-progress deadline before die-loudly fires
    /// against a non-reading parent.
    pub stdout_deadline: Duration,
    /// The maximum inbound frame body, and the writer's buffer capacity —
    /// which must be at least this, so a single legal frame can never trip
    /// the writer's own overflow ceiling.
    pub max_frame_bytes: usize,
}

/// Serve JSON-RPC over the given streams until an end condition, running
/// `on_operator_intent` exactly once — before any session drains — when the
/// end is an operator shutdown.
///
/// `on_operator_intent` may fail (it records intent to durable storage); its
/// failure is logged on the shutdown path rather than swallowed by the caller,
/// so an operator sees that the intent was not persisted. The drain still runs:
/// abandoning it would orphan the hosted children, a worse outcome than the
/// intent gap, which a clean exit closes anyway through the exit code.
///
/// Generic over the streams so the binary passes its captured stdio and the
/// tests pass an in-process duplex; the contract is identical either way.
pub async fn serve<R, W>(
    ctx: RuntimeContext,
    reader: R,
    writer: W,
    control: ServeControl,
    on_operator_intent: impl FnOnce() -> std::io::Result<()> + Send,
) -> ServeOutcome
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (bounded, mut fatal) = BoundedWriter::new(
        writer,
        WriterConfig {
            capacity_bytes: control.max_frame_bytes,
            drain_deadline: control.stdout_deadline,
            farewell: crate::notify::stdout_blocked_farewell,
        },
    );
    let outbound = Outbound::new(bounded);
    let mut frames = FrameReader::new(reader, control.max_frame_bytes);
    let mut dispatcher = Dispatcher::new(ctx, outbound.clone(), control.shutdown.clone());
    let mut shutdown_rx = control.shutdown.subscribe();

    let reason = loop {
        // Observe a shutdown already requested before this loop began awaiting.
        // The signal handlers are wired before `serve` is called, so a signal
        // landing in that window sets the flag before the first `changed()`
        // could see it — and `changed()` only reports transitions *after* the
        // receiver's current version, so without this check that request would
        // be lost and the runtime would never drain. `borrow` (not
        // `borrow_and_update`) leaves the change for `changed()` to also catch.
        if *shutdown_rx.borrow() {
            break EndReason::ShutdownRequested;
        }
        tokio::select! {
            frame = frames.next_frame() => match frame {
                Ok(Some(frame)) => {
                    // A notification (no id) yields no response and is not
                    // answered; only a request produces one to enqueue.
                    if let Some(response) = dispatcher.dispatch(frame).await
                        && outbound
                            .send(crate::framing::encode(&response.encode()))
                            .is_err()
                    {
                        break EndReason::Fatal;
                    }
                    // An `attach` stashed its subscription; spawn the forwarder
                    // now, after the acknowledgement is queued, so no
                    // `session.event` can precede the ack it answers.
                    dispatcher.spawn_pending_attach();
                }
                Ok(None) => break EndReason::StdinEof,
                Err(error) => {
                    // Defer the terminal frame: it is emitted below, after the
                    // forwarders are aborted, so nothing can trail it.
                    break EndReason::ProtocolError(framing_error_frame(&error));
                }
            },
            changed = shutdown_rx.changed() => {
                // A closed sender cannot happen while the dispatcher holds one,
                // but were it to, treat it as a request to stop rather than
                // spin: either way the intent is to end.
                if changed.is_err() || *shutdown_rx.borrow() {
                    break EndReason::ShutdownRequested;
                }
            }
            () = fatal.fired() => break EndReason::Fatal,
        }
    };

    // The operator paths — and only they — record intent, before the drain.
    let operator = matches!(reason, EndReason::StdinEof | EndReason::ShutdownRequested);
    if operator && let Err(error) = on_operator_intent() {
        // The intent could not be recorded. A kill during the drain that
        // follows would then read as a crash, but the lock releases on exit
        // regardless, so a supervisor's restart still succeeds — and a clean
        // exit signals the intended stop through its exit code. Loud, not fatal.
        tracing::error!(%error, "could not record operator shutdown intent before draining");
    }
    // On an operator shutdown, drain the sessions and *then* flush each
    // forwarder's final events — the caller wants them. On a protocol close or
    // a dead wire, do the opposite: abort the forwarders *first*, so none can
    // enqueue a `session.event` / `session.eof`, and only then emit the terminal
    // `transport.error` — now guaranteed the last frame — before draining the
    // sessions (whose closed/eof have nowhere to go with the forwarders gone).
    if operator {
        dispatcher.drain(control.drain_grace).await;
        dispatcher.end_subscriptions(true).await;
    } else {
        dispatcher.end_subscriptions(false).await;
        if let EndReason::ProtocolError(Some(frame)) = &reason {
            let _ = outbound.send(frame.clone());
        }
        dispatcher.drain(control.drain_grace).await;
    }
    drop(dispatcher);
    let flushed = outbound.reclaim_and_shutdown().await;
    if operator && (!flushed || fatal.is_fired()) {
        // The operator asked to stop and the intent is recorded, so this still
        // exits clean — but the goodbye frames may not have reached a caller
        // that stopped reading mid-drain, which is worth a line.
        tracing::warn!("the final frames may not have been delivered before exit");
    }

    match reason {
        EndReason::StdinEof | EndReason::ShutdownRequested => ServeOutcome::Drained,
        EndReason::Fatal => ServeOutcome::StdoutBlocked,
        EndReason::ProtocolError(_) => ServeOutcome::ProtocolClosed,
    }
}

/// Why the select loop ended, before it is collapsed to an outcome. Kept
/// separate so the operator-intent decision reads off the reason directly.
enum EndReason {
    StdinEof,
    ShutdownRequested,
    Fatal,
    /// A framing failure the peer caused. It carries the terminal
    /// `transport.error` frame *deferred*, so the serve tail can abort the
    /// forwarders before emitting it — then no forwarder can slip a
    /// `session.event` in behind the frame promised to be last.
    ProtocolError(Option<bytes::Bytes>),
}

/// The `transport.error` frame for a framing failure the peer caused: a body
/// past the size cap, or a header block the framer could not parse. Both are
/// terminal for the connection, so this is the last frame it emits.
fn framing_error_frame(error: &FrameError) -> Option<bytes::Bytes> {
    use agent_bridge_events::TransportErrorCode;
    match error {
        FrameError::TooLarge { .. } => Some(transport_error_frame(
            TransportErrorCode::FrameTooLarge,
            &error.to_string(),
        )),
        FrameError::Malformed(_) => Some(transport_error_frame(
            TransportErrorCode::MalformedFrame,
            &error.to_string(),
        )),
        // A read-side IO failure is a local transport failure, not malformed
        // input from the peer, and the connection is already gone — so nothing
        // is emitted rather than blaming the peer with a malformed-frame event.
        FrameError::Io(_) => None,
    }
}
