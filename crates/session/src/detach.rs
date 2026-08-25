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
