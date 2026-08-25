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

/// Run `work` on a detached, named thread; the returned receiver
/// resolves with its result. Dropping the receiver abandons the thread
/// to finish or fail on its own. A receiver that errors means the thread
/// could not be spawned or died before answering.
pub(crate) fn detached<T: Send + 'static>(
    name: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> oneshot::Receiver<T> {
    let (tx, rx) = oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
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
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
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
