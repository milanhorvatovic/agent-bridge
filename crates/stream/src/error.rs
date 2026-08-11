//! Everything this layer can fail at, as one typed enum.
//!
//! Typed rather than stringly for the same reason the terminal layer's
//! errors are: the layers above have to *decide* on these — a session
//! decides whether a failure is worth ending over, and the transport
//! decides which protocol error code it becomes — and neither decision can
//! be made against a flattened message.

use std::io;

/// A failure of the stream layer.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// The reader's bridge thread could not be started, which leaves a
    /// terminal stream nothing will ever forward: an allocated session that
    /// cannot work, reported as a failure to stand the session up rather
    /// than handed back as a reader that never produces a byte.
    #[error("the reader's bridge thread could not be started: {0}")]
    BridgeSpawnFailed(#[source] io::Error),
}
