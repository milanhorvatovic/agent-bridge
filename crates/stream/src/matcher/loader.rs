//! Pattern-pack loading: `patterns/<cli>/<version>/*.yaml` into validated
//! records, or an error naming the record that stopped it.
//!
//! A pack is authored by hand and reviewed as text, so every rejection here
//! is written for the author reading it: which file, which record, what is
//! wrong. Parsing is two-pass for exactly that reason — the file is first
//! read as plain YAML values so each record's `name` is in hand before the
//! typed deserialization that might reject it, and a failure half-way
//! through a file still says *which* record rather than which byte offset.
//!
//! Validation here is everything that can be decided without compiling an
//! expression: the record shape, the closed template vocabulary, the emit
//! table, screen records (a locked shape this loader does not yet accept),
//! duplicate names, and templates that read captures a substring matcher
//! could never produce. Regex compilation — and the rejection event a
//! failure there becomes — happens at registration, in the engine.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agent_bridge_adapter_api::{PatternRecord, TextMatcherType};

use super::template::{groups_read, validate_emit_spec};

/// Why a pack did not load. Registration rejects the adapter's patterns as
/// a set: one bad record fails the pack, because a pack that half-loads
/// would detect approvals with whichever half survived.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The pack directory could not be read.
    #[error("pattern pack {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The directory exists but holds no pack files — almost always a wrong
    /// path, and silently loading zero patterns would look like a working
    /// adapter that recognizes nothing.
    #[error("pattern pack {path}: no .yaml or .yml files")]
    EmptyDir { path: PathBuf },
    /// The file is not parseable YAML at all.
    #[error("pattern pack {label}: {message}")]
    Syntax { label: String, message: String },
    /// The file parses, but not as a list of records.
    #[error("pattern pack {label}: expected a YAML list of pattern records")]
    NotAList { label: String },
    /// One record is malformed; the message says how.
    #[error("pattern pack {label}, record `{record}`: {message}")]
    Record {
        label: String,
        record: String,
        message: String,
    },
    /// A `type: screen` record. The screen record shape is locked but its
    /// loading lands with the first pack that carries one; until then screen
    /// matchers register through the code path.
    #[error(
        "pattern pack {label}, record `{record}`: `type: screen` records are not loadable yet — \
         screen matchers register through the code path"
    )]
    ScreenRecord { label: String, record: String },
    /// Two records share a name. An id that names two matchers can name
    /// neither in an event or a disable decision.
    #[error("pattern pack {label}: record `{record}` is declared twice")]
    DuplicateName { label: String, record: String },
}

/// Loads every pack file in one version directory, in file-name order.
///
/// The order is part of the contract: record order breaks priority ties, so
/// it must not depend on the platform's directory iteration. Files sort by
/// name, records keep their in-file order, and the concatenation is the
/// pack.
pub fn load_dir(dir: &Path) -> Result<Vec<PatternRecord>, LoadError> {
    let entries = std::fs::read_dir(dir).map_err(|source| LoadError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| LoadError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "yaml" || extension == "yml")
        {
            files.push(path);
        }
    }
    if files.is_empty() {
        return Err(LoadError::EmptyDir {
            path: dir.to_path_buf(),
        });
    }
    files.sort();

    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    for path in files {
        let label = path.display().to_string();
        let text = std::fs::read_to_string(&path).map_err(|source| LoadError::Io {
            path: path.clone(),
            source,
        })?;
        parse_into(&label, &text, &mut records, &mut seen)?;
    }
    Ok(records)
}

/// Parses one pack file's text. `label` is what errors call the file — a
/// path for a file on disk, any name the caller likes for embedded text.
pub fn parse_pack(label: &str, text: &str) -> Result<Vec<PatternRecord>, LoadError> {
    let mut records = Vec::new();
    let mut seen = BTreeSet::new();
    parse_into(label, text, &mut records, &mut seen)?;
    Ok(records)
}

fn parse_into(
    label: &str,
    text: &str,
    records: &mut Vec<PatternRecord>,
    seen: &mut BTreeSet<String>,
) -> Result<(), LoadError> {
    let raw: serde_norway::Value =
        serde_norway::from_str(text).map_err(|error| LoadError::Syntax {
            label: label.to_string(),
            message: error.to_string(),
        })?;
    let serde_norway::Value::Sequence(items) = raw else {
        return Err(LoadError::NotAList {
            label: label.to_string(),
        });
    };
    for (index, item) in items.into_iter().enumerate() {
        // The record's name, before the typed pass that may reject it — an
        // error about "record 3" helps nobody editing a fifty-line file.
        let name = item
            .get("name")
            .and_then(serde_norway::Value::as_str)
            .map_or_else(|| format!("#{}", index + 1), str::to_string);
        if item
            .get("matcher")
            .and_then(|matcher| matcher.get("type"))
            .and_then(serde_norway::Value::as_str)
            == Some("screen")
        {
            return Err(LoadError::ScreenRecord {
                label: label.to_string(),
                record: name,
            });
        }
        let record: PatternRecord =
            serde_norway::from_value(item).map_err(|error| LoadError::Record {
                label: label.to_string(),
                record: name.clone(),
                message: error.to_string(),
            })?;
        validate(label, &record)?;
        if !seen.insert(record.name.clone()) {
            return Err(LoadError::DuplicateName {
                label: label.to_string(),
                record: record.name,
            });
        }
        records.push(record);
    }
    Ok(())
}

/// The structural checks deserialization cannot express.
fn validate(label: &str, record: &PatternRecord) -> Result<(), LoadError> {
    let reject = |message: String| LoadError::Record {
        label: label.to_string(),
        record: record.name.clone(),
        message,
    };
    if record.name.trim().is_empty() {
        return Err(LoadError::Record {
            label: label.to_string(),
            record: "#unnamed".to_string(),
            message: "a record needs a non-empty name".to_string(),
        });
    }
    if record.matcher.source.is_empty() {
        return Err(reject("`matcher.source` is empty".to_string()));
    }
    validate_emit_spec(&record.emits).map_err(reject)?;
    // A substring matcher captures nothing, so a template reading
    // `matches.<group>` from one could only ever render an empty field.
    if record.matcher.kind == TextMatcherType::Substring
        && let Some(group) = groups_read(&record.emits).next()
    {
        return Err(reject(format!(
            "a `substring` matcher has no capture groups for `matches.{group}` to read; \
             use a `regex` matcher"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bridge_adapter_api::Anchor;

    const APPROVAL: &str = r#"
- name: approval_write
  matcher:
    type: regex
    source: '^(?P<prompt>Allow .+\?) \[y/N\]$'
    anchor: line_start
  emits:
    event_type: prompt.approval_required
    fields:
      approval_id: '{{ uuid4() }}'
      prompt: '{{ matches.prompt }}'
      options: ['y', 'n']
"#;

    #[test]
    fn a_well_formed_pack_parses_with_order_preserved() {
        let two = format!(
            "{APPROVAL}- name: second\n  matcher: {{ type: substring, source: 'ready' }}\n  \
             emits:\n    event_type: tool.call_started\n    fields:\n      call_id: \
             '{{{{ uuid4() }}}}'\n      tool: probe\n"
        );
        let records = parse_pack("inline", &two).expect("two records");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].name, "approval_write");
        assert_eq!(records[0].matcher.anchor, Some(Anchor::LineStart));
        assert_eq!(records[1].name, "second");
    }

    #[test]
    fn a_screen_record_is_rejected_naming_the_record() {
        let yaml = r#"
- name: permission_dialog
  matcher:
    type: screen
    anchor:
      kind: literal
      needle: 'Do you want to proceed?'
  emits:
    event_type: prompt.approval_required
"#;
        let error = parse_pack("inline", yaml).expect_err("screen records are code-path only");
        let message = error.to_string();
        assert!(message.contains("permission_dialog"), "got: {message}");
        assert!(message.contains("code path"), "got: {message}");
    }

    #[test]
    fn a_malformed_record_error_names_the_record_not_the_offset() {
        let yaml = r#"
- name: fine
  matcher: { type: substring, source: 'ok' }
  emits:
    event_type: tool.call_started
    fields:
      call_id: '{{ uuid4() }}'
      tool: probe
- name: broken
  matcher: { type: regex }
  emits:
    event_type: tool.call_started
"#;
        let message = parse_pack("inline", yaml)
            .expect_err("missing source")
            .to_string();
        assert!(message.contains("`broken`"), "got: {message}");
    }

    #[test]
    fn duplicate_names_are_rejected_across_a_load() {
        let yaml = format!("{APPROVAL}{APPROVAL}");
        let message = parse_pack("inline", &yaml)
            .expect_err("duplicate name")
            .to_string();
        assert!(message.contains("approval_write"), "got: {message}");
        assert!(message.contains("twice"), "got: {message}");
    }

    #[test]
    fn a_substring_record_reading_captures_is_rejected() {
        let yaml = r#"
- name: hopeful
  matcher: { type: substring, source: '[y/N]' }
  emits:
    event_type: prompt.approval_required
    fields:
      approval_id: '{{ uuid4() }}'
      prompt: '{{ matches.prompt }}'
"#;
        let message = parse_pack("inline", yaml)
            .expect_err("substring has no groups")
            .to_string();
        assert!(message.contains("hopeful"), "got: {message}");
        assert!(message.contains("capture groups"), "got: {message}");
    }

    #[test]
    fn an_unparseable_file_and_a_non_list_file_fail_as_files() {
        assert!(matches!(
            parse_pack("inline", ": not yaml : ["),
            Err(LoadError::Syntax { .. })
        ));
        assert!(matches!(
            parse_pack("inline", "name: not-a-list"),
            Err(LoadError::NotAList { .. })
        ));
    }

    #[test]
    fn load_dir_reads_files_in_name_order_and_rejects_empty_dirs() {
        let dir =
            std::env::temp_dir().join(format!("agent-bridge-loader-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create test dir");
        // Written in reverse of the order they must load in.
        std::fs::write(
            dir.join("20-tool.yaml"),
            "- name: tool\n  matcher: { type: substring, source: 'marker' }\n  emits:\n    \
             event_type: tool.call_started\n    fields:\n      call_id: '{{ uuid4() }}'\n      \
             tool: probe\n",
        )
        .expect("write");
        std::fs::write(dir.join("10-approval.yaml"), APPROVAL).expect("write");
        std::fs::write(dir.join("README.md"), "not a pack file").expect("write");

        let records = load_dir(&dir).expect("two files");
        assert_eq!(
            records
                .iter()
                .map(|record| record.name.as_str())
                .collect::<Vec<_>>(),
            vec!["approval_write", "tool"],
        );

        let empty = dir.join("empty");
        std::fs::create_dir_all(&empty).expect("create empty dir");
        assert!(matches!(load_dir(&empty), Err(LoadError::EmptyDir { .. })));
        assert!(matches!(
            load_dir(&dir.join("missing")),
            Err(LoadError::Io { .. })
        ));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
