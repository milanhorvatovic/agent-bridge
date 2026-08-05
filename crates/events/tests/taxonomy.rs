//! The inventory: what the taxonomy publishes, and what it deliberately
//! does not.
//!
//! The generated inventory is what the drift gate holds the conformance
//! corpus to, so it has to be complete and it has to be right. Complete is
//! checked against a hand-written instance of every event type; right is
//! checked against the two names that must never appear in it.

mod support;

use agent_bridge_events::*;
use support::every_event_kind;

/// The names in the generated inventory, in order.
fn published() -> Vec<String> {
    taxonomy()
        .into_iter()
        .map(|entry| entry.event_type)
        .collect()
}

#[test]
fn the_inventory_covers_every_event_type_and_nothing_else() {
    // The inventory is derived from the enum; this list is written by hand.
    // Requiring them to match means an event type cannot be added without a
    // sample to test it with, and a sample cannot outlive its type.
    let mut sampled: Vec<String> = every_event_kind()
        .iter()
        .map(|kind| kind.event_type().to_owned())
        .collect();
    let mut inventoried = published();
    sampled.sort();
    inventoried.sort();
    assert_eq!(
        sampled, inventoried,
        "the sampled event types and the generated inventory disagree"
    );
}

#[test]
fn the_reported_event_type_is_the_serialized_one() {
    // Prefix subscriptions filter on the reported name while the wire
    // carries the serialized one; if those two ever disagree, a subscription
    // silently stops matching the events it was written for.
    for kind in every_event_kind() {
        let document = serde_json::to_value(&kind).expect("serialization is infallible");
        assert_eq!(
            document["type"],
            *kind.event_type(),
            "{:?} reports a different type than it serializes",
            kind
        );
        let namespace = kind.namespace();
        assert!(
            kind.event_type().starts_with(&format!("{namespace}.")),
            "{} is not within its reported namespace {namespace}",
            kind.event_type()
        );
    }
}

#[test]
fn health_is_a_snapshot_and_the_event_is_the_transition() {
    // Asking for the runtime's health is a request, not an event. The
    // taxonomy carries only the transition — a `runtime.health` event would
    // be a second, silently different answer to the same question, and this
    // pairing has been reintroduced often enough to be worth a test.
    let published = published();
    assert!(
        published
            .iter()
            .any(|name| name == "runtime.health_changed"),
        "the health transition event is missing"
    );
    assert!(
        !published.iter().any(|name| name == "runtime.health"),
        "runtime.health is a request for a snapshot, never an event type"
    );
}

#[test]
fn the_end_of_a_subscription_is_not_an_event() {
    // A subscription ending says nothing about the session, which usually
    // keeps running. Publishing it here would tell every other subscriber
    // that something happened to the session when nothing did.
    assert!(
        !published().iter().any(|name| name == "session.eof"),
        "the end of a subscription is a transport notification, not an event"
    );
}

#[test]
fn only_the_documented_exceptions_are_not_ring_events() {
    // Everything is broadcast and replayable unless there is a reason it
    // cannot be. Pinning the exceptions means a new type cannot quietly
    // become one.
    let exceptions: Vec<(String, EmitClass)> = taxonomy()
        .into_iter()
        .filter(|entry| entry.class != EmitClass::Ring)
        .map(|entry| (entry.event_type, entry.class))
        .collect();
    assert_eq!(
        exceptions,
        vec![
            (
                "session.reconnected".to_owned(),
                EmitClass::SubscriptionNotification
            ),
            (
                "session.reconnecting".to_owned(),
                EmitClass::SubscriptionNotification
            ),
            ("session.writer_changed".to_owned(), EmitClass::Reserved),
        ]
    );
}

#[test]
fn the_published_namespaces_are_the_documented_ones() {
    // A namespace is what a consumer subscribes to by prefix, so adding one
    // is a contract change even though it needs no version bump. Listing
    // them makes that visible in review.
    let mut namespaces: Vec<String> = every_event_kind()
        .iter()
        .map(|kind| kind.namespace().to_owned())
        .collect();
    namespaces.sort();
    namespaces.dedup();
    assert_eq!(
        namespaces,
        [
            "adapter",
            "lifecycle",
            "prompt",
            "pty",
            "runtime",
            "session",
            "stream",
            "tool",
            "transport"
        ]
    );
}
