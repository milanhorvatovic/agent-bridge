//! Structural validation of every golden trace in the corpus.
//!
//! The traces are forward contracts: the comparator that enforces them
//! semantically arrives with the harness runner, so until then an authoring
//! error — invalid JSON, a missing required field, a seq gap, a CRLF — would
//! sit unnoticed in the repo and surface as a confusing comparator failure
//! much later. This check moves that discovery to the commit that introduces
//! the error.
//!
//! Checked per trace record: one JSON object per line; required fields with
//! required types (`seq` integer, `monotonic_ns` integer, `event_type`
//! dotted string, `payload` object, `schema_version` "1"); optionally-typed
//! optional fields; `seq` starting at 1 and gap-free (each trace captures a
//! single session). Checked per file: UTF-8, at least one record, LF-only,
//! trailing newline.
//!
//! The corpus holds a second fixture kind alongside the conformance
//! scenarios: **captured-session fixtures**, recorded from a live CLI by
//! the interactive probe's `record` lane and laid out one directory level
//! deeper — `<cli>/<version>/<scenario>-<cols>x<rows>/`, because a capture
//! is pinned to the CLI version that produced it. They carry recorded
//! inputs (`input.bytes` + sidecars), not golden traces, so they get their
//! own structural check: the required artifact set is present and non-empty.

use std::path::{Path, PathBuf};

use serde_json::Value;

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus")
}

/// Every file a committed conformance-scenario directory must carry.
const SCENARIO_FILES: [&str; 3] = ["scenario.json", "expected.ndjson", "manifest.yaml"];

/// Every file a captured-session fixture directory must carry. Hook and
/// transcript artifacts are per-CLI extras on top; the byte stream, its
/// timing, the driver step log, and the manifest are the invariant core.
const CAPTURED_FILES: [&str; 4] = [
    "input.bytes",
    "input.timing.ndjson",
    "steps.ndjson",
    "manifest.yaml",
];

#[test]
fn trace_structural_validation_all() {
    let root = corpus_root();
    let mut errors: Vec<String> = Vec::new();
    let mut scenarios = 0;

    for cli_dir in list_dirs(&root) {
        for entry in list_dirs(&cli_dir) {
            // A conformance scenario is a leaf directory of files; a
            // version directory of captured fixtures holds subdirectories.
            // `scenario.json` is the discriminating file: a conformance
            // scenario cannot exist without one, and a capture directory
            // never carries one.
            if entry.join("scenario.json").is_file() {
                scenarios += 1;
                for required in SCENARIO_FILES {
                    if !entry.join(required).is_file() {
                        errors.push(format!("{}: missing {required}", entry.display()));
                    }
                }
                let trace = entry.join("expected.ndjson");
                if trace.is_file() {
                    validate_trace(&trace, &mut errors);
                }
                continue;
            }
            let captured = list_dirs(&entry);
            if captured.is_empty() {
                errors.push(format!(
                    "{}: neither a conformance scenario (no scenario.json) nor a version \
                     directory of captured fixtures (no subdirectories)",
                    entry.display()
                ));
                continue;
            }
            for fixture in captured {
                scenarios += 1;
                for required in CAPTURED_FILES {
                    let path = fixture.join(required);
                    if !path.is_file() {
                        errors.push(format!("{}: missing {required}", fixture.display()));
                    } else if std::fs::metadata(&path).is_ok_and(|meta| meta.len() == 0) {
                        errors.push(format!("{}: {required} is empty", fixture.display()));
                    }
                }
            }
        }
    }

    assert!(
        scenarios > 0,
        "no scenario directories under {}",
        root.display()
    );
    assert!(
        errors.is_empty(),
        "corpus structural validation failed:\n{}",
        errors.join("\n")
    );
}

fn list_dirs(root: &Path) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(root)
        .unwrap_or_else(|err| panic!("{}: cannot list: {err}", root.display()));
    let mut dirs: Vec<PathBuf> = entries
        .map(|entry| entry.expect("directory listing must succeed").path())
        .filter(|path| path.is_dir())
        .collect();
    // Deterministic error ordering regardless of filesystem enumeration order.
    dirs.sort();
    dirs
}

fn validate_trace(path: &Path, errors: &mut Vec<String>) {
    let mut fail = |message: String| errors.push(format!("{}: {message}", path.display()));

    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => return fail(format!("cannot read: {err}")),
    };
    if bytes.is_empty() {
        return fail("empty — a golden trace asserts at least one event".into());
    }
    if bytes.contains(&b'\r') {
        fail("carries CR bytes — the trace format is LF-only".into());
    }
    if bytes.last() != Some(&b'\n') {
        fail("missing the required trailing newline".into());
    }
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(err) => return fail(format!("not UTF-8: {err}")),
    };

    let mut expected_seq: u64 = 1;
    for (line_number, line) in text.lines().enumerate() {
        let where_ = format!("line {}", line_number + 1);
        let record: Value = match serde_json::from_str(line) {
            Ok(record) => record,
            Err(err) => {
                fail(format!("{where_}: invalid JSON: {err}"));
                continue;
            }
        };
        let Value::Object(record) = record else {
            fail(format!("{where_}: a trace record must be a JSON object"));
            continue;
        };

        match record.get("seq").and_then(Value::as_u64) {
            Some(seq) if seq == expected_seq => expected_seq += 1,
            Some(seq) => {
                fail(format!(
                    "{where_}: seq {seq} — must be {expected_seq}: seq starts at 1 and is gap-free within a session"
                ));
                expected_seq = seq + 1;
            }
            None => fail(format!("{where_}: missing or non-integer \"seq\"")),
        }
        if record.get("monotonic_ns").and_then(Value::as_u64).is_none() {
            fail(format!("{where_}: missing or non-integer \"monotonic_ns\""));
        }
        match record.get("event_type").and_then(Value::as_str) {
            Some(event_type) if event_type.contains('.') => {}
            Some(event_type) => fail(format!(
                "{where_}: event_type \"{event_type}\" is not a dotted hierarchical name"
            )),
            None => fail(format!("{where_}: missing or non-string \"event_type\"")),
        }
        if !record.get("payload").is_some_and(Value::is_object) {
            fail(format!("{where_}: missing or non-object \"payload\""));
        }
        match record.get("schema_version").and_then(Value::as_str) {
            Some("1") => {}
            Some(other) => fail(format!(
                "{where_}: schema_version \"{other}\" — this corpus is authored at \"1\""
            )),
            None => fail(format!(
                "{where_}: missing or non-string \"schema_version\""
            )),
        }
        // Optional envelope fields: unknown extras are legal (consumers must
        // ignore what they do not know), but a known field with the wrong
        // type is an authoring error.
        for optional in ["approval_id", "correlation_id", "session_id"] {
            if let Some(value) = record.get(optional)
                && !(value.is_string() || value.is_null())
            {
                fail(format!("{where_}: \"{optional}\" must be a string or null"));
            }
        }
    }
}
