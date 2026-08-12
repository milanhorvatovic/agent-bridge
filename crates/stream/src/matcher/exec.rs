//! The bounded executor: matcher evaluation off the dispatch path.
//!
//! The load model that motivates this is simple arithmetic: the session
//! cap times the per-session event rate times the per-line evaluation
//! budget is more CPU than one event loop has. So dispatch stays on the
//! stream task and evaluation runs here — a small dedicated pool with a
//! bounded queue, where a full queue is a visible backpressure signal the
//! dispatcher decides about rather than an invisibly growing buffer.
//!
//! This lands as structure: the seam the per-session pipeline will call
//! through, sized by defaults that nothing has tuned yet. The pool's width
//! and depth become informed numbers when the load harness measures the
//! aggregate envelope; the seam's shape is what this change fixes.
//!
//! The deadline belongs to the caller's await, not to the pool. A caller
//! that stops waiting simply drops its receiver; the evaluation runs to
//! completion on its worker and the send of its result fails into the
//! void. That is the discard semantics the safety ceiling wants for an
//! evaluation that never returns in time — the thread is not preempted,
//! the result is just no longer anyone's answer.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tokio::sync::oneshot;

/// Pool shape. Defaults are placeholders with a job: keep the structure
/// honest until the load harness supplies measured numbers.
#[derive(Debug, Clone, Copy)]
pub struct ExecutorConfig {
    /// Worker threads evaluating chains.
    pub workers: usize,
    /// Chains that may wait for a worker before submission fails.
    pub queue_depth: usize,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            queue_depth: 64,
        }
    }
}

/// The queue is full: the backpressure signal. The caller — the dispatch
/// side — decides what a full evaluation queue means; this type only makes
/// the condition impossible to miss.
#[derive(Debug, thiserror::Error)]
#[error("matcher evaluation queue is full")]
pub struct ExecutorFull;

type Job = Box<dyn FnOnce() + Send>;

/// A fixed pool of evaluation workers behind a bounded queue.
///
/// What the pool does not do is as much a contract as what it does. A task
/// cannot be cancelled — arbitrary code offers no seam to preempt — so an
/// evaluation that never returns consumes its worker for good, and the
/// pool does not replace workers. That is deliberate: replacement would
/// trade a visible failure for an unbounded thread leak, and the honest
/// signal is the one this bound already produces — with every worker
/// consumed, [`try_submit`](Self::try_submit) fails persistently, which a
/// dispatcher cannot mistake for a healthy pool. Whether to answer that
/// signal by disabling a matcher, rebuilding the pool, or ending the
/// session is dispatch policy, decided where dispatch is wired.
pub struct BoundedExecutor {
    sender: Option<SyncSender<Job>>,
    workers: Vec<JoinHandle<()>>,
    /// Evaluations accepted and not yet finished — queued or running.
    /// What shutdown consults to decide whether joining can end.
    outstanding: Arc<AtomicUsize>,
}

impl BoundedExecutor {
    pub fn new(config: ExecutorConfig) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<Job>(config.queue_depth.max(1));
        let receiver = Arc::new(Mutex::new(receiver));
        let outstanding = Arc::new(AtomicUsize::new(0));
        let workers = (0..config.workers.max(1))
            .map(|index| {
                let receiver = Arc::clone(&receiver);
                let outstanding = Arc::clone(&outstanding);
                std::thread::Builder::new()
                    .name(format!("matcher-exec-{index}"))
                    .spawn(move || worker_loop(&receiver, &outstanding))
                    .expect("spawning a named thread only fails when the OS is out of threads")
            })
            .collect();
        Self {
            sender: Some(sender),
            workers,
            outstanding,
        }
    }

    /// Queues one evaluation, returning the handle its result will arrive
    /// on. Await it — with a deadline, on the dispatch side — or drop it
    /// to discard the result.
    ///
    /// A full queue returns [`ExecutorFull`] instead of blocking: the
    /// dispatch path must never wait on the evaluation path, or the
    /// backpressure this bound exists to surface would become a stall.
    pub fn try_submit<T, F>(&self, task: F) -> Result<oneshot::Receiver<T>, ExecutorFull>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let job: Job = Box::new(move || {
            let result = task();
            // A dropped receiver is a caller that stopped waiting; the
            // result is discarded by construction, not by cleanup code.
            let _ = sender.send(result);
        });
        // Counted before the send so the worker's decrement can never race
        // ahead of it; undone if the queue refuses.
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        match self
            .sender
            .as_ref()
            .expect("present until drop")
            .try_send(job)
        {
            Ok(()) => Ok(receiver),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                Err(ExecutorFull)
            }
        }
    }
}

impl std::fmt::Debug for BoundedExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BoundedExecutor({} workers)", self.workers.len())
    }
}

impl Drop for BoundedExecutor {
    fn drop(&mut self) {
        // Closing the channel is the shutdown signal. Joining is the tidy
        // ending — but only when nothing is in flight: an abandoned
        // evaluation running to completion is a supported outcome of the
        // deadline model, and shutdown must not inherit its wait. Idle
        // pool: join, leaving nothing behind. Anything outstanding: say
        // so and detach — the workers exit on their own when their work
        // ends, and a worker whose work never ends was lost either way;
        // hanging the session's shutdown next to it would double the
        // damage.
        self.sender = None;
        if self.outstanding.load(Ordering::Acquire) == 0 {
            for worker in self.workers.drain(..) {
                let _ = worker.join();
            }
        } else {
            tracing::warn!(
                outstanding = self.outstanding.load(Ordering::Acquire),
                "evaluations still in flight at shutdown; detaching workers rather than waiting"
            );
            self.workers.clear();
        }
    }
}

fn worker_loop(receiver: &Mutex<Receiver<Job>>, outstanding: &AtomicUsize) {
    loop {
        // Hold the lock only to receive: a worker evaluating a chain must
        // not block its siblings' access to the queue.
        let job = match receiver.lock() {
            Ok(receiver) => receiver.recv(),
            Err(_) => return,
        };
        let Ok(job) = job else { return };
        // A panicking matcher loses its own evaluation, never the worker:
        // the pool's width is capacity, not a panic budget.
        if let Err(panic) = std::panic::catch_unwind(AssertUnwindSafe(job)) {
            let message = panic
                .downcast_ref::<&str>()
                .map(|&s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".to_string());
            tracing::error!(panic = %message, "matcher evaluation panicked; worker continues");
        }
        outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_submitted_evaluation_returns_its_result() {
        let executor = BoundedExecutor::new(ExecutorConfig::default());
        let receiver = executor.try_submit(|| 6 * 7).expect("queue has room");
        assert_eq!(receiver.blocking_recv().expect("worker ran the task"), 42);
    }

    #[test]
    fn a_full_queue_is_a_visible_signal_not_a_stall() {
        let executor = BoundedExecutor::new(ExecutorConfig {
            workers: 1,
            queue_depth: 1,
        });
        // One task occupies the worker; hold it there until released.
        let (release, released) = std::sync::mpsc::channel::<()>();
        let _running = executor
            .try_submit(move || {
                let _ = released.recv();
            })
            .expect("first task starts");
        // Give the worker a moment to take the first task off the queue.
        std::thread::sleep(Duration::from_millis(20));
        let _queued = executor.try_submit(|| ()).expect("one slot in the queue");
        let overflow = executor.try_submit(|| ());
        assert!(overflow.is_err(), "the bound is the backpressure signal");
        release.send(()).expect("release the worker");
    }

    #[tokio::test(start_paused = false)]
    async fn the_deadline_lives_at_the_await_point_and_late_results_discard() {
        let executor = BoundedExecutor::new(ExecutorConfig::default());
        let receiver = executor
            .try_submit(|| {
                std::thread::sleep(Duration::from_millis(100));
                "late"
            })
            .expect("queue has room");
        let outcome = tokio::time::timeout(Duration::from_millis(10), receiver).await;
        assert!(outcome.is_err(), "the caller's deadline fired first");

        // The pool is not wedged by the abandoned evaluation: the next
        // submission still runs to a result.
        let next = executor.try_submit(|| "fresh").expect("queue has room");
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(500), next)
                .await
                .expect("well within any deadline")
                .expect("worker sent it"),
            "fresh"
        );
    }

    #[test]
    fn drop_does_not_wait_for_an_abandoned_evaluation() {
        let executor = BoundedExecutor::new(ExecutorConfig {
            workers: 1,
            queue_depth: 1,
        });
        let (release, released) = std::sync::mpsc::channel::<()>();
        let _abandoned = executor
            .try_submit(move || {
                let _ = released.recv();
            })
            .expect("queue has room");
        // Give the worker a moment to take the task.
        std::thread::sleep(Duration::from_millis(20));

        let started = std::time::Instant::now();
        drop(executor);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "shutdown must not inherit an abandoned evaluation's wait"
        );
        // Let the detached worker finish and exit on the closed channel.
        let _ = release.send(());
    }

    #[test]
    fn a_panicking_task_costs_itself_not_the_worker() {
        let executor = BoundedExecutor::new(ExecutorConfig {
            workers: 1,
            queue_depth: 4,
        });
        let poisoned = executor
            .try_submit(|| panic!("a matcher bug"))
            .expect("queue has room");
        assert!(
            poisoned.blocking_recv().is_err(),
            "the panicked task's sender was dropped, not resolved"
        );
        let survivor = executor.try_submit(|| 7).expect("queue has room");
        assert_eq!(
            survivor.blocking_recv().expect("the lone worker survived"),
            7
        );
    }
}
