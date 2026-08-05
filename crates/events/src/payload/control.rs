//! `session.*` — the control events a subscriber needs in order to trust
//! what it is reading.
//!
//! A subscriber that drops and re-attaches otherwise cannot tell a quiet
//! session from a lost connection, and one that finds its writes rejected
//! cannot tell why. These events close both gaps.
//!
//! `session.reconnecting` and `session.reconnected` are delivered to the
//! re-attaching subscriber alone rather than broadcast: a broadcast
//! `reconnected` would carry a sequence number *higher* than the older events
//! it introduces, which is precisely the ordering rule the rest of the system
//! is built on. They are events in shape and vocabulary, and subscription
//! notifications in delivery.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Payload of `session.reconnecting` — a subscriber is re-attaching to a
/// live session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionReconnecting {
    /// The sequence number the subscriber asked to resume from, or `null`
    /// when it asked for the live stream without backfill.
    pub from_seq: Option<u64>,
    /// The re-attaching subscriber.
    pub subscriber: String,
}

/// Payload of `session.reconnected` — the re-attach completed, with what the
/// subscriber did or did not get back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionReconnected {
    /// What backfill delivered.
    pub replay: ReplayInfo,
}

/// The outcome of a backfill request: how much history the subscriber got
/// back, and whether any was already gone.
///
/// Three outcomes, three shapes: the requested position was still buffered
/// and everything since it was replayed; it had already been evicted and
/// nothing was; or no backfill was asked for.
///
/// The gap case is the one that matters: history is bounded, so a subscriber
/// that was away too long cannot be given what it missed. Saying so
/// explicitly — with the earliest sequence number still available, and a
/// screen snapshot where the adapter keeps one — lets the subscriber resync
/// deliberately instead of silently believing it has the whole stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReplayInfo {
    /// The sequence number replay actually started from, or `null` when
    /// nothing was replayed.
    pub replayed_from: Option<u64>,
    /// How many buffered events were delivered before the live stream
    /// resumed.
    pub events_replayed: u64,
    /// `true` when the requested position had already been evicted, so some
    /// events can never be delivered.
    pub gap: bool,
    /// The oldest sequence number still buffered. Present on a gap, where it
    /// tells the subscriber how much it lost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earliest_seq: Option<u64>,
    /// The session's reconstructed screen at re-attach time, for adapters
    /// that keep one. Present on a gap, where it is the only way to convey
    /// state the evicted events carried.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_snapshot: Option<ScreenSnapshot>,
}

impl ReplayInfo {
    /// The requested position was still buffered: every event since it was
    /// replayed, and nothing was lost.
    ///
    /// One constructor per outcome, so the field combinations that describe
    /// no outcome at all — a gap that also replayed events, a replay from
    /// nowhere — are not reachable through this crate.
    pub fn within_ring(replayed_from: u64, events_replayed: u64) -> Self {
        Self {
            replayed_from: Some(replayed_from),
            events_replayed,
            gap: false,
            earliest_seq: None,
            screen_snapshot: None,
        }
    }

    /// The requested position had already been evicted: nothing was
    /// replayed, and the subscriber is attached at the live head with the
    /// events between then and now permanently unavailable.
    pub fn gap(earliest_seq: u64, screen_snapshot: Option<ScreenSnapshot>) -> Self {
        Self {
            replayed_from: None,
            events_replayed: 0,
            gap: true,
            earliest_seq: Some(earliest_seq),
            screen_snapshot,
        }
    }

    /// The subscriber asked for the live stream without backfill.
    pub fn live_from_head() -> Self {
        Self {
            replayed_from: None,
            events_replayed: 0,
            gap: false,
            earliest_seq: None,
            screen_snapshot: None,
        }
    }
}

/// The session's reconstructed screen: what a terminal attached to the CLI
/// would be showing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ScreenSnapshot {
    /// Screen width in columns.
    pub cols: u32,
    /// Screen height in rows.
    pub rows: u32,
    /// Where the cursor is.
    pub cursor: CursorPosition,
    /// The screen contents, row-major: `cells[row][col]`.
    //
    // The per-cell encoding — character plus display attributes — is settled
    // by the virtual-terminal layer, which does not exist yet. Carrying cells
    // opaquely until it does is the honest option: guessing an encoding here
    // would publish a contract this crate cannot keep, and the alternative
    // (leaving snapshots out of the taxonomy) would leave the gap case with
    // no way to convey state at all.
    pub cells: Vec<Vec<Value>>,
}

/// A cursor position on the reconstructed screen, zero-based from the top
/// left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct CursorPosition {
    /// Row, from the top.
    pub row: u32,
    /// Column, from the left.
    pub col: u32,
}

/// Payload of `session.writer_changed` — which subscriber may write to the
/// session changed hands.
///
/// Published so every reader can reflect the current writer, including the
/// case where there now is none. The type is part of the contract from the
/// start even though nothing emits it yet: a runtime serving one caller has
/// no writer to transfer, and the callers that will need this — several
/// clients sharing one session — should be able to write against the shape
/// before it starts arriving.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct SessionWriterChanged {
    /// The subscriber that may now write, or `null` when nobody may.
    pub writer: Option<String>,
    /// The subscriber that could write before, or `null` when nobody could.
    pub previous_writer: Option<String>,
    /// Why ownership moved.
    pub reason: WriterChangeReason,
}

/// Why write ownership of a session moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriterChangeReason {
    /// A subscriber claimed it.
    Acquire,
    /// The writer gave it up.
    Release,
    /// The writer's connection went away and the runtime released it on the
    /// writer's behalf.
    TransportDrop,
}
