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
    /// The buffer level at which the drain deadlines arm. Not a hard
    /// admission cap: [`BoundedWriter::enqueue`] never blocks and never
    /// drops, so the buffer can run past this line — for at most one
    /// drain deadline of zero progress under a stalled sink, or
    /// [`SUSTAINED_OVERFLOW_FACTOR`] deadlines outright under a trickling
    /// one — before die-loudly ends the process. The bound is enforced in
    /// time, by exiting, which is the exit contract's stance. A single
    /// frame larger than this is refused outright
    /// ([`WriterError::FrameTooLarge`]): it could never drain back under
    /// the line, so accepting it would schedule a guaranteed death.
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
    /// Die-loudly has fired: the caller must not buffer further, because
    /// there is no recovery from a non-reading parent.
    #[error("writer sealed after stdout_blocked")]
    Sealed,
    /// The frame alone exceeds `capacity_bytes`, so it could never drain
    /// back under the arming line.
    #[error("frame larger than writer capacity")]
    FrameTooLarge,
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
/// sink. Dropping the handle is a clean shutdown request: the task
/// finishes writing what is buffered — giving each attempt the drain
/// deadline — and exits without firing the fatal.
#[derive(Debug)]
pub struct BoundedWriter {
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    capacity_bytes: usize,
    state: Mutex<BufferState>,
    /// Wakes the drain task when a frame arrives or the handle drops.
    wake: Notify,
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
        });
        let (tx, rx) = watch::channel(false);
        tokio::spawn(run(inner, Arc::clone(&shared), config, tx));
        (Self { shared }, FatalSignal { rx })
    }

    /// Non-blocking enqueue of one framed message. Never waits and never
    /// drops: a frame the sink cannot take yet is buffered, and a sink
    /// that stays stalled past the drain deadline ends in die-loudly, not
    /// in lost frames — dropping a protocol frame would corrupt the wire
    /// for whatever conversation survives it.
    pub fn enqueue(&self, frame: Bytes) -> Result<(), WriterError> {
        {
            let mut state = lock(&self.shared.state);
            if state.sealed {
                return Err(WriterError::Sealed);
            }
            if frame.len() > self.shared.capacity_bytes {
                return Err(WriterError::FrameTooLarge);
            }
            if frame.is_empty() {
                // Nothing to write; queueing it would only hand the sink
                // a zero-length write that reads as a dead sink.
                return Ok(());
            }
            state.buffered += frame.len();
            if state.buffered >= self.shared.capacity_bytes && state.over_capacity_since.is_none() {
                state.over_capacity_since = Some(tokio::time::Instant::now());
            }
            state.queue.push_back(frame);
        }
        self.shared.wake.notify_one();
        Ok(())
    }
}

impl Drop for BoundedWriter {
    fn drop(&mut self) {
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

/// The drain task: move buffered bytes into the sink, one deadline-bounded
/// attempt at a time. Progress restarts the per-attempt clock; an attempt
/// that expires with the buffer at or past the arming line is the
/// sustained non-drain the die-loudly contract names, and dies loudly —
/// as does a buffer held over the line for [`SUSTAINED_OVERFLOW_FACTOR`]
/// deadlines outright, however much trickle arrived in between.
async fn run<W>(mut inner: W, shared: Arc<Shared>, config: WriterConfig, fired: watch::Sender<bool>)
where
    W: AsyncWrite + Unpin,
{
    // The front frame rides here while partially written; its unwritten
    // tail stays counted in `buffered`.
    let mut current: Option<Bytes> = None;
    loop {
        let over_capacity = {
            let state = lock(&shared.state);
            state
                .over_capacity_since
                .map(|since| (since, state.handle_dropped))
        };
        if let Some((since, handle_dropped)) = over_capacity
            && tokio::time::Instant::now()
                >= since + config.drain_deadline * SUSTAINED_OVERFLOW_FACTOR
        {
            if handle_dropped {
                tracing::debug!("bounded writer abandoned undrained tail after handle drop");
                return;
            }
            die_loudly(
                &mut inner,
                &shared,
                &config,
                &fired,
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
            // with the farewell attempt left to fail as it will. Zero on a
            // nonempty buffer only — empty frames never enter the queue.
            Ok(Ok(_zero)) => {
                die_loudly(
                    &mut inner,
                    &shared,
                    &config,
                    &fired,
                    "sink accepts no bytes",
                )
                .await;
                return;
            }
            Ok(Err(error)) => {
                tracing::debug!(%error, "transport sink failed before the drain deadline");
                die_loudly(&mut inner, &shared, &config, &fired, "sink failed").await;
                return;
            }
            Err(_elapsed) => {
                let (armed, handle_dropped) = {
                    let state = lock(&shared.state);
                    (
                        state.buffered >= shared.capacity_bytes,
                        state.handle_dropped,
                    )
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
                if armed {
                    die_loudly(
                        &mut inner,
                        &shared,
                        &config,
                        &fired,
                        "buffer full past deadline",
                    )
                    .await;
                    return;
                }
                // Under the arming line: not yet the buffer-fills case
                // the runtime exits on. Try again; the clock restarts.
            }
        }
    }
}

/// The exit sequence, in its fixed order: seal (so `enqueue` refuses and
/// the buffer frees), say it loudly, attempt the farewell frame, fire the
/// signal. Reached at most once by construction — only the single drain
/// task calls it, and every call site returns immediately after.
async fn die_loudly<W>(
    inner: &mut W,
    shared: &Shared,
    config: &WriterConfig,
    fired: &watch::Sender<bool>,
    cause: &'static str,
) where
    W: AsyncWrite + Unpin,
{
    let discarded = {
        let mut state = lock(&shared.state);
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
    let _ = tokio::time::timeout(config.drain_deadline, async {
        let _ = inner.write_all(&config.farewell).await;
        let _ = inner.flush().await;
    })
    .await;
    let _ = fired.send(true);
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

    /// A frame that could never drain under the arming line is refused
    /// outright rather than accepted as a scheduled death.
    #[tokio::test(start_paused = true)]
    async fn oversized_frames_are_refused() {
        let (sink, _state) = sink(0, 0, None);
        let (writer, _fatal) = BoundedWriter::new(sink, config());
        assert_eq!(
            writer.enqueue(Bytes::from(vec![7u8; 65])),
            Err(WriterError::FrameTooLarge)
        );
        // At the line exactly is still admissible, and an empty frame is
        // a no-op rather than a zero-length write.
        writer.enqueue(Bytes::from(vec![7u8; 64])).unwrap();
        writer.enqueue(Bytes::new()).unwrap();
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
