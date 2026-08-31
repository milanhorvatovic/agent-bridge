//! The bounded write buffer with die-loudly semantics.
//!
//! The design's flow-control table ends at the process boundary: if the
//! caller stops reading stdout and the write buffer fills, the runtime
//! says so once and exits — there is no recovery from a non-reading
//! parent, and wedging silently against one is the failure mode this whole
//! policy family exists to forbid. What "says so" can promise is worth
//! being exact about, because the parent this fires against is by
//! definition one that may not be listening: the fatal signal and the log
//! are guaranteed, and the final `transport.error` frame is attempted —
//! best-effort, since a sink that has stopped reading will refuse it, and
//! deliberately withheld when a half-written frame would turn the goodbye
//! into corruption. This module is that
//! row, built as a reusable primitive: generic over [`AsyncWrite`] so the
//! state machine runs against a mock sink in tests, wired to the real
//! stdout behind the transport's framer when that layer lands.
//!
//! The shape mirrors the bus's lag policy at a different granularity:
//! progress accounting instead of a queue bound check, a drain deadline
//! instead of a grace window, and a terminal act that is a process
//! decision rather than a subscription one. Deadlines read
//! [`tokio::time`], the same mechanism as the bus's grace window, so both
//! become virtual under paused-clock tests.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use bytes::{Buf, Bytes};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, watch};

/// Tuning the writer accepts at construction.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    /// The buffer level at which the drain deadlines arm. Not an
    /// admission cap at the line itself: [`BoundedWriter::enqueue`] never
    /// blocks and never drops an accepted frame, so the buffer can run
    /// past this line — for at most one drain deadline of zero progress
    /// under a stalled sink, [`SUSTAINED_OVERFLOW_FACTOR`] deadlines
    /// outright under a trickling one, and never past
    /// [`HARD_OVERFLOW_FACTOR`] arming lines of bytes, the synchronous
    /// ceiling `enqueue` itself enforces — before die-loudly ends the
    /// process. The bound is enforced by exiting, which is the exit
    /// contract's stance. A frame larger than the line (but under the
    /// ceiling) is admitted like any other — partial writes drain it
    /// below the line on a healthy sink, and the deadlines catch a dead
    /// one; refusing it would break the wire for a frame the protocol's
    /// own frame cap already accepted.
    ///
    /// One sizing constraint follows for whoever wires this to real
    /// stdout: the protocol's maximum frame has to fit *under the
    /// ceiling*, not merely over the line. A single legal frame crossing
    /// [`HARD_OVERFLOW_FACTOR`] × this value would seal the writer and end
    /// the process — the right answer for a buffer that can never drain,
    /// the wrong one entirely for one large message. Configure this at or
    /// above the frame cap and the question does not arise.
    pub capacity_bytes: usize,
    /// How long the sink may make zero progress — with the buffer at or
    /// past `capacity_bytes` — before die-loudly fires, measured from
    /// whichever came later, the last accepted bytes or the moment the
    /// buffer armed. Forward progress restarts this clock, but not the
    /// sustained-overflow one above it. Write attempts are cut short at
    /// the next instant either clock could produce a verdict, so neither
    /// waits on an attempt that happens to be in flight.
    pub drain_deadline: Duration,
    /// Produces the final frame — the one `transport.error` of code
    /// `stdout_blocked` — attempted best-effort on the way down. Framing
    /// belongs to the transport layer, not to this crate, so the caller
    /// supplies it, built on the fatal path when it is needed. Against a
    /// truly non-reading parent the write usually fails, which is why it is
    /// best-effort and why the tracing log and the [`FatalSignal`] carry the
    /// same fact.
    pub farewell: fn() -> Bytes,
}

/// What [`BoundedWriter::enqueue`] can refuse.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WriterError {
    /// Die-loudly has fired — a drain deadline expired, or this very
    /// enqueue crossed the hard overflow ceiling and performed the
    /// terminal transition itself. The caller must not buffer further:
    /// there is no recovery from a non-reading parent.
    #[error("writer sealed after stdout_blocked")]
    Sealed,
}

/// How [`BoundedWriter::shutdown`] ended: whether the buffered tail was
/// actually delivered.
///
/// A bare `bool` said only whether the drain task *returned*, which
/// conflated a clean flush with a tail abandoned against a stalled sink —
/// so a caller could not tell that its final frames were dropped. These
/// three keep the outcomes apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Every buffered frame reached the sink before the task returned.
    Flushed,
    /// The task returned, but the tail was not delivered: the sink stayed
    /// dead past the drain deadlines, so the writer abandoned it —
    /// die-loudly on a non-reading parent, or the runtime exiting around a
    /// stalled sink. Any goodbye frame is best-effort.
    Abandoned,
    /// The drain task did not return on its own terms: it panicked in the
    /// sink's `poll_write`, or was cancelled by a runtime shutting down
    /// around it. The writer is sealed and the fatal has fired.
    Faulted,
}

/// Fires at most once — enforced, not merely intended: every path that
/// can end the writer shares one atomic transition, so a cancellation
/// provoked by the signal cannot produce a second one. The runtime main
/// loop listens
/// and runs the graceful child-cleanup path — the *transport* is
/// unrecoverable; PTY cleanup still runs — then exits nonzero. Cheap to
/// clone; every clone observes the same state, and one that attaches after
/// the firing still sees it.
///
/// A listener that wants the farewell frame's attempt to have happened
/// before it exits awaits [`BoundedWriter::shutdown`] after observing the
/// signal: on the hard-ceiling path the attempt is still owed to the drain
/// task's next turn, and joining it is what gives that turn.
#[derive(Debug, Clone)]
pub struct FatalSignal {
    rx: watch::Receiver<bool>,
}

impl FatalSignal {
    /// Wait until die-loudly fires. Never completes on a writer that shut
    /// down cleanly instead — a clean shutdown is not a fatal, and the
    /// listener's other branches (shutdown methods, signals) are the ones
    /// meant to win that race.
    pub async fn fired(&mut self) {
        loop {
            if *self.rx.borrow_and_update() {
                return;
            }
            if self.rx.changed().await.is_err() {
                // The drain task finished without firing: the fatal can
                // never come.
                std::future::pending::<()>().await;
            }
        }
    }

    /// Whether die-loudly has fired, without waiting.
    pub fn is_fired(&self) -> bool {
        *self.rx.borrow()
    }
}

/// The enqueue handle over the bounded buffer; the drain task holds the
/// sink. Ending it has two shapes: [`BoundedWriter::shutdown`] awaits the
/// drain task and so guarantees the buffered tail was given its chance,
/// while dropping the handle requests the same flush best-effort — a
/// runtime that exits right after the drop may abort the task mid-drain,
/// which is why a transport that must not lose tail frames awaits
/// `shutdown` instead. Asking for either shutdown is not itself a fatal;
/// what distinguishes them is what a failure *during* one means. A bare
/// drop leaves nobody waiting, so a cancellation afterwards is that
/// shutdown's tail and stays quiet, while a panic or cancellation during
/// an awaited `shutdown` is a failure its caller is waiting to hear about
/// and still fires.
#[derive(Debug)]
pub struct BoundedWriter {
    shared: Arc<Shared>,
    task: Option<tokio::task::JoinHandle<ShutdownOutcome>>,
}

/// No path has announced the fatal.
const FATAL_NONE: u8 = 0;
/// A path has logged the diagnostic and owes the signal.
const FATAL_CLAIMED: u8 = 1;
/// The signal has gone out; there is nothing left for any path to do.
const FATAL_SIGNALLED: u8 = 2;

#[derive(Debug)]
struct Shared {
    capacity_bytes: usize,
    /// Where the one-and-only fatal announcement has got to. Several
    /// paths can reach it — the ceiling from a caller's thread, the
    /// deadlines from the drain task, the task guard from a cancellation
    /// — and between them they must produce exactly one diagnostic and
    /// exactly one signal, including when the runtime reacts to the
    /// signal by cancelling the very task that was still saying goodbye.
    fatal: AtomicU8,
    state: Mutex<BufferState>,
    /// Wakes the drain task when a frame arrives or the handle drops.
    wake: Notify,
    /// The fatal's sending half, here rather than in the drain task so the
    /// hard-ceiling path in `enqueue` can fire it synchronously — the
    /// terminal decision must not depend on the drain task getting
    /// scheduled.
    fired: watch::Sender<bool>,
}

#[derive(Debug)]
struct BufferState {
    /// Frames not yet fully written, in order; the drain task additionally
    /// holds the partially-written front frame out of this queue.
    queue: VecDeque<Bytes>,
    /// Unwritten bytes across the queue and the drained-but-unfinished
    /// front frame — the number the arming line compares against.
    buffered: usize,
    /// When the buffer last crossed the arming line, `None` while under
    /// it. Feeds the sustained-overflow deadline: partial progress resets
    /// the per-attempt clock but never this one, so a trickling reader
    /// cannot hold the buffer over capacity indefinitely.
    over_capacity_since: Option<tokio::time::Instant>,
    sealed: bool,
    /// How the handle ended, which decides what a cancellation means.
    handle: HandleState,
}

/// What the enqueue handle has done, from the drain task's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandleState {
    /// Still held: a task dying now is a failure nobody asked for.
    Held,
    /// Dropped without awaiting. The caller asked for a shutdown and is
    /// not waiting for the answer, so a runtime cancelling the detached
    /// task afterwards is the tail of that shutdown, not a fault.
    Dropped,
    /// [`BoundedWriter::shutdown`] is waiting on the task. A panic or a
    /// cancellation here is a failure the caller will be told about, and
    /// its listeners are owed the fatal.
    AwaitedShutdown,
}

impl BoundedWriter {
    /// Spawn the drain task over `inner` and hand back the enqueue handle
    /// with the fatal signal's receiving half.
    ///
    /// Must be called inside a tokio runtime — the drain task is where the
    /// deadline lives.
    ///
    /// # Panics
    ///
    /// When `capacity_bytes` is 0 — a buffer that arms on its first byte
    /// is a misconfiguration, refused at the construction site — or when
    /// `drain_deadline` is zero, which would fire die-loudly on the first
    /// scheduling hiccup instead of on a stalled parent.
    pub fn new<W>(inner: W, config: WriterConfig) -> (Self, FatalSignal)
    where
        W: AsyncWrite + Unpin + Send + 'static,
    {
        assert!(
            config.capacity_bytes >= 1,
            "capacity_bytes must be at least 1"
        );
        assert!(
            !config.drain_deadline.is_zero(),
            "drain_deadline must be nonzero"
        );
        assert!(
            config.drain_deadline <= MAX_DRAIN_DEADLINE,
            "drain_deadline must be at most {MAX_DRAIN_DEADLINE:?}"
        );
        // The ceiling is the arming line times a small factor, so a
        // capacity that cannot be multiplied has no ceiling: the product
        // saturates at `usize::MAX`, nothing can ever exceed it, and the
        // bound this writer advertises quietly stops existing — leaving
        // the byte accounting to overflow instead of sealing. Refused
        // here, where the number came from.
        assert!(
            config.capacity_bytes <= MAX_CAPACITY_BYTES,
            "capacity_bytes must leave room for the {HARD_OVERFLOW_FACTOR}x hard overflow ceiling"
        );
        let (tx, rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            capacity_bytes: config.capacity_bytes,
            fatal: AtomicU8::new(FATAL_NONE),
            state: Mutex::new(BufferState {
                queue: VecDeque::new(),
                buffered: 0,
                over_capacity_since: None,
                sealed: false,
                handle: HandleState::Held,
            }),
            wake: Notify::new(),
            fired: tx,
        });
        let task = tokio::spawn(supervise(inner, Arc::clone(&shared), config));
        (
            Self {
                shared,
                task: Some(task),
            },
            FatalSignal { rx },
        )
    }

    /// Clean shutdown with the completion guarantee `Drop` cannot give:
    /// returns once the drain task has finished — every buffered frame
    /// written, or the tail abandoned against a sink that stayed dead past
    /// its deadline-bounded attempts.
    ///
    /// The returned [`ShutdownOutcome`] says which. `Flushed` only when
    /// every buffered frame reached the sink. `Abandoned` when the task
    /// returned but left the tail undelivered — die-loudly on a
    /// non-reading parent, or the runtime exiting around a stalled sink,
    /// where [`FatalSignal::is_fired`] tells those two apart. `Faulted`
    /// when the task did not return on its own terms: it panicked — the
    /// sink's `poll_write` is caller code — or was cancelled by a runtime
    /// shutting down around it. The writer is sealed and the fatal has
    /// fired in that case too, so a listener is woken rather than left
    /// waiting on a task that no longer exists.
    pub async fn shutdown(mut self) -> ShutdownOutcome {
        lock(&self.shared.state).handle = HandleState::AwaitedShutdown;
        self.shared.wake.notify_one();
        match self.task.take() {
            // The drain task ends on its own once the handle is marked
            // shutting down; awaiting it is what makes the flush a
            // guarantee rather than a race against runtime teardown.
            Some(task) => task.await.unwrap_or(ShutdownOutcome::Faulted),
            None => ShutdownOutcome::Flushed,
        }
    }

    /// Non-blocking enqueue of one framed message. Never waits and never
    /// drops an accepted frame: a frame the sink cannot take yet is
    /// buffered, and a sink that stays stalled past the drain deadlines
    /// ends in die-loudly, not in lost frames — dropping a protocol frame
    /// would corrupt the wire for whatever conversation survives it. The
    /// buffer's byte growth is capped synchronously at the hard overflow
    /// ceiling ([`HARD_OVERFLOW_FACTOR`] × the arming line): a frame that
    /// would cross it is refused as [`WriterError::Sealed`] and the
    /// die-loudly transition happens right here, under this call's lock —
    /// a producer that never yields to the runtime cannot outgrow the
    /// bound while the drain task waits for a turn that may never come.
    pub fn enqueue(&self, frame: Bytes) -> Result<(), WriterError> {
        let ceiling_discard = {
            let mut state = lock(&self.shared.state);
            if state.sealed {
                return Err(WriterError::Sealed);
            }
            if frame.is_empty() {
                // Nothing to write; queueing it would only hand the sink
                // a zero-length write that reads as a dead sink.
                return Ok(());
            }
            // Representable because the constructor refused a capacity
            // that could not be multiplied.
            let ceiling = self.shared.capacity_bytes * HARD_OVERFLOW_FACTOR;
            if state.buffered.saturating_add(frame.len()) > ceiling {
                state.sealed = true;
                state.buffered = 0;
                state.over_capacity_since = None;
                Some(std::mem::take(&mut state.queue))
            } else {
                state.buffered += frame.len();
                if state.buffered >= self.shared.capacity_bytes
                    && state.over_capacity_since.is_none()
                {
                    state.over_capacity_since = Some(tokio::time::Instant::now());
                }
                state.queue.push_back(frame);
                None
            }
        };
        let Some(discarded) = ceiling_discard else {
            self.shared.wake.notify_one();
            return Ok(());
        };
        // The synchronous terminal: the discarded buffer freed outside the
        // lock, the fatal fired from this very call, and the drain task
        // woken to attempt the best-effort farewell on its next turn.
        //
        // The ordering differs from the deadline paths deliberately, and
        // the difference is worth stating: `die_loudly` seals, logs,
        // attempts the farewell, and only then fires, because it *is* the
        // drain task and can await. This path cannot await, and firing
        // later would mean not firing at all against the producer this
        // ceiling exists for — one that never yields the runtime a turn.
        // So the fatal goes out first and the farewell rides the task's
        // next turn, which means it races a main loop that tears the
        // runtime down on the signal. That is within the farewell's stated
        // nature (best-effort against a parent that has stopped reading),
        // the log above is the diagnostic that does not race, and a
        // shutdown path that wants the attempt made can await
        // `BoundedWriter::shutdown`, which joins the drain task.
        drop(discarded);
        self.shared
            .claim_fatal("write buffer outgrew the hard overflow ceiling");
        // Nothing here can await, so the signal goes out now and the
        // farewell is left to the drain task's next turn.
        self.shared.signal_fatal();
        self.shared.wake.notify_one();
        Err(WriterError::Sealed)
    }
}

impl Drop for BoundedWriter {
    fn drop(&mut self) {
        // Best-effort half of the shutdown contract: the drain task is
        // told to finish and flush, but nothing awaits it here — a Drop
        // cannot. `shutdown` is the guaranteed path, and this runs at the
        // end of it too, which is why it only marks a handle that was
        // still held: an awaited shutdown must not be downgraded to a
        // bare drop on its way out.
        {
            let mut state = lock(&self.shared.state);
            if state.handle == HandleState::Held {
                state.handle = HandleState::Dropped;
            }
        }
        self.shared.wake.notify_one();
    }
}

/// How many drain deadlines the buffer may sit at or past the arming line
/// before die-loudly fires regardless of progress. The per-attempt
/// deadline catches a parent that stopped reading; this absolute one
/// catches a parent that reads just enough to keep resetting it while the
/// buffer grows — trickling below the producer's rate is not reading in
/// any sense that keeps the process healthy. A multiple rather than a
/// second config knob until the transport stage wires real stdout and can
/// say what a defensible tunable looks like.
const SUSTAINED_OVERFLOW_FACTOR: u32 = 4;

/// The largest drain deadline the writer accepts, checked at
/// construction. The deadlines are multiplied and added to monotonic
/// instants; a value past this has stopped being a tuning knob, and
/// refusing it at the call site keeps a deployment typo from becoming an
/// arithmetic panic that would kill the drain task before it could seal or
/// fire the fatal — leaving a listener waiting on a signal that can never
/// come, the exact wedge this primitive exists to prevent.
const MAX_DRAIN_DEADLINE: Duration = Duration::from_secs(60 * 60);

/// The synchronous byte half of the same bound: how many arming lines of
/// bytes `enqueue` will hold before it performs the terminal transition
/// itself, under its own lock. The deadlines above live in the drain task,
/// and a producer that never yields to the runtime could otherwise outgrow
/// memory while that task waits for a turn — the ceiling makes the bound
/// hold with no scheduler cooperation at all.
const HARD_OVERFLOW_FACTOR: usize = 4;

/// The largest `capacity_bytes` [`BoundedWriter::new`] accepts. Past it the
/// [`HARD_OVERFLOW_FACTOR`]× ceiling would not fit in a `usize`, and the
/// constructor asserts against exactly this bound. It is target-dependent —
/// `usize::MAX` is a quarter the size on a 32-bit build — so a caller sizing a
/// frame cap must clamp to it rather than to a fixed literal, or a value that
/// is valid on 64-bit can overshoot the bound and panic startup on 32-bit.
pub const MAX_CAPACITY_BYTES: usize = usize::MAX / HARD_OVERFLOW_FACTOR;

/// The drain task with its last promise kept: however it ends, a listener
/// waiting on the fatal is either woken or was never owed a signal — and
/// a shutdown the runtime cut short is not owed one.
///
/// The sink is caller-supplied and `poll_write` is its code, so the task
/// can die by panic; a runtime tearing down can cancel it mid-write. Both
/// leave the transport unwritable, which is the die-loudly condition
/// exactly — and both would otherwise leave the buffer unsealed and the
/// signal unfired, so a runtime awaiting the fatal would wait for a task
/// that no longer exists. The guard fires on the way out unless the drain
/// returned on its own terms.
async fn supervise<W>(inner: W, shared: Arc<Shared>, config: WriterConfig) -> ShutdownOutcome
where
    W: AsyncWrite + Unpin,
{
    let mut guard = TaskGuard {
        shared: Arc::clone(&shared),
        defused: false,
    };
    let outcome = run(inner, shared, config).await;
    guard.defused = true;
    outcome
}

impl Shared {
    /// Claim the announcement and emit its diagnostic, or find that
    /// another path got there first. The claim and the log go together so
    /// that one cause is recorded — the first true one — rather than a
    /// later path relabelling the same death.
    ///
    /// The diagnostic is delivered inside a containment boundary, because
    /// the installed tracing subscriber is the embedder's code and every
    /// caller here signals immediately afterwards. A subscriber that
    /// panics would otherwise leave the fatal claimed and never
    /// signalled — a listener waiting forever on a writer that is already
    /// dead, which is the wedge this module exists to forbid, and the
    /// hard-ceiling path has no backstop for it: that panic unwinds on a
    /// producer's thread, and a drain task that later returns normally
    /// defuses its own guard. From [`TaskGuard::drop`] the same panic is
    /// worse still, landing during the unwind of an already-panicking
    /// drain task and ending the process outright. Nothing is held across
    /// the call but the claim the atomic already made, so asserting
    /// unwind safety states a fact rather than a hope.
    ///
    /// What no discipline here can outrun is a subscriber that *blocks*:
    /// `tracing` delivers on the calling thread, so one that never
    /// returns stalls whatever path was announcing. That exposure is the
    /// embedder's to manage and is shared by every log statement in this
    /// crate; what is specific to this one — that a failed announcement
    /// could cancel the exit it was announcing — is what the containment
    /// removes.
    fn claim_fatal(&self, cause: &'static str) -> bool {
        if self
            .fatal
            .compare_exchange(
                FATAL_NONE,
                FATAL_CLAIMED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return false;
        }
        // A failed announcement is discarded rather than reported: the
        // subscriber that would carry any report of it is the thing that
        // just failed. The signal the callers send next is what a runtime
        // acts on, and it still goes out.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracing::error!(
                code = "stdout_blocked",
                cause,
                "the transport writer cannot continue; signalling runtime exit — there is no \
                 recovery from a caller that has stopped reading"
            );
        }));
        true
    }

    /// Send the signal unless it has already gone out. Separate from the
    /// claim because the deadline paths attempt their farewell in between
    /// — and because a cancellation landing in that gap must still be able
    /// to signal, or a listener waits on a task that no longer exists.
    fn signal_fatal(&self) {
        if self.fatal.swap(FATAL_SIGNALLED, Ordering::AcqRel) != FATAL_SIGNALLED {
            let _ = self.fired.send(true);
        }
    }
}

/// How long the next write attempt may run: the whole drain deadline when
/// no verdict is pending, and otherwise the time until the nearest one —
/// the zero-progress deadline while the buffer has been over the arming
/// line for less than that, the sustained-overflow deadline after. The
/// floor keeps a deadline that has just passed from spinning the loop.
fn write_budget(
    shared: &Shared,
    config: &WriterConfig,
    last_progress: tokio::time::Instant,
) -> Duration {
    const FLOOR: Duration = Duration::from_millis(1);

    let Some(since) = lock(&shared.state).over_capacity_since else {
        return config.drain_deadline;
    };
    let next = zero_progress_deadline(since, last_progress, config)
        .min(since + config.drain_deadline * SUSTAINED_OVERFLOW_FACTOR);
    next.saturating_duration_since(tokio::time::Instant::now())
        .max(FLOOR)
        .min(config.drain_deadline)
}

/// When silence becomes a verdict: a full drain deadline after whichever
/// came later, the last time the sink took bytes or the moment the buffer
/// crossed the arming line. Both halves matter — progress restarts the
/// clock, and a buffer that has only just armed gets its whole window
/// rather than inheriting however long the sink had already been quiet
/// while there was still room.
fn zero_progress_deadline(
    armed_since: tokio::time::Instant,
    last_progress: tokio::time::Instant,
    config: &WriterConfig,
) -> tokio::time::Instant {
    last_progress.max(armed_since) + config.drain_deadline
}

/// Fires the terminal state if the drain task ends any way but returning.
struct TaskGuard {
    shared: Arc<Shared>,
    defused: bool,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        if self.defused {
            return;
        }
        // Plain `lock()` would re-panic during an unwind; losing the seal
        // matters more than reporting the poison here. Sealing happens
        // whatever ended the task: nothing will drain what is buffered
        // now, so admitting more of it would only be a lie.
        let handle = match self.shared.state.lock() {
            Ok(mut state) => {
                state.sealed = true;
                state.buffered = 0;
                state.over_capacity_since = None;
                state.queue.clear();
                state.handle
            }
            Err(_) => HandleState::Held,
        };
        // A fatal already claimed must be finished regardless of how this
        // task ended — that claimant was cancelled between saying what
        // happened and signalling it, and a listener is waiting.
        if self.shared.fatal.load(Ordering::Acquire) != FATAL_NONE {
            self.shared.signal_fatal();
            return;
        }
        if handle == HandleState::Dropped {
            // The runtime cut short a shutdown nobody is waiting on. That
            // is not a transport failure and must not be reported as one:
            // the drop contract promises no fatal, and a supervisor that
            // maps the signal onto an exit code would otherwise call a
            // clean exit a crash. An *awaited* shutdown is the opposite
            // case — its caller is waiting to be told, and its listeners
            // are owed the signal — so it falls through.
            tracing::debug!(
                "the transport writer's drain task was cancelled after its handle was \
                 dropped; abandoning the undrained tail without a fatal"
            );
            return;
        }
        // Nothing had gone wrong and nobody asked for a shutdown, so the
        // task dying is itself the failure.
        self.shared
            .claim_fatal("the drain task panicked or was cancelled");
        self.shared.signal_fatal();
    }
}

/// The drain task: move buffered bytes into the sink, one deadline-bounded
/// attempt at a time. Progress restarts the per-attempt clock; an attempt
/// that expires with the buffer at or past the arming line is the
/// sustained non-drain the die-loudly contract names, and dies loudly —
/// as does a buffer held over the line for [`SUSTAINED_OVERFLOW_FACTOR`]
/// deadlines outright, however much trickle arrived in between.
async fn run<W>(mut inner: W, shared: Arc<Shared>, config: WriterConfig) -> ShutdownOutcome
where
    W: AsyncWrite + Unpin,
{
    // The front frame rides here while partially written; its unwritten
    // tail stays counted in `buffered`.
    let mut current: Option<Bytes> = None;
    // When the sink last accepted anything. The zero-progress verdict is
    // measured from here rather than from the length of an attempt: an
    // attempt is now cut short at the next policy instant, so its own
    // duration says nothing about how long the sink has been silent.
    let mut last_progress = tokio::time::Instant::now();
    // Whether bytes of `current` have already reached the sink. A farewell
    // written on top of a half-delivered frame is not a message — under
    // length-prefixed framing the parent is still reading the previous
    // frame's body and would swallow it as the tail.
    let mut mid_frame = false;
    loop {
        let (sealed, over_capacity_since) = {
            let state = lock(&shared.state);
            (state.sealed, state.over_capacity_since)
        };
        if sealed {
            // The hard-ceiling path in `enqueue` performed the terminal
            // transition and fired the fatal; the task's remaining share
            // is the best-effort farewell.
            attempt_farewell(&mut inner, &config, mid_frame).await;
            return ShutdownOutcome::Abandoned;
        }
        if let Some(since) = over_capacity_since
            && tokio::time::Instant::now()
                >= since
                    + config
                        .drain_deadline
                        .saturating_mul(SUSTAINED_OVERFLOW_FACTOR)
        {
            die_loudly(
                &mut inner,
                &shared,
                &config,
                "buffer over capacity past the sustained deadline",
                mid_frame,
            )
            .await;
            return ShutdownOutcome::Abandoned;
        }
        if current.is_none() {
            let (next, handle_ended) = {
                let mut state = lock(&shared.state);
                (state.queue.pop_front(), state.handle != HandleState::Held)
            };
            match next {
                Some(frame) => {
                    current = Some(frame);
                    mid_frame = false;
                }
                None if handle_ended => {
                    // Clean shutdown: every buffered frame has been written.
                    // The tail counts as delivered only if the final flush
                    // also lands — a buffered sink can accept every write and
                    // then fail or stall on flush — so its result decides the
                    // outcome, not the writes alone.
                    return match tokio::time::timeout(config.drain_deadline, inner.flush()).await {
                        Ok(Ok(())) => ShutdownOutcome::Flushed,
                        _ => ShutdownOutcome::Abandoned,
                    };
                }
                None => {
                    shared.wake.notified().await;
                    continue;
                }
            }
        }
        let chunk = current.as_mut().expect("refilled above");
        // An attempt runs until the next instant a policy verdict could be
        // due, never past it. Bounding it at a flat `drain_deadline`
        // instead lets an attempt already in flight when the buffer arms
        // carry the verdict nearly a second deadline late, and lets the
        // sustained window expire unnoticed until the attempt happens to
        // end — which would make the ceiling this code advertises as four
        // deadlines behave like nearly five.
        let budget = write_budget(&shared, &config, last_progress);
        match tokio::time::timeout(budget, inner.write(&chunk[..])).await {
            Ok(Ok(written)) if written > 0 => {
                last_progress = tokio::time::Instant::now();
                chunk.advance(written);
                {
                    let mut state = lock(&shared.state);
                    // The ceiling path can seal — and zero the accounting —
                    // from another thread while this write is in flight, so
                    // the bytes it just reported may already be accounted
                    // for by a buffer that no longer exists. Subtracting
                    // anyway underflows and kills the drain task before it
                    // can say goodbye; the seal is the whole truth once it
                    // has happened, and the loop top acts on it next.
                    if !state.sealed {
                        state.buffered -= written;
                        if state.buffered < shared.capacity_bytes {
                            state.over_capacity_since = None;
                        }
                    }
                }
                if chunk.is_empty() {
                    current = None;
                    mid_frame = false;
                } else {
                    mid_frame = true;
                }
            }
            // A sink that reports zero acceptance or an error (a closed
            // pipe, most likely) is past not-reading: same terminal path,
            // with the farewell attempt left to fail as it will — unless
            // the handle is already dropped, where the runtime chose its
            // own exit and the clean-shutdown contract holds, exactly as
            // on the timeout path. Zero on a nonempty buffer only — empty
            // frames never enter the queue.
            Ok(Ok(_zero)) => {
                die_loudly(
                    &mut inner,
                    &shared,
                    &config,
                    "sink accepts no bytes",
                    mid_frame,
                )
                .await;
                return ShutdownOutcome::Abandoned;
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, "transport sink failed before the drain deadline");
                die_loudly(&mut inner, &shared, &config, "sink failed", mid_frame).await;
                return ShutdownOutcome::Abandoned;
            }
            Err(_elapsed) => {
                let (sealed, armed_since, handle_ended) = {
                    let state = lock(&shared.state);
                    (
                        state.sealed,
                        state.over_capacity_since,
                        state.handle != HandleState::Held,
                    )
                };
                if sealed {
                    // A ceiling seal that landed while this attempt was in
                    // flight owes a farewell, and a handle drop arriving
                    // afterwards must not cancel it — awaiting `shutdown`
                    // is exactly how a fatal listener asks for this
                    // attempt to be made, so treating that drop as a clean
                    // exit would defeat the one mechanism documented for
                    // it. `die_loudly` arbitrates and attempts.
                    die_loudly(
                        &mut inner,
                        &shared,
                        &config,
                        "farewell owed after ceiling seal",
                        mid_frame,
                    )
                    .await;
                    return ShutdownOutcome::Abandoned;
                }
                if handle_ended {
                    // The runtime let go of the writer and the sink still
                    // will not take the tail: nobody is coming for these
                    // bytes, and firing a fatal during the clean shutdown
                    // the dropped handle announced would contradict it —
                    // however full the buffer is, the runtime is already
                    // exiting by its own choice.
                    tracing::debug!("bounded writer abandoned undrained tail after handle drop");
                    return ShutdownOutcome::Abandoned;
                }
                // The verdict needs both clocks: this attempt made zero
                // progress for a full deadline, AND the buffer has been at
                // or past the arming line for a full deadline. Checking
                // only "armed now" would let an enqueue that crossed the
                // line mid-attempt inherit this attempt's nearly-expired
                // timeout and die almost immediately — arming starts the
                // buffer's own clock, not the attempt's.
                if let Some(since) = armed_since
                    && tokio::time::Instant::now()
                        >= zero_progress_deadline(since, last_progress, &config)
                {
                    die_loudly(
                        &mut inner,
                        &shared,
                        &config,
                        "buffer full past deadline",
                        mid_frame,
                    )
                    .await;
                    return ShutdownOutcome::Abandoned;
                }
                // Under the arming line, or over it for less than a full
                // deadline: not yet the buffer-fills case the runtime
                // exits on. Try again; the clock restarts.
            }
        }
    }
}

/// The exit sequence, in its fixed order: seal (so `enqueue` refuses and
/// the buffer frees), say it loudly, attempt the farewell frame, fire the
/// signal. The verdict is arbitrated inside the seal's own critical
/// section: a handle drop that lands there first wins outright — the
/// clean-shutdown contract promises the fatal never fires once the runtime
/// chose its own exit, and checking `handle_dropped` anywhere outside this
/// lock would leave a window for the drop to arrive between the check and
/// the seal. Reached from the drain task's death paths; the hard-ceiling
/// path in `enqueue` seals under the same lock, so every terminal
/// transition is decided at exactly one place at a time.
async fn die_loudly<W>(
    inner: &mut W,
    shared: &Shared,
    config: &WriterConfig,
    cause: &'static str,
    mid_frame: bool,
) where
    W: AsyncWrite + Unpin,
{
    // One lock decides which of the three ends this is; the acting half
    // runs after it, because the farewell awaits and a guard must not.
    let verdict = {
        let mut state = lock(&shared.state);
        if state.sealed {
            // The ceiling path sealed and fired already, leaving the
            // farewell owed to whichever turn of this task got here
            // first. This is that turn: every caller returns after us, so
            // an early return here would be the attempt never happening.
            Verdict::FarewellOwed
        } else if state.handle != HandleState::Held {
            Verdict::CleanDrop
        } else {
            state.sealed = true;
            state.buffered = 0;
            state.over_capacity_since = None;
            Verdict::Die(std::mem::take(&mut state.queue))
        }
    };
    match verdict {
        Verdict::FarewellOwed => attempt_farewell(inner, config, mid_frame).await,
        Verdict::CleanDrop => tracing::debug!(
            cause,
            "bounded writer abandoned undrained tail after handle drop"
        ),
        Verdict::Die(discarded) => {
            // Freed outside the lock, like every unbounded-size drop in
            // this crate's critical-section discipline.
            drop(discarded);
            shared.claim_fatal(cause);
            // This path can await, so the parent gets its goodbye before
            // the runtime is told to go.
            attempt_farewell(inner, config, mid_frame).await;
            shared.signal_fatal();
        }
    }
}

/// Which end this is, decided under one lock so the acting half can await.
enum Verdict {
    /// Already sealed by the hard-ceiling path, which cannot await: the
    /// farewell it owes is this task's to attempt.
    FarewellOwed,
    /// The runtime chose its own exit before anything went wrong here.
    CleanDrop,
    /// This call is the terminal transition, and carries the buffer it
    /// discarded out of the lock.
    Die(VecDeque<Bytes>),
}

/// The best-effort goodbye: one deadline-bounded try at the pre-encoded
/// farewell frame and a flush, results ignored — against a truly
/// non-reading parent both fail, which is exactly why the tracing log and
/// the [`FatalSignal`] carry the same fact.
///
/// Withheld entirely when a frame was left half-written. The farewell is a
/// framed message, and the parent reading a length-prefixed stream is
/// still consuming the previous frame's body: appending to that gives it
/// the farewell's bytes as somebody else's tail and then a parse error,
/// which is worse than silence — it corrupts the stream instead of
/// explaining it. The fatal signal and the log carry the notice in that
/// case, as they do whenever the sink refuses the frame anyway.
async fn attempt_farewell<W>(inner: &mut W, config: &WriterConfig, mid_frame: bool)
where
    W: AsyncWrite + Unpin,
{
    if mid_frame {
        // Deliberately not a second `stdout_blocked` event: the failure
        // has already been announced under that code, and repeating it
        // here would double-count one death in anything alerting on the
        // code. This is a detail of that announcement, not another one.
        tracing::warn!(
            "the final transport.error frame is withheld: a frame was left half-written, so \
             appending the goodbye would corrupt the stream rather than explain it"
        );
        return;
    }
    // The transport supplies this frame; invoke the producer here, on the
    // fatal path, where the goodbye is actually needed.
    let farewell = (config.farewell)();
    let _ = tokio::time::timeout(config.drain_deadline, async {
        let _ = inner.write_all(&farewell).await;
        let _ = inner.flush().await;
    })
    .await;
}

/// This module's critical sections hold plain state moves that cannot
/// panic, so a poisoned lock is unreachable from its own code; saying so
/// loudly beats recovering a buffer in unknown shape.
fn lock(mutex: &Mutex<BufferState>) -> MutexGuard<'_, BufferState> {
    mutex
        .lock()
        .expect("writer lock poisoned: an enqueue or drain panicked")
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// A scriptable parent-side pipe: accepts a byte budget and stalls
    /// when it runs dry. `farewell_room` stages residual room a stopped
    /// pipe can still hold for exactly the farewell frame — keyed to the
    /// frame's bytes because a timed-out write attempt is re-polled by its
    /// timeout before being dropped, so budget released blindly would feed
    /// the stalled frame instead. What the sink swallowed is recorded so
    /// tests can see the farewell attempt (or its absence).
    #[derive(Debug, Default)]
    struct SinkState {
        budget: usize,
        per_call: usize,
        /// Accepted once, whole, even at zero budget.
        farewell_room: Option<Vec<u8>>,
        /// Fail the next write once — the sink-error death path.
        fail_once: bool,
        /// Fail every `poll_flush` — a buffered sink that took the writes
        /// but cannot get them out.
        fail_flush: bool,
        /// Panic inside `poll_write` — the sink is caller code, and this
        /// is what its dying looks like from in here.
        panic_on_write: bool,
        waker: Option<Waker>,
        written: Vec<u8>,
    }

    struct ScriptedSink(Arc<Mutex<SinkState>>);

    impl ScriptedSink {
        fn top_up(state: &Arc<Mutex<SinkState>>, bytes: usize) {
            let waker = {
                let mut sink = state.lock().unwrap();
                sink.budget += bytes;
                sink.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    impl AsyncWrite for ScriptedSink {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let mut sink = self.0.lock().unwrap();
            assert!(!sink.panic_on_write, "scripted sink panic");
            if sink.fail_once {
                sink.fail_once = false;
                return Poll::Ready(Err(std::io::Error::other("scripted sink failure")));
            }
            if sink.budget == 0 {
                if sink.farewell_room.as_deref() == Some(buf) {
                    sink.farewell_room = None;
                    sink.written.extend_from_slice(buf);
                    return Poll::Ready(Ok(buf.len()));
                }
                sink.waker = Some(cx.waker().clone());
                return Poll::Pending;
            }
            let take = buf.len().min(sink.budget).min(sink.per_call);
            sink.budget -= take;
            sink.written.extend_from_slice(&buf[..take]);
            Poll::Ready(Ok(take))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            if self.0.lock().unwrap().fail_flush {
                return Poll::Ready(Err(std::io::Error::other("scripted flush failure")));
            }
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn sink(
        budget: usize,
        per_call: usize,
        farewell_room: Option<&[u8]>,
    ) -> (ScriptedSink, Arc<Mutex<SinkState>>) {
        let shared = Arc::new(Mutex::new(SinkState {
            budget,
            per_call,
            farewell_room: farewell_room.map(<[u8]>::to_vec),
            ..SinkState::default()
        }));
        (ScriptedSink(Arc::clone(&shared)), shared)
    }

    fn config() -> WriterConfig {
        WriterConfig {
            capacity_bytes: 64,
            drain_deadline: Duration::from_millis(500),
            farewell: || Bytes::from_static(b"FAREWELL"),
        }
    }

    fn frame() -> Bytes {
        Bytes::from(vec![7u8; 8])
    }

    /// The full die-loudly sequence against a sink that never drains:
    /// exactly one firing, the buffer sealed, later enqueues refused.
    #[tokio::test(start_paused = true)]
    async fn bounded_writer_state_machine() {
        let (sink, state) = sink(0, 0, None);
        let (writer, mut fatal) = BoundedWriter::new(sink, config());
        // Fill past the arming line; every enqueue is accepted — the
        // bound is enforced in time by the exit, not by refusing frames.
        for _ in 0..9 {
            writer.enqueue(frame()).unwrap();
        }
        assert!(!fatal.is_fired());
        fatal.fired().await;
        assert!(fatal.is_fired());
        // Sealed: the caller must not buffer further.
        assert_eq!(writer.enqueue(frame()), Err(WriterError::Sealed));
        // The farewell was attempted against the stalled sink and took
        // nothing — best-effort means exactly this.
        assert!(state.lock().unwrap().written.is_empty());
    }

    /// Any forward progress restarts the deadline: a parent that keeps
    /// reading a little, each read inside the deadline, keeps the writer
    /// alive well past several deadlines of wall time — death comes one
    /// full deadline after the last progress.
    #[tokio::test(start_paused = true)]
    async fn partial_progress_resets_the_deadline() {
        let (sink, state) = sink(8, usize::MAX, None);
        let (writer, mut fatal) = BoundedWriter::new(sink, config());
        // 24 frames = 192 bytes: past the 64-byte arming line for the
        // whole test, so only progress keeps the writer alive.
        for _ in 0..24 {
            writer.enqueue(frame()).unwrap();
        }
        let start = tokio::time::Instant::now();
        // Five top-ups, each 300 ms apart — inside the 500 ms deadline.
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(!fatal.is_fired(), "died despite steady partial progress");
            ScriptedSink::top_up(&state, 8);
        }
        fatal.fired().await;
        let elapsed = start.elapsed();
        // Survived the 1.5 s of trickling — three deadlines' worth — and
        // died one deadline after the last top-up.
        assert!(
            elapsed >= Duration::from_millis(1500),
            "the deadline did not reset on progress: died at {elapsed:?}"
        );
        assert_eq!(state.lock().unwrap().written.len(), 48);
    }

    /// A frame larger than the arming line is a frame like any other: a
    /// healthy sink drains it below the line through partial writes, and
    /// no deadline fires. (An empty frame is a no-op rather than a
    /// zero-length write.)
    #[tokio::test(start_paused = true)]
    async fn oversized_frames_drain_on_a_healthy_sink() {
        let (sink, state) = sink(usize::MAX, 16, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        writer.enqueue(Bytes::from(vec![7u8; 128])).unwrap();
        writer.enqueue(Bytes::new()).unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(state.lock().unwrap().written.len(), 128);
        assert!(!fatal.is_fired());
    }

    /// `shutdown` returns only once the buffered tail has been written —
    /// the completion guarantee a bare drop cannot give.
    #[tokio::test(start_paused = true)]
    async fn shutdown_awaits_the_flushed_tail() {
        let (sink, state) = sink(usize::MAX, 8, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        for _ in 0..4 {
            writer.enqueue(frame()).unwrap();
        }
        assert_eq!(writer.shutdown().await, ShutdownOutcome::Flushed);
        assert_eq!(state.lock().unwrap().written.len(), 32);
        assert!(!fatal.is_fired());
    }

    /// A shutdown that cannot drain the tail reports it. With the sink dead
    /// but the buffer under the arming line — so the over-capacity
    /// die-loudly never fires — the drain abandons the stuck frames and
    /// returns `Abandoned`, not the `Flushed` a bare "the task returned"
    /// would have given. The fatal stays unfired, which is exactly why the
    /// outcome has to carry the loss: a caller watching only the fatal
    /// would exit as though the goodbye had landed.
    #[tokio::test(start_paused = true)]
    async fn shutdown_reports_an_abandoned_tail_against_a_stalled_sink() {
        let (sink, state) = sink(0, 0, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        // Four frames = 32 bytes, under the 64-byte arming line.
        for _ in 0..4 {
            writer.enqueue(frame()).unwrap();
        }
        assert_eq!(writer.shutdown().await, ShutdownOutcome::Abandoned);
        assert!(
            state.lock().unwrap().written.is_empty(),
            "the stalled sink received nothing, yet the tail was reported lost, not flushed"
        );
        assert!(
            !fatal.is_fired(),
            "an abandoned shutdown tail is not a fatal"
        );
    }

    /// A shutdown whose writes all land but whose final flush does not still
    /// reports `Abandoned`: a buffered sink can accept every byte and then
    /// fail to get them out, so the writes landing is not the tail arriving.
    #[tokio::test(start_paused = true)]
    async fn shutdown_reports_abandoned_when_the_final_flush_fails() {
        let (sink, state) = sink(usize::MAX, 8, None);
        state.lock().unwrap().fail_flush = true;
        let (writer, fatal) = BoundedWriter::new(sink, config());
        for _ in 0..4 {
            writer.enqueue(frame()).unwrap();
        }
        assert_eq!(writer.shutdown().await, ShutdownOutcome::Abandoned);
        // Every write was accepted — but the flush that would deliver them
        // failed, so the outcome is the loss, not a clean flush.
        assert_eq!(state.lock().unwrap().written.len(), 32);
        assert!(!fatal.is_fired());
    }

    /// Partial progress resets the per-attempt deadline but not the
    /// sustained one: a parent trickling a few bytes inside every
    /// deadline, forever, still meets the exit once the buffer has sat
    /// over the arming line for the sustained-overflow window.
    #[tokio::test(start_paused = true)]
    async fn trickling_reader_cannot_stave_off_the_exit() {
        let (sink, state) = sink(8, usize::MAX, None);
        let (writer, mut fatal) = BoundedWriter::new(sink, config());
        // 24 frames = 192 bytes: over the 64-byte line for the whole test.
        for _ in 0..24 {
            writer.enqueue(frame()).unwrap();
        }
        let start = tokio::time::Instant::now();
        // Top-ups every 300 ms — always inside the 500 ms per-attempt
        // deadline, so only the sustained clock can end this.
        for _ in 0..8 {
            tokio::time::sleep(Duration::from_millis(300)).await;
            ScriptedSink::top_up(&state, 8);
        }
        fatal.fired().await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(2_000),
            "died before the sustained window despite steady trickle: {elapsed:?}"
        );
        assert_eq!(writer.enqueue(frame()), Err(WriterError::Sealed));
    }

    /// A dropped handle is a clean shutdown even when the tail cannot
    /// drain: the buffer may be full and the sink dead, and the fatal
    /// still must not fire — the runtime already chose to exit.
    #[tokio::test(start_paused = true)]
    async fn dropped_handle_with_a_stalled_full_buffer_never_fires() {
        let (sink, state) = sink(0, 0, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        for _ in 0..9 {
            writer.enqueue(frame()).unwrap();
        }
        drop(writer);
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(!fatal.is_fired(), "a clean shutdown produced a fatal");
        assert!(state.lock().unwrap().written.is_empty());
    }

    /// Arming starts the buffer's own clock: an enqueue that crosses the
    /// line while a write attempt is already mid-timeout must still get a
    /// full deadline before the verdict, not inherit the attempt's nearly
    /// expired one.
    #[tokio::test(start_paused = true)]
    async fn arming_mid_attempt_still_gets_a_full_deadline() {
        // The farewell is accepted instantly, so the fatal's timing
        // reflects when the verdict was reached rather than how long the
        // goodbye took against a dead sink.
        let (sink, _state) = sink(0, 0, Some(b"FAREWELL"));
        let (writer, mut fatal) = BoundedWriter::new(sink, config());
        // Under the line: the write attempt stalls but nothing arms.
        for _ in 0..4 {
            writer.enqueue(frame()).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        // Cross the line 400 ms into the attempt's 500 ms timeout.
        let armed_at = tokio::time::Instant::now();
        for _ in 0..8 {
            writer.enqueue(frame()).unwrap();
        }
        fatal.fired().await;
        let armed_for = armed_at.elapsed();
        assert!(
            armed_for >= Duration::from_millis(500),
            "died only {armed_for:?} after arming — the attempt's clock leaked into the verdict"
        );
        assert!(
            armed_for < Duration::from_millis(550),
            "died {armed_for:?} after arming — an attempt in flight carried the verdict late"
        );
    }

    /// The hard ceiling holds with zero scheduler cooperation: a producer
    /// that never yields cannot outgrow it, because the enqueue that
    /// crosses it performs the terminal transition itself — synchronously,
    /// fatal included.
    #[tokio::test(start_paused = true)]
    async fn hard_ceiling_seals_synchronously_without_scheduler_help() {
        let (sink, _state) = sink(0, 0, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        // Ceiling = 4 × the 64-byte line = 256 bytes: 32 eight-byte
        // frames fit exactly; the 33rd must be refused. No await between
        // enqueues — the drain task never gets a turn, which is the point.
        for i in 0..33 {
            let outcome = writer.enqueue(frame());
            if i < 32 {
                assert_eq!(outcome, Ok(()), "frame {i} is under the ceiling");
                assert!(!fatal.is_fired());
            } else {
                assert_eq!(outcome, Err(WriterError::Sealed));
            }
        }
        // Fired by the enqueue itself, observable before any await.
        assert!(fatal.is_fired());
        assert_eq!(writer.enqueue(frame()), Err(WriterError::Sealed));
    }

    /// A tracing subscriber that panics on delivery — the embedder's own
    /// code failing at the moment the writer tries to announce its death.
    struct PanickingSubscriber;

    impl tracing::Subscriber for PanickingSubscriber {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, _: &tracing::Event<'_>) {
            panic!("the embedder's tracing subscriber panicked");
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// The diagnostic must not be able to cancel the exit it describes.
    /// The ceiling path announces on a producer's thread and signals
    /// immediately after, with no backstop between the two: a subscriber
    /// that panics there would leave the fatal claimed and never sent,
    /// and a drain task that goes on to return normally defuses the one
    /// guard that could have finished it — so a listener would wait
    /// forever on a writer that is already sealed.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_log_subscriber_cannot_swallow_the_fatal() {
        let (sink, _state) = sink(0, 0, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        // One frame past the 256-byte ceiling, so this single enqueue is
        // the terminal transition and the announcement happens under it.
        let refusal = tracing::subscriber::with_default(PanickingSubscriber, || {
            writer.enqueue(Bytes::from(vec![7u8; 257]))
        });

        assert_eq!(
            refusal,
            Err(WriterError::Sealed),
            "a panicking subscriber escaped into the producer's enqueue"
        );
        assert!(
            fatal.is_fired(),
            "a panicking log subscriber suppressed the fatal the runtime waits on"
        );
    }

    /// An awaited shutdown is the opposite case from a bare drop: the
    /// caller is waiting to be told what happened, and its listeners are
    /// owed the signal. A sink that panics once `shutdown` is under way is
    /// a failure, not the tail of a clean exit.
    #[tokio::test(start_paused = true)]
    async fn a_panic_during_an_awaited_shutdown_still_fires() {
        let (sink, state) = sink(usize::MAX, usize::MAX, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        writer.enqueue(frame()).unwrap();
        // The sink dies exactly when the awaited drain reaches it.
        state.lock().unwrap().panic_on_write = true;

        assert_eq!(
            writer.shutdown().await,
            ShutdownOutcome::Faulted,
            "a panicking drain is not a clean shutdown"
        );
        assert!(
            fatal.is_fired(),
            "an awaited shutdown swallowed the failure its caller was waiting for"
        );
    }

    /// A runtime tearing down after a plain handle drop cancels the
    /// detached drain task. That is the tail end of a shutdown the caller
    /// asked for, not a transport failure, and the drop contract promises
    /// no fatal for it — a supervisor mapping the signal onto an exit code
    /// would otherwise report a crash for a clean exit.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_task_after_a_plain_drop_is_not_a_fatal() {
        let (sink, _state) = sink(0, 0, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        // Buffered work the sink will never take, so the tail is genuinely
        // undrained when the teardown arrives.
        for _ in 0..4 {
            writer.enqueue(frame()).unwrap();
        }
        let shared = Arc::clone(&writer.shared);
        drop(writer);

        // The teardown: the runtime cancels the task it no longer owns.
        drop(TaskGuard {
            shared,
            defused: false,
        });

        assert!(
            !fatal.is_fired(),
            "a shutdown the runtime cut short was reported as a transport failure"
        );
    }

    /// The runtime reacting to the fatal by tearing down the drain task
    /// mid-goodbye must not produce a second fatal. That cancellation runs
    /// the guard, and the guard's job is to cover the case where nothing
    /// had signalled yet — not to announce the same death twice to a
    /// listener that is already acting on the first.
    #[tokio::test(start_paused = true)]
    async fn a_cancellation_after_the_fatal_does_not_repeat_it() {
        let (sink, _state) = sink(0, 0, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        while writer.enqueue(frame()).is_ok() {}
        assert!(fatal.is_fired(), "the ceiling fires synchronously");

        // A listener that has already seen the signal.
        let mut seen = fatal.clone();
        assert!(*seen.rx.borrow_and_update());

        // The teardown the signal provoked, arriving while the drain task
        // still owed its farewell.
        drop(TaskGuard {
            shared: Arc::clone(&writer.shared),
            defused: false,
        });

        assert!(
            tokio::time::timeout(Duration::from_secs(1), seen.rx.changed())
                .await
                .is_err(),
            "the cancellation announced the same fatal a second time"
        );
    }

    /// A drain task that dies — a panicking sink, or a runtime cancelling
    /// it mid-write — leaves the transport unwritable, which is the
    /// die-loudly condition. It must seal and signal on the way out, or a
    /// runtime waiting for the fatal waits on a task that no longer
    /// exists; the timeout below is what turns that wedge into a failure
    /// instead of a hung test.
    #[tokio::test(start_paused = true)]
    async fn a_drain_task_that_dies_still_seals_and_signals() {
        let (sink, state) = sink(usize::MAX, usize::MAX, None);
        state.lock().unwrap().panic_on_write = true;
        let (writer, mut fatal) = BoundedWriter::new(sink, config());
        writer.enqueue(frame()).unwrap();

        tokio::time::timeout(Duration::from_secs(5), fatal.fired())
            .await
            .expect("the fatal must fire when the drain task dies");
        assert_eq!(
            writer.enqueue(frame()),
            Err(WriterError::Sealed),
            "a dead drain seals the writer"
        );
        assert_eq!(
            writer.shutdown().await,
            ShutdownOutcome::Faulted,
            "an abnormal end must not report a clean drain"
        );
    }

    /// The farewell a ceiling seal owes is still attempted when a fatal
    /// listener does exactly what the contract tells it to: await
    /// `shutdown`, which joins the drain task. A dropped handle must not
    /// be mistaken for a clean exit *after* the seal — that would cancel
    /// the very attempt the mechanism exists to give a turn.
    #[tokio::test(start_paused = true)]
    async fn shutdown_after_a_ceiling_seal_still_says_goodbye() {
        let (sink, state) = sink(0, usize::MAX, Some(b"FAREWELL"));
        let (writer, fatal) = BoundedWriter::new(sink, config());
        for _ in 0..4 {
            writer.enqueue(frame()).unwrap();
        }
        // Let the drain task stall inside a write, then cross the ceiling.
        tokio::time::sleep(Duration::from_millis(1)).await;
        while writer.enqueue(frame()).is_ok() {}
        assert!(fatal.is_fired());

        assert_eq!(
            writer.shutdown().await,
            ShutdownOutcome::Abandoned,
            "the drain ends on its own terms but reports the tail undelivered"
        );
        let written = state.lock().unwrap().written.clone();
        assert!(
            written.ends_with(b"FAREWELL"),
            "the owed farewell was cancelled by the shutdown that asked for it"
        );
    }

    /// The same owed farewell when the stalled write ends in a sink error
    /// rather than a timeout: that path returns from the task directly, so
    /// leaving the attempt to the loop top would mean never making it.
    #[tokio::test(start_paused = true)]
    async fn a_sink_error_after_a_ceiling_seal_still_says_goodbye() {
        let (sink, state) = sink(0, usize::MAX, Some(b"FAREWELL"));
        let (writer, fatal) = BoundedWriter::new(sink, config());
        for _ in 0..4 {
            writer.enqueue(frame()).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
        while writer.enqueue(frame()).is_ok() {}
        assert!(fatal.is_fired());

        // Wake the stalled write into a failure.
        state.lock().unwrap().fail_once = true;
        ScriptedSink::top_up(&state, 0);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let written = state.lock().unwrap().written.clone();
        assert!(
            written.ends_with(b"FAREWELL"),
            "the sink-error path skipped the farewell the ceiling still owed"
        );
    }

    /// A farewell appended to a half-written frame is not a message: the
    /// parent is still reading the previous frame's body under
    /// length-prefixed framing, so it would swallow the goodbye as that
    /// frame's tail and then fail to parse. Withheld in that case — the
    /// log and the fatal carry the notice instead of the stream carrying
    /// corruption.
    #[tokio::test(start_paused = true)]
    async fn a_half_written_frame_withholds_the_farewell() {
        // Four bytes of the first eight-byte frame reach the sink, then it
        // stalls: the writer dies mid-frame.
        let (sink, state) = sink(4, usize::MAX, Some(b"FAREWELL"));
        let (writer, mut fatal) = BoundedWriter::new(sink, config());
        for _ in 0..12 {
            writer.enqueue(frame()).unwrap();
        }
        fatal.fired().await;
        let written = state.lock().unwrap().written.clone();
        assert_eq!(written.len(), 4, "only the partial frame reached the sink");
        assert!(
            !written.ends_with(b"FAREWELL"),
            "the farewell was appended to a half-written frame"
        );
    }

    /// A capacity too large to multiply has no ceiling at all — the
    /// product saturates and nothing can exceed it — so it is refused
    /// where it was written rather than silently turning a bounded writer
    /// into an unbounded queue.
    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "capacity_bytes")]
    async fn a_capacity_without_room_for_the_ceiling_is_refused() {
        let (sink, _state) = sink(0, 0, None);
        let _ = BoundedWriter::new(
            sink,
            WriterConfig {
                capacity_bytes: usize::MAX,
                ..config()
            },
        );
    }

    /// A deadline large enough to overflow the sustained-window
    /// arithmetic is refused where it can be fixed. Left to run, it would
    /// panic inside the drain task — which seals nothing and fires
    /// nothing, leaving a listener waiting on a signal that can never
    /// come.
    #[tokio::test(start_paused = true)]
    #[should_panic(expected = "drain_deadline")]
    async fn an_unrepresentable_drain_deadline_is_refused_at_construction() {
        let (sink, _state) = sink(0, 0, None);
        let _ = BoundedWriter::new(
            sink,
            WriterConfig {
                drain_deadline: Duration::MAX,
                ..config()
            },
        );
    }

    /// The ceiling can seal from another thread while a write is in
    /// flight, zeroing an accounting the completing write still expects to
    /// decrement. The drain task must survive that and go on to say
    /// goodbye — an underflow here would kill it silently, one panic short
    /// of the farewell the exit contract promises.
    #[tokio::test(start_paused = true)]
    async fn a_ceiling_seal_during_an_in_flight_write_keeps_the_drain_alive() {
        let (sink, state) = sink(0, usize::MAX, Some(b"FAREWELL"));
        let (writer, fatal) = BoundedWriter::new(sink, config());
        // Well under the 256-byte ceiling: enough for the drain task to
        // pick a frame up and stall inside its write.
        for _ in 0..4 {
            writer.enqueue(frame()).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
        // Cross the ceiling while that write is still pending.
        while writer.enqueue(frame()).is_ok() {}
        assert!(fatal.is_fired(), "the ceiling fires synchronously");
        // Now let the in-flight write complete with progress.
        ScriptedSink::top_up(&state, 8);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let written = state.lock().unwrap().written.clone();
        assert!(
            written.ends_with(b"FAREWELL"),
            "the drain task died before its farewell; wrote {} bytes",
            written.len()
        );
    }

    /// A healthy sink drains everything and a dropped handle is a clean
    /// shutdown: the tail is flushed and the fatal never fires.
    #[tokio::test(start_paused = true)]
    async fn clean_shutdown_flushes_and_never_fires() {
        let (sink, state) = sink(usize::MAX, usize::MAX, None);
        let (writer, fatal) = BoundedWriter::new(sink, config());
        for _ in 0..4 {
            writer.enqueue(frame()).unwrap();
        }
        drop(writer);
        // Give the drain task the runtime; paused time auto-advances.
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(state.lock().unwrap().written.len(), 32);
        assert!(!fatal.is_fired());
    }

    /// The farewell frame reaches a sink that still has residual room at
    /// the moment of death — a pipe with slack left when its reader
    /// stopped — and reaches it exactly once.
    #[tokio::test(start_paused = true)]
    async fn farewell_is_attempted_once_where_room_exists() {
        // Room for two frames before the stall, plus residual room the
        // pipe will yield only to the farewell frame.
        let (sink, state) = sink(16, usize::MAX, Some(b"FAREWELL"));
        let (writer, mut fatal) = BoundedWriter::new(sink, config());
        // 12 frames = 96 bytes; 80 remain buffered at the stall, past the
        // 64-byte arming line.
        for _ in 0..12 {
            writer.enqueue(frame()).unwrap();
        }
        fatal.fired().await;
        let written = state.lock().unwrap().written.clone();
        assert!(
            written.ends_with(b"FAREWELL"),
            "written ({} bytes): {:?}",
            written.len(),
            String::from_utf8_lossy(&written)
        );
        let farewells = written
            .windows(b"FAREWELL".len())
            .filter(|window| window == b"FAREWELL")
            .count();
        assert_eq!(farewells, 1, "the farewell is exactly-once");
    }
}
