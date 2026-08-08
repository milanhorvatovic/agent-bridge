//! Serde round-trips of the documented event shapes.
//!
//! The JSON here is the documented contract, copied verbatim: the envelope
//! with its `type` discriminant and `payload` object, the payload shape of
//! each event type, explicit nulls on the nullable correlation fields, and
//! `monotonic_ns` absent when unknown. A round-trip failure means the types
//! no longer produce the documented wire shape — which is a contract change,
//! never a refactor.

mod support;

use agent_bridge_events::*;
use serde_json::{Value, json};
use support::{envelope, every_event_kind};

/// Parse, serialize, and require value equality with the input — so both
/// directions of the mapping are pinned.
fn roundtrip(input: Value) -> Event {
    let event: Event =
        serde_json::from_value(input.clone()).expect("the documented example must deserialize");
    let back = serde_json::to_value(&event).expect("serialization is infallible");
    assert_eq!(back, input, "serializing back must reproduce the document");
    event
}

/// The same, for a documented `type` / `payload` pair on its own — which is
/// exactly what an event's discriminant and payload are, envelope aside.
fn roundtrip_kind(input: Value) -> EventKind {
    let kind: EventKind =
        serde_json::from_value(input.clone()).expect("the documented example must deserialize");
    let back = serde_json::to_value(&kind).expect("serialization is infallible");
    assert_eq!(back, input, "serializing back must reproduce the document");
    assert_eq!(
        input["type"],
        *kind.event_type(),
        "the reported event type must be the one on the wire"
    );
    kind
}

#[test]
fn envelope_base_shape_roundtrips() {
    // The full envelope: every shared field present, correlation fields
    // explicitly null, a stream.token payload with the adapter source tag.
    let event = roundtrip(json!({
        "schema_version": 1,
        "type": "stream.token",
        "session_id": "0b8ee0e4-9f4f-4e6b-8f0a-3a80cf9c17d1",
        "seq": 42,
        "monotonic_ns": 12_345_678_901_234_u64,
        "ts": "2026-05-16T08:00:00.123Z",
        "approval_id": null,
        "correlation_id": null,
        "payload": { "source": "claude", "content": "Analyzing repository..." }
    }));
    assert_eq!(event.schema_version, SCHEMA_VERSION);
    assert_eq!(event.seq, 42);
    assert_eq!(event.kind.namespace(), "stream");
    let EventKind::StreamToken(payload) = &event.kind else {
        panic!("expected stream.token, got {:?}", event.kind);
    };
    assert_eq!(payload.source.as_deref(), Some("claude"));
    assert_eq!(payload.content, "Analyzing repository...");
}

#[test]
fn approval_required_carries_its_approval_id() {
    // The one event type where the envelope's approval_id is non-null by
    // contract, with the offered options in the payload.
    let event = roundtrip(json!({
        "schema_version": 1,
        "type": "prompt.approval_required",
        "session_id": "0b8ee0e4-9f4f-4e6b-8f0a-3a80cf9c17d1",
        "seq": 7,
        "ts": "2026-05-16T08:00:01.000Z",
        "approval_id": "a-7f3",
        "correlation_id": null,
        "payload": { "prompt": "Allow filesystem write?", "options": ["y", "n"] }
    }));
    assert_eq!(event.approval_id.as_deref(), Some("a-7f3"));
    let EventKind::PromptApprovalRequired(payload) = &event.kind else {
        panic!("expected prompt.approval_required, got {:?}", event.kind);
    };
    assert_eq!(payload.prompt, "Allow filesystem write?");
    assert_eq!(
        payload.options.as_deref(),
        Some(["y".to_owned(), "n".to_owned()].as_slice())
    );
}

#[test]
fn an_approval_prompt_cannot_be_built_without_its_id() {
    // The construction contract: the payload type is sealed, so this
    // constructor — which takes the id the caller resolves — is the only way
    // a producer can emit an approval prompt. (The seal is what makes that
    // true; a test cannot demonstrate code that does not compile, so what is
    // asserted here is that the one available path fills the field in.)
    let body = EventBody::approval_required(
        "a-7f3",
        ApprovalPrompt::new("Allow filesystem write?").options(["y", "n"]),
    );
    assert_eq!(body.approval_id.as_deref(), Some("a-7f3"));
    let EventKind::PromptApprovalRequired(payload) = &body.kind else {
        panic!("expected prompt.approval_required, got {:?}", body.kind);
    };
    assert_eq!(payload.prompt, "Allow filesystem write?");

    // The contrast that makes it meaningful: every other event is built
    // uncorrelated, and stays uncorrelated even while approvals are pending.
    let unrelated = EventBody::new(EventKind::StreamToken(StreamToken {
        source: None,
        content: "still working".to_owned(),
    }));
    assert_eq!(unrelated.approval_id, None);
}

#[test]
fn lifecycle_events_roundtrip() {
    // created names its adapter, closed reports how the session ended.
    // session_id null exercises the required-but-nullable envelope contract.
    let created = roundtrip(json!({
        "schema_version": 1,
        "type": "lifecycle.session.created",
        "session_id": null,
        "seq": 0,
        "ts": "2026-05-16T08:00:00.000Z",
        "approval_id": null,
        "correlation_id": null,
        "payload": { "adapter": "fake" }
    }));
    let EventKind::LifecycleSessionCreated(payload) = &created.kind else {
        panic!("expected lifecycle.session.created, got {:?}", created.kind);
    };
    assert_eq!(payload.adapter.as_deref(), Some("fake"));

    let closed = roundtrip(json!({
        "schema_version": 1,
        "type": "lifecycle.session.closed",
        "session_id": "0b8ee0e4-9f4f-4e6b-8f0a-3a80cf9c17d1",
        "seq": 9,
        "ts": "2026-05-16T08:00:05.000Z",
        "approval_id": null,
        "correlation_id": null,
        "payload": { "exit_code": 0, "duration_ms": 5_000, "drained": false }
    }));
    let EventKind::LifecycleSessionClosed(payload) = &closed.kind else {
        panic!("expected lifecycle.session.closed, got {:?}", closed.kind);
    };
    assert_eq!(payload.exit_code, Some(0));
    assert_eq!(payload.drained, Some(false));
    // Byte counts are optional for the same reason the exit code is: what a
    // close knows depends on how it happened.
    assert_eq!(payload.bytes_read, None);
}

#[test]
fn documented_payload_shapes_roundtrip() {
    // Every payload shape the contract documents, verbatim. The pairs are
    // `type` + `payload` — an envelope's discriminant and its payload — so
    // what is pinned here is exactly what an integrator reads off the wire.
    let cases = [
        json!({ "type": "stream.token",
                "payload": { "source": "claude", "content": "Analyzing repository..." } }),
        json!({ "type": "stream.stderr",
                "payload": { "content": "Unhandled exception" } }),
        json!({ "type": "stream.unrecognized_output",
                "payload": { "content": "unfamiliar prompt format (post-ANSI-strip)" } }),
        json!({ "type": "tool.call_started",
                "payload": { "call_id": "t-9c2", "tool": "bash", "command": "git status" } }),
        json!({ "type": "tool.call_completed",
                "payload": { "call_id": "t-9c2", "exit_code": 0, "duration_ms": 134 } }),
        json!({ "type": "tool.call_failed",
                "payload": { "call_id": "t-9c2", "reason": "timeout" } }),
        json!({ "type": "tool.result",
                "payload": { "call_id": "t-9c2", "content": "On branch main\n..." } }),
        json!({ "type": "prompt.approval_required",
                "payload": { "prompt": "Allow filesystem write?", "options": ["y", "n"] } }),
        json!({ "type": "session.reconnecting",
                "payload": { "from_seq": 142, "subscriber": "s-3a" } }),
        json!({ "type": "session.reconnected",
                "payload": { "replay": { "replayed_from": 142, "events_replayed": 17,
                                         "gap": false } } }),
        json!({ "type": "session.writer_changed",
                "payload": { "writer": "s-7b", "previous_writer": "s-3a",
                             "reason": "acquire" } }),
        json!({ "type": "session.writer_changed",
                "payload": { "writer": null, "previous_writer": "s-7b",
                             "reason": "release" } }),
        json!({ "type": "session.writer_changed",
                "payload": { "writer": null, "previous_writer": "s-3a",
                             "reason": "transport_drop" } }),
        json!({ "type": "runtime.error",
                "payload": { "code": "log_disk_full",
                             "message": "log volume is full" } }),
        json!({ "type": "pty.error",
                "payload": { "code": "encoding_replacement",
                             "message": "undecodable bytes were replaced",
                             "detail": { "replacements": 3 } } }),
        json!({ "type": "adapter.version_warning",
                "payload": { "adapter": "claude", "detected_version": "2.1.201",
                             "supported_range": ">=2.0.0, <2.1.0" } }),
        json!({ "type": "runtime.health_changed",
                "payload": { "status": "degraded", "previous": "ok",
                             "reason": "log volume below 5% free" } }),
    ];
    for case in cases {
        roundtrip_kind(case);
    }
}

#[test]
fn the_three_replay_shapes_match_the_documented_payloads() {
    // Backfill has three outcomes and one payload shape per outcome. The
    // constructors are the only way to build them, so what is checked is
    // that each produces the documented JSON.
    assert_eq!(
        serde_json::to_value(ReplayInfo::within_ring(142, 17)).unwrap(),
        json!({ "replayed_from": 142, "events_replayed": 17, "gap": false })
    );
    assert_eq!(
        serde_json::to_value(ReplayInfo::live_from_head()).unwrap(),
        json!({ "replayed_from": null, "events_replayed": 0, "gap": false })
    );
    // A cell in the default style carries the character and nothing else;
    // one that is drawn differently carries only what differs. Both shapes
    // are the same object, which is what lets `cells[row][col]` be read
    // without a second code path.
    let styled = ScreenCell {
        ch: 'b',
        width: 1,
        style: CellStyle {
            foreground: Some(CellColor::Indexed(4)),
            intensity: CellIntensity::Bold,
            underline: true,
            ..CellStyle::default()
        },
    };
    let snapshot = ScreenSnapshot {
        cols: 80,
        rows: 24,
        cursor: CursorPosition { row: 3, col: 12 },
        cells: vec![vec![ScreenCell::plain('a'), styled], Vec::new()],
    };
    assert_eq!(
        serde_json::to_value(ReplayInfo::gap(9_120, Some(snapshot))).unwrap(),
        json!({ "replayed_from": null, "events_replayed": 0, "gap": true,
                "earliest_seq": 9_120,
                "screen_snapshot": { "cols": 80, "rows": 24,
                                     "cursor": { "row": 3, "col": 12 },
                                     "cells": [[{ "ch": "a" },
                                                { "ch": "b",
                                                  "style": {
                                                      "foreground": { "indexed": 4 },
                                                      "intensity": "bold",
                                                      "underline": true } }],
                                               []] } })
    );
    // A gap with no snapshot omits the field rather than spelling absence a
    // second way, matching how the envelope treats an unknown monotonic_ns.
    assert_eq!(
        serde_json::to_value(ReplayInfo::gap(9_120, None)).unwrap(),
        json!({ "replayed_from": null, "events_replayed": 0, "gap": true,
                "earliest_seq": 9_120 })
    );
}

#[test]
fn every_event_type_roundtrips_byte_stably() {
    // The sweep: serialize each event, read it back, serialize again, and
    // require the two documents to be identical. Anything that survives a
    // round trip only by losing a field fails here.
    for (seq, kind) in every_event_kind().into_iter().enumerate() {
        let event = envelope(seq as u64, kind);
        let first = serde_json::to_string(&event).expect("serialization is infallible");
        let parsed: Event = serde_json::from_str(&first).unwrap_or_else(|err| {
            panic!("{}: does not deserialize: {err}", event.kind.event_type())
        });
        let second = serde_json::to_string(&parsed).expect("serialization is infallible");
        assert_eq!(
            first,
            second,
            "{}: not byte-stable",
            event.kind.event_type()
        );
        assert_eq!(
            parsed,
            event,
            "{}: not value-stable",
            event.kind.event_type()
        );
    }
}
