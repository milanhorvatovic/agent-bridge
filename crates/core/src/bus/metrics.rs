//! The bus's supervisor-action accounting.
//!
//! One counter today: lag disconnects. `runtime.health` reports it as part
//! of `supervisor_actions_last_minute` when the health surface lands in a
//! later phase — windowing a monotonic count into "last minute" is that
//! snapshot's job, deliberately not this crate's, so the bus carries the
//! cheapest thing that cannot be wrong: an increment at the action site.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A shared, read-anywhere view of the bus's own actions, from
/// [`EventBus::metrics`](super::EventBus::metrics). Cheap to clone —
/// clones read the same counters.
#[derive(Debug, Clone, Default)]
pub struct BusMetrics {
    disconnect_subscriber: Arc<AtomicU64>,
}

impl BusMetrics {
    /// How many subscriptions the bus has sealed for lag over its
    /// lifetime. Monotonic; never reset.
    pub fn disconnect_subscriber_count(&self) -> u64 {
        self.disconnect_subscriber.load(Ordering::Relaxed)
    }

    pub(crate) fn record_disconnect_subscriber(&self) {
        self.disconnect_subscriber.fetch_add(1, Ordering::Relaxed);
    }
}
