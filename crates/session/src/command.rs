//! The actor's single mutation inlet.
//!
//! Every way a session's state can change — a caller's request, a signal
//! from the stream, a source announcing an approval — arrives as one of
//! these on one bounded queue, so every transition is serialized by
//! construction and FIFO input ordering is the queue's order rather than a
//! locking discipline.

use agent_bridge_events::ApprovalPrompt;
use agent_bridge_pty::Dimensions;
use bytes::Bytes;
use tokio::sync::oneshot;

use crate::approval::{ApprovalDecision, ApprovalId, ApprovalIdentity, ApprovalResolution};
use crate::error::SessionError;

/// A caller's reply channel: the typed result, or silence if the caller
/// stopped waiting.
pub(crate) type Reply<T> = oneshot::Sender<Result<T, SessionError>>;

// No `Debug`: `Send` carries operator input, which is content — a
// payload is printed by deliberate decision, never by a derive on
// whatever holds it.
pub(crate) enum SessionCommand {
    /// Forward input bytes to the CLI. FIFO — the queue is the order —
    /// and input-only: approval resolution is its own command.
    Send { input: Bytes, reply: Reply<()> },
    /// Resolve one pending approval by id.
    ResolveApproval {
        id: ApprovalId,
        decision: ApprovalDecision,
        reply: Reply<()>,
    },
    /// Deliver the interrupt byte and cancel every pending approval.
    Interrupt { reply: Reply<()> },
    /// Change the terminal geometry. Bounds are validated before the
    /// command is queued; the actor performs the terminal call.
    Resize {
        dimensions: Dimensions,
        reply: Reply<()>,
    },
    /// Close the session — hint-first when `force` is false, straight to
    /// termination when true. The reply resolves once `Closed` is reached.
    Close { force: bool, reply: Reply<()> },
    /// A source (Phase-2 hook listener or screen matcher; a test until
    /// then) announces a pending approval. The reply carries the entry's
    /// id — the hook's own, or the one the actor minted for a screen
    /// detection — and the channel the source parks on for its resolution.
    ApprovalDetected {
        identity: ApprovalIdentity,
        prompt: ApprovalPrompt,
        reply: Reply<(ApprovalId, oneshot::Receiver<ApprovalResolution>)>,
    },
    /// The CLI acknowledged a forwarded interrupt — the signal that drives
    /// the `Interrupted` edge. Which mechanism produces it is the source's
    /// business (the claude ack is a screen marker; the fixture's is
    /// injected).
    InterruptAcknowledged,
    /// The CLI resumed after an interrupt.
    Resumed,
    /// The transport peer dropped: clear writer ownership (state only in
    /// v1 — see `SubscriberId`).
    TransportDropped,
    /// Decoded output reached the session — drives the `Connecting →
    /// Running` first-output edge. Later chunks are liveness, nothing more:
    /// content interpretation belongs to the stream pipeline, and byte
    /// accounting to the reader's final stats.
    Output,
    /// The terminal's stream ended. On POSIX this is the prompt form of
    /// child-exit detection; the liveness poll is the platform-neutral one
    /// (a pseudo-console's stream outlives its child). The reader's
    /// accounting travels through the reader task's join handle, not here.
    StreamEnded,
    /// An encoding incident from the reader, to publish as `pty.error`.
    Incident(agent_bridge_stream::EncodingIncident),
    /// The terminal itself failed — a read that died, a write the terminal
    /// refused for a reason that is not the child having exited. A session
    /// cannot continue on a failed terminal, so this routes to the failure
    /// close the state machine reserves for it. The cause rides along
    /// when the reporting task had one; the durable-flag fallback path
    /// cannot carry it and publishes the generic form.
    TerminalFailure(Option<String>),
    /// The shutdown-hint sequence finished dispatching, keystrokes
    /// delivered: the drain window measures the CLI's chance to exit
    /// *after* the hint, so it arms here rather than at the spawn of the
    /// hint task.
    HintDispatched,
}
