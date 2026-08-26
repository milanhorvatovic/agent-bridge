//! Blocking work the runtime must not own.
//!
//! `spawn_blocking` hands an operation to the async runtime's blocking
//! pool, and that pool is the runtime's: a task that never returns holds
//! a pool worker for the life of the process and holds runtime shutdown
//! with it. The bounded waits in the launch and close paths exist
//! precisely because their operations can hang without bound — a census
//! the operating system will not answer, a log open or flush against a
//! stalled mount — so those operations ride a detached thread instead.
//! The caller's await and timeout are exactly as before; what changes is
//! ownership: a hang leaks one anonymous thread the runtime neither
//! tracks nor waits for, which is the loss the caller's timeout has
//! already put on the record.
//!
//! This is only for operations with no internal deadline of their own.
//! Terminal writes, signals, and terminations are bounded inside the
//! terminal layer and stay on the runtime's pool, where instrumentation
//! can see them.

use tokio::sync::oneshot;

/// Process-wide ceiling on detached threads still running. Each hang
/// leaks exactly one thread by design — that is the ownership trade the
/// module doc describes — but a persistently stalled filesystem must
/// not turn that per-operation loss into unbounded native-thread
/// accumulation across many sessions: past the budget, new operations
/// are refused loudly (the receiver reports a closed channel, which
/// every caller already maps to its degrade path) instead of spawning.
const DETACHED_BUDGET: usize = 256;

/// Outstanding detached threads; released when each thread finishes.
static OUTSTANDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One unit of the budget, released on drop — including when the spawn
/// itself fails, because the unspawned closure that owns it is dropped.
struct Slot;

impl Slot {
    fn acquire(name: &str) -> Option<Self> {
        use std::sync::atomic::Ordering;
        let mut seen = OUTSTANDING.load(Ordering::Relaxed);
        loop {
            if seen >= DETACHED_BUDGET {
                tracing::error!(
                    thread = name,
                    outstanding = seen,
                    "the detached-thread budget is exhausted; refusing the operation"
                );
                return None;
            }
            match OUTSTANDING.compare_exchange_weak(
                seen,
                seen + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Self),
                Err(now) => seen = now,
            }
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        OUTSTANDING.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

/// Run `work` on a detached, named thread; the returned receiver
/// resolves with its result. Dropping the receiver abandons the thread
/// to finish or fail on its own. A receiver that errors means the thread
/// could not be spawned or died before answering.
pub(crate) fn detached<T: Send + 'static>(
    name: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> oneshot::Receiver<T> {
    let (tx, rx) = oneshot::channel();
    let Some(slot) = Slot::acquire(name) else {
        return rx;
    };
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let _slot = slot;
            let _ = tx.send(work());
        });
    if let Err(error) = spawned {
        // The sender was consumed by the closure that never ran, so the
        // receiver reports a closed channel; the caller's error arm
        // carries it from there. The record lands here, where the cause
        // is known.
        tracing::error!(%error, thread = name, "a detached blocking thread could not spawn");
    }
    rx
}

/// A delivered result, armed with its disposal: dropped unclaimed, it
/// runs the disposal itself. The arming exists for one interleaving a
/// send-failure check cannot cover — a send that lands in the caller's
/// final poll before its deadline succeeds, and the expiring timeout
/// then drops the receiver with the value queued and forever unread.
/// The queued value's drop is where the disposal fires.
pub(crate) struct Abandonable<T, F: FnOnce(T)> {
    value: Option<T>,
    dispose: Option<F>,
}

impl<T, F: FnOnce(T)> Abandonable<T, F> {
    /// Take the value and disarm the disposal — the caller owns it now.
    pub(crate) fn claim(mut self) -> T {
        self.dispose = None;
        self.value
            .take()
            .expect("a delivered value is claimed at most once")
    }
}

impl<T, F: FnOnce(T)> Drop for Abandonable<T, F> {
    fn drop(&mut self) {
        if let (Some(value), Some(dispose)) = (self.value.take(), self.dispose.take()) {
            dispose(value);
        }
    }
}

/// Like [`detached`], but the result carries a disposal path for an
/// answer nobody claims: work that acquired something real (a spawned
/// child) is ended instead of leaked, whether the send was refused
/// outright or the value sat queued when the receiver was dropped. The
/// disposal can therefore run at the receiver's drop site — an async
/// context — so it must never block; blocking cleanup goes onto its own
/// thread inside the callback.
pub(crate) fn detached_with_abandon<T, F>(
    name: &str,
    work: impl FnOnce() -> T + Send + 'static,
    abandoned: F,
) -> oneshot::Receiver<Abandonable<T, F>>
where
    T: Send + 'static,
    F: FnOnce(T) + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let Some(slot) = Slot::acquire(name) else {
        return rx;
    };
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let _slot = slot;
            let armed = Abandonable {
                value: Some(work()),
                dispose: Some(abandoned),
            };
            // Both loss paths end at the same Drop: a refused send drops
            // `armed` right here, and a delivered-but-unread answer
            // drops it inside the receiver.
            let _ = tx.send(armed);
        });
    if let Err(error) = spawned {
        tracing::error!(%error, thread = name, "a detached blocking thread could not spawn");
    }
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn an_exhausted_budget_refuses_instead_of_spawning() {
        // Filling the counter directly stands in for a process full of
        // hung threads; the refused operation must answer as a closed
        // channel — the same reading every caller's degrade arm takes.
        OUTSTANDING.fetch_add(DETACHED_BUDGET, Ordering::AcqRel);
        let refused = detached("budget-test", || 1);
        let outcome = refused.await;
        OUTSTANDING.fetch_sub(DETACHED_BUDGET, Ordering::AcqRel);
        assert!(
            outcome.is_err(),
            "a refused operation reports a closed channel"
        );
    }

    #[tokio::test]
    async fn a_finished_thread_releases_its_budget_slot() {
        let before = OUTSTANDING.load(Ordering::Acquire);
        let answered = detached("budget-release-test", || 7)
            .await
            .expect("the thread answers");
        assert_eq!(answered, 7);
        // The slot releases when the thread finishes, which may trail
        // the answer by a beat.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while OUTSTANDING.load(Ordering::Acquire) > before {
            assert!(
                std::time::Instant::now() < deadline,
                "the slot was never released"
            );
            std::thread::yield_now();
        }
    }
}
