//! The published artifacts: that they are what the types generate, that
//! they accept what the types emit, and that they still reject what the
//! contract forbids.
//!
//! An artifact nobody validates against is a document, not a contract. These
//! tests close the loop in both directions — every typed event must pass the
//! committed schema, and a schema that accepts anything must fail here.

mod support;

use std::path::PathBuf;

use agent_bridge_events::*;
use serde_json::{Value, json};
use support::{envelope, every_event_kind};

fn committed(name: &str) -> Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../schema")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "{}: cannot read the committed artifact ({err}) — generate it with \
             `cargo run -p agent-bridge-events --bin schema-gen`",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("a committed artifact must parse")
}

#[test]
fn typed_events_validate_against_the_committed_envelope_schema() {
    // Through the committed file, not an in-memory regeneration, so a stale
    // artifact fails here too.
    let schema = committed("events.schema.json");
    let validator = jsonschema::validator_for(&schema).expect("the committed schema must compile");

    let unknown = EventKind::Unknown(UnknownEvent {
        event_type: "lifecycle.session.hibernated".to_owned(),
        payload: serde_json::Map::new(),
    });
    for (seq, kind) in every_event_kind().into_iter().chain([unknown]).enumerate() {
        let event = envelope(seq as u64, kind);
        let event_type = event.kind.event_type().to_owned();
        let document = serde_json::to_value(&event).expect("serialization is infallible");
        assert!(
            validator.validate(&document).is_ok(),
            "the committed schema rejects {event_type}: {document}"
        );
    }
}

#[test]
fn the_committed_envelope_schema_rejects_what_the_contract_forbids() {
    // A schema that accepts anything would make the assertions above
    // meaningless; pin each envelope rule to a rejection. Note what is
    // absent here: an unknown dotted event type is *not* a violation — that
    // is the additive-growth rule, asserted from the typed side above — but
    // a published type over a payload that misses its required fields is.
    let validator = jsonschema::validator_for(&committed("events.schema.json"))
        .expect("the committed schema must compile");
    for (label, broken) in [
        (
            "missing session_id",
            json!({"schema_version": 1, "seq": 0, "ts": "2026-05-16T08:00:00.000Z",
                   "approval_id": null, "correlation_id": null,
                   "type": "stream.token", "payload": {"content": "x"}}),
        ),
        (
            "published type over a malformed payload",
            json!({"schema_version": 1, "session_id": null, "seq": 0,
                   "ts": "2026-05-16T08:00:00.000Z", "approval_id": null,
                   "correlation_id": null, "type": "stream.token", "payload": {}}),
        ),
        (
            "undotted event type",
            json!({"schema_version": 1, "session_id": null, "seq": 0,
                   "ts": "2026-05-16T08:00:00.000Z", "approval_id": null,
                   "correlation_id": null, "type": "token", "payload": {}}),
        ),
        (
            "wrong schema_version",
            json!({"schema_version": 2, "session_id": null, "seq": 0,
                   "ts": "2026-05-16T08:00:00.000Z", "approval_id": null,
                   "correlation_id": null, "type": "stream.token", "payload": {"content": "x"}}),
        ),
        (
            "approval prompt with a null approval_id",
            json!({"schema_version": 1, "session_id": null, "seq": 0,
                   "ts": "2026-05-16T08:00:00.000Z", "approval_id": null,
                   "correlation_id": null, "type": "prompt.approval_required",
                   "payload": {"prompt": "?"}}),
        ),
        (
            "tool call without the id that pairs it",
            json!({"schema_version": 1, "session_id": null, "seq": 0,
                   "ts": "2026-05-16T08:00:00.000Z", "approval_id": null,
                   "correlation_id": null, "type": "tool.call_started",
                   "payload": {"tool": "bash"}}),
        ),
        (
            "screen cell claiming a width outside the three that mean anything",
            json!({"schema_version": 1, "session_id": null, "seq": 0,
                   "ts": "2026-05-16T08:00:00.000Z", "approval_id": null,
                   "correlation_id": null, "type": "session.reconnected",
                   "payload": {"replay": {"replayed_from": null, "events_replayed": 0,
                                          "gap": true, "earliest_seq": 1,
                                          "screen_snapshot": {
                                              "cols": 1, "rows": 1,
                                              "cursor": {"row": 0, "col": 0},
                                              "styles": [{}],
                                              "cells": [[{"ch": "x", "width": 47}]]}}}}),
        ),
        (
            "writer change with an invented reason",
            json!({"schema_version": 1, "session_id": null, "seq": 0,
                   "ts": "2026-05-16T08:00:00.000Z", "approval_id": null,
                   "correlation_id": null, "type": "session.writer_changed",
                   "payload": {"writer": null, "previous_writer": null,
                               "reason": "abdicated"}}),
        ),
        (
            "null monotonic_ns (omit the field instead)",
            json!({"schema_version": 1, "session_id": null, "seq": 0,
                   "monotonic_ns": null, "ts": "2026-05-16T08:00:00.000Z",
                   "approval_id": null, "correlation_id": null,
                   "type": "stream.token", "payload": {"content": "x"}}),
        ),
    ] {
        assert!(
            validator.validate(&broken).is_err(),
            "{label}: the committed schema must reject this envelope"
        );
    }
}

#[test]
fn the_committed_inventory_is_the_one_the_types_generate() {
    // The freshness gate byte-compares in CI; this says the same thing in a
    // form that names what changed, so a forgotten regeneration reads as
    // "the inventory is stale" rather than as a diff of a thousand lines.
    let committed = committed("event-taxonomy.json");
    let generated = taxonomy_manifest();
    assert_eq!(
        committed, generated,
        "the committed taxonomy inventory is stale — regenerate it with \
         `cargo run -p agent-bridge-events --bin schema-gen`"
    );
}

#[test]
fn artifact_generation_is_deterministic() {
    // The freshness gate byte-compares regenerated output against the
    // committed artifact, which is only sound if generation is a pure
    // function. Generate twice and require byte equality, and require the
    // canonical-form invariants the committed files rely on: LF-only, one
    // trailing newline, valid JSON.
    for (name, first, second) in [
        (
            "events.schema.json",
            canonical_json(&event_schema()),
            canonical_json(&event_schema()),
        ),
        (
            "trace-record.schema.json",
            canonical_json(&trace_record_schema()),
            canonical_json(&trace_record_schema()),
        ),
        (
            "event-taxonomy.json",
            canonical_json(&taxonomy_manifest()),
            canonical_json(&taxonomy_manifest()),
        ),
    ] {
        assert_eq!(first, second, "{name}: generation must be deterministic");
        assert!(!first.contains('\r'), "{name}: artifacts are LF-only");
        assert!(first.ends_with('\n'), "{name}: one trailing newline");
        assert!(!first.ends_with("\n\n"), "{name}: one trailing newline");
        serde_json::from_str::<Value>(&first).expect("artifacts must parse as JSON");
    }
}

#[test]
fn canonical_json_sorts_keys_and_indents_stably() {
    // The canonical form is what makes artifact diffs reviewable and the
    // byte compare OS-independent; pin it on a small value so a formatting
    // regression fails here, not as an inscrutable freshness-gate diff.
    let value = json!({"b": [1, {"z": null, "a": "x"}], "a": {}, "c": "τ"});
    assert_eq!(
        canonical_json(&value),
        "{\n  \"a\": {},\n  \"b\": [\n    1,\n    {\n      \"a\": \"x\",\n      \"z\": null\n    }\n  ],\n  \"c\": \"τ\"\n}\n"
    );
}
