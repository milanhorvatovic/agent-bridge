//! One live CLI session, from launch to exit.
//!
//! The state machine that owns a session's lifecycle: starting the child,
//! tracking the approvals waiting on an answer — more than one can be
//! outstanding at a time, so this is a set and not a slot —
//! orchestrating interrupts, and running the shutdown sequence through a
//! bounded, verified cleanup: the containment census's verdict is typed
//! onto the closed event (`cleanup_verified`), and what the operating
//! system refuses to end is loudly recorded for supervision rather than
//! silently claimed gone.
//!
//! Single-writer ownership is the rule that makes the rest safe. Many
//! readers may observe a session; exactly one task mutates it: every
//! command, stream signal, and approval announcement enters one bounded
//! queue consumed by the session's actor, so a reconnecting client and a
//! live subscriber can never disagree about what the session is doing. The
//! lifecycle machine's full topology lives in [`state`] as one table, and
//! every transition is observable as a `lifecycle.*` event and a
//! session-log record.
//!
//! This crate sits below the runtime core in the dependency direction, so
//! the bus reaches it as a capability: the core hands each session an
//! [`EventSink`] at spawn, and the session's last act is sealing it. The
//! registry that mints [`SessionId`]s and enforces the session caps lives
//! in the core; what lives here is one session's whole life.

#![forbid(unsafe_code)]

mod actor;
mod approval;
mod close;
mod command;
mod error;
mod id;
mod logfile;
mod metadata;
mod state;

pub use actor::{
    EventSink, SessionConfig, SessionHandle, SessionSpec, SinkSealed, SpawnedSession, spawn_session,
};
pub use approval::{
    ApprovalDecision, ApprovalId, ApprovalIdentity, ApprovalResolution, ApprovalSource,
};
pub use error::SessionError;
pub use id::{InvalidSessionId, SessionId, SubscriberId};
pub use metadata::SessionMetadata;
pub use state::{Edge, SessionState, transition};

// The create seam's adapter-facing shapes, re-exported so the registry —
// which sits *above* this crate — reaches them without a dependency of its
// own on the adapter contract.
pub use agent_bridge_adapter_api::{InputStep, LaunchSpec, ShutdownHint, ShutdownSignal};

use agent_bridge_pty::Dimensions;

/// The widest terminal a session may hold, in columns.
///
/// The largest permitted request is 200×100: a screen's memory is
/// proportional to its area and a reconnect snapshot travels whole in one
/// frame, so the bound is a published contract rather than a private
/// guardrail.
pub const MAX_COLS: u16 = 200;

/// The tallest terminal a session may hold, in rows. See [`MAX_COLS`].
pub const MAX_ROWS: u16 = 100;

/// Validate a requested terminal geometry against the session bound.
///
/// The wire contract names no bound of its own, and a screen allocates
/// in proportion to area — 65 535 × 65 535 is a 63 GiB grid and a process
/// abort. The layer below carries a backstop that
/// silently keeps no screen; this is the loud half — an invalid-params
/// refusal (`-32602`) naming the bound, at create and at resize, before
/// anything allocates.
pub fn validate_dimensions(cols: u16, rows: u16) -> Result<Dimensions, SessionError> {
    if cols == 0 || rows == 0 || cols > MAX_COLS || rows > MAX_ROWS {
        return Err(SessionError::InvalidDimensions {
            cols,
            rows,
            max_cols: MAX_COLS,
            max_rows: MAX_ROWS,
        });
    }
    Ok(Dimensions { cols, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bound_is_the_largest_permitted_screen_request() {
        assert!(validate_dimensions(200, 100).is_ok(), "the stated maximum");
        assert!(validate_dimensions(80, 24).is_ok(), "the default");
        assert!(validate_dimensions(1, 1).is_ok(), "the degenerate minimum");
        for (cols, rows) in [(0, 24), (80, 0), (201, 100), (200, 101), (65_535, 65_535)] {
            let refusal = validate_dimensions(cols, rows).expect_err("out of bounds");
            assert_eq!(refusal.jsonrpc_code(), -32602, "{cols}x{rows}");
        }
    }
}
