//! The bounded write buffer with die-loudly semantics.
//!
//! The design's flow-control table ends at the process boundary: if the
//! caller stops reading stdout and the write buffer fills, the runtime
//! emits one final `transport.error` and exits — there is no recovery from
//! a non-reading parent, and wedging silently against one is the failure
//! mode this whole policy family exists to forbid. This module is that
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
    pub capacity_bytes: usize,
    /// How long the sink may make zero progress — with the buffer at or
    /// past `capacity_bytes` — before die-loudly fires. Forward progress
    /// restarts this clock, but not the sustained-overflow one above it.
    pub drain_deadline: Duration,
    /// The pre-encoded final frame — the one `transport.error` of code
    /// `stdout_blocked` — attempted best-effort on the way down. Encoded
    /// by the caller because framing belongs to the transport layer, not
    /// to this crate; against a truly non-reading parent the attempt
    /// usually fails, which is why it is best-effort and why the tracing
    /// log and the [`FatalSignal`] carry the same fact.
    pub farewell: Bytes,
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

/// Fires at most once, when die-loudly has: the runtime main loop listens
/// and runs the graceful child-cleanup path — the *transport* is
/// unrecoverable; PTY cleanup still runs — then exits nonzero. Cheap to
/// clone; every clone observes the same state, and one that attaches after
/// the firing still sees it.
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
/// `shutdown` instead. Neither shape ever fires the fatal.
#[derive(Debug)]
pub struct BoundedWriter {
    shared: Arc<Shared>,
    task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug)]
struct Shared {
    capacity_bytes: usize,
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
    handle_dropped: bool,
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
        let (tx, rx) = watch::channel(false);
        let shared = Arc::new(Shared {
            capacity_bytes: config.capacity_bytes,
            state: Mutex::new(BufferState {
                queue: VecDeque::new(),
                buffered: 0,
                over_capacity_since: None,
                sealed: false,
                handle_dropped: false,
            }),
            wake: Notify::new(),
            fired: tx,
        });
        let task = tokio::spawn(run(inner, Arc::clone(&shared), config));
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
    /// its deadline-bounded attempts. The fatal never fires on this path;
    /// the runtime is exiting by its own choice.
    pub async fn shutdown(mut self) {
        lock(&self.shared.state).handle_dropped = true;
        self.shared.wake.notify_one();
        if let Some(task) = self.task.take() {
            // The drain task ends on its own once the handle is marked
            // dropped; awaiting it is what makes the flush a guarantee
            // rather than a race against runtime teardown.
            let _ = task.await;
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
            let ceiling = self
                .shared
                .capacity_bytes
                .saturating_mul(HARD_OVERFLOW_FACTOR);
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
        drop(discarded);
        tracing::error!(
            code = "stdout_blocked",
            cause = "hard overflow ceiling",
            "write buffer outgrew the overflow ceiling; emitting one transport.error and \
             signalling runtime exit — there is no recovery from a non-reading parent"
        );
        let _ = self.shared.fired.send(true);
        self.shared.wake.notify_one();
        Err(WriterError::Sealed)
    }
}

impl Drop for BoundedWriter {
    fn drop(&mut self) {
        // Best-effort half of the shutdown contract: the drain task is
        // told to finish and flush, but nothing awaits it here — a Drop
        // cannot. `shutdown` is the guaranteed path.
        lock(&self.shared.state).handle_dropped = true;
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

/// The synchronous byte half of the same bound: how many arming lines of
/// bytes `enqueue` will hold before it performs the terminal transition
/// itself, under its own lock. The deadlines above live in the drain task,
/// and a producer that never yields to the runtime could otherwise outgrow
/// memory while that task waits for a turn — the ceiling makes the bound
/// hold with no scheduler cooperation at all.
const HARD_OVERFLOW_FACTOR: usize = 4;

/// The drain task: move buffered bytes into the sink, one deadline-bounded
/// attempt at a time. Progress restarts the per-attempt clock; an attempt
/// that expires with the buffer at or past the arming line is the
/// sustained non-drain the die-loudly contract names, and dies loudly —
/// as does a buffer held over the line for [`SUSTAINED_OVERFLOW_FACTOR`]
/// deadlines outright, however much trickle arrived in between.
async fn run<W>(mut inner: W, shared: Arc<Shared>, config: WriterConfig)
where
    W: AsyncWrite + Unpin,
{
    // The front frame rides here while partially written; its unwritten
    // tail stays counted in `buffered`.
    let mut current: Option<Bytes> = None;
    loop {
        let (sealed, over_capacity_since) = {
            let state = lock(&shared.state);
            (state.sealed, state.over_capacity_since)
        };
        if sealed {
            // The hard-ceiling path in `enqueue` performed the terminal
            // transition and fired the fatal; the task's remaining share
            // is the best-effort farewell.
            attempt_farewell(&mut inner, &config).await;
            return;
        }
        if let Some(since) = over_capacity_since
            && tokio::time::Instant::now()
                >= since + config.drain_deadline * SUSTAINED_OVERFLOW_FACTOR
        {
            die_loudly(
                &mut inner,
                &shared,
                &config,
                "buffer over capacity past the sustained deadline",
            )
            .await;
            return;
        }
        if current.is_none() {
            let (next, handle_dropped) = {
                let mut state = lock(&shared.state);
                (state.queue.pop_front(), state.handle_dropped)
            };
            match next {
                Some(frame) => current = Some(frame),
                None if handle_dropped => {
                    // Clean shutdown: everything buffered has been
                    // written; a final flush gets the same best-effort
                    // budget as any attempt.
                    let _ = tokio::time::timeout(config.drain_deadline, inner.flush()).await;
                    return;
                }
                None => {
                    shared.wake.notified().await;
                    continue;
                }
            }
        }
        let chunk = current.as_mut().expect("refilled above");
        match tokio::time::timeout(config.drain_deadline, inner.write(&chunk[..])).await {
            Ok(Ok(written)) if written > 0 => {
                chunk.advance(written);
                {
                    let mut state = lock(&shared.state);
                    state.buffered -= written;
                    if state.buffered < shared.capacity_bytes {
                        state.over_capacity_since = None;
                    }
                }
                if chunk.is_empty() {
                    current = None;
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
                die_loudly(&mut inner, &shared, &config, "sink accepts no bytes").await;
                return;
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, "transport sink failed before the drain deadline");
                die_loudly(&mut inner, &shared, &config, "sink failed").await;
                return;
            }
            Err(_elapsed) => {
                let (armed_since, handle_dropped) = {
                    let state = lock(&shared.state);
                    (state.over_capacity_since, state.handle_dropped)
                };
                if handle_dropped {
                    // The runtime let go of the writer and the sink still
                    // will not take the tail: nobody is coming for these
                    // bytes, and firing a fatal during the clean shutdown
                    // the dropped handle announced would contradict it —
                    // however full the buffer is, the runtime is already
                    // exiting by its own choice.
                    tracing::debug!("bounded writer abandoned undrained tail after handle drop");
                    return;
                }
                // The verdict needs both clocks: this attempt made zero
                // progress for a full deadline, AND the buffer has been at
                // or past the arming line for a full deadline. Checking
                // only "armed now" would let an enqueue that crossed the
                // line mid-attempt inherit this attempt's nearly-expired
                // timeout and die almost immediately — arming starts the
                // buffer's own clock, not the attempt's.
                if let Some(since) = armed_since
                    && tokio::time::Instant::now().duration_since(since) >= config.drain_deadline
                {
                    die_loudly(&mut inner, &shared, &config, "buffer full past deadline").await;
                    return;
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
async fn die_loudly<W>(inner: &mut W, shared: &Shared, config: &WriterConfig, cause: &'static str)
where
    W: AsyncWrite + Unpin,
{
    let discarded = {
        let mut state = lock(&shared.state);
        if state.sealed {
            // The ceiling path won; its farewell runs at the loop top.
            return;
        }
        if state.handle_dropped {
            tracing::debug!(
                cause,
                "bounded writer abandoned undrained tail after handle drop"
            );
            return;
        }
        state.sealed = true;
        state.buffered = 0;
        state.over_capacity_since = None;
        std::mem::take(&mut state.queue)
    };
    // Freed outside the lock, like every unbounded-size drop in this
    // crate's critical-section discipline.
    drop(discarded);
    tracing::error!(
        code = "stdout_blocked",
        cause,
        "caller stopped reading the transport output; emitting one transport.error and \
         signalling runtime exit — there is no recovery from a non-reading parent"
    );
    attempt_farewell(inner, config).await;
    let _ = shared.fired.send(true);
}

/// The best-effort goodbye: one deadline-bounded try at the pre-encoded
/// farewell frame and a flush, results ignored — against a truly
/// non-reading parent both fail, which is exactly why the tracing log and
/// the [`FatalSignal`] carry the same fact.
async fn attempt_farewell<W>(inner: &mut W, config: &WriterConfig)
where
    W: AsyncWrite + Unpin,
{
    let _ = tokio::time::timeout(config.drain_deadline, async {
        let _ = inner.write_all(&config.farewell).await;
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
            farewell: Bytes::from_static(b"FAREWELL"),
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
        writer.shutdown().await;
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
        let (sink, _state) = sink(0, 0, None);
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
