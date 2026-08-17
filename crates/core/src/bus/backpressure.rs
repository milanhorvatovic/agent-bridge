//! The lag policy's state machine and its coarse sweep.
//!
//! The contract this module carries is the flow-control policy for the
//! bus→subscriber edge: a per-subscriber bounded queue, a grace window
//! that separates "momentarily behind" from "not draining", and a
//! disconnect that is announced — the terminal `transport.error` of code
//! `subscriber_lagging` — never silent. The policy is *evaluated* where
//! delivery happens, in the channel's drain step (`deliver` in
//! [`super`]); what lives here is the per-subscriber state those
//! evaluations move through, and the timer that resolves a lag no publish
//! is left to observe.
//!
//! Deadlines are read off [`tokio::time::Instant`], not the std clock the
//! ring's age bound uses, so one mechanism serves both this grace window
//! and the bounded writer's drain deadline — and both become virtual under
//! `tokio::time::pause`, which is what lets the precision assertions live
//! in paused-clock tests while the sweep tick stays coarse.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use agent_bridge_events::Event;
use tokio::time::Instant;

use super::{BusInner, SubscriberSlot, lock};

/// The bus side of the runtime's flow-control contract, carried as
/// configuration so the deployment config's `[transport]` table maps onto
/// it field for field.
#[derive(Debug, Clone, Copy)]
pub struct BackpressureConfig {
    /// How many undelivered events one subscriber's queue holds —
    /// `transport.subscriber_queue_bound` in the deployment config. Must
    /// be at least 1. A stalled subscriber additionally holds one parked
    /// event in its overflow slot and one reserved terminal slot, so its
    /// bound-side memory is `queue_bound + 2` events, exactly.
    pub queue_bound: usize,
    /// How long a full subscriber gets to drain before the bus disconnects
    /// it — `transport.subscriber_grace_seconds` in the deployment config.
    pub grace: Duration,
}

impl Default for BackpressureConfig {
    fn default() -> Self {
        // The documented `[transport]` deployment defaults, fixed here so
        // the binary's TOML loader becomes a field-for-field mapping, not
        // a second place deciding values.
        Self {
            queue_bound: 1024,
            grace: Duration::from_secs(2),
        }
    }
}

/// Where one subscriber stands against the lag policy. Owned by its
/// [`SubscriberSlot`] and touched only by the channel's single drainer,
/// which is what lets the transitions stay plain moves instead of
/// synchronized state.
#[derive(Debug)]
pub(crate) enum LagState {
    /// Keeping up: every admitted event went straight into the queue.
    Healthy,
    /// The queue filled and one event is parked in the overflow slot. The
    /// subscriber survives if the parked event flushes before the deadline
    /// — that is the whole point of the grace window — and every flush is
    /// a fresh episode: a subscriber oscillating at the bound is draining,
    /// slowly, and a lossless slow drain is throughput's problem, not this
    /// policy's.
    Parked {
        parked: Arc<Event>,
        deadline: ArmedState,
    },
    /// Events arrived past the queue *and* the overflow slot: the stream
    /// this subscriber observes is now gapped, which no later drain can
    /// repair, so the episode can only end in the announced disconnect.
    /// The parked event was dropped at this transition — it is part of
    /// `lost` — and nothing further is delivered; the count rides to the
    /// terminal event's detail so the loss is stated, never silent.
    Lossy { deadline: ArmedState, lost: u64 },
}

/// A grace deadline, or the reason there is none yet: a re-attached
/// subscriber still draining its preloaded replay buffer is busy catching
/// up by instruction, and holding the stopwatch on it would punish exactly
/// the behavior backfill asks for. The deadline arms at the first policy
/// touch after the replay drains (1.7b left this flag; the state lives
/// here).
#[derive(Debug, Clone, Copy)]
pub(crate) enum ArmedState {
    Armed(Instant),
    AwaitingReplayDrain,
}

impl LagState {
    /// Whether this state's deadline has passed — the one question the
    /// sweep and every delivery ask first. Arms a deadline that was
    /// waiting on the replay drain, which is why it takes `&mut`: the
    /// grace window starts at the first observation after the drain, not
    /// retroactively at the park.
    pub(crate) fn expired(&mut self, now: Instant, replay_drained: bool, grace: Duration) -> bool {
        let deadline = match self {
            Self::Healthy => return false,
            Self::Parked { deadline, .. } | Self::Lossy { deadline, .. } => deadline,
        };
        match deadline {
            ArmedState::Armed(at) => now >= *at,
            ArmedState::AwaitingReplayDrain => {
                if replay_drained {
                    *deadline = ArmedState::Armed(now + grace);
                }
                false
            }
        }
    }
}

/// How often the sweep visits each channel looking for expired deadlines.
/// Coarse on purpose: the tick bounds how *late* past its grace window an
/// idle-stream lag can resolve, nothing else, and the paused-clock tests
/// carry the precision assertions so this never has to be fine.
pub(crate) const SWEEP_TICK: Duration = Duration::from_millis(250);

/// The timer half of lag detection: deadlines are checked on every
/// delivery, but a subscriber whose stream goes quiet after filling its
/// queue would otherwise hold its lag forever — no publish arrives to
/// observe it. This task sweeps every channel each tick and resolves
/// exactly that case.
///
/// Holds the bus weakly so a dropped bus is a stopped sweeper, and skips
/// any channel with an active drainer — that drainer is already checking
/// deadlines, and the next tick will catch whatever it missed.
pub(crate) fn spawn_sweeper(inner: &Arc<BusInner>) {
    let weak = Arc::downgrade(inner);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let Some(inner) = weak.upgrade() else { return };
            let channels: Vec<_> = lock(&inner.sessions).values().cloned().collect();
            for channel in channels {
                channel.sweep(inner.anchor);
            }
            inner.global.sweep(inner.anchor);
        }
    });
}

/// Whether this slot's subscription has finished its preloaded replay —
/// the flag [`ArmedState::AwaitingReplayDrain`] waits on. Relaxed is
/// enough: the flag only ever moves false→true, and observing the flip a
/// beat late merely starts the grace window one observation later.
pub(crate) fn replay_drained(slot: &SubscriberSlot) -> bool {
    slot.replay_drained.load(Ordering::Relaxed)
}
