//! The one write end of the wire, shared by everything that emits a frame.
//!
//! Every outbound frame — a response, a `session.event`, a `session.eof` — is
//! enqueued through the core's bounded, die-loudly writer: the single
//! legitimate owner of the process's stdout, which floods, warns once, and
//! exits rather than wedging against a parent that has stopped reading. The
//! serve loop and each attach task hold a clone of this handle; at drain the
//! serve loop reclaims the last one to flush the buffered tail with the
//! guarantee a bare drop cannot give.

use std::sync::Arc;

use agent_bridge_core::{BoundedWriter, ShutdownOutcome, WriterError};
use bytes::Bytes;

/// A clone-shareable handle to the outbound writer. Cloning shares one
/// bounded buffer and one die-loudly decision; the enqueue is non-blocking
/// and internally synchronized, so any number of tasks may send concurrently.
#[derive(Clone)]
pub struct Outbound(Arc<BoundedWriter>);

impl Outbound {
    /// Wrap the bounded writer for sharing.
    pub fn new(writer: BoundedWriter) -> Self {
        Self(Arc::new(writer))
    }

    /// Enqueue one already-framed message. `Err` means die-loudly has fired —
    /// the parent stopped reading — and the caller must stop writing; there
    /// is no recovery, and the fatal signal the serve loop watches carries
    /// the same fact.
    pub fn send(&self, frame: Bytes) -> Result<(), WriterError> {
        self.0.enqueue(frame)
    }

    /// Reclaim the sole remaining writer and flush its tail with the
    /// completion guarantee, returning the [`ShutdownOutcome`] — whether the
    /// tail was actually delivered. Only succeeds once every other clone —
    /// the dispatcher's, every attach task's — has been dropped; if one
    /// somehow outlives the join, the writer is dropped instead, taking the
    /// best-effort flush its `Drop` gives, and the tail's delivery cannot be
    /// confirmed.
    pub async fn reclaim_and_shutdown(self) -> ShutdownOutcome {
        match Arc::try_unwrap(self.0) {
            Ok(writer) => writer.shutdown().await,
            Err(still_shared) => {
                tracing::warn!(
                    "an outbound handle outlived the drain; flushing best-effort on drop"
                );
                drop(still_shared);
                ShutdownOutcome::Abandoned
            }
        }
    }
}
