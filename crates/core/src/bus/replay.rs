//! The backfill outcome: what a re-attaching subscriber gets back, decided
//! in one place.
//!
//! [`ReplayPlan`] is the bus's half of the reconnect contract — one variant
//! per documented `session.reconnected.replay` shape.
//! The bus computes the plan and preloads the replay slice; *delivering*
//! the `session.reconnecting` / `session.reconnected` notifications is the
//! session layer's job, to the re-attaching subscriber alone. They are
//! subscription-scoped by design: published through the bus they would
//! carry a `seq` higher than the older events they introduce, which is
//! exactly the ordering rule the rest of the system leans on.
//!
//! The gap outcome is a payload shape, never a protocol error. No code
//! path here — or anywhere on the attach path — constructs `-32004`; that
//! code stays reserved for a future explicit `session.replay` method,
//! and the reserved-pattern drift gate holds the line.

use std::collections::VecDeque;
use std::sync::Arc;

use agent_bridge_events::{Event, ReplayInfo, ScreenSnapshot};

use super::BusError;
use super::filter::FilterSet;
use super::ring::Ring;

/// The computed backfill outcome for one
/// [`subscribe_from`](super::EventBus::subscribe_from) call.
///
/// Whatever the variant, the subscription is attached at head — the plan
/// describes what history came with it, never whether it exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayPlan {
    /// The requested position is still held: everything from `from_seq` to
    /// head is preloaded on the subscription, ahead of the live stream.
    WithinRing {
        /// The `from_seq` the subscriber asked for.
        replayed_from: u64,
        /// How many events the subscription will deliver before its first
        /// live one. With the unfiltered attach path this is exactly
        /// `head − from_seq`; under a narrower filter it is the count the
        /// filter admits, because the field reports what is *delivered* —
        /// a decision the wire contract inherits and should freeze
        /// deliberately when its attach method grows `from_seq`, since the
        /// published reconnect contract only specifies the unfiltered
        /// path.
        events_replayed: u64,
    },
    /// The requested position was already evicted: nothing is replayed and
    /// the events between `from_seq` and `earliest_seq` are permanently
    /// unavailable — said plainly instead of delivering a hole.
    Gap {
        /// The oldest `seq` a request could still have been served from.
        /// When even that is gone (the ring emptied), this is head: the
        /// first position the next event will make replayable.
        earliest_seq: u64,
    },
    /// No `from_seq` was asked for: live from head, nothing replayed,
    /// nothing missing.
    LiveFromHead,
}

impl ReplayPlan {
    /// The `session.reconnected.replay` payload this plan maps onto.
    ///
    /// `screen_snapshot` is consulted only by the gap shape — it exists to
    /// carry state the evicted events can no longer convey, so the shapes
    /// that lost nothing never include one. The bus takes it as a value
    /// because it never knows whether a session keeps a screen: the caller
    /// owning that answer passes `None` when effective `tui_aware` is off,
    /// and the wire's omitted key falls out of the type.
    pub fn to_replay_info(&self, screen_snapshot: Option<ScreenSnapshot>) -> ReplayInfo {
        match self {
            Self::WithinRing {
                replayed_from,
                events_replayed,
            } => ReplayInfo::within_ring(*replayed_from, *events_replayed),
            Self::Gap { earliest_seq } => ReplayInfo::gap(*earliest_seq, screen_snapshot),
            Self::LiveFromHead => ReplayInfo::live_from_head(),
        }
    }
}

/// Decide the plan and clone out the replay slice, under the caller's lock.
///
/// Called with the channel's critical section held — the same one that
/// registers the subscriber slot at head — so the slice and the live stream
/// are contiguous in `seq` by construction: nothing can publish between
/// the slice's end and the slot's first live event, which is the whole
/// defense against a reconnect that silently skips or repeats.
///
/// A `from_seq` past head is refused rather than served: it claims events
/// that were never stamped, so attaching it at head as "nothing missed"
/// would hand later live events to a caller whose bookkeeping says it has
/// already seen past them. When the wire's attach method grows
/// `from_seq`, the transport maps this onto its invalid-params surface.
pub(crate) fn plan(
    ring: &Ring,
    head: u64,
    from_seq: Option<u64>,
    filters: &FilterSet,
    session_id: &str,
) -> Result<(ReplayPlan, VecDeque<Arc<Event>>), BusError> {
    let Some(from_seq) = from_seq else {
        return Ok((ReplayPlan::LiveFromHead, VecDeque::new()));
    };
    if from_seq > head {
        return Err(BusError::FromSeqBeyondHead {
            session_id: session_id.to_owned(),
            from_seq,
            head,
        });
    }
    if from_seq == head {
        // Nothing was missed, so the ring — which may have evicted
        // everything by now — has no say: an empty replay is within-ring,
        // not a gap.
        return Ok((
            ReplayPlan::WithinRing {
                replayed_from: from_seq,
                events_replayed: 0,
            },
            VecDeque::new(),
        ));
    }
    match ring.entries_from(from_seq) {
        Some(entries) => {
            // Reserved up front — the pre-filter slice length is exactly
            // `head − from_seq` — so the capture under the caller's lock is
            // one allocation instead of a doubling climb through ~a dozen
            // reallocations for a full ring. A narrow filter over-reserves
            // by at most that same slice of pointers, freed with the
            // buffer when the subscription drops.
            let mut replay: VecDeque<Arc<Event>> = VecDeque::with_capacity(
                usize::try_from(head - from_seq).expect("a ring slice fits usize"),
            );
            replay.extend(
                entries
                    .filter(|event| filters.admits(event))
                    .map(Arc::clone),
            );
            let events_replayed = u64::try_from(replay.len())
                .expect("a ring bounded in memory cannot hold more than u64::MAX events");
            Ok((
                ReplayPlan::WithinRing {
                    replayed_from: from_seq,
                    events_replayed,
                },
                replay,
            ))
        }
        // `from_seq < head` means the missing events existed; an empty
        // ring then reports head as the earliest — the first position that
        // could not have been lost.
        None => Ok((
            ReplayPlan::Gap {
                earliest_seq: ring.earliest_seq().unwrap_or(head),
            },
            VecDeque::new(),
        )),
    }
}
