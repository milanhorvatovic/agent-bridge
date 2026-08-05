//! The cross-artifact consistency check: every committed golden trace must
//! validate against the *committed* trace-record schema.
//!
//! Two artifacts claim to describe the same thing — the golden traces under
//! `tests/corpus/` and `schema/trace-record.schema.json` — and nothing else
//! forces them to agree. This test does: each trace line is validated
//! against the committed schema file (not an in-memory regeneration, so a
//! stale or hand-edited artifact fails here too) and parsed through the
//! typed [`TraceRecord`], so the schema, the types, and the corpus stay one
//! contract.

use std::path::{Path, PathBuf};

use agent_bridge_events::TraceRecord;
use serde_json::Value;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn committed_schema() -> jsonschema::Validator {
    let path = repo_root().join("schema/trace-record.schema.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!(
            "{}: cannot read the committed schema ({err}) — generate it with \
             `cargo run -p agent-bridge-events --bin schema-gen`",
            path.display()
        )
    });
    let schema: Value = serde_json::from_str(&text).expect("the committed schema must parse");
    jsonschema::validator_for(&schema).expect("the committed schema must compile")
}

/// Every `expected.ndjson` under the corpus, recursively.
fn golden_traces(dir: &Path, into: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|err| panic!("{}: cannot list: {err}", dir.display()));
    for entry in entries {
        let path = entry.expect("directory listing must succeed").path();
        if path.is_dir() {
            golden_traces(&path, into);
        } else if path
            .file_name()
            .is_some_and(|name| name == "expected.ndjson")
        {
            into.push(path);
        }
    }
}

#[test]
fn golden_traces_validate_against_record_schema() {
    let validator = committed_schema();
    let mut traces = Vec::new();
    golden_traces(&repo_root().join("tests/corpus"), &mut traces);
    traces.sort();
    assert!(
        traces.len() >= 3,
        "expected at least the three starter traces, found {}",
        traces.len()
    );

    let mut errors: Vec<String> = Vec::new();
    for trace in &traces {
        let text = std::fs::read_to_string(trace)
            .unwrap_or_else(|err| panic!("{}: cannot read: {err}", trace.display()));
        for (line_number, line) in text.lines().enumerate() {
            let where_ = format!("{}:{}", trace.display(), line_number + 1);
            let record: Value = match serde_json::from_str(line) {
                Ok(record) => record,
                Err(err) => {
                    errors.push(format!("{where_}: invalid JSON: {err}"));
                    continue;
                }
            };
            if let Err(err) = validator.validate(&record) {
                errors.push(format!("{where_}: schema violation: {err}"));
            }
            if let Err(err) = serde_json::from_str::<TraceRecord>(line) {
                errors.push(format!("{where_}: does not parse as a TraceRecord: {err}"));
            }
        }
    }
    assert!(
        errors.is_empty(),
        "golden traces disagree with the committed trace-record schema:\n{}",
        errors.join("\n")
    );
}

#[test]
fn the_documented_example_trace_validates() {
    // The example in docs/trace-format.md, verbatim — kept honest here so
    // the published format document cannot show records its own schema
    // rejects.
    const EXAMPLE: &str = concat!(
        r#"{"seq":1,"monotonic_ns":1200,"event_type":"lifecycle.session.created","payload":{"adapter":"fake"},"schema_version":"1"}"#,
        "\n",
        r#"{"seq":2,"monotonic_ns":2400,"event_type":"lifecycle.session.running","payload":{},"approval_id":null,"schema_version":"1"}"#,
        "\n",
        r#"{"seq":3,"monotonic_ns":5100,"event_type":"stream.token","payload":{"content":"Reading file..."},"correlation_id":"send-1","schema_version":"1"}"#,
        "\n",
        r#"{"seq":4,"monotonic_ns":8000,"event_type":"prompt.approval_required","payload":{"prompt":"Allow filesystem write?","options":["y","n"]},"approval_id":"ap-c4d5","schema_version":"1"}"#,
        "\n",
        r#"{"seq":5,"monotonic_ns":9900,"event_type":"lifecycle.session.closed","payload":{"exit_code":0},"schema_version":"1"}"#,
        "\n",
    );
    let validator = committed_schema();
    for (line_number, line) in EXAMPLE.lines().enumerate() {
        let record: Value = serde_json::from_str(line).expect("example lines are JSON");
        assert!(
            validator.validate(&record).is_ok(),
            "docs/trace-format.md example line {} does not validate",
            line_number + 1
        );
    }
}

#[test]
fn the_schema_rejects_malformed_records() {
    // A validator that accepts everything would make the corpus check above
    // meaningless; pin each required-field and shape rule to a rejection.
    let validator = committed_schema();
    for (label, record) in [
        (
            "missing seq",
            r#"{"monotonic_ns":1,"event_type":"stream.token","payload":{"content":"x"}}"#,
        ),
        (
            "missing monotonic_ns",
            r#"{"seq":1,"event_type":"stream.token","payload":{"content":"x"}}"#,
        ),
        (
            "undotted event_type",
            r#"{"seq":1,"monotonic_ns":1,"event_type":"token","payload":{}}"#,
        ),
        (
            "non-object payload",
            r#"{"seq":1,"monotonic_ns":1,"event_type":"stream.token","payload":"x"}"#,
        ),
        (
            "wrong trace-format version",
            r#"{"seq":1,"monotonic_ns":1,"event_type":"stream.token","payload":{},"schema_version":"2"}"#,
        ),
    ] {
        let record: Value = serde_json::from_str(record).expect("test records are JSON");
        assert!(
            validator.validate(&record).is_err(),
            "{label}: the schema must reject this record"
        );
    }
}
