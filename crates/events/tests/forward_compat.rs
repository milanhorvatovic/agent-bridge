//! The compatibility contract, from the consumer's side.
//!
//! The taxonomy grows within `schema_version` 1: new event types, new
//! optional payload fields, new error codes. Every one of those means a
//! consumer compiled against today's types will one day read a document it
//! does not fully recognize, and the contract says it must keep going. These
//! tests are that promise, made concrete against documents from a
//! hypothetical newer producer.

use agent_bridge_events::*;
use serde_json::{Value, json};

#[test]
fn unknown_event_types_are_tolerated() {
    // A type this revision does not publish deserializes to the fallback —
    // with the type name and payload carried through — and serializes back
    // unchanged, so a relay can pass it on without understanding it.
    let document = json!({
        "schema_version": 1,
        "type": "lifecycle.session.hibernated",
        "session_id": "0b8ee0e4-9f4f-4e6b-8f0a-3a80cf9c17d1",
        "seq": 3,
        "ts": "2026-05-16T08:00:02.000Z",
        "approval_id": null,
        "correlation_id": null,
        "payload": { "reason": "idle", "resumable": true }
    });
    let event: Event = serde_json::from_value(document.clone())
        .expect("an unknown event type must not fail the envelope");
    let EventKind::Unknown(unknown) = &event.kind else {
        panic!("expected the Unknown fallback, got {:?}", event.kind);
    };
    assert_eq!(unknown.event_type, "lifecycle.session.hibernated");
    assert_eq!(unknown.payload.get("reason"), Some(&json!("idle")));
    assert_eq!(event.kind.event_type(), "lifecycle.session.hibernated");
    assert_eq!(event.kind.namespace(), "lifecycle");
    assert_eq!(
        serde_json::to_value(&event).expect("serialization is infallible"),
        document
    );
}

#[test]
fn unknown_payload_fields_are_tolerated() {
    // New optional payload fields arrive without a schema_version bump, so
    // a consumer on the current types must read a future producer's events.
    let event: Event = serde_json::from_value(json!({
        "schema_version": 1,
        "type": "stream.token",
        "session_id": null,
        "seq": 1,
        "ts": "2026-05-16T08:00:00.000Z",
        "approval_id": null,
        "correlation_id": null,
        "payload": { "content": "hi", "a_future_field": {"nested": true} }
    }))
    .expect("unknown payload fields must be ignored, not rejected");
    let EventKind::StreamToken(payload) = &event.kind else {
        panic!("expected stream.token, got {:?}", event.kind);
    };
    assert_eq!(payload.content, "hi");
}

#[test]
fn unknown_error_codes_are_tolerated() {
    // A new code under an existing error type is additive too. The code is
    // carried verbatim rather than collapsed into a catch-all, so a consumer
    // can log it, route on it, or pass it along.
    let document = json!({
        "type": "transport.error",
        "payload": { "code": "handshake_refused", "message": "peer refused the handshake" }
    });
    let kind: EventKind =
        serde_json::from_value(document.clone()).expect("an unknown code must not fail the event");
    let EventKind::TransportError(payload) = &kind else {
        panic!("expected transport.error, got {kind:?}");
    };
    assert_eq!(
        payload.code,
        TransportErrorCode::Unknown("handshake_refused".to_owned())
    );
    assert_eq!(
        serde_json::to_value(&kind).expect("serialization is infallible"),
        document
    );

    // And a code this revision does publish still resolves to its variant —
    // the tolerant arm is tried last, not first.
    let known: EventKind = serde_json::from_value(json!({
        "type": "transport.error",
        "payload": { "code": "frame_too_large", "message": "frame exceeds the cap" }
    }))
    .expect("a published code deserializes");
    let EventKind::TransportError(payload) = &known else {
        panic!("expected transport.error, got {known:?}");
    };
    assert_eq!(payload.code, TransportErrorCode::FrameTooLarge);
}

#[test]
fn a_published_type_over_a_wrong_payload_falls_back_rather_than_failing() {
    // The deliberate consequence of the tolerant reader: an event whose
    // payload does not fit the shape its type declares still arrives, as
    // Unknown. Rejecting it is the schema's job — a consumer holding these
    // types must never be the component that drops an event.
    let kind: EventKind = serde_json::from_value(json!({
        "type": "stream.token",
        "payload": { "text": "the field is named content" }
    }))
    .expect("a mismatched payload must not fail the event");
    let EventKind::Unknown(unknown) = &kind else {
        panic!("expected the Unknown fallback, got {kind:?}");
    };
    assert_eq!(unknown.event_type, "stream.token");
}

#[test]
fn absence_has_one_spelling_on_the_way_out() {
    // An explicit null where the contract says "absent" is read as absence
    // and normalized away on the next serialize, so a slightly-off producer
    // cannot seed a second spelling into a corpus that passed through here.
    let event: Event = serde_json::from_value(json!({
        "schema_version": 1,
        "type": "stream.token",
        "session_id": null,
        "seq": 1,
        "monotonic_ns": null,
        "ts": "2026-05-16T08:00:00.000Z",
        "approval_id": null,
        "correlation_id": null,
        "payload": { "content": "hi" }
    }))
    .expect("an explicit null reads as absence");
    assert_eq!(event.monotonic_ns, None);
    let back = serde_json::to_value(&event).expect("serialization is infallible");
    assert_eq!(back.get("monotonic_ns"), None::<&Value>);
}
