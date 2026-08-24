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
//! known, the stream sealed, the log closed. Each verification is bounded
//! rather than absolute: cleanup the operating system refuses to complete
//! is announced loudly and the close still finishes — a lifecycle wedged
//! over an unkillable process or a stalled disk serves nobody, and what
//! survives such an ending is supervision's to reclaim.

use agent_bridge_adapter_api::{InputStep, ShutdownHint, ShutdownSignal};
use agent_bridge_events::{EventBody, EventKind, LifecycleSessionClosed};
use agent_bridge_pty::Signal;
use bytes::Bytes;
use serde_json::{Map, json};
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};
use tokio::time::Instant;

use crate::actor::{Actor, WriteRequest, exit_status_of, exited_early_payload};
use crate::command::{Reply, SessionCommand};
use crate::error::SessionError;
use crate::logfile::LogLevel;
use crate::state::{Edge, SessionState, transition};

/// How long finalize will wait for the reader task after the terminal
/// handle is dropped. Generous — the stream ends with the terminal — and
/// bounded so a defect in the layer below cannot wedge the close path.
const READER_JOIN_LIMIT: Duration = Duration::from_secs(10);

/// How long finalize will wait for the input-writer task once it has been
/// told to stop. The write possibly in flight is bounded by the terminal
/// layer's own per-write deadline; this larger bound is the backstop for
/// a defect below it, past which the task is aborted and the write it
/// detaches is on the record.
const WRITER_JOIN_LIMIT: Duration = Duration::from_secs(10);

/// How long finalize will wait for the session log to flush shut. Bounded
/// for the same reason logging is never load-bearing anywhere else: a
/// stalled filesystem must not hold `Closed` hostage, so past this the
/// writer thread is abandoned to finish or fail on its own and the loss
/// is on the record.
const LOG_CLOSE_LIMIT: Duration = Duration::from_secs(5);

/// Which way a session leaves through [`Actor::finalize`].
pub(crate) enum CloseRoute {
    /// The table row into `Closed` this route takes.
    Edge(Edge),
    /// A child that ended while `Connecting`. Whether that is "exited
    /// before output" is not decidable at the exit signal: the signal and
    /// the pump's first-output notification ride different tasks, so a
    /// child that wrote and exited in one breath can be observed dead
    /// first. Finalize joins the pump and lets its verdict pick between
    /// `ChildExitedBeforeOutput` and the full `Running → Closing`
    /// routing.
    ConnectingExit,
    /// The terminal failed while `Connecting`, child not known to be gone.
    /// Same deferred output-race classification as [`Self::ConnectingExit`]
    /// — but the paired `pty.error` was already published by the failure
    /// handler, and no child exit is synthesized: the child may have been
    /// alive right up to the terminate below, and the events must not
    /// claim it left on its own.
    ConnectingFailure,
}

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
                if force {
                    // A force-close during a graceful close escalates now
                    // — whether the drain window is already armed or the
                    // hint is still dispatching — but asks first, the
                    // same question the deadline path asks: a child that
                    // already exited voluntarily (its notification may
                    // still be queued behind this very command) drained,
                    // and reporting the force would blame a hint that
                    // worked. `drained: false` keeps its published meaning
                    // — the close escalated before a voluntary exit.
                    self.drain_deadline = None;
                    let exited_in_window = self.pty.as_ref().is_some_and(|pty| !pty.alive());
                    self.finalize(
                        CloseRoute::Edge(Edge::CloseComplete),
                        Some(exited_in_window),
                    )
                    .await;
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
                    self.finalize(CloseRoute::Edge(Edge::CloseComplete), None)
                        .await;
                } else {
                    self.apply_shutdown_hint().await;
                    // An input hint arms the drain window when its task
                    // reports the last keystroke delivered — the window
                    // measures the CLI's chance to exit *after* the hint,
                    // and counting the hint's own writes and pauses
                    // against it escalated slow hints mid-sequence. The
                    // other hint kinds dispatch synchronously above, so
                    // they arm at once. Until the window arms, a
                    // voluntary exit is still observed as drained and a
                    // force-close still escalates; the unarmed span is
                    // itself bounded by the writer's per-write deadlines.
                    if !matches!(self.shutdown_hint, ShutdownHint::Input(_)) {
                        self.drain_deadline = Some(Instant::now() + self.config.stdin_drain);
                    }
                }
            }
        }
    }

    async fn apply_shutdown_hint(&mut self) {
        match self.shutdown_hint.clone() {
            ShutdownHint::Input(steps) => {
                let Some(writer) = &self.writer else { return };
                let input = writer.tx.clone();
                let dispatched = self.loopback.clone();
                // The dispatch gets the same patience the drain window
                // gets, because settle pauses are adapter data with no
                // inherent bound and the graceful close must stay bounded
                // end to end: at most one budget dispatching, one budget
                // draining, then escalation. Each pause is clamped as a
                // second guard so no single adapter value can outlive the
                // budget on its own.
                let budget = self.config.stdin_drain;
                // On its own task so a settle pause never stops the actor
                // from processing the force-close that may arrive mid-hint.
                // Tracked so finalize can cancel it: the task holds a
                // writer-sender clone, and an uncancelled settle would
                // otherwise hold the writer join — and with it the whole
                // close — for as long as the adapter's pauses add up to.
                self.hint_task = Some(tokio::spawn(async move {
                    let dispatch = async {
                        for step in steps {
                            match step {
                                InputStep::Write(text) => {
                                    // Queue admission is not delivery: the
                                    // settle that follows promises the CLI
                                    // reaction time after the keystroke
                                    // arrived, so each write is awaited
                                    // through the writer's own completion
                                    // before any pause starts.
                                    let (reply, delivered) = tokio::sync::oneshot::channel();
                                    let request = WriteRequest {
                                        bytes: Bytes::from(text.into_bytes()),
                                        reply: Some(reply),
                                    };
                                    if input.send(request).await.is_err() {
                                        break;
                                    }
                                    match delivered.await {
                                        Ok(Ok(())) => {}
                                        Ok(Err(error)) => {
                                            tracing::warn!(%error, "shutdown hint write failed");
                                            break;
                                        }
                                        Err(_) => break,
                                    }
                                }
                                InputStep::Settle(pause) => {
                                    tokio::time::sleep(pause.min(budget)).await;
                                }
                            }
                        }
                    };
                    if tokio::time::timeout(budget, dispatch).await.is_err() {
                        tracing::warn!(
                            "shutdown hint exceeded its dispatch budget; arming the drain window"
                        );
                    }
                    // Delivered (or cut short past its budget): the drain
                    // window may start measuring. An awaited send, so a
                    // momentarily full queue delays the arming rather
                    // than losing it.
                    let _ = dispatched.send(SessionCommand::HintDispatched).await;
                }));
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
    pub(crate) async fn finalize(&mut self, route: CloseRoute, drained: Option<bool>) {
        self.drain_deadline = None;
        self.interrupt_pending = false;

        // An edge route is judged before any irreversible teardown, so a
        // pair the table rejects fails loudly while nothing has happened
        // yet. The session still ends: every caller of finalize has
        // committed to that, and stranding a reaped, sealed session in a
        // live-looking state — unreapable, its waiters parked forever —
        // would be the one outcome worse than a wrong edge label. The
        // table check is the alarm, not the brake. The Connecting routes
        // defer the judgment to after the joins below, where the pump's
        // verdict picks the path — and validate the derived edges there.
        if let CloseRoute::Edge(edge) = &route {
            let checked = transition(self.state, *edge).unwrap_or_else(|error| {
                tracing::error!(%error, "finalize took an edge the table rejects; closing anyway");
                SessionState::Closed
            });
            debug_assert_eq!(checked, SessionState::Closed);
        }

        // The hint task holds a writer-sender clone and may be mid-settle;
        // nothing it still had to type matters to a session that is
        // ending, and leaving it running would hold the writer join below
        // hostage to the adapter's pauses.
        if let Some(hint) = self.hint_task.take() {
            hint.abort();
        }

        // Input side down first: stopped, then joined, before the child
        // is touched. The stop flag makes the writer drop everything
        // still queued without typing it at a child that is about to be
        // terminated — dropped requests answer their callers through
        // their reply channels — while the one write possibly in flight
        // is awaited to completion, because an abort would merely detach
        // a blocking write that still owns the terminal, and `Closed`
        // must not become observable over that. The join is bounded as a
        // backstop for a write deadline that never fires.
        if let Some(writer) = self.writer.take() {
            writer.stop.store(true, Ordering::Relaxed);
            drop(writer.tx);
            let mut task = writer.task;
            if tokio::time::timeout(WRITER_JOIN_LIMIT, &mut task)
                .await
                .is_err()
            {
                tracing::error!("the input writer did not end within its limit; aborting it");
                task.abort();
                let _ = task.await;
            }
        }

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
            // layer provides for exactly this question. A non-empty first
            // answer gets a short grace — reaping lags the kill by a
            // scheduler beat — before it is declared a violation; a census
            // that stays non-empty is announced loudly and the close still
            // completes, because holding `Closed` hostage to a process the
            // terminate sequence could not end would wedge the lifecycle
            // over cleanup that is the supervisor's province.
            if let Some(pty) = self.pty.clone() {
                // On the blocking pool: a census can be a whole
                // process-table walk, and the retry loop may take it
                // several times.
                let verdict = tokio::task::spawn_blocking(move || {
                    let mut verdict = pty.contained();
                    for _ in 0..3 {
                        match &verdict {
                            Ok(pids) if !pids.is_empty() => {
                                std::thread::sleep(Duration::from_millis(50));
                                verdict = pty.contained();
                            }
                            _ => break,
                        }
                    }
                    verdict
                })
                .await
                .unwrap_or_else(|_| Err(std::io::Error::other("the census task panicked")));
                match verdict {
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

        // Releasing the terminal handle is what ends the output stream on
        // the platform where child exit alone does not (a pseudo-console
        // holds its pipe open until the handle closes).
        self.pty = None;

        // Readers joined, and the session's byte accounting collected from
        // the reader's own equation. `None` until a report is in hand: a
        // reader that panicked or outlived its join forfeits its
        // accounting, and an unknown count must stay an absence rather
        // than harden into a measured zero.
        let mut bytes_read = None;
        if let Some(mut reader) = self.reader.take() {
            match tokio::time::timeout(READER_JOIN_LIMIT, &mut reader).await {
                Ok(Ok(report)) => bytes_read = Some(report.stats.bytes_in),
                Ok(Err(_)) => tracing::error!("the reader task panicked"),
                Err(_) => {
                    // A reader that outlives its bound is ended, not
                    // detached: Closed must not become observable with a
                    // reader still running behind it. Its accounting is
                    // forfeit and the loss is on the record.
                    tracing::error!(
                        "the reader did not end after the terminal closed; aborting it"
                    );
                    reader.abort();
                    let _ = reader.await;
                }
            }
        }
        // The pump ends when the reader drops the text channel. Its
        // verdict is read from the shared flag rather than its return
        // value, because the pump can be parked enqueueing its signal into
        // the very command queue finalize no longer drains — the flag is
        // set before that send, so aborting a parked pump loses nothing.
        if let Some(mut pump) = self.pump.take()
            && tokio::time::timeout(READER_JOIN_LIMIT, &mut pump)
                .await
                .is_err()
        {
            pump.abort();
            let _ = pump.await;
        }
        let saw_output = self
            .pump_saw_output
            .load(std::sync::atomic::Ordering::Relaxed);
        // The incident pump carries nothing a closing session still needs.
        if let Some(pump) = self.incident_pump.take() {
            pump.abort();
        }

        // A Connecting ending is classified here, with the pump's verdict
        // in hand: a child that produced visible output before the signals
        // landed still ran — the ladder reports Running and routes the
        // ending as a post-Running failure rather than pretending the
        // output never existed. A child that painted nothing takes the
        // exited-before-output row. Only the exit route announces a child
        // exit; a terminal failure already published its own fault, and
        // the child may have been alive until the terminate above.
        if matches!(
            route,
            CloseRoute::ConnectingExit | CloseRoute::ConnectingFailure
        ) {
            if saw_output {
                // The session ran before it ended, and subscribers learn
                // those facts in that order: Running first, then the
                // fault, then the failure routing.
                let _ = self.apply_edge(Edge::FirstOutput);
                if matches!(route, CloseRoute::ConnectingExit) {
                    self.publish(EventBody::new(EventKind::PtyError(exited_early_payload())));
                }
                let _ = self.apply_edge(Edge::PostRunningFailure);
            } else if matches!(route, CloseRoute::ConnectingExit) {
                self.publish(EventBody::new(EventKind::PtyError(exited_early_payload())));
            }
            // The deferred judgment, completed: the derived final row is
            // held to the table exactly as an Edge route's is up top —
            // same alarm, same refusal to strand the session.
            let final_edge = if saw_output {
                Edge::CloseComplete
            } else {
                Edge::ChildExitedBeforeOutput
            };
            let checked = transition(self.state, final_edge).unwrap_or_else(|error| {
                tracing::error!(
                    %error,
                    "finalize derived an edge the table rejects; closing anyway"
                );
                SessionState::Closed
            });
            debug_assert_eq!(checked, SessionState::Closed);
        }

        // A close that raced an approval announcement still sweeps it.
        self.approvals.cancel_all();

        // The final record is assembled locally and published into the
        // shared snapshot only at the flip below: `closed_at` documents
        // itself as "when the session reached Closed", so it must not be
        // readable through a handle while the state still says otherwise.
        let closed_at = SystemTime::now();
        let metadata = {
            let mut metadata = self
                .shared
                .metadata
                .lock()
                .expect("the metadata lock is never poisoned: holders do not panic")
                .clone();
            metadata.closed_at = Some(closed_at);
            if saw_output && metadata.started_at.is_none() {
                // The child spoke, but its exit outran the first-output
                // signal; the close instant is the latest honest reading.
                metadata.started_at = Some(closed_at);
            }
            metadata.exit = exit;
            metadata.bytes_read = bytes_read.unwrap_or(0);
            metadata.bytes_written = self.shared.bytes_written.load(Ordering::Relaxed);
            metadata
        };

        // A session that failed before launch has no byte counts — the
        // payload contract's words. No terminal stack ever stood, so zero
        // would be a measurement that was never taken. A read count the
        // reader never reported is equally absent, not zero.
        let launch_failed = matches!(route, CloseRoute::Edge(Edge::LaunchFailed));
        let payload = LifecycleSessionClosed {
            exit_code: metadata.exit_code(),
            duration_ms: metadata.duration_ms(),
            bytes_read: if launch_failed { None } else { bytes_read },
            bytes_written: (!launch_failed).then_some(metadata.bytes_written),
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
        // state stands for — under a bound, because logging is never
        // load-bearing and a stalled disk must not wedge the lifecycle.
        if let Some(log) = self.log.take() {
            match tokio::time::timeout(
                LOG_CLOSE_LIMIT,
                tokio::task::spawn_blocking(move || log.close()),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(_)) => tracing::error!("the log-close task panicked"),
                Err(_) => tracing::error!(
                    "the session log did not close within its limit; abandoning its writer"
                ),
            }
        }

        // Every route ends here: the Edge routes were checked against the
        // table up top, the Connecting routes validated their derived row
        // above — Closed is the only place finalize can leave a session.
        // The record lands first, so an observer who reads Closed always
        // finds the finished record behind it.
        *self
            .shared
            .metadata
            .lock()
            .expect("the metadata lock is never poisoned: holders do not panic") = metadata;
        self.state = SessionState::Closed;
        let _ = self.state_tx.send(SessionState::Closed);

        for reply in self.close_replies.drain(..) {
            let _ = reply.send(Ok(()));
        }
    }
}
