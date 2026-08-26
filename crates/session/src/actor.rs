//! The session actor: one task that owns all of a session's mutable state.
//!
//! Commands from callers, signals from the stream, and announcements from
//! approval sources are all senders into one bounded queue, so every state
//! transition happens on exactly one
//! task: the transition table never races itself, the multi-pending
//! approval set is mutated from one place, and FIFO input ordering is the
//! queue's order rather than a locking discipline.
//!
//! The actor is deliberately `select!`-free. Its one loop receives from the
//! queue, bounded by the next deadline it owes anyone — the close path's
//! drain window, or the liveness poll. The poll exists because child exit
//! is not observable from the stream alone on every platform: a POSIX
//! terminal ends its stream when the child exits, but a pseudo-console's
//! stream ends only when the handle is dropped, so "is the child alive" is
//! asked of the process, coarsely, rather than inferred from bytes.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use agent_bridge_adapter_api::{LaunchSpec, ShutdownHint};
use agent_bridge_events::{
    ApprovalPrompt, EventBody, EventKind, LifecycleSessionAwaitingApproval,
    LifecycleSessionClosing, LifecycleSessionConnecting, LifecycleSessionCreated,
    LifecycleSessionInterrupted, LifecycleSessionLaunching, LifecycleSessionRunning, PtyErrorCode,
    PtyErrorPayload,
};
use agent_bridge_pty::{Dimensions, ExitStatus, Pty, PtyError, SpawnSpec, Spawned};
use agent_bridge_stream::{
    EncodingIncident, PtyChunkSource, ReaderConfig, ReaderOutputs, ReaderReport, StreamReader,
    Stripper,
};
use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::approval::{
    ApprovalDecision, ApprovalId, ApprovalIdentity, ApprovalResolution, ApprovalSource,
    PendingApproval, PendingApprovals,
};
use crate::close::CloseRoute;
use crate::command::{Reply, SessionCommand};
use crate::error::SessionError;
use crate::id::{SessionId, SubscriberId};
use crate::logfile::{LogLevel, SessionLog};
use crate::metadata::SessionMetadata;
use crate::state::{Edge, SessionState, transition};
use crate::validate_dimensions;

/// Where a session's events go, and how its stream is ended.
///
/// The seam that keeps the dependency direction acyclic: this crate sits
/// below `core`, which owns the bus, so the bus reaches the actor as a
/// capability handed in at spawn rather than as a dependency. `core`
/// implements it over the bus's `Publisher` and `EventBus::seal_session`;
/// tests implement it over a vector.
pub trait EventSink: Send + 'static {
    /// Complete and fan out one event, returning its stamped `seq`.
    fn publish(&self, body: EventBody) -> Result<u64, SinkSealed>;
    /// End the stream: no further publishes, no new subscribers. Called
    /// exactly once, after the session's last event.
    fn seal(&self);
}

/// The sink refused a publish because the stream has already ended.
///
/// The actor treats this as a bug worth a loud log — sealing is the last
/// thing the actor itself does — never as something to surface to a caller.
#[derive(Debug, thiserror::Error)]
#[error("the event sink is sealed")]
pub struct SinkSealed;

/// How a session is tuned. Populated from configuration by the wiring
/// layer; this crate knows the defaults.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Where the per-session log lives: `<log_dir>/sessions/<id>.log`.
    pub log_dir: PathBuf,
    /// How long a non-forced close waits for a voluntary exit after the
    /// shutdown hint (`stdin_drain_seconds`, default 30).
    pub stdin_drain: Duration,
    /// The grace the terminal layer's termination sequence gets between
    /// its polite and forceful halves.
    pub terminate_grace: Duration,
    /// How often the actor asks whether the child is still alive. Coarse
    /// on purpose — this is exit *detection*, not supervision.
    pub liveness_poll: Duration,
    /// The command queue's bound. A full queue backpressures callers.
    /// Must be at least 1 and within the async runtime's channel ceiling;
    /// [`spawn_session`] refuses anything else at the construction site.
    pub command_capacity: usize,
    /// Mirror event payloads into the session log (`logs.mirror_payloads`,
    /// default off — metadata is always mirrored, content is opt-in).
    pub mirror_payloads: bool,
}

impl SessionConfig {
    /// Panic on a configuration no session can run under. Deliberately an
    /// assertion, not a `Result`: these are deployment mistakes, and the
    /// refusal belongs at a construction site — [`spawn_session`] runs it
    /// for direct consumers, and the registry runs it once at its own
    /// construction, *before* any lock exists to poison, so a bad value
    /// fails the process at startup rather than mid-create while the
    /// session map is held.
    ///
    /// # Panics
    ///
    /// When `command_capacity` is zero or past the async runtime's channel
    /// ceiling (the runtime would panic far from the misconfigured value);
    /// when `liveness_poll` is zero (every wake deadline would read as
    /// already elapsed, spinning the actor's loop); or when any
    /// deadline-bearing duration exceeds the one-day ceiling (each lands
    /// in instant-plus-duration arithmetic somewhere, and an absurd value
    /// panics that arithmetic far from the setting).
    pub fn assert_valid(&self) {
        assert!(
            self.command_capacity >= 1,
            "command_capacity must be at least 1"
        );
        assert!(
            self.command_capacity <= usize::MAX >> 3,
            "command_capacity exceeds the runtime's channel-capacity ceiling"
        );
        assert!(
            !self.liveness_poll.is_zero(),
            "liveness_poll must be nonzero"
        );
        for (name, value) in [
            ("stdin_drain", self.stdin_drain),
            ("terminate_grace", self.terminate_grace),
            ("liveness_poll", self.liveness_poll),
        ] {
            assert!(
                value <= DEADLINE_CEILING,
                "{name} exceeds the deadline ceiling of one day"
            );
        }
    }

    /// The contract defaults, logging under `log_dir`.
    pub fn new(log_dir: impl Into<PathBuf>) -> Self {
        Self {
            log_dir: log_dir.into(),
            stdin_drain: Duration::from_secs(30),
            terminate_grace: Duration::from_secs(5),
            liveness_poll: Duration::from_millis(200),
            command_capacity: 64,
            mirror_payloads: false,
        }
    }
}

/// Everything a session needs to exist: identity, what to launch, how to
/// stop it, and tuning.
pub struct SessionSpec {
    /// The registry-assigned identity.
    pub session_id: SessionId,
    /// The adapter's registry name, e.g. `"fixture"`.
    pub adapter: String,
    /// The adapter's launch description; the actor converts it into the
    /// terminal layer's spawn request.
    pub launch: LaunchSpec,
    /// The adapter's preferred exit request, applied first on a non-forced
    /// close.
    pub shutdown_hint: ShutdownHint,
    /// The creating peer, as the initial writer owner (state only in v1).
    pub creator: Option<SubscriberId>,
    /// Tuning.
    pub config: SessionConfig,
}

/// The read-side snapshot the handle serves without asking the actor.
pub(crate) struct Shared {
    pub(crate) session_id: SessionId,
    pub(crate) adapter: String,
    pub(crate) metadata: std::sync::Mutex<SessionMetadata>,
    pub(crate) writer: std::sync::Mutex<Option<SubscriberId>>,
    pub(crate) bytes_written: AtomicU64,
    /// When the session came to exist, on the monotonic clock — the
    /// origin the lifetime is measured from, because subtracting wall
    /// timestamps hands a stepped clock the power to inflate or erase a
    /// duration that elapsed normally.
    pub(crate) created_monotonic: std::time::Instant,
    /// The monotonic close stamp, set once at the `Closed` flip. The
    /// metadata's wall-clock `closed_at` is the record a caller reads;
    /// this is its monotonic companion for in-process ordering, which a
    /// stepped wall clock cannot reorder.
    pub(crate) closed_monotonic: std::sync::OnceLock<std::time::Instant>,
}

/// What `spawn_session` hands back: the handle, and the launch outcome.
///
/// They are separate because the create flow needs both halves at
/// different moments: the registry inserts the handle before the child
/// exists (so a concurrent `session.create` sees a consistent registry),
/// then awaits `launch` to answer the caller — `Ok` once the session
/// reaches `Connecting`, or the typed launch failure after the state
/// machine has already walked `Launching → Closed` with its paired error
/// events.
pub struct SpawnedSession {
    /// The cheap-clone control surface.
    pub handle: SessionHandle,
    /// Resolves when the session is standing (or has failed to stand).
    pub launch: oneshot::Receiver<Result<(), SessionError>>,
}

/// Start a session's actor task.
///
/// Validates the requested geometry first (a refusal the caller can
/// read, never a silent degradation) and spawns the actor; everything
/// that can block — the log open, the terminal allocation — happens on
/// the actor's side of the seam, so this call itself never touches the
/// disk and is safe to make while holding a lock. Must be called within
/// a tokio runtime.
///
/// A session must be *closed*; dropping [`SpawnedSession`] and every
/// [`SessionHandle`] clone does not end it — the actor keeps running and
/// the child keeps living, with nothing left holding a way to reach
/// them. The registry always retains a handle for exactly this reason;
/// a direct consumer of this API takes on the same obligation.
pub fn spawn_session(
    spec: SessionSpec,
    sink: Box<dyn EventSink>,
) -> Result<SpawnedSession, SessionError> {
    let dimensions = spec
        .launch
        .dimensions
        .map(|(cols, rows)| validate_dimensions(cols, rows))
        .transpose()?;

    // Refused loudly at the construction site, the same stance the bus
    // takes on its queue bound — see [`SessionConfig::assert_valid`] for
    // the catalog. Callers who hold locks across spawn validate earlier,
    // at their own construction; the registry does.
    spec.config.assert_valid();
    let (commands_tx, commands_rx) = mpsc::channel(spec.config.command_capacity);
    let (state_tx, state_rx) = watch::channel(SessionState::Created);
    let shared = Arc::new(Shared {
        session_id: spec.session_id,
        adapter: spec.adapter.clone(),
        metadata: std::sync::Mutex::new(SessionMetadata {
            adapter: spec.adapter.clone(),
            dimensions: dimensions.unwrap_or_default(),
            created_at: SystemTime::now(),
            started_at: None,
            closed_at: None,
            exit: None,
            bytes_read: None,
            bytes_written: 0,
            duration: None,
        }),
        writer: std::sync::Mutex::new(spec.creator),
        bytes_written: AtomicU64::new(0),
        created_monotonic: std::time::Instant::now(),
        closed_monotonic: std::sync::OnceLock::new(),
    });
    let (launch_tx, launch_rx) = oneshot::channel();

    let actor = Actor {
        launch: spec.launch,
        dimensions,
        shutdown_hint: spec.shutdown_hint,
        config: spec.config,
        shared: Arc::clone(&shared),
        sink,
        log: None,
        state: SessionState::Created,
        state_tx,
        commands: commands_rx,
        loopback: commands_tx.clone(),
        pty: None,
        writer: None,
        reader: None,
        pump: None,
        incident_pump: None,
        hint_task: None,
        approvals: PendingApprovals::default(),
        terminal_fault_cause: None,
        pump_saw_output: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pump_first_output: Arc::new(std::sync::OnceLock::new()),
        terminal_failed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        terminal_fault: Arc::new(std::sync::Mutex::new(None)),
        stream_ended: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        interrupt_pending: false,
        drain_deadline: None,
        judge_drain_at_terminate: false,
        next_liveness: Instant::now(),
        close_replies: Vec::new(),
    };
    tokio::spawn(actor.run(launch_tx));

    Ok(SpawnedSession {
        handle: SessionHandle {
            shared,
            commands: commands_tx,
            state: state_rx,
        },
        launch: launch_rx,
    })
}

/// The control surface for one live session. There is deliberately no
/// `stream_events` here: subscription is the Core-owned bus's contract —
/// a session emits, the bus serves its readers.
///
/// Cheap to clone; all clones command the same actor. Reads (`state`,
/// `metadata`, `writer`) are served from a shared snapshot the actor keeps
/// current, so observing a session never queues behind mutating it.
#[derive(Clone)]
pub struct SessionHandle {
    shared: Arc<Shared>,
    commands: mpsc::Sender<SessionCommand>,
    state: watch::Receiver<SessionState>,
}

// Hand-written: shape and identity only. The derive would
// be safe today and a trap tomorrow — the first content-bearing field added
// to the handle's reach would print transitively.
impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("session_id", &self.shared.session_id)
            .field("adapter", &self.shared.adapter)
            .field("state", &*self.state.borrow())
            .finish_non_exhaustive()
    }
}

impl SessionHandle {
    /// UUIDv4, assigned by the registry at create.
    pub fn session_id(&self) -> SessionId {
        self.shared.session_id
    }

    /// The adapter hosting this session — the registry key.
    pub fn adapter(&self) -> &str {
        &self.shared.adapter
    }

    /// Where the session is in its lifecycle, as a read-only snapshot.
    pub fn state(&self) -> SessionState {
        *self.state.borrow()
    }

    /// When the session reached `Closed`, on the monotonic clock — the
    /// companion to the metadata's wall-clock `closed_at`, for in-process
    /// ordering that clock steps cannot reorder. `None` while the session
    /// lives.
    pub fn closed_instant(&self) -> Option<std::time::Instant> {
        self.shared.closed_monotonic.get().copied()
    }

    /// The session's descriptive record: adapter, geometry, timestamps,
    /// exit, byte counts.
    ///
    /// `bytes_written` is live — read from the input writer's own counter
    /// while the session runs. `bytes_read` settles at close: the reader
    /// owns its accounting until its final report, so a live read shows
    /// `None` and the closed record shows the total — or keeps the
    /// absence, for an accounting the reader forfeited.
    pub fn metadata(&self) -> SessionMetadata {
        let mut metadata = self
            .shared
            .metadata
            .lock()
            .expect("the metadata lock is never poisoned: holders do not panic")
            .clone();
        if metadata.closed_at.is_none() {
            metadata.bytes_written = self.shared.bytes_written.load(Ordering::Relaxed);
        }
        metadata
    }

    /// The current writer owner, if any (state only in v1).
    pub fn writer(&self) -> Option<SubscriberId> {
        self.shared
            .writer
            .lock()
            .expect("the writer lock is never poisoned: holders do not panic")
            .clone()
    }

    /// Forward input bytes to the CLI (FIFO — the actor queue is the
    /// order). Input-only: approval resolution is
    /// [`SessionHandle::resolve_approval`], never an input echo.
    pub async fn send(&self, input: Bytes) -> Result<(), SessionError> {
        self.request(|reply| SessionCommand::Send { input, reply })
            .await
    }

    /// Resolve one pending approval — a dedicated method, never a `send`
    /// echo. `approval_id` must match an entry in the pending set; a
    /// stale or unknown id is rejected with
    /// [`SessionError::ApprovalIdMismatch`] and every pending prompt stays
    /// pending. In `Running` the same stale verdict answers, because the
    /// set is empty there and any id names an approval already resolved
    /// or withdrawn — the reading the withdrawal event promises.
    /// Resolving in any other state outside `AwaitingApproval` is
    /// [`SessionError::InvalidStateForOperation`].
    pub async fn resolve_approval(
        &self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), SessionError> {
        self.request(|reply| SessionCommand::ResolveApproval {
            id: approval_id,
            decision,
            reply,
        })
        .await
    }

    /// Interrupt what the CLI is doing without ending the session: the
    /// control byte, written into the terminal (which delivery its CLI
    /// honours is the adapter's declaration). Cancels **every** pending
    /// approval — from `AwaitingApproval` the emptied set returns the
    /// state to `Running` at once, so it never claims approvals that no
    /// longer exist — and the `Interrupted` state lands on the CLI's
    /// acknowledgement signal, the evidence the CLI actually stopped.
    pub async fn interrupt(&self) -> Result<(), SessionError> {
        self.request(|reply| SessionCommand::Interrupt { reply })
            .await
    }

    /// Change the terminal geometry. The same bound as create applies —
    /// out of range is refused before the terminal hears about it.
    pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), SessionError> {
        let dimensions = validate_dimensions(cols, rows)?;
        self.request(|reply| SessionCommand::Resize { dimensions, reply })
            .await
    }

    /// Close the session. `force = false` applies the adapter's
    /// `ShutdownHint`, waits the drain window for a voluntary exit, then
    /// escalates to the terminal layer's termination sequence; `force =
    /// true` skips the hint. Resolves once `Closed` is reached and the
    /// cleanup invariants have been verified — verification is bounded:
    /// cleanup the operating system refuses to complete (a containment
    /// census that stays populated, a reader or log writer outliving its
    /// join limit) is announced loudly in the runtime log and the close
    /// still completes, because wedging the lifecycle over it would help
    /// nobody; reclaiming whatever survives belongs to supervision.
    ///
    /// Close paths race, and a second close arriving late is a race
    /// resolved rather than an error: closing a session that is already
    /// `Closed` — even one whose actor has fully wound down — succeeds.
    pub async fn close(&self, force: bool) -> Result<(), SessionError> {
        if self.state() == SessionState::Closed {
            return Ok(());
        }
        // A second graceful close during `Closing` is coalesced onto the
        // state watch instead of parking a reply with the actor: its
        // contract — resolved at `Closed` — is exactly what the watch
        // reports, and adding nothing, it should cost nothing. Parked
        // actor-side, each such call would hold a reply channel for as
        // long as the drain window runs, letting a caller accumulate
        // waiters past every queue bound. A force still travels as a
        // command: it changes the close, and the actor must hear it.
        if !force && self.state() == SessionState::Closing {
            return match self.wait_closed().await {
                SessionState::Closed => Ok(()),
                // The watch ended on a state that is not `Closed`: the
                // actor is gone without finishing its close, and none of
                // the invariants a graceful Ok would claim were reached.
                _ => Err(SessionError::SessionClosed),
            };
        }
        match self
            .request(|reply| SessionCommand::Close { force, reply })
            .await
        {
            // The mailbox can close before the state flips — finalize
            // seals its inbox and may still spend a bounded while in
            // `Closing` — so a raced close waits for the watch's verdict
            // instead of sampling the state mid-finalize: `Ok` for a
            // close that finished, the error kept for an actor that died
            // in some other state.
            Err(SessionError::SessionClosed) => match self.wait_closed().await {
                SessionState::Closed => Ok(()),
                _ => Err(SessionError::SessionClosed),
            },
            outcome => outcome,
        }
    }

    /// Source-facing: announce a pending approval and receive its id and
    /// the channel its resolution arrives on.
    ///
    /// The Phase-2 hook listener and screen matcher are the real callers;
    /// until they land, the conformance driver and the tests stand in.
    /// The identity is the source's contract in type form: a hook carries
    /// the CLI's `tool_use_id` verbatim, and a screen detection carries
    /// nothing — the actor mints its UUIDv4 and returns it here. The
    /// returned receiver yields the caller's decision, or
    /// [`ApprovalResolution::Cancelled`] when an interrupt or close swept
    /// the set.
    pub async fn announce_approval(
        &self,
        identity: ApprovalIdentity,
        prompt: ApprovalPrompt,
    ) -> Result<(ApprovalId, oneshot::Receiver<ApprovalResolution>), SessionError> {
        self.request(|reply| SessionCommand::ApprovalDetected {
            identity,
            prompt,
            reply,
        })
        .await
    }

    /// Source-facing: the CLI acknowledged a forwarded interrupt. Drives
    /// the `→ Interrupted` edge; spurious signals are ignored.
    pub async fn interrupt_acknowledged(&self) {
        let _ = self
            .commands
            .send(SessionCommand::InterruptAcknowledged)
            .await;
    }

    /// Source-facing: the CLI resumed after an interrupt. Drives the
    /// `Interrupted → Running` edge; spurious signals are ignored.
    pub async fn resumed(&self) {
        let _ = self.commands.send(SessionCommand::Resumed).await;
    }

    /// The transport peer dropped: clear writer ownership — state only;
    /// nothing re-acquires it in v1.
    pub async fn transport_dropped(&self) {
        if self
            .commands
            .send(SessionCommand::TransportDropped)
            .await
            .is_err()
        {
            // The actor is gone or its mailbox already closed: the
            // contract is cleared-on-drop, and the ownership lives in
            // shared state precisely so the clearing can outlive the
            // actor's ability to do it.
            *self
                .shared
                .writer
                .lock()
                .expect("the writer lock is never poisoned: holders do not panic") = None;
        }
    }

    /// Resolves once the session reaches `Closed` — or once its actor is
    /// gone, whichever comes first. The returned state says which: `Closed`
    /// is the normal ending with every cleanup invariant verified; anything
    /// else means the actor ended without finalizing — a defect — and the
    /// caller decides what that costs it, the way the registry's watcher
    /// seals the abandoned stream. Waiting forever on a dead actor is the
    /// one behavior this method refuses to have.
    pub async fn wait_closed(&self) -> SessionState {
        let mut state = self.state.clone();
        loop {
            let current = *state.borrow_and_update();
            if current == SessionState::Closed {
                return current;
            }
            if state.changed().await.is_err() {
                return current;
            }
        }
    }

    async fn request<T>(
        &self,
        build: impl FnOnce(Reply<T>) -> SessionCommand,
    ) -> Result<T, SessionError> {
        let (reply, outcome) = oneshot::channel();
        self.commands
            .send(build(reply))
            .await
            .map_err(|_| SessionError::SessionClosed)?;
        outcome.await.map_err(|_| SessionError::SessionClosed)?
    }
}

/// A queued input write on its way to the terminal.
pub(crate) struct WriteRequest {
    pub(crate) bytes: Bytes,
    /// `None` when nobody awaits the delivery — a failure is then logged
    /// rather than returned. The shutdown hint *does* await its writes
    /// (each settle must measure from the keystroke's delivery), so it
    /// passes `Some` like any caller; the option is about whether an
    /// answer is wanted, not about who originated the write.
    pub(crate) reply: Option<Reply<()>>,
}

/// The input-writer task: one queue, sequential blocking writes.
///
/// Input goes through its own task rather than the actor because a
/// terminal write can block up to the spec's deadline, and an actor stuck
/// in a write could not process the interrupt that exists to cut exactly
/// that situation short. FIFO survives: one queue, drained in order.
pub(crate) struct InputWriter {
    pub(crate) tx: mpsc::Sender<WriteRequest>,
    pub(crate) task: JoinHandle<()>,
    /// Finalize's teardown signal. Once set, the loop stops writing and
    /// drains its queue by dropping it — an abort could not do this,
    /// because aborting the task detaches a blocking write already in
    /// flight rather than ending it, and `Closed` must not become
    /// observable while a write still owns the terminal.
    pub(crate) stop: Arc<std::sync::atomic::AtomicBool>,
}

fn spawn_input_writer(
    pty: Arc<dyn Pty>,
    shared: Arc<Shared>,
    loopback: mpsc::Sender<SessionCommand>,
    terminal_failed: Arc<std::sync::atomic::AtomicBool>,
    terminal_fault: Arc<std::sync::Mutex<Option<String>>>,
) -> InputWriter {
    let (tx, mut rx) = mpsc::channel::<WriteRequest>(16);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let task = tokio::spawn(async move {
        while let Some(WriteRequest { bytes, reply }) = rx.recv().await {
            if stopping.load(Ordering::Relaxed) {
                // Teardown: nothing still queued gets typed at a session
                // that is ending. The dropped request answers its caller
                // through the closed reply channel.
                continue;
            }
            let pty = Arc::clone(&pty);
            let len = bytes.len() as u64;
            let outcome = tokio::task::spawn_blocking(move || pty.write(&bytes)).await;
            match outcome {
                Ok(Ok(())) => {
                    shared.bytes_written.fetch_add(len, Ordering::Relaxed);
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(()));
                    }
                }
                Ok(Err(error)) => {
                    // A deadline that expired mid-buffer still delivered
                    // its prefix: `StdinBlocked` carries only the
                    // unwritten suffix, and what the child received is
                    // real input the accounting must not lose.
                    if let PtyError::StdinBlocked { unwritten } = &error {
                        let delivered = len.saturating_sub(unwritten.len() as u64);
                        if delivered > 0 {
                            shared.bytes_written.fetch_add(delivered, Ordering::Relaxed);
                        }
                    }
                    // A failed terminal cannot service anything after this
                    // write either: the caller gets the typed cause, and
                    // the actor is told so the session routes to its
                    // failure close instead of staying live on a dead
                    // terminal. A blocked write stays recoverable and an
                    // exited child is the liveness poll's finding.
                    let cause =
                        matches!(error, PtyError::TerminalFailed(_)).then(|| error.to_string());
                    if let Some(reply) = reply {
                        let _ = reply.send(Err(SessionError::Pty(error)));
                    } else {
                        tracing::warn!(%error, "runtime-originated input write failed");
                    }
                    if let Some(cause) = cause {
                        // Cause first, then the flag (`Release`, paired
                        // with the loop's `Acquire` swap, publishes the
                        // slot), then the command: the command is the
                        // prompt path, the flag-and-slot pair the
                        // guaranteed one, and both carry the OS cause.
                        // Then stop: a failed terminal serves no further
                        // writes, and the dropped queue answers every
                        // waiting caller through its closed reply channel.
                        *terminal_fault
                            .lock()
                            .expect("a fault-slot lock holder panicked") = Some(cause.clone());
                        terminal_failed.store(true, Ordering::Release);
                        let _ = loopback.try_send(SessionCommand::TerminalFailure(Some(cause)));
                        return;
                    }
                }
                Err(_) => {
                    if let Some(reply) = reply {
                        let _ = reply.send(Err(SessionError::Pty(PtyError::TerminalFailed(
                            std::io::Error::other("the input write task panicked"),
                        ))));
                    }
                    // A panicked write left the terminal in an unknown
                    // state; treated as the fatal case above.
                    let cause = "the input write task panicked".to_string();
                    *terminal_fault
                        .lock()
                        .expect("a fault-slot lock holder panicked") = Some(cause.clone());
                    terminal_failed.store(true, Ordering::Release);
                    let _ = loopback.try_send(SessionCommand::TerminalFailure(Some(cause)));
                    return;
                }
            }
        }
    });
    InputWriter { tx, task, stop }
}

pub(crate) struct Actor {
    pub(crate) launch: LaunchSpec,
    pub(crate) dimensions: Option<Dimensions>,
    pub(crate) shutdown_hint: ShutdownHint,
    pub(crate) config: SessionConfig,
    pub(crate) shared: Arc<Shared>,
    pub(crate) sink: Box<dyn EventSink>,
    pub(crate) log: Option<SessionLog>,
    pub(crate) state: SessionState,
    pub(crate) state_tx: watch::Sender<SessionState>,
    pub(crate) commands: mpsc::Receiver<SessionCommand>,
    pub(crate) loopback: mpsc::Sender<SessionCommand>,
    pub(crate) pty: Option<Arc<dyn Pty>>,
    pub(crate) writer: Option<InputWriter>,
    pub(crate) reader: Option<JoinHandle<ReaderReport>>,
    pub(crate) pump: Option<JoinHandle<bool>>,
    pub(crate) incident_pump: Option<JoinHandle<()>>,
    pub(crate) hint_task: Option<JoinHandle<()>>,
    pub(crate) approvals: PendingApprovals,
    /// The pump's verdict, readable without its join: set before the pump
    /// parks anywhere, so a finalize that must abort a wedged pump still
    /// learns whether visible output ever existed.
    pub(crate) pump_saw_output: Arc<std::sync::atomic::AtomicBool>,
    /// When the pump first saw visible output — stamped at the
    /// observation, before the flag, so `started_at` records when the
    /// child actually spoke rather than when the signal was processed:
    /// in the output/exit race the close instant could otherwise stand
    /// in, a reading taken after termination and every join.
    pub(crate) pump_first_output: Arc<std::sync::OnceLock<SystemTime>>,
    /// The non-droppable half of terminal-failure delivery. The writer and
    /// reader tasks announce a fatal failure with a `TerminalFailure`
    /// command for promptness, but a full queue may refuse it — and unlike
    /// an exited child, a dead terminal over a live child leaves nothing
    /// for the liveness poll to notice. The flag is set before the send is
    /// tried and read on every loop pass, so the signal survives the
    /// refusal.
    pub(crate) terminal_failed: Arc<std::sync::atomic::AtomicBool>,
    /// The cause half of that signal: producers stash the OS error here
    /// before setting the flag, so the guaranteed path publishes the same
    /// diagnostics the droppable command carries — a refused send costs
    /// promptness, never the cause.
    pub(crate) terminal_fault: Arc<std::sync::Mutex<Option<String>>>,
    /// The non-droppable half of stream-end delivery, for the same reason:
    /// a stream that ended over a child still alive (closed descriptors, a
    /// failed consumer) is invisible to the liveness poll, so a refused
    /// `StreamEnded` command would leave a live actor with no reader
    /// forever.
    pub(crate) stream_ended: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) interrupt_pending: bool,
    /// The cause of a terminal failure on its way to a Connecting-route
    /// finalize, which publishes the fault inside its classification —
    /// stashed here because the route enum carries no payload.
    pub(crate) terminal_fault_cause: Option<String>,
    pub(crate) drain_deadline: Option<Instant>,
    /// Set at drain-deadline expiry so finalize samples the drain
    /// verdict at the escalation boundary rather than at the wake:
    /// between the two stands the writer teardown, and a child that
    /// exits voluntarily in that span was never escalated at — which
    /// the payload contract words as `drained: true`.
    pub(crate) judge_drain_at_terminate: bool,
    pub(crate) next_liveness: Instant,
    pub(crate) close_replies: Vec<Reply<()>>,
}

/// How long the actor waits for its log to open before launching without
/// one. Logging is never load-bearing; a hung mount forfeits the diary,
/// not the session.
const LOG_OPEN_LIMIT: Duration = Duration::from_secs(5);

/// The ceiling every deadline-bearing config duration is held to at the
/// construction site. See the refusal in [`spawn_session`].
const DEADLINE_CEILING: Duration = Duration::from_secs(86_400);

/// How long the terminal may take to stand up before the create fails
/// instead of wedging: a healthy spawn answers in milliseconds, and past
/// this bound a stalled filesystem is the operating story. The abandoned
/// spawn ends its own late child on the detached thread.
const LAUNCH_LIMIT: Duration = Duration::from_secs(30);

impl Actor {
    async fn run(mut self, launch_outcome: oneshot::Sender<Result<(), SessionError>>) {
        // The log opens here, off the create path's lock and onto the
        // blocking pool: directory creation and a file open are disk work
        // an async worker must not perform inline, and never load-bearing —
        // a session whose diary cannot open still runs, and says so where
        // the runtime log can see it.
        let log_dir = self.config.log_dir.clone();
        let session_id = self.shared.session_id;
        self.log = match tokio::time::timeout(
            LOG_OPEN_LIMIT,
            crate::detach::detached("session-log-open", move || {
                SessionLog::open(&log_dir, &session_id)
            }),
        )
        .await
        {
            Ok(Ok(Ok(log))) => Some(log),
            Ok(Ok(Err(error))) => {
                tracing::warn!(session_id = %self.shared.session_id, %error, "session log could not open");
                None
            }
            Ok(Err(_)) => {
                tracing::warn!(session_id = %self.shared.session_id, "the log-open thread died before answering");
                None
            }
            // A hung mount must not hold the create hostage: the session
            // launches without its diary, loudly, and the abandoned open
            // finishes or fails on its own thread.
            Err(_) => {
                tracing::warn!(session_id = %self.shared.session_id, "session log open timed out; launching without a log");
                None
            }
        };

        let mut fields = Map::new();
        fields.insert("adapter".into(), json!(self.shared.adapter));
        self.log_record(LogLevel::Info, "lifecycle.session.created", fields);
        self.publish(EventBody::new(EventKind::LifecycleSessionCreated(
            LifecycleSessionCreated {
                adapter: Some(self.shared.adapter.clone()),
            },
        )));

        if let Err(error) = self.apply_edge(Edge::Launch) {
            unreachable!("Created always launches: {error}");
        }
        match self.stand_up().await {
            Ok(()) => {
                let _ = self.apply_edge(Edge::PtyExecOk);
                let _ = launch_outcome.send(Ok(()));
            }
            Err(error) => {
                self.publish(EventBody::new(EventKind::PtyError(pty_error_payload(
                    &error,
                ))));
                let mut fields = Map::new();
                fields.insert("code".into(), json!(error.code()));
                self.log_record(LogLevel::Error, "pty.error", fields);
                self.finalize(CloseRoute::Edge(Edge::LaunchFailed), None)
                    .await;
                let _ = launch_outcome.send(Err(SessionError::LaunchFailed(error)));
                return;
            }
        }

        self.next_liveness = Instant::now() + self.config.liveness_poll;
        loop {
            // The non-droppable check: a fatal terminal failure whose
            // command the full queue refused is picked up here, at worst
            // one liveness tick after it was flagged.
            if self
                .terminal_failed
                .swap(false, std::sync::atomic::Ordering::Acquire)
            {
                // The slot travels with the flag, so even this fallback
                // publishes the OS cause the producer had in hand; the
                // queued command, when it also got through, is a
                // duplicate the ended session ignores.
                let cause = self
                    .terminal_fault
                    .lock()
                    .expect("a fault-slot lock holder panicked")
                    .take();
                self.handle_terminal_failure(cause).await;
                if self.state == SessionState::Closed {
                    break;
                }
            }
            // First output rides the same guaranteed-flag pattern as the
            // terminal failure above: the pump's command is best-effort,
            // and this poll is what makes the Running transition survive
            // a refused send.
            if self.state == SessionState::Connecting
                && self
                    .pump_saw_output
                    .load(std::sync::atomic::Ordering::Acquire)
            {
                self.handle_first_output();
            }
            // Stream-end rides the same guaranteed-flag pattern: the
            // reader's command is best-effort, and a stream that ended
            // over a live child is exactly the ending the liveness poll
            // cannot recover.
            if self
                .stream_ended
                .swap(false, std::sync::atomic::Ordering::Relaxed)
            {
                self.handle_stream_ended().await;
                if self.state == SessionState::Closed {
                    break;
                }
            }
            // A wake that is already due is serviced before the queue is
            // asked: `timeout_at` only fires when `recv` has nothing to
            // yield, so under sustained command traffic the deadline arm
            // would otherwise never run — a flood of input could postpone
            // child-exit detection and an armed drain deadline without
            // bound.
            if let Some(at) = self.next_wake()
                && Instant::now() >= at
            {
                self.on_wake().await;
                if self.state == SessionState::Closed {
                    break;
                }
                continue;
            }
            let received = match self.next_wake() {
                Some(at) => match tokio::time::timeout_at(at, self.commands.recv()).await {
                    Ok(received) => received,
                    Err(_) => {
                        self.on_wake().await;
                        if self.state == SessionState::Closed {
                            break;
                        }
                        continue;
                    }
                },
                None => self.commands.recv().await,
            };
            let Some(command) = received else { break };
            self.handle(command).await;
            if self.state == SessionState::Closed {
                break;
            }
        }
    }

    /// Allocate the terminal, start the child, attach the readers — the
    /// create flow's steps 6–7, all-or-nothing: on any failure the child
    /// (if it started) is terminated and nothing is left attached.
    async fn stand_up(&mut self) -> Result<(), PtyError> {
        let spawn_spec = to_spawn_spec(&self.launch, self.dimensions);
        let mut fields = Map::new();
        // The argv head only: argument values may carry
        // secrets, the program name does not.
        fields.insert(
            "program".into(),
            json!(spawn_spec.program.display().to_string()),
        );
        self.log_record(LogLevel::Info, "session.launch", fields);

        // The spawn rides the detached seam with a bound, like the log
        // open and the census: executable and cwd resolution can block
        // without bound on a stalled filesystem, and an unbounded spawn
        // here wedges the sole actor task — the abandonment guard's
        // force-close included — and holds runtime shutdown with it. A
        // launch that outlives its limit fails the create; a child the
        // abandoned spawn delivers anyway is ended wherever the race
        // left it — on the spawn thread when the send was refused, from
        // the dropped receiver when the answer sat queued — because a
        // terminal nobody owns must not outlive the launch that gave up
        // on it.
        let spawn = crate::detach::detached_with_abandon(
            "session-launch",
            move || agent_bridge_pty::spawn(&spawn_spec),
            |late: Result<Spawned, PtyError>| {
                if let Ok(spawned) = late {
                    tracing::warn!("an abandoned launch delivered a child late; terminating it");
                    // On its own thread: the drop that runs this can sit
                    // in the actor's async context (a queued answer the
                    // expired timeout never read), and a terminate must
                    // not block there.
                    let disposal = std::thread::Builder::new()
                        .name("session-launch-abandon".to_string())
                        .spawn(move || {
                            let _ = spawned.pty.terminate(Duration::from_secs(2));
                        });
                    if let Err(error) = disposal {
                        tracing::error!(%error, "the abandoned-launch disposal thread could not spawn");
                    }
                }
            },
        );
        let Spawned { pty, output } = match tokio::time::timeout(LAUNCH_LIMIT, spawn).await {
            Ok(Ok(delivered)) => delivered.claim()?,
            Ok(Err(_)) => {
                return Err(PtyError::AllocFailed(std::io::Error::other(
                    "the spawn thread died before answering",
                )));
            }
            Err(_) => {
                return Err(PtyError::AllocFailed(std::io::Error::other(
                    "the terminal did not stand up within its limit; the launch is abandoned",
                )));
            }
        };
        let pty: Arc<dyn Pty> = Arc::from(pty);

        let source = match PtyChunkSource::spawn(
            output,
            format!("session-output-{}", self.shared.session_id),
        ) {
            Ok(source) => source,
            Err(error) => {
                // A stream nobody will forward is a session that could not
                // be stood up (the terminal layer's own precedent:
                // reported as allocation failure, not handed back
                // half-working). The live terminal is kept for finalize
                // rather than terminated inline: a child and its
                // containment already exist, and only the close path's
                // machinery terminates with escalation, runs the census,
                // and types the verdict onto the closed payload — an
                // inline kill with a discarded result could reach
                // `Closed` past a live tree with no verdict at all.
                self.pty = Some(pty);
                return Err(PtyError::AllocFailed(std::io::Error::other(
                    error.to_string(),
                )));
            }
        };

        let (text_tx, mut text_rx) = mpsc::channel::<String>(4);
        let (incident_tx, mut incident_rx) = mpsc::channel::<EncodingIncident>(16);
        let reader = StreamReader::new(
            ReaderConfig::default(),
            ReaderOutputs {
                text: text_tx,
                // No reconstructed screen in this layer: the matcher
                // pipeline owns one where an adapter wants it (Phase 2).
                vt: None,
                incidents: incident_tx,
            },
        );
        let loopback = self.loopback.clone();
        let terminal_failed = Arc::clone(&self.terminal_failed);
        let terminal_fault = Arc::clone(&self.terminal_fault);
        let stream_ended = Arc::clone(&self.stream_ended);
        self.reader = Some(tokio::spawn(async move {
            let report = reader.run(source).await;
            // A stream that *failed* is a terminal fault, not a child
            // exit: the child may be alive behind a dead terminal, and
            // the events should say which happened.
            let ended = if let agent_bridge_stream::ReaderEnd::Stream(
                agent_bridge_pty::EndOfStream::Failed(error),
            ) = &report.end
            {
                // Cause, then flag: unlike a stream that merely ended
                // (the liveness poll's finding either way), a failed
                // terminal over a live child has no second detector, so
                // this signal must survive a refused send — and the slot
                // keeps the OS cause beside it.
                let cause = error.to_string();
                *terminal_fault
                    .lock()
                    .expect("a fault-slot lock holder panicked") = Some(cause.clone());
                terminal_failed.store(true, std::sync::atomic::Ordering::Release);
                SessionCommand::TerminalFailure(Some(cause))
            } else {
                // Durable for the same reason as the failure: a stream
                // that ends over a live child has no second detector, so
                // the flag guarantees what the send can only offer.
                stream_ended.store(true, std::sync::atomic::Ordering::Relaxed);
                SessionCommand::StreamEnded
            };
            let _ = loopback.try_send(ended);
            report
        }));

        let loopback = self.loopback.clone();
        let pump_saw_output = Arc::clone(&self.pump_saw_output);
        let pump_first_output = Arc::clone(&self.pump_first_output);
        self.pump = Some(tokio::spawn(async move {
            // "First output observed" means the child *painted* something:
            // the stream is stripped here and only visible content counts.
            // A pseudo-console synthesizes control sequences on attach —
            // screen clear, cursor home — before a silent child has written
            // a byte, so counting raw chunks would put every Windows
            // session in Running the moment it connected. Whitespace is
            // excluded on purpose, not as an accident of the stripping:
            // attach noise also carries blank padding and line motion once
            // the sequences are stripped, and a character that paints
            // nothing visible does not make a session "producing" — a CLI
            // that emits only a newline before exiting ends as
            // exited-before-output, which is what an operator watching the
            // screen saw. Later chunks are
            // drained so the reader never stalls on a consumer nobody has
            // attached yet (Phase 2 replaces this tail with the
            // strip-and-match pipeline), and the verdict is returned for
            // the close path, which needs it to classify an exit that
            // raced these signals.
            let mut stripper = Stripper::new();
            let mut saw_output = false;
            while let Some(chunk) = text_rx.recv().await {
                if !saw_output
                    && stripper
                        .feed(&chunk)
                        .text
                        .chars()
                        .any(|ch| !ch.is_whitespace())
                {
                    saw_output = true;
                    // The observation instant first, then the flag: any
                    // reader of the flag finds the timestamp already set.
                    let _ = pump_first_output.set(SystemTime::now());
                    // `Release`, paired with the `Acquire` loads: the
                    // store is the publication barrier for the timestamp
                    // above, and a relaxed pair could let a reader see
                    // the flag without the instant it promises.
                    // The flag is the guaranteed path and the command the
                    // prompt one — the same split the terminal-failure
                    // signal uses. An awaited send here could park the
                    // pump on a full queue after finalize stopped
                    // draining it; a parked pump stops draining the text
                    // channel, the reader stalls behind that, and the
                    // close pays both join limits and forfeits the
                    // accounting. The actor polls the flag every loop
                    // pass, so a refused send costs one wake at worst.
                    pump_saw_output.store(true, std::sync::atomic::Ordering::Release);
                    let _ = loopback.try_send(SessionCommand::Output);
                }
            }
            saw_output
        }));

        let loopback = self.loopback.clone();
        self.incident_pump = Some(tokio::spawn(async move {
            while let Some(incident) = incident_rx.recv().await {
                if loopback
                    .send(SessionCommand::Incident(incident))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }));

        self.writer = Some(spawn_input_writer(
            Arc::clone(&pty),
            Arc::clone(&self.shared),
            self.loopback.clone(),
            Arc::clone(&self.terminal_failed),
            Arc::clone(&self.terminal_fault),
        ));
        self.pty = Some(pty);
        Ok(())
    }

    fn next_wake(&self) -> Option<Instant> {
        let liveness = self.pty.as_ref().map(|_| self.next_liveness);
        match (liveness, self.drain_deadline) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) => Some(a),
            (None, deadline) => deadline,
        }
    }

    async fn on_wake(&mut self) {
        // Approvals whose announcer vanished after delivery are reaped on
        // the poll: a dropped resolution receiver means nobody can ever
        // hear a decision, and the entry could only hold the state and
        // the set's capacity. If the sweep empties the set, the state
        // follows — the same edge an ordinary resolution takes.
        let orphaned = self.approvals.reap_orphaned();
        if !orphaned.is_empty() {
            for id in &orphaned {
                tracing::warn!(
                    session_id = %self.shared.session_id,
                    approval_id = %id.0,
                    "pending approval abandoned by its announcer; entry removed"
                );
                // Announced per id: the prompt reached the stream, and a
                // runtime-initiated ending has no informed actor unless
                // the stream says so.
                self.publish(EventBody::approval_withdrawn(id.0.clone()));
            }
            if self.approvals.is_empty() && self.state == SessionState::AwaitingApproval {
                let _ = self.apply_edge(Edge::ApprovalResolved);
            }
        }
        let now = Instant::now();
        if let Some(deadline) = self.drain_deadline
            && now >= deadline
        {
            self.drain_deadline = None;
            // The drain verdict is sampled inside finalize at the
            // escalation boundary, not here: between this wake and the
            // terminate stands the writer teardown — seconds wide at its
            // bound — and a child that exits voluntarily in that span
            // was never escalated at, which the payload contract words
            // as `drained: true`. A sample taken now would blame the
            // hint for scheduler latency.
            self.judge_drain_at_terminate = true;
            self.finalize(CloseRoute::Edge(Edge::CloseComplete), None)
                .await;
            return;
        }
        if now >= self.next_liveness {
            self.next_liveness = now + self.config.liveness_poll;
            if let Some(pty) = &self.pty
                && !pty.alive()
            {
                self.handle_child_exit().await;
            }
        }
    }

    async fn handle(&mut self, command: SessionCommand) {
        match command {
            SessionCommand::Send { input, reply } => self.handle_send(input, reply).await,
            SessionCommand::ResolveApproval {
                id,
                decision,
                reply,
            } => {
                self.handle_resolve(&id, decision, reply);
            }
            SessionCommand::Interrupt { reply } => self.handle_interrupt(reply).await,
            SessionCommand::Resize { dimensions, reply } => {
                self.handle_resize(dimensions, reply).await;
            }
            SessionCommand::Close { force, reply } => self.handle_close(force, reply).await,
            SessionCommand::ApprovalDetected {
                identity,
                prompt,
                reply,
            } => self.handle_approval_detected(identity, prompt, reply),
            SessionCommand::InterruptAcknowledged => self.handle_interrupt_acknowledged(),
            SessionCommand::Resumed => self.handle_resumed(),
            SessionCommand::TransportDropped => self.clear_writer(),
            SessionCommand::Output => self.handle_first_output(),
            SessionCommand::StreamEnded => self.handle_stream_ended().await,
            SessionCommand::Incident(incident) => self.handle_incident(&incident),
            SessionCommand::TerminalFailure(cause) => self.handle_terminal_failure(cause).await,
            SessionCommand::HintDispatched => self.handle_hint_dispatched(),
        }
    }

    async fn handle_send(&mut self, input: Bytes, reply: Reply<()>) {
        match self.state {
            // `AwaitingApproval` deliberately stays writable. Input and
            // approval control are separated by contract — send forwards
            // bytes and never resolves an approval; the dedicated method
            // does — and a pending dialog legitimately needs keys that
            // are not an answer (navigation, a detail pane). A writer
            // typing past its own pending prompt bypasses only its own
            // bookkeeping; the screen source that announces a dialog owns
            // observing its dismissal, and reconciling the set then is
            // that source's contract, not a reason to seal the terminal.
            SessionState::Connecting
            | SessionState::Running
            | SessionState::AwaitingApproval
            | SessionState::Interrupted => {
                let Some(writer) = &self.writer else {
                    let _ = reply.send(Err(SessionError::SessionClosed));
                    return;
                };
                // `try_send`, never an await: the actor is the control
                // plane, and parking it on a full input queue would put
                // the interrupt that exists to cut a wedged child short
                // behind the very writes it should cut. A full queue means
                // the child has stopped draining its input; the send is
                // refused with the bytes intact — the same verdict, and
                // the same returned suffix, a write deadline would reach —
                // and the caller decides whether to retry.
                match writer.tx.try_send(WriteRequest {
                    bytes: input,
                    reply: Some(reply),
                }) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(request)) => {
                        if let Some(reply) = request.reply {
                            let _ = reply.send(Err(SessionError::Pty(PtyError::StdinBlocked {
                                unwritten: request.bytes.to_vec(),
                            })));
                        }
                    }
                    Err(mpsc::error::TrySendError::Closed(request)) => {
                        if let Some(reply) = request.reply {
                            let _ = reply.send(Err(SessionError::SessionClosed));
                        }
                    }
                }
            }
            SessionState::Closed => {
                let _ = reply.send(Err(SessionError::SessionClosed));
            }
            state => {
                let _ = reply.send(Err(SessionError::InvalidStateForOperation {
                    state,
                    op: "send",
                }));
            }
        }
    }

    fn handle_resolve(&mut self, id: &ApprovalId, decision: ApprovalDecision, reply: Reply<()>) {
        if self.state != SessionState::AwaitingApproval {
            let refusal = if self.state == SessionState::Closed {
                SessionError::SessionClosed
            } else if self.state == SessionState::Running {
                // Running means the pending set is empty, and an id
                // offered here names an approval the session already
                // answered for — a resolution that raced the exit from
                // `AwaitingApproval`, or a withdrawal whose contract
                // promises the stale verdict from that point on. The
                // mismatch blames the id; a wrong-state refusal would
                // misdirect the caller toward the session instead.
                SessionError::ApprovalIdMismatch
            } else {
                SessionError::InvalidStateForOperation {
                    state: self.state,
                    op: "resolve_approval",
                }
            };
            let _ = reply.send(Err(refusal));
            return;
        }
        let before = self.approvals.len();
        let outcome = self
            .approvals
            .resolve(id, ApprovalResolution::from(decision));
        // A shrink behind an error is the orphan removal: the entry left
        // the set even as the caller hears the stale verdict, and the
        // published prompt's ending belongs on the stream like any other
        // runtime-initiated withdrawal.
        if outcome.is_err() && self.approvals.len() < before {
            self.publish(EventBody::approval_withdrawn(id.0.clone()));
        }
        // The set can shrink on either arm, so the state follows the set
        // here, not the reply.
        if self.approvals.is_empty() && self.state == SessionState::AwaitingApproval {
            let _ = self.apply_edge(Edge::ApprovalResolved);
        }
        match outcome {
            Ok(()) => {
                let mut fields = Map::new();
                fields.insert("approval_id".into(), json!(id.0));
                fields.insert("pending".into(), json!(self.approvals.len()));
                self.log_record(LogLevel::Debug, "session.approval_resolved", fields);
                let _ = reply.send(Ok(()));
            }
            Err(error) => {
                let _ = reply.send(Err(error));
            }
        }
    }

    async fn handle_interrupt(&mut self, reply: Reply<()>) {
        match self.state {
            // A repeated interrupt while an acknowledgement is still in
            // flight is delivered, not coalesced: a second Ctrl+C is a
            // real operator action with real CLI meaning (many CLIs
            // escalate on it), and a bridge that swallowed it would
            // change the terminal's behavior. The pending flag gates
            // whether an acknowledgement means anything, not how many
            // interrupts may travel; correlating acks to deliveries is
            // the acknowledging source's contract, the same boundary as
            // the deliberately absent ack timeout.
            SessionState::Running | SessionState::AwaitingApproval => {
                let Some(pty) = self.pty.clone() else {
                    let _ = reply.send(Err(SessionError::SessionClosed));
                    return;
                };
                // On the blocking pool: the control write waits up to its
                // own short deadline for room, and a parked async worker
                // is the wrong place to spend it.
                let outcome = tokio::task::spawn_blocking(move || pty.interrupt()).await;
                match outcome.unwrap_or_else(|_| {
                    Err(PtyError::TerminalFailed(std::io::Error::other(
                        "the interrupt task panicked",
                    )))
                }) {
                    Ok(()) => {
                        // The whole pending set resolves as
                        // cancelled *now* — a held hook reply must not
                        // dangle to its timeout while the ack travels.
                        self.approvals.cancel_all();
                        self.log_record(LogLevel::Debug, "session.interrupt_sent", Map::new());
                        if self.state == SessionState::AwaitingApproval {
                            // The sweep emptied the set that state stands
                            // for, so the resolved edge returns to Running
                            // at once — the state never claims approvals
                            // that no longer exist, and with no deadline
                            // on an acknowledgement it otherwise would,
                            // unboundedly. `Interrupted` itself still
                            // waits for the acknowledgement below: the
                            // published event means the CLI acknowledged,
                            // and delivery is not that evidence.
                            let _ = self.apply_edge(Edge::ApprovalResolved);
                        }
                        self.interrupt_pending = true;
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        let cause =
                            matches!(error, PtyError::TerminalFailed(_)).then(|| error.to_string());
                        let _ = reply.send(Err(SessionError::Pty(error)));
                        if let Some(cause) = cause {
                            self.handle_terminal_failure(Some(cause)).await;
                        }
                    }
                }
            }
            SessionState::Closed => {
                let _ = reply.send(Err(SessionError::SessionClosed));
            }
            state => {
                let _ = reply.send(Err(SessionError::InvalidStateForOperation {
                    state,
                    op: "interrupt",
                }));
            }
        }
    }

    /// The terminal failed an operation with the child not known to be
    /// gone: publish the fault and take the failure close — a session
    /// cannot continue on a terminal it can neither read nor write.
    async fn handle_terminal_failure(&mut self, cause: Option<String>) {
        // Both halves of the durable signal are consumed no matter which
        // path delivered the failure — the loop's flag check or the
        // queued command — so a close racing in behind this handler
        // cannot read the flag again and publish the same fault twice.
        self.terminal_failed
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.terminal_fault
            .lock()
            .expect("a fault-slot lock holder panicked")
            .take();
        match self.state {
            SessionState::Connecting => {
                // The fault is published inside finalize, not here: the
                // pump's verdict may show the child produced visible
                // output first, and then subscribers must hear `running`
                // before the fault — publishing now would put the error
                // ahead of the state it interrupted. The cause rides on
                // the actor, because the route enum carries no payload.
                self.terminal_fault_cause = cause;
                self.finalize(CloseRoute::ConnectingFailure, None).await;
            }
            SessionState::Running | SessionState::AwaitingApproval | SessionState::Interrupted => {
                self.approvals.cancel_all();
                self.interrupt_pending = false;
                self.publish(EventBody::new(EventKind::PtyError(
                    terminal_failed_payload(cause.as_deref()),
                )));
                let _ = self.apply_edge(Edge::PostRunningFailure);
                self.finalize(CloseRoute::Edge(Edge::CloseComplete), None)
                    .await;
            }
            SessionState::Closing => {
                // Published on this route too: without it the stream
                // reads as a normal closing → closed and the reason the
                // close stopped being graceful is lost.
                self.publish(EventBody::new(EventKind::PtyError(
                    terminal_failed_payload(cause.as_deref()),
                )));
                // `drained` stays absent: the field answers for the hint,
                // and a session that ended by failing answered nothing —
                // `false` is reserved for drain expiry and force-close,
                // and reporting the failure as either would misclassify
                // it as a rejected shutdown hint.
                self.drain_deadline = None;
                self.finalize(CloseRoute::Edge(Edge::CloseComplete), None)
                    .await;
            }
            SessionState::Created | SessionState::Launching | SessionState::Closed => {}
        }
    }

    fn handle_interrupt_acknowledged(&mut self) {
        if !self.interrupt_pending {
            tracing::debug!("interrupt acknowledgement with no interrupt pending; ignored");
            return;
        }
        if matches!(
            self.state,
            SessionState::Running | SessionState::AwaitingApproval
        ) {
            self.interrupt_pending = false;
            // An approval announced between delivery and acknowledgement
            // belongs to the interrupted turn: the same cancellation sweep
            // covers it — and it is why AwaitingApproval is accepted here,
            // since such an announcement legitimately moves the state
            // while the acknowledgement is in flight; skipping it would
            // strand the session with a pending interrupt nothing can
            // complete.
            self.approvals.cancel_all();
            let _ = self.apply_edge(Edge::Interrupt);
        }
    }

    /// The hint finished dispatching with its keystrokes delivered: the
    /// drain window arms now, measuring what it claims to — the CLI's
    /// chance to exit after the hint — instead of counting the hint's own
    /// delivery time against it.
    fn handle_hint_dispatched(&mut self) {
        if self.state == SessionState::Closing && self.drain_deadline.is_none() {
            self.drain_deadline = Some(Instant::now() + self.config.stdin_drain);
        }
    }

    fn handle_resumed(&mut self) {
        if self.state == SessionState::Interrupted {
            let _ = self.apply_edge(Edge::Resumed);
        } else {
            tracing::debug!(state = %self.state, "resume signal outside Interrupted; ignored");
        }
    }

    fn handle_approval_detected(
        &mut self,
        identity: ApprovalIdentity,
        prompt: ApprovalPrompt,
        reply: Reply<(ApprovalId, oneshot::Receiver<ApprovalResolution>)>,
    ) {
        // The id is derived at the one mutation point, never accepted for
        // a screen source: the mint happening here is what makes the
        // documented contract — hook ids verbatim, screen ids UUIDv4 —
        // impossible to violate from outside.
        let (id, source) = match identity {
            ApprovalIdentity::Hook(id) => (id, ApprovalSource::Hook),
            ApprovalIdentity::Screen => (
                ApprovalId(uuid::Uuid::new_v4().to_string()),
                ApprovalSource::Screen,
            ),
        };
        match self.state {
            SessionState::Running | SessionState::AwaitingApproval => {
                let (resolver, resolution) = oneshot::channel();
                let entry = PendingApproval {
                    source,
                    since: std::time::Instant::now(),
                    resolver,
                };
                match self.approvals.insert(id.clone(), entry) {
                    Ok(()) => {
                        // An announcer that is already gone gets nothing
                        // published: a prompt nobody can resolve must not
                        // reach the stream or hold the state. The check
                        // does not consume the reply, so the ordering
                        // below survives it.
                        if reply.is_closed() {
                            tracing::warn!(
                                session_id = %self.shared.session_id,
                                approval_id = %id.0,
                                "approval announcement abandoned before delivery; entry removed"
                            );
                            let _ = self.approvals.resolve(&id, ApprovalResolution::Cancelled);
                            return;
                        }
                        // The prompt (the cause) first, then the state (its
                        // consequence), and only then the reply — pinned by
                        // the lifecycle tests: an announcer that has heard
                        // its answer must find the prompt already on the
                        // stream, never racing it.
                        self.publish(EventBody::approval_required(id.0.clone(), prompt));
                        if self.state == SessionState::Running {
                            let _ = self.apply_edge(Edge::ApprovalDetected);
                        }
                        if reply.send(Ok((id.clone(), resolution))).is_err() {
                            // Dropped in the window between the check and
                            // the send: withdraw the entry the same way
                            // the wake sweep would, state included.
                            tracing::warn!(
                                session_id = %self.shared.session_id,
                                approval_id = %id.0,
                                "approval announcement abandoned at delivery; entry removed"
                            );
                            let _ = self.approvals.resolve(&id, ApprovalResolution::Cancelled);
                            // The prompt reached the stream, so its ending
                            // must too: a withdrawal has no informed actor
                            // unless the stream says so. The fact first,
                            // then the state it changes.
                            self.publish(EventBody::approval_withdrawn(id.0.clone()));
                            if self.approvals.is_empty()
                                && self.state == SessionState::AwaitingApproval
                            {
                                let _ = self.apply_edge(Edge::ApprovalResolved);
                            }
                        }
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            }
            SessionState::Closed => {
                let _ = reply.send(Err(SessionError::SessionClosed));
            }
            state => {
                let _ = reply.send(Err(SessionError::InvalidStateForOperation {
                    state,
                    op: "approval_detected",
                }));
            }
        }
    }

    async fn handle_resize(&mut self, dimensions: Dimensions, reply: Reply<()>) {
        match self.state {
            SessionState::Connecting
            | SessionState::Running
            | SessionState::AwaitingApproval
            | SessionState::Interrupted => {
                let Some(pty) = self.pty.clone() else {
                    let _ = reply.send(Err(SessionError::SessionClosed));
                    return;
                };
                let outcome = tokio::task::spawn_blocking(move || pty.resize(dimensions)).await;
                match outcome {
                    Ok(Ok(())) => {
                        self.shared
                            .metadata
                            .lock()
                            .expect("the metadata lock is never poisoned: holders do not panic")
                            .dimensions = dimensions;
                        let _ = reply.send(Ok(()));
                    }
                    Ok(Err(error)) => {
                        // One refusal is a race, not a failure to apply:
                        // the terminal holds the new geometry even though
                        // the child could not be told yet, so the record
                        // reflects it while the caller learns to reissue.
                        // A failed *terminal* is another matter: the
                        // caller gets the cause, and the session takes
                        // its failure close rather than staying live on
                        // hardware that no longer answers.
                        if matches!(error, PtyError::ResizeBeforeReady) {
                            self.shared
                                .metadata
                                .lock()
                                .expect("the metadata lock is never poisoned: holders do not panic")
                                .dimensions = dimensions;
                        }
                        let cause =
                            matches!(error, PtyError::TerminalFailed(_)).then(|| error.to_string());
                        let _ = reply.send(Err(SessionError::Pty(error)));
                        if let Some(cause) = cause {
                            self.handle_terminal_failure(Some(cause)).await;
                        }
                    }
                    Err(_) => {
                        let _ = reply.send(Err(SessionError::Pty(PtyError::TerminalFailed(
                            std::io::Error::other("the resize task panicked"),
                        ))));
                        // A panicked resize left the terminal in an
                        // unknown state — the same verdict the panicked
                        // write reaches.
                        self.handle_terminal_failure(Some("the resize task panicked".to_string()))
                            .await;
                    }
                }
            }
            SessionState::Closed => {
                let _ = reply.send(Err(SessionError::SessionClosed));
            }
            state => {
                let _ = reply.send(Err(SessionError::InvalidStateForOperation {
                    state,
                    op: "resize",
                }));
            }
        }
    }

    fn handle_first_output(&mut self) {
        if self.state == SessionState::Connecting {
            // The pump stamped the observation before it raised the
            // flag; command-queue latency must not become part of the
            // record.
            let started = self
                .pump_first_output
                .get()
                .copied()
                .unwrap_or_else(SystemTime::now);
            self.shared
                .metadata
                .lock()
                .expect("the metadata lock is never poisoned: holders do not panic")
                .started_at = Some(started);
            let _ = self.apply_edge(Edge::FirstOutput);
        }
    }

    async fn handle_stream_ended(&mut self) {
        // The stream ending proves the terminal is done, not that the
        // child is: EOF means the terminal side closed, and a failed
        // consumer ends the stream the same way. A child still alive
        // behind an ended stream is a terminal failure — publishing a
        // child exit for it would be false, and during a graceful close
        // it would claim `drained: true` for a hint nothing honored.
        // The two endings race at the boundary, though: a child on its
        // way out closes its terminal before it becomes waitable, so an
        // EOF can be observed a beat before the exit can be reaped. The
        // liveness answer gets a short grace to settle before the
        // terminal is blamed — a voluntary exit misread as a terminal
        // fault would be the false story in the other direction.
        let mut alive = self.pty.as_ref().is_some_and(|pty| pty.alive());
        for _ in 0..5 {
            if !alive {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            alive = self.pty.as_ref().is_some_and(|pty| pty.alive());
        }
        if alive {
            self.handle_terminal_failure(Some(
                "the output stream ended while the child was still alive".to_string(),
            ))
            .await;
            return;
        }
        match self.state {
            SessionState::Connecting
            | SessionState::Running
            | SessionState::AwaitingApproval
            | SessionState::Interrupted
            | SessionState::Closing => self.handle_child_exit().await,
            SessionState::Created | SessionState::Launching | SessionState::Closed => {}
        }
    }

    /// The child is gone (or its terminal is): route per the state machine.
    async fn handle_child_exit(&mut self) {
        match self.state {
            SessionState::Connecting => {
                // Whether this is "exited before output" is not knowable
                // yet: the exit signal and the pump's first-output signal
                // ride different tasks, so a child that wrote and exited
                // in one breath can be observed dead first. The route is
                // resolved inside finalize, after the pump has been
                // joined and its verdict is authoritative.
                self.finalize(CloseRoute::ConnectingExit, None).await;
            }
            SessionState::Running | SessionState::AwaitingApproval | SessionState::Interrupted => {
                // Approvals expire on exit: nobody is coming to answer
                // these.
                self.approvals.cancel_all();
                self.interrupt_pending = false;
                self.publish(EventBody::new(EventKind::PtyError(exited_early_payload())));
                // A post-Running failure goes through Closing — the
                // table has no shortcut to take.
                let _ = self.apply_edge(Edge::PostRunningFailure);
                self.finalize(CloseRoute::Edge(Edge::CloseComplete), None)
                    .await;
            }
            SessionState::Closing => {
                // A voluntary exit while Closing is the hint working —
                // whether the drain window is already armed or the input
                // hint is still dispatching. Forced closes never observe
                // this state: they finalize inline, so being here at all
                // implies the graceful path.
                self.drain_deadline = None;
                self.finalize(CloseRoute::Edge(Edge::CloseComplete), Some(true))
                    .await;
            }
            SessionState::Created | SessionState::Launching | SessionState::Closed => {}
        }
    }

    pub(crate) fn handle_incident(&mut self, incident: &EncodingIncident) {
        self.publish(EventBody::new(EventKind::PtyError(incident.to_payload())));
    }

    pub(crate) fn clear_writer(&mut self) {
        *self
            .shared
            .writer
            .lock()
            .expect("the writer lock is never poisoned: holders do not panic") = None;
    }

    /// Take one edge: transition, snapshot, log, and emit the entered
    /// state's lifecycle event. `Closed` is the one entry this never
    /// emits — its event carries a payload only the close path can fill,
    /// so `finalize` owns it.
    pub(crate) fn apply_edge(&mut self, edge: Edge) -> Result<(), SessionError> {
        let next = transition(self.state, edge)?;
        self.state = next;
        let kind = match next {
            SessionState::Launching => Some(EventKind::LifecycleSessionLaunching(
                LifecycleSessionLaunching {},
            )),
            SessionState::Connecting => Some(EventKind::LifecycleSessionConnecting(
                LifecycleSessionConnecting {},
            )),
            SessionState::Running => Some(EventKind::LifecycleSessionRunning(
                LifecycleSessionRunning {},
            )),
            SessionState::AwaitingApproval => Some(EventKind::LifecycleSessionAwaitingApproval(
                LifecycleSessionAwaitingApproval {},
            )),
            SessionState::Interrupted => Some(EventKind::LifecycleSessionInterrupted(
                LifecycleSessionInterrupted {},
            )),
            SessionState::Closing => Some(EventKind::LifecycleSessionClosing(
                LifecycleSessionClosing {},
            )),
            SessionState::Created | SessionState::Closed => None,
        };
        if let Some(kind) = kind {
            let event_type = kind.event_type().to_owned();
            self.log_record(LogLevel::Info, &event_type, Map::new());
            self.publish(EventBody::new(kind));
        }
        // The watch is notified only after the event exists on the
        // stream: `SessionHandle::state()` is a public surface, and a
        // state observable there before its lifecycle event would let a
        // reader see `Running` on a stream that has not yet said so.
        let _ = self.state_tx.send(next);
        Ok(())
    }

    /// Publish one event and mirror its metadata into the session log at
    /// `debug` — metadata always, payload only when the operator opted
    /// in. The mirror record is built only when a log exists to take it;
    /// a session without one pays nothing for its diary, and an
    /// unmirrored record pays one serialization pass for its byte count,
    /// never a materialized JSON tree.
    pub(crate) fn publish(&mut self, body: EventBody) {
        let event_type = body.kind.event_type().to_owned();
        let approval_id = body.approval_id.clone();
        let mirror = if self.log.is_some() {
            if self.config.mirror_payloads {
                // The payload alone: the record's `event` field already
                // names the type, and mirroring the tagged wrapper would
                // say it twice while inflating the byte count.
                let payload = body.kind.payload_value();
                Some((payload.to_string().len(), Some(payload)))
            } else {
                // Only the byte count is wanted: one serialization pass,
                // no tree.
                Some((body.kind.payload_bytes(), None))
            }
        } else {
            None
        };
        match self.sink.publish(body) {
            Ok(seq) => {
                let Some((payload_bytes, payload)) = mirror else {
                    return;
                };
                let mut fields = Map::new();
                fields.insert("seq".into(), json!(seq));
                fields.insert("payload_bytes".into(), json!(payload_bytes));
                if let Some(approval_id) = approval_id {
                    fields.insert("approval_id".into(), json!(approval_id));
                }
                // Opt-in only — and never for an approval prompt even
                // then: its text can carry exactly the credentials the
                // log contract forbids on disk, and the redaction pass
                // that would make mirroring it safe lands with a later
                // layer. Until then absence is the only safe spelling.
                if let Some(payload) = payload
                    && event_type != "prompt.approval_required"
                {
                    fields.insert("payload".into(), payload);
                }
                self.log_record(LogLevel::Debug, &event_type, fields);
            }
            Err(SinkSealed) => {
                tracing::error!(
                    session_id = %self.shared.session_id,
                    event_type,
                    "publish after seal — the seal must be the session's last act"
                );
            }
        }
    }

    pub(crate) fn log_record(&mut self, level: LogLevel, event: &str, fields: Map<String, Value>) {
        if let Some(log) = &mut self.log {
            log.record(level, event, fields);
        }
    }
}

fn to_spawn_spec(launch: &LaunchSpec, dimensions: Option<Dimensions>) -> SpawnSpec {
    let mut spec = SpawnSpec::new(launch.program.clone());
    spec.args = launch.args.iter().map(OsString::from).collect();
    spec.env = launch
        .env
        .iter()
        .map(|(name, value)| (OsString::from(name), OsString::from(value)))
        .collect();
    spec.cwd = launch.cwd.clone();
    spec.dimensions = dimensions;
    spec
}

/// The `pty.error` payload for a typed terminal-layer failure: the
/// variant's published code, its rendered message, nothing more — the
/// message already excludes content by the terminal layer's own contract.
///
/// Matched on the variants, exhaustively and with no fallback, so a new
/// terminal-layer failure mode is a compile error here until somebody
/// chooses its published code — the same discipline the terminal crate's
/// own code table holds itself to, extended across the crate seam. A
/// string-keyed match with an `Unknown` catch-all was the first shape and
/// was retired deliberately: it demoted every future variant to `unknown`
/// on the wire with nothing failing.
pub(crate) fn pty_error_payload(error: &PtyError) -> PtyErrorPayload {
    let code = match error {
        PtyError::AllocFailed(_) => PtyErrorCode::PtyAllocFailed,
        PtyError::ChildExecFailed(_) => PtyErrorCode::ChildExecFailed,
        PtyError::StdinBlocked { .. } => PtyErrorCode::StdinBlocked,
        PtyError::ChildExitedEarly(_) => PtyErrorCode::ChildExitedEarly,
        PtyError::SignalFailed { .. } => PtyErrorCode::SignalDeliveryFailed,
        PtyError::ResizeBeforeReady => PtyErrorCode::EarlyResize,
        PtyError::TerminalFailed(_) => PtyErrorCode::PtyIoFailed,
    };
    PtyErrorPayload {
        code,
        message: error.to_string(),
        detail: Map::new(),
    }
}

/// The paired `pty.error` payload for a terminal that failed under a
/// live child — one spelling wherever the fault is announced, carrying
/// the operating system's own words when the reporting path had them.
/// The generic form is only for the durable-flag fallback, whose atomic
/// cannot carry a message.
pub(crate) fn terminal_failed_payload(cause: Option<&str>) -> PtyErrorPayload {
    pty_error_payload(&PtyError::TerminalFailed(std::io::Error::other(
        cause.unwrap_or("the terminal failed").to_string(),
    )))
}

/// The `pty.error` paired with an unexpected child exit — the failure
/// event the lifecycle contract requires beside the failure-routing
/// edges.
pub(crate) fn exited_early_payload() -> PtyErrorPayload {
    PtyErrorPayload {
        code: PtyErrorCode::ChildExitedEarly,
        message: "the CLI process exited unexpectedly".to_owned(),
        detail: Map::new(),
    }
}

/// How a finalized child ended, for the close path in `close.rs`.
pub(crate) fn exit_status_of(
    outcome: Result<Result<ExitStatus, PtyError>, tokio::task::JoinError>,
) -> Option<ExitStatus> {
    match outcome {
        Ok(Ok(status)) => Some(status),
        Ok(Err(PtyError::ChildExitedEarly(status))) => Some(status),
        Ok(Err(error)) => {
            tracing::error!(%error, "terminate failed; exit status unknown");
            None
        }
        Err(_) => {
            tracing::error!("the terminate task panicked; exit status unknown");
            None
        }
    }
}
