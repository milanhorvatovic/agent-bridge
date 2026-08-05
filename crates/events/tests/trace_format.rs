//! The NDJSON trace format: line discipline, and the mapping between an
//! emitted event and its stored record.
//!
//! Golden traces are compared byte for byte across three operating systems,
//! so the line rules are not stylistic: one CRLF, one missing trailing
//! newline, and a comparison that should have passed reports a difference
//! that is not there.

use std::io::Cursor;

use agent_bridge_events::*;
use serde_json::json;

fn record(seq: u64, event_type: &str) -> TraceRecord {
    TraceRecord {
        seq,
        monotonic_ns: seq * 1_000,
        event_type: event_type.to_owned(),
        payload: serde_json::Map::new(),
        session_id: None,
        approval_id: None,
        correlation_id: None,
        schema_version: Some(TRACE_SCHEMA_VERSION.to_owned()),
    }
}

fn written(records: &[TraceRecord]) -> String {
    let mut out = Vec::new();
    write_records(&mut out, records).expect("writing to memory cannot fail");
    String::from_utf8(out).expect("records are UTF-8")
}

#[test]
fn writing_produces_lf_lines_and_a_trailing_newline() {
    let out = written(&[
        record(1, "lifecycle.session.created"),
        record(2, "lifecycle.session.running"),
    ]);
    assert!(!out.contains('\r'), "the trace format is LF-only: {out:?}");
    assert!(out.ends_with('\n'), "a trace ends with a newline");
    assert_eq!(out.lines().count(), 2);
    // One record per line, compact: a trace is diffed and grepped by line,
    // which pretty-printing would take away.
    assert!(out.lines().all(|line| line.starts_with('{')));
}

#[test]
fn reading_returns_what_writing_wrote() {
    let records = vec![
        record(1, "lifecycle.session.created"),
        record(2, "stream.token"),
    ];
    let read: Vec<TraceRecord> = read_records(Cursor::new(written(&records)))
        .collect::<Result<_, _>>()
        .expect("what the writer produced must read back");
    assert_eq!(read, records);
}

#[test]
fn a_crlf_line_is_rejected_where_it_is() {
    let text = "{\"seq\":1,\"monotonic_ns\":1,\"event_type\":\"stream.token\",\"payload\":{}}\r\n";
    let errors: Vec<TraceError> = read_records(Cursor::new(text))
        .filter_map(Result::err)
        .collect();
    assert!(
        matches!(errors.as_slice(), [TraceError::CarriageReturn { line: 1 }]),
        "expected a CRLF report for line 1, got {errors:?}"
    );
}

#[test]
fn an_unterminated_final_line_is_rejected() {
    // The trailing newline is what tells a reader the file was not cut off
    // mid-write, which is the one corruption a reader can still see.
    let text = "{\"seq\":1,\"monotonic_ns\":1,\"event_type\":\"stream.token\",\"payload\":{}}";
    let errors: Vec<TraceError> = read_records(Cursor::new(text))
        .filter_map(Result::err)
        .collect();
    assert!(
        matches!(
            errors.as_slice(),
            [TraceError::MissingTrailingNewline { line: 1 }]
        ),
        "expected a truncation report for line 1, got {errors:?}"
    );
}

#[test]
fn a_malformed_line_does_not_hide_the_rest() {
    // A corpus file with one bad record should report that record, not stop
    // at it: a comparator that gives up on the first problem makes fixing a
    // trace a game of one round trip per line.
    let text = concat!(
        "{\"seq\":1,\"monotonic_ns\":1,\"event_type\":\"stream.token\",\"payload\":{}}\n",
        "\n",
        "{\"seq\":2,\"monotonic_ns\":2}\n",
        "{\"seq\":3,\"monotonic_ns\":3,\"event_type\":\"stream.token\",\"payload\":{}}\n",
    );
    let outcomes: Vec<Result<TraceRecord, TraceError>> = read_records(Cursor::new(text)).collect();
    assert_eq!(outcomes.len(), 4);
    assert!(outcomes[0].is_ok());
    assert!(matches!(
        outcomes[1],
        Err(TraceError::BlankLine { line: 2 })
    ));
    assert!(matches!(
        outcomes[2],
        Err(TraceError::Record { line: 3, .. })
    ));
    assert_eq!(outcomes[3].as_ref().expect("line 4 is a record").seq, 3);
}

#[test]
fn unknown_top_level_fields_are_ignored() {
    // The record format's own forward-compatibility rule: producers may add
    // optional fields without bumping the format version.
    let text = concat!(
        r#"{"seq":1,"monotonic_ns":1,"event_type":"stream.token","payload":{"content":"hi"},"#,
        r#""captured_by":"a future writer"}"#,
        "\n",
    );
    let read: Vec<TraceRecord> = read_records(Cursor::new(text))
        .collect::<Result<_, _>>()
        .expect("an unknown top-level field must be ignored, not rejected");
    assert_eq!(read[0].event_type, "stream.token");
}

#[test]
fn an_event_becomes_a_record_under_the_stored_field_names() {
    // One type, two serializations: the wire names the discriminant `type`
    // and versions the event stream with an integer; a stored record names
    // it `event_type` and versions the file format with a string. Confusing
    // the two is what this mapping exists to prevent.
    let event = Event {
        schema_version: SCHEMA_VERSION,
        session_id: Some("0b8ee0e4".to_owned()),
        seq: 4,
        monotonic_ns: Some(8_000),
        ts: "2026-05-16T08:00:00.123Z".to_owned(),
        approval_id: Some("a-7f3".to_owned()),
        correlation_id: Some("send-1".to_owned()),
        kind: EventBody::approval_required(
            "a-7f3",
            ApprovalPrompt::new("Allow filesystem write?").options(["y", "n"]),
        )
        .kind,
    };
    let record = TraceRecord::from_event(&event).expect("the event carries a monotonic reading");
    assert_eq!(
        serde_json::to_value(&record).expect("serialization is infallible"),
        json!({
            "seq": 4,
            "monotonic_ns": 8_000,
            "event_type": "prompt.approval_required",
            "payload": { "prompt": "Allow filesystem write?", "options": ["y", "n"] },
            "session_id": "0b8ee0e4",
            "approval_id": "a-7f3",
            "correlation_id": "send-1",
            "schema_version": "1"
        })
    );
    // The wall-clock timestamp is deliberately not stored: it is not an
    // ordering key and it is not reproducible, so a comparison that included
    // it would fail on every replay.
    assert_eq!(TRACE_SCHEMA_VERSION, "1");
}

#[test]
fn an_event_without_a_monotonic_reading_is_not_a_record() {
    // The record format requires the reading that replay pacing is measured
    // from. Substituting a zero would put a lie in the corpus.
    let event = Event {
        schema_version: SCHEMA_VERSION,
        session_id: None,
        seq: 1,
        monotonic_ns: None,
        ts: "2026-05-16T08:00:00.000Z".to_owned(),
        approval_id: None,
        correlation_id: None,
        kind: EventKind::LifecycleSessionRunning(LifecycleSessionRunning {}),
    };
    assert!(TraceRecord::from_event(&event).is_none());
}

#[test]
fn a_record_resolves_back_to_its_event() {
    let mut known = record(1, "stream.token");
    known.payload = serde_json::Map::from_iter([("content".to_owned(), json!("hi"))]);
    let EventKind::StreamToken(payload) = known.to_kind() else {
        panic!("a published type resolves to its variant");
    };
    assert_eq!(payload.content, "hi");

    // And a record from a newer corpus resolves to the fallback rather than
    // failing, so a comparator reading it keeps working.
    let future = record(2, "lifecycle.session.hibernated");
    let EventKind::Unknown(unknown) = future.to_kind() else {
        panic!("an unpublished type resolves to the fallback");
    };
    assert_eq!(unknown.event_type, "lifecycle.session.hibernated");
}
