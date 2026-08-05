//! Serde round-trips of the documented envelope examples.
//!
//! The JSON shapes asserted here are the documented contract: the envelope
//! with its `type` discriminant and `payload` object, explicit nulls on the
//! nullable correlation fields, and `monotonic_ns` absent when unknown. A
//! round-trip failure means the types no longer produce the documented
//! wire shape — which is a contract change, never a refactor.

use agent_bridge_events::{Event, EventKind};
use serde_json::{Value, json};

/// Parse, assert the typed view, serialize, and require value equality
/// with the input — so both directions of the mapping are pinned.
fn roundtrip(input: Value) -> Event {
    let event: Event =
        serde_json::from_value(input.clone()).expect("the documented example must deserialize");
    let back = serde_json::to_value(&event).expect("serialization is infallible");
    assert_eq!(back, input, "serializing back must reproduce the document");
    event
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
    assert_eq!(event.schema_version, 1);
    assert_eq!(event.seq, 42);
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
fn lifecycle_events_roundtrip() {
    // The two lifecycle payloads with fields: created names its adapter,
    // closed carries the exit code. session_id null exercises the
    // required-but-nullable envelope contract.
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
        "payload": { "exit_code": 0 }
    }));
    let EventKind::LifecycleSessionClosed(payload) = &closed.kind else {
        panic!("expected lifecycle.session.closed, got {:?}", closed.kind);
    };
    assert_eq!(payload.exit_code, Some(0));
}

#[test]
fn unknown_payload_fields_are_tolerated() {
    // The compatibility contract: new optional payload fields arrive
    // without a schema_version bump, so a consumer on the current types
    // must read a future producer's events without error.
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
fn unknown_event_types_are_tolerated() {
    // The other half of the same contract: new event *types* arrive within
    // schema_version 1, so an envelope whose type this revision does not
    // enumerate must deserialize — to the Unknown fallback, with the type
    // name and payload carried through — and serialize back unchanged.
    let event = roundtrip(json!({
        "schema_version": 1,
        "type": "tool.call_started",
        "session_id": "0b8ee0e4-9f4f-4e6b-8f0a-3a80cf9c17d1",
        "seq": 3,
        "ts": "2026-05-16T08:00:02.000Z",
        "approval_id": null,
        "correlation_id": null,
        "payload": { "call_id": "t-9c2", "tool": "bash" }
    }));
    let EventKind::Unknown(unknown) = &event.kind else {
        panic!("expected the Unknown fallback, got {:?}", event.kind);
    };
    assert_eq!(unknown.event_type, "tool.call_started");
    assert_eq!(
        unknown.payload.get("call_id"),
        Some(&serde_json::Value::String("t-9c2".into()))
    );
}

#[test]
fn typed_events_validate_against_the_committed_envelope_schema() {
    // The other half of the contract loop: what the types serialize must be
    // what the committed artifact accepts — through the committed file, not
    // an in-memory regeneration, so a stale artifact fails here too.
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../schema/events.schema.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "{}: cannot read the committed schema ({err}) — generate it with \
             `cargo run -p agent-bridge-events --bin schema-gen`",
            path.display()
        )
    });
    let schema: Value = serde_json::from_str(&text).expect("the committed schema must parse");
    let validator = jsonschema::validator_for(&schema).expect("the committed schema must compile");

    // One envelope per starter event type, serialized from the types.
    for (seq, kind) in [
        EventKind::LifecycleSessionCreated(agent_bridge_events::LifecycleSessionCreated {
            adapter: Some("fake".into()),
        }),
        EventKind::LifecycleSessionLaunching(Default::default()),
        EventKind::LifecycleSessionConnecting(Default::default()),
        EventKind::LifecycleSessionRunning(Default::default()),
        EventKind::LifecycleSessionClosing(Default::default()),
        EventKind::LifecycleSessionClosed(agent_bridge_events::LifecycleSessionClosed {
            exit_code: Some(0),
        }),
        EventKind::StreamToken(agent_bridge_events::StreamToken {
            source: Some("fake".into()),
            content: "Hello world.".into(),
        }),
        EventKind::StreamUnrecognizedOutput(agent_bridge_events::StreamUnrecognizedOutput {
            content: "unfamiliar prompt format".into(),
        }),
        EventKind::PromptApprovalRequired(agent_bridge_events::PromptApprovalRequired {
            prompt: "Allow filesystem write?".into(),
            options: Some(vec!["y".into(), "n".into()]),
        }),
        EventKind::Unknown(agent_bridge_events::UnknownEvent {
            event_type: "lifecycle.turn.completed".into(),
            payload: serde_json::Map::new(),
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let seq = seq as u64;
        let approval_id =
            matches!(kind, EventKind::PromptApprovalRequired(_)).then(|| "a-7f3".to_owned());
        let event = Event {
            schema_version: 1,
            session_id: Some("0b8ee0e4-9f4f-4e6b-8f0a-3a80cf9c17d1".into()),
            seq,
            monotonic_ns: Some(1_000 * (seq + 1)),
            ts: "2026-05-16T08:00:00.123Z".into(),
            approval_id,
            correlation_id: None,
            kind,
        };
        let value = serde_json::to_value(&event).expect("serialization is infallible");
        assert!(
            validator.validate(&value).is_ok(),
            "the committed schema rejects a typed event: {value}"
        );
    }

    // And the artifact must still *reject* — a schema that accepts anything
    // would make the assertions above meaningless. Note what is absent
    // here: an unknown dotted event type is NOT a violation (the additive-
    // growth rule, asserted from the typed side above) — but a *published*
    // type over a payload that misses its required fields is.
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
fn schema_generation_is_deterministic() {
    // The freshness gate byte-compares regenerated output against the
    // committed artifact, which is only sound if generation is a pure
    // function. Generate twice and require byte equality, and require the
    // canonical-form invariants the committed files rely on: LF-only, one
    // trailing newline, valid JSON.
    for (name, first, second) in [
        (
            "events.schema.json",
            agent_bridge_events::canonical_json(&agent_bridge_events::event_schema()),
            agent_bridge_events::canonical_json(&agent_bridge_events::event_schema()),
        ),
        (
            "trace-record.schema.json",
            agent_bridge_events::canonical_json(&agent_bridge_events::trace_record_schema()),
            agent_bridge_events::canonical_json(&agent_bridge_events::trace_record_schema()),
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
        agent_bridge_events::canonical_json(&value),
        "{\n  \"a\": {},\n  \"b\": [\n    1,\n    {\n      \"a\": \"x\",\n      \"z\": null\n    }\n  ],\n  \"c\": \"τ\"\n}\n"
    );
}
