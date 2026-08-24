//! The close path: hint first, a bounded wait for a voluntary exit, then
//! escalation — and the cleanup invariants, verified rather than assumed.
//!
//! Hint first is the contract: a non-forced
//! close applies the adapter's hint, waits the drain window for the
//! CLI to leave on its own, and only then reaches for the terminal layer's
//! termination sequence. The hint is an accelerator the runtime never
//! depends on; the escalation is what it guarantees. A forced close skips
//! straight to the guarantee.
//!
//! `finalize` is the one exit from every route — caller close, drain
//! expiry, launch failure, child exit — so the `Closed` invariants hold on
//! all of them: process group or job empty (asked, not hoped), input
//! writer joined, readers joined, the `closed` event emitted with what is
//! known, the stream sealed, the log closed.

use agent_bridge_adapter_api::{InputStep, ShutdownHint, ShutdownSignal};
use agent_bridge_events::{EventBody, EventKind, LifecycleSessionClosed};
use agent_bridge_pty::Signal;
use bytes::Bytes;
use serde_json::{Map, json};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

use crate::actor::{Actor, WriteRequest, exit_status_of};
use crate::command::Reply;
use crate::error::SessionError;
use crate::logfile::LogLevel;
use crate::state::{Edge, SessionState, transition};

/// How long finalize will wait for the reader task after the terminal
/// handle is dropped. Generous — the stream ends with the terminal — and
/// bounded so a defect in the layer below cannot wedge the close path.
const READER_JOIN_LIMIT: Duration = Duration::from_secs(10);

impl Actor {
    pub(crate) async fn handle_close(&mut self, force: bool, reply: Reply<()>) {
        match self.state {
            // Close paths race, and a late second close is normal rather
            // than an error (the same stance the bus takes on a second
            // seal).
            SessionState::Closed => {
                let _ = reply.send(Ok(()));
            }
            SessionState::Closing => {
                self.close_replies.push(reply);
                if force && self.drain_deadline.is_some() {
                    // A force-close during a graceful drain escalates now
                    // rather than at the deadline. `drained: false` is the
                    // published meaning — the close escalated before a
                    // voluntary exit — whether the window expired or a
                    // force cut the wait short; a window that never
                    // existed at all is the absent case.
                    self.drain_deadline = None;
                    self.finalize(Edge::CloseComplete, Some(false)).await;
                }
            }
            SessionState::Created | SessionState::Launching => {
                let _ = reply.send(Err(SessionError::InvalidStateForOperation {
                    state: self.state,
                    op: "close",
                }));
            }
            SessionState::Connecting
            | SessionState::Running
            | SessionState::AwaitingApproval
            | SessionState::Interrupted => {
                // Approvals expire on close: every parked source
                // hears Cancelled, never a timeout.
                self.approvals.cancel_all();
                self.interrupt_pending = false;
                let _ = self.apply_edge(Edge::CloseRequested);
                self.close_replies.push(reply);
                if force {
                    // Skipped hint: `drained` is omitted from the closed
                    // payload — the drain window never existed, so neither
                    // answer about it would be true.
                    self.finalize(Edge::CloseComplete, None).await;
                } else {
                    self.apply_shutdown_hint().await;
                    self.drain_deadline = Some(Instant::now() + self.config.stdin_drain);
                }
            }
        }
    }

    async fn apply_shutdown_hint(&mut self) {
        match self.shutdown_hint.clone() {
            ShutdownHint::Input(steps) => {
                let Some(writer) = &self.writer else { return };
                let input = writer.tx.clone();
                // On its own task so a settle pause never stops the actor
                // from processing the force-close that may arrive mid-hint.
                tokio::spawn(async move {
                    for step in steps {
                        match step {
                            InputStep::Write(text) => {
                                let request = WriteRequest {
                                    bytes: Bytes::from(text.into_bytes()),
                                    reply: None,
                                };
                                if input.send(request).await.is_err() {
                                    break;
                                }
                            }
                            InputStep::Settle(pause) => tokio::time::sleep(pause).await,
                        }
                    }
                });
            }
            ShutdownHint::Signal(signal) => {
                let Some(pty) = self.pty.clone() else { return };
                let signal = match signal {
                    ShutdownSignal::Interrupt => Signal::Interrupt,
                    ShutdownSignal::Terminate => Signal::Terminate,
                };
                let outcome = tokio::task::spawn_blocking(move || pty.signal(signal)).await;
                if let Ok(Err(error)) = outcome {
                    // The hint failing is not the close failing: the drain
                    // window runs and the escalation still lands.
                    tracing::warn!(%error, "shutdown hint signal was not delivered");
                }
            }
            ShutdownHint::CloseStdin => {
                // Undeliverable in v1: the terminal layer exposes no
                // per-direction close — a terminal's input *is* the
                // terminal. The drain window and escalation below still
                // end the session; a deliberate, stated gap until an
                // adapter actually declares this hint.
                tracing::warn!(
                    "ShutdownHint::CloseStdin cannot be delivered over a PTY; relying on escalation"
                );
                self.log_record(
                    LogLevel::Warn,
                    "session.shutdown_hint_undeliverable",
                    Map::new(),
                );
            }
        }
    }

    /// The one exit: reap, verify, account, announce, seal, close the log,
    /// and only then flip the state and answer the closers.
    ///
    /// `final_edge` is the table row into `Closed` this route takes
    /// (`CloseComplete`, `LaunchFailed`, or `ChildExitedBeforeOutput`);
    /// `drained` is `Some` only when a drain window existed to answer for.
    ///
    /// `Closed` becoming observable is deliberately the *last* act: the
    /// handle's close fast path and `wait_closed` treat the state as "the
    /// cleanup invariants have been verified", so flipping it before the
    /// seal and the log join would let a concurrent observer read a diary
    /// missing its final record or a stream that has not ended.
    pub(crate) async fn finalize(&mut self, final_edge: Edge, drained: Option<bool>) {
        self.drain_deadline = None;
        self.interrupt_pending = false;

        // The edge is judged before any irreversible teardown, so a route
        // arriving with a pair the table rejects fails loudly while
        // nothing has happened yet. The session still ends: every caller
        // of finalize has committed to that, and stranding a reaped,
        // sealed session in a live-looking state — unreapable, its
        // waiters parked forever — would be the one outcome worse than a
        // wrong edge label. The table check is the alarm, not the brake.
        let next = transition(self.state, final_edge).unwrap_or_else(|error| {
            tracing::error!(%error, "finalize took an edge the table rejects; closing anyway");
            SessionState::Closed
        });
        debug_assert_eq!(next, SessionState::Closed);

        // Reap the child and everything it spawned. On a voluntary exit
        // this returns promptly with the status; on a non-cooperating
        // child it is the SIGTERM → grace → SIGKILL / job-terminate
        // escalation. Either way it returns only once nothing remains in
        // the process group or job.
        let mut exit = None;
        if let Some(pty) = self.pty.clone() {
            let grace = self.config.terminate_grace;
            let outcome = tokio::task::spawn_blocking(move || pty.terminate(grace)).await;
            exit = exit_status_of(outcome);

            // The invariant, asked of the operating system rather than
            // inferred from terminate's return: nothing is left inside the
            // session. `contained` is the on-demand census the terminal
            // layer provides for exactly this question.
            if let Some(pty) = &self.pty {
                match pty.contained() {
                    Ok(pids) if pids.is_empty() => {}
                    Ok(pids) => tracing::error!(
                        session_id = %self.shared.session_id,
                        remaining = pids.len(),
                        "cleanup invariant violated: processes remain after terminate"
                    ),
                    Err(error) => tracing::warn!(
                        session_id = %self.shared.session_id,
                        %error,
                        "could not census the session after terminate"
                    ),
                }
            }
        }

        // Input side down: no further writes, the writer task joined.
        if let Some(writer) = self.writer.take() {
            drop(writer.tx);
            let _ = writer.task.await;
        }

        // Releasing the terminal handle is what ends the output stream on
        // the platform where child exit alone does not (a pseudo-console
        // holds its pipe open until the handle closes).
        self.pty = None;

        // Readers joined, and the session's byte accounting collected from
        // the reader's own equation.
        let mut bytes_read = 0;
        if let Some(reader) = self.reader.take() {
            match tokio::time::timeout(READER_JOIN_LIMIT, reader).await {
                Ok(Ok(report)) => bytes_read = report.stats.bytes_in,
                Ok(Err(_)) => tracing::error!("the reader task panicked"),
                Err(_) => tracing::error!("the reader did not end after the terminal closed"),
            }
        }
        // The pumps carry nothing a closing session still needs; they end
        // when their channels close, and aborting covers the timeout case
        // above.
        if let Some(pump) = self.pump.take() {
            pump.abort();
        }
        if let Some(pump) = self.incident_pump.take() {
            pump.abort();
        }

        // A close that raced an approval announcement still sweeps it.
        self.approvals.cancel_all();

        let closed_at = SystemTime::now();
        let metadata = {
            let mut metadata = self
                .shared
                .metadata
                .lock()
                .expect("the metadata lock is never poisoned: holders do not panic");
            metadata.closed_at = Some(closed_at);
            metadata.exit = exit;
            metadata.bytes_read = bytes_read;
            metadata.bytes_written = self.shared.bytes_written.load(Ordering::Relaxed);
            metadata.clone()
        };

        let payload = LifecycleSessionClosed {
            exit_code: metadata.exit_code(),
            duration_ms: metadata.duration_ms(),
            bytes_read: Some(metadata.bytes_read),
            bytes_written: Some(metadata.bytes_written),
            drained,
        };
        let mut fields = Map::new();
        if let Some(code) = metadata.exit_code() {
            fields.insert("exit_code".into(), json!(code));
        }
        if let Some(drained) = drained {
            fields.insert("drained".into(), json!(drained));
        }
        self.log_record(LogLevel::Info, "lifecycle.session.closed", fields);
        self.publish(EventBody::new(EventKind::LifecycleSessionClosed(payload)));

        // Sealed after the last event, so every subscriber drains to the
        // `closed` announcement and then observes the end of the stream.
        self.sink.seal();

        // The diary's flushed, joined ending — before Closed is
        // observable, because "log closed" is one of the invariants the
        // state stands for.
        if let Some(log) = self.log.take() {
            let _ = tokio::task::spawn_blocking(move || log.close()).await;
        }

        self.state = next;
        let _ = self.state_tx.send(next);

        for reply in self.close_replies.drain(..) {
            let _ = reply.send(Ok(()));
        }
    }
}
