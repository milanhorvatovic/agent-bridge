//! The lifecycle state machine — the session lifecycle diagram, as one
//! table.
//!
//! This module is deliberately nothing but the diagram transcribed: the
//! states, the edge alphabet, and a constant table with one row per
//! labelled arrow. The transition function is a lookup over the table, and
//! the coverage test iterates the full `SessionState × Edge` product
//! against a hand-transcribed mirror of the diagram — an edit to either
//! copy that forgets the other disagrees out loud.

use crate::error::SessionError;

/// Where a session is in its life.
///
/// The sequence a caller can rely on is `Created → Launching → Connecting →
/// Running`, then `Closing → Closed`, with `AwaitingApproval` and
/// `Interrupted` as the two states a running session enters and leaves
/// again. `Connecting` is load-bearing: it separates "child alive, nothing
/// painted yet" (a spinner) from "child producing" (a transcript), which
/// callers cannot otherwise tell apart except by the absence of events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// The registry entry exists; no terminal has been allocated yet.
    Created,
    /// The terminal is allocated and the CLI process is being started.
    Launching,
    /// The CLI process is alive; no output has been observed yet.
    Connecting,
    /// First output observed; the session is live.
    Running,
    /// At least one approval is pending a human decision — the state
    /// means "≥ 1 pending", never "exactly one".
    AwaitingApproval,
    /// An interrupt was forwarded and the CLI acknowledged it.
    Interrupted,
    /// Termination has been initiated.
    Closing,
    /// The session has ended; only metadata remains.
    Closed,
}

impl SessionState {
    /// Every state, for exhaustive iteration in the coverage tests.
    pub const ALL: [SessionState; 8] = [
        SessionState::Created,
        SessionState::Launching,
        SessionState::Connecting,
        SessionState::Running,
        SessionState::AwaitingApproval,
        SessionState::Interrupted,
        SessionState::Closing,
        SessionState::Closed,
    ];
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SessionState::Created => "created",
            SessionState::Launching => "launching",
            SessionState::Connecting => "connecting",
            SessionState::Running => "running",
            SessionState::AwaitingApproval => "awaiting_approval",
            SessionState::Interrupted => "interrupted",
            SessionState::Closing => "closing",
            SessionState::Closed => "closed",
        })
    }
}

/// The edge alphabet — one variant per labelled arrow in the diagram.
///
/// Failure routing is encoded as distinct variants on purpose:
/// `LaunchFailed` and `ChildExitedBeforeOutput` are the only edges that may
/// reach `Closed` without passing `Closing`, and `PostRunningFailure` exists
/// so that a failure after `Running` *cannot* take that shortcut — the table
/// simply has no row for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    /// Begin standing the session up.
    Launch,
    /// The terminal is allocated, the child is executing, and the readers
    /// are attached.
    PtyExecOk,
    /// Standing the session up failed; there is nothing to close.
    LaunchFailed,
    /// The child produced its first output.
    FirstOutput,
    /// The child exited before ever producing output.
    ChildExitedBeforeOutput,
    /// The first pending approval was detected.
    ApprovalDetected,
    /// The *last* pending approval was resolved — the state exits only
    /// when the set empties.
    ApprovalResolved,
    /// The CLI acknowledged a forwarded interrupt.
    Interrupt,
    /// The CLI resumed after an interrupt.
    Resumed,
    /// A caller asked the session to close.
    CloseRequested,
    /// The child failed after `Running` — exit, crash, or terminal failure.
    PostRunningFailure,
    /// The close sequence finished: every cleanup invariant was verified
    /// under its bound, with any escape announced loudly and typed onto
    /// the closed payload. The edge records completion, not a verdict —
    /// `cleanup_verified` on the payload carries that.
    CloseComplete,
}

impl Edge {
    /// Every edge, for exhaustive iteration in the coverage tests.
    pub const ALL: [Edge; 12] = [
        Edge::Launch,
        Edge::PtyExecOk,
        Edge::LaunchFailed,
        Edge::FirstOutput,
        Edge::ChildExitedBeforeOutput,
        Edge::ApprovalDetected,
        Edge::ApprovalResolved,
        Edge::Interrupt,
        Edge::Resumed,
        Edge::CloseRequested,
        Edge::PostRunningFailure,
        Edge::CloseComplete,
    ];

    /// The edge's name, carried in the typed rejection so an error names
    /// what was attempted.
    pub fn name(self) -> &'static str {
        match self {
            Edge::Launch => "launch",
            Edge::PtyExecOk => "pty_exec_ok",
            Edge::LaunchFailed => "launch_failed",
            Edge::FirstOutput => "first_output",
            Edge::ChildExitedBeforeOutput => "child_exited_before_output",
            Edge::ApprovalDetected => "approval_detected",
            Edge::ApprovalResolved => "approval_resolved",
            Edge::Interrupt => "interrupt",
            Edge::Resumed => "resumed",
            Edge::CloseRequested => "close",
            Edge::PostRunningFailure => "post_running_failure",
            Edge::CloseComplete => "close_complete",
        }
    }
}

/// The lifecycle diagram, one row per arrow.
///
/// The topology this table induces — the set of `(from, to)` state pairs —
/// must equal the diagram's arrow set exactly; the coverage test holds it to
/// a hand-transcribed mirror of the diagram. Three arrows carry two edges
/// each (`Running → Closing`, `AwaitingApproval → Closing`, and
/// `Interrupted → Closing` are reachable by caller close *and* by
/// post-`Running` failure), which is deliberate: the wire needs to tell the
/// two apart even though the diagram draws one line.
pub(crate) const TRANSITIONS: &[(SessionState, Edge, SessionState)] = &[
    (SessionState::Created, Edge::Launch, SessionState::Launching),
    (
        SessionState::Launching,
        Edge::PtyExecOk,
        SessionState::Connecting,
    ),
    (
        SessionState::Launching,
        Edge::LaunchFailed,
        SessionState::Closed,
    ),
    (
        SessionState::Connecting,
        Edge::FirstOutput,
        SessionState::Running,
    ),
    (
        SessionState::Connecting,
        Edge::CloseRequested,
        SessionState::Closing,
    ),
    (
        SessionState::Connecting,
        Edge::ChildExitedBeforeOutput,
        SessionState::Closed,
    ),
    (
        SessionState::Running,
        Edge::ApprovalDetected,
        SessionState::AwaitingApproval,
    ),
    (
        SessionState::Running,
        Edge::Interrupt,
        SessionState::Interrupted,
    ),
    (
        SessionState::Running,
        Edge::CloseRequested,
        SessionState::Closing,
    ),
    (
        SessionState::Running,
        Edge::PostRunningFailure,
        SessionState::Closing,
    ),
    (
        SessionState::AwaitingApproval,
        Edge::ApprovalResolved,
        SessionState::Running,
    ),
    (
        SessionState::AwaitingApproval,
        Edge::Interrupt,
        SessionState::Interrupted,
    ),
    (
        SessionState::AwaitingApproval,
        Edge::CloseRequested,
        SessionState::Closing,
    ),
    (
        SessionState::AwaitingApproval,
        Edge::PostRunningFailure,
        SessionState::Closing,
    ),
    (
        SessionState::Interrupted,
        Edge::Resumed,
        SessionState::Running,
    ),
    (
        SessionState::Interrupted,
        Edge::CloseRequested,
        SessionState::Closing,
    ),
    (
        SessionState::Interrupted,
        Edge::PostRunningFailure,
        SessionState::Closing,
    ),
    (
        SessionState::Closing,
        Edge::CloseComplete,
        SessionState::Closed,
    ),
];

/// Pure transition function; an illegal edge is a typed rejection, never a
/// panic.
///
/// Every mutation of a session's state goes through here, from exactly one
/// task — the actor — so the table is the complete answer to "what can
/// happen next" and the rejection is the `-32006` the wire reports.
pub fn transition(from: SessionState, edge: Edge) -> Result<SessionState, SessionError> {
    TRANSITIONS
        .iter()
        .find(|(state, candidate, _)| *state == from && *candidate == edge)
        .map(|(_, _, to)| *to)
        .ok_or(SessionError::InvalidStateForOperation {
            state: from,
            op: edge.name(),
        })
}
