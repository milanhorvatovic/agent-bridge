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

    for cli_dir in list_dirs_rejecting_files(&root, &mut errors) {
        for entry in list_dirs_rejecting_files(&cli_dir, &mut errors) {
            // A conformance scenario is a leaf directory of files; a
            // version directory of captured fixtures holds subdirectories.
            // `scenario.json` is the discriminating file: a conformance
            // scenario cannot exist without one, and a capture directory
            // never carries one.
            if is_real_file(&entry.join("scenario.json")) {
                scenarios += 1;
                for required in SCENARIO_FILES {
                    if !is_real_file(&entry.join(required)) {
                        errors.push(format!(
                            "{}: {required} missing, or not a real regular file (symlinks are rejected)",
                            entry.display()
                        ));
                    }
                }
                let trace = entry.join("expected.ndjson");
                if is_real_file(&trace) {
                    validate_trace(&trace, &mut errors);
                }
                continue;
            }
            let captured = list_dirs_rejecting_files(&entry, &mut errors);
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
                    if !is_real_file(&path) {
                        errors.push(format!(
                            "{}: {required} missing, or not a real regular file (symlinks are rejected)",
                            fixture.display()
                        ));
                    } else if std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.len() == 0) {
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

/// A real regular file. `is_file` follows symlinks, and this gate rejects
/// symlinks wherever they point — at the leaf files as much as at the
/// container levels, or a linked `scenario.json` could smuggle outside
/// content into a tree the gate claims to own.
fn is_real_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

/// The real subdirectories of a corpus container level, with every other
/// entry reported as an error. The container levels — the root of CLIs,
/// each CLI's scenarios/versions, each version's fixtures — hold
/// directories only; a stray file there (OS litter, an editor backup) is an
/// entry no check owns, which is exactly what this gate exists to prevent.
/// Classification is by `symlink_metadata`, so a symlink is a stray even
/// when it points at a directory: a link can smuggle content from outside
/// the corpus into a tree this gate claims to own.
fn list_dirs_rejecting_files(root: &Path, errors: &mut Vec<String>) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(root)
        .unwrap_or_else(|err| panic!("{}: cannot list: {err}", root.display()));
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut strays: Vec<String> = Vec::new();
    for entry in entries {
        let path = entry.expect("directory listing must succeed").path();
        let meta = std::fs::symlink_metadata(&path)
            .unwrap_or_else(|err| panic!("{}: cannot inspect: {err}", path.display()));
        if meta.is_dir() {
            dirs.push(path);
        } else {
            strays.push(format!(
                "{}: not a real directory — corpus container levels hold directories only, \
                 and symlinks are rejected wherever they point",
                path.display()
            ));
        }
    }
    // Deterministic ordering regardless of filesystem enumeration order.
    dirs.sort();
    strays.sort();
    errors.append(&mut strays);
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
