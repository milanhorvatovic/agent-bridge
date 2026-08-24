//! Everything this layer can refuse or fail at, as one typed enum.
//!
//! Typed rather than stringly for the same reason as everywhere else in the
//! workspace: the transport maps these onto the protocol's error-code
//! table 1:1, and a registry deciding whether a create failed or was refused
//! cannot decide against a flattened message. The mapping lives here — with
//! the variant, where adding one forces choosing its code — and the test
//! below holds it total.

use agent_bridge_pty::PtyError;

use crate::state::SessionState;

/// A session operation that could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The session is in a state that cannot service the operation — the
    /// state machine has no such edge, or the operation's own validity
    /// table excludes the state.
    #[error("invalid state {state} for {op}")]
    InvalidStateForOperation {
        /// The state the session was in.
        state: SessionState,
        /// What was attempted.
        op: &'static str,
    },
    /// `resolve_approval` named an `approval_id` that is not in the pending
    /// set — stale, mistyped, or another session's. The pending prompts are
    /// untouched.
    #[error("approval id mismatch: no such pending approval")]
    ApprovalIdMismatch,
    /// A source announced an `approval_id` that is already pending. Ids
    /// correlate resolutions to prompts, so a duplicate could resolve the
    /// wrong one; the announcement is refused and the existing entry stays.
    #[error("approval id already pending")]
    ApprovalAlreadyPending,
    /// A live operation was attempted on a session that has ended.
    #[error("session closed")]
    SessionClosed,
    /// The session could not be stood up: terminal allocation, exec, or
    /// reader attachment failed.
    #[error("launch failed: {0}")]
    LaunchFailed(#[source] PtyError),
    /// The requested terminal geometry is outside what a session may hold.
    ///
    /// A screen allocates in proportion to area, so an unbounded request
    /// is an allocation attack with one caller. The bound is the largest
    /// permitted request (200×100); past it the create or resize is
    /// refused with this readable error rather than silently degraded.
    #[error(
        "terminal dimensions {cols}x{rows} out of bounds \
         (cols 1..={max_cols}, rows 1..={max_rows})"
    )]
    InvalidDimensions {
        /// Requested width.
        cols: u16,
        /// Requested height.
        rows: u16,
        /// The largest width a session may hold.
        max_cols: u16,
        /// The largest height a session may hold.
        max_rows: u16,
    },
    /// The pending-approval set is at its bound; the announcement is
    /// refused with the set untouched. A session holding this many
    /// unresolved prompts is not a working approval flow but a source
    /// stuck announcing — bounded so it cannot grow the actor's memory
    /// for the session's lifetime.
    #[error("the pending-approval set is at its capacity of {limit}")]
    PendingApprovalsAtCapacity {
        /// The bound the set is held to.
        limit: usize,
    },
    /// A second screen-detected prompt was announced while one is pending —
    /// a violation of the screen path's retained one-dialog-at-a-time
    /// rule, surfaced to the announcing source. Hook-sourced approvals are
    /// exempt: coexisting in a set is their contract.
    #[error("second screen-detected prompt while one is pending")]
    ScreenApprovalContractViolation,
    /// A terminal operation failed after launch — a blocked write, a
    /// refused resize, a signal that did not take. The typed cause is
    /// carried so a caller can decide whether the session survives it.
    #[error("terminal operation failed: {0}")]
    Pty(#[source] PtyError),
}

impl SessionError {
    /// The JSON-RPC error code the transport reports for this failure —
    /// stated with the variant, so a new failure mode cannot land without
    /// someone choosing its code.
    pub fn jsonrpc_code(&self) -> i32 {
        match self {
            SessionError::InvalidStateForOperation { .. } => -32006,
            SessionError::ApprovalIdMismatch => -32007,
            SessionError::SessionClosed => -32003,
            SessionError::LaunchFailed(_) => -32005,
            // JSON-RPC's own invalid-params code: the refusal happens at
            // parameter validation, before a session exists to be in a
            // state.
            SessionError::InvalidDimensions { .. } => -32602,
            // Internal-error class: neither is a caller mistake with a
            // dedicated code — one is a source violating its contract, the
            // other an operating-system failure the events on the bus
            // describe in full.
            SessionError::ApprovalAlreadyPending => -32603,
            SessionError::PendingApprovalsAtCapacity { .. } => -32603,
            SessionError::ScreenApprovalContractViolation => -32603,
            SessionError::Pty(_) => -32603,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One of each variant, listed by hand so a new variant fails to
    /// compile here until somebody decides which code it carries.
    fn one_of_each() -> Vec<SessionError> {
        vec![
            SessionError::InvalidStateForOperation {
                state: SessionState::Closed,
                op: "interrupt",
            },
            SessionError::ApprovalIdMismatch,
            SessionError::ApprovalAlreadyPending,
            SessionError::SessionClosed,
            SessionError::LaunchFailed(PtyError::ChildExecFailed(std::io::Error::from(
                std::io::ErrorKind::NotFound,
            ))),
            SessionError::InvalidDimensions {
                cols: 65_535,
                rows: 65_535,
                max_cols: 200,
                max_rows: 100,
            },
            SessionError::PendingApprovalsAtCapacity { limit: 32 },
            SessionError::ScreenApprovalContractViolation,
            SessionError::Pty(PtyError::ResizeBeforeReady),
        ]
    }

    #[test]
    fn error_code_mapping_is_total_and_matches_the_protocol_table() {
        let codes: Vec<i32> = one_of_each()
            .iter()
            .map(SessionError::jsonrpc_code)
            .collect();
        assert_eq!(
            codes,
            [
                -32006, -32007, -32603, -32003, -32005, -32602, -32603, -32603, -32603
            ],
            "every variant maps to its protocol code, in declaration order"
        );
    }

    #[test]
    fn a_rejected_geometry_names_the_bound_the_caller_broke() {
        // The requirement is an error the caller can read: the
        // refusal must say both what was asked and what is allowed.
        let message = SessionError::InvalidDimensions {
            cols: 65_535,
            rows: 65_535,
            max_cols: 200,
            max_rows: 100,
        }
        .to_string();
        assert!(message.contains("65535x65535"), "{message}");
        assert!(message.contains("1..=200"), "{message}");
        assert!(message.contains("1..=100"), "{message}");
    }
}
