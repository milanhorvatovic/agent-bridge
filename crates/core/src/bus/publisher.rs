//! The one handle that stamps a session's envelopes.
//!
//! Everything the gap-free `seq` contract promises hangs on there being
//! exactly one of these per session. The bus enforces that at registration
//! — a second request for a live session is an error — and this type
//! enforces it structurally by not being `Clone`: sharing a `Publisher`
//! across tasks is done by reference (publish takes `&self`), never by
//! minting a second stamping site.

use std::sync::Arc;
use std::time::Instant;

use agent_bridge_events::EventBody;

use super::{BusError, Channel};

/// A session's single publishing handle, returned by
/// [`EventBus::register_session`](super::EventBus::register_session).
///
/// Deliberately not `Clone` — the choke-point discipline is structural, not
/// conventional. Dropping the `Publisher` does *not* seal the session:
/// sealing is the explicit
/// [`EventBus::seal_session`](super::EventBus::seal_session) call, so a
/// handle can die with its owning task while the session's subscribers live
/// on.
#[derive(Debug)]
pub struct Publisher {
    pub(crate) channel: Arc<Channel>,
    pub(crate) anchor: Instant,
}

impl Publisher {
    /// Complete the envelope and fan the event out — synchronous, and never
    /// blocked by a subscriber.
    ///
    /// The bus fills what only it can get right: `schema_version`, the
    /// session's id, the next consecutive `seq` (from 0 at registration),
    /// the RFC 3339 `ts`, and `monotonic_ns`. The sequence increment and
    /// the per-subscriber queue pushes happen inside one short critical
    /// section, so every subscriber's queue order is `seq` order even when
    /// this handle is shared across tasks. Delivery is `try_send` per
    /// subscriber; a full queue is that subscriber's problem, never the
    /// publisher's.
    ///
    /// Returns the stamped `seq`. Fails only once the session is sealed.
    pub fn publish(&self, body: EventBody) -> Result<u64, BusError> {
        self.channel.publish(body, self.anchor)
    }

    /// The session this handle stamps for.
    pub fn session_id(&self) -> &str {
        self.channel
            .session_id
            .as_deref()
            .expect("a Publisher is only ever constructed over a session channel")
    }
}
