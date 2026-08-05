//! The taxonomy inventory: every published event type, and what the runtime
//! does with it.
//!
//! Three artifacts have to agree about which events exist — the types here,
//! the golden traces the conformance corpus asserts, and the notifications
//! the transport delivers — and nothing but a check makes them. The inventory
//! is generated (`schema/event-taxonomy.json`) from the same derive that
//! produces the event schema, so it cannot list a type the code does not have
//! or miss one it does; `cargo xtask drift-gate` reads it and holds the
//! corpus to it.

use crate::schema::published_event_types;

/// What the runtime does with an event of a given type.
///
/// The distinction matters to anyone reading a trace: only ring events are
/// broadcast, ordered, and replayable, so a comparator that expected the
/// other two to show up in a session's stream would be asserting against
/// something that never enters it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitClass {
    /// Broadcast to every subscriber, ordered by `seq`, and buffered for
    /// backfill.
    Ring,
    /// Delivered to one subscriber as part of its own subscription, never
    /// broadcast and never buffered. A re-attach notification carries a
    /// sequence position rather than occupying one.
    SubscriptionNotification,
    /// Published as a contract, emitted by nothing yet.
    Reserved,
}

impl EmitClass {
    /// The spelling used in the generated inventory.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ring => "ring",
            Self::SubscriptionNotification => "subscription_notification",
            Self::Reserved => "reserved",
        }
    }
}

/// One event type and its emit class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaxonomyEntry {
    /// The dotted event-type name.
    pub event_type: String,
    /// What the runtime does with it.
    pub class: EmitClass,
}

/// The event types that are not ordinary ring events.
///
/// Everything else is [`EmitClass::Ring`], which is why only the exceptions
/// are listed: a new event type is a ring event unless someone decides
/// otherwise, and defaulting the other way would let a type slip into the
/// inventory as "not broadcast" through inattention.
const EXCEPTIONS: &[(&str, EmitClass)] = &[
    // Re-attach is a conversation between the runtime and the subscriber
    // doing it. Broadcasting it would tell every *other* subscriber about a
    // reconnection that did not concern them, with a sequence number newer
    // than the older events it introduces.
    ("session.reconnecting", EmitClass::SubscriptionNotification),
    ("session.reconnected", EmitClass::SubscriptionNotification),
    // A runtime serving a single caller has no writer to transfer. The type
    // is published so the callers that will need it can be written against
    // it before it starts arriving.
    ("session.writer_changed", EmitClass::Reserved),
];

/// Every event type this revision publishes, with its emit class.
///
/// The names come from the same derive that generates the event schema —
/// that is, from the enum itself — so this inventory cannot drift from the
/// types. What is stated by hand is only the classification.
///
/// # Panics
///
/// If an exception names a type the taxonomy does not publish. That is a
/// stale entry left behind by a rename, and generating an inventory from it
/// would publish a classification for an event that cannot occur.
pub fn taxonomy() -> Vec<TaxonomyEntry> {
    let published = published_event_types();
    for (event_type, _) in EXCEPTIONS {
        assert!(
            published.iter().any(|published| published == event_type),
            "the emit-class exception for `{event_type}` names a type the taxonomy does not \
             publish — remove the exception, or restore the type"
        );
    }
    let mut entries: Vec<TaxonomyEntry> = published
        .into_iter()
        .map(|event_type| {
            let class = EXCEPTIONS
                .iter()
                .find(|(exception, _)| *exception == event_type)
                .map_or(EmitClass::Ring, |(_, class)| *class);
            TaxonomyEntry { event_type, class }
        })
        .collect();
    entries.sort_by(|left, right| left.event_type.cmp(&right.event_type));
    entries
}
