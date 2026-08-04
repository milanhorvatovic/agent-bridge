//! Captured-fixture discovery and ground-truth loading.
//!
//! The corpus layout is `tests/corpus/<cli>/<version>/<scenario>-<cols>x<rows>/`
//! with the artifact set the capture rig commits. Replay needs three of those
//! artifacts: `input.bytes` and `input.timing.ndjson` (the stream), and
//! `steps.ndjson` (the driver's labeled step log, which is the ground truth
//! false-negative accounting measures against). Everything else in a fixture
//! belongs to other pipeline configurations.
//!
//! Discovery is strict: a directory that looks like a fixture but is missing
//! a required artifact is an error, not a skip — a silently thinner corpus
//! would silently flatter the numbers.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Artifacts every replayable fixture must carry.
const REQUIRED_FILES: [&str; 3] = ["input.bytes", "input.timing.ndjson", "steps.ndjson"];

/// Identity of one captured fixture, parsed from its corpus path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FixtureId {
    pub cli: String,
    pub version: String,
    pub scenario: String,
    pub cols: u16,
    pub rows: u16,
}

impl fmt::Display for FixtureId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}/{}/{}-{}x{}",
            self.cli, self.version, self.scenario, self.cols, self.rows
        )
    }
}

/// One replayable fixture: its identity and its directory on disk.
#[derive(Clone, Debug)]
pub struct Fixture {
    pub id: FixtureId,
    pub dir: PathBuf,
}

/// One record of the driver's step log. Only the fields the ground-truth
/// mapping keys on are read; the rest of each record stays in the file.
#[derive(Debug, Default, Deserialize)]
pub struct StepRecord {
    pub step: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub hook: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub marker: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

/// Enumerate the replayable fixtures for the given CLIs, sorted by path so
/// every run and every OS sees the same order.
pub fn discover(corpus_root: &Path, clis: &[String]) -> Result<Vec<Fixture>, String> {
    let mut fixtures = Vec::new();
    for cli in clis {
        let cli_dir = corpus_root.join(cli);
        for version_dir in sorted_subdirs(&cli_dir)? {
            let scenario_dirs = sorted_subdirs(&version_dir)?;
            if scenario_dirs.is_empty() {
                return Err(format!(
                    "{}: version directory holds no fixtures",
                    version_dir.display()
                ));
            }
            for scenario_dir in scenario_dirs {
                fixtures.push(fixture_from_dir(cli, &version_dir, &scenario_dir)?);
            }
        }
    }
    Ok(fixtures)
}

fn sorted_subdirs(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let mut subdirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("{}: {err}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        }
    }
    subdirs.sort();
    Ok(subdirs)
}

fn fixture_from_dir(cli: &str, version_dir: &Path, dir: &Path) -> Result<Fixture, String> {
    let name = dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{}: non-UTF-8 fixture name", dir.display()))?;
    let version = version_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{}: non-UTF-8 version name", version_dir.display()))?;

    let (scenario, dims) = name.rsplit_once('-').ok_or_else(|| {
        format!(
            "{}: fixture name lacks a -<cols>x<rows> suffix",
            dir.display()
        )
    })?;
    let (cols, rows) = dims
        .split_once('x')
        .and_then(|(cols, rows)| Some((cols.parse().ok()?, rows.parse().ok()?)))
        .ok_or_else(|| format!("{}: cannot parse dimensions from {dims:?}", dir.display()))?;

    for required in REQUIRED_FILES {
        let path = dir.join(required);
        if !path.is_file() {
            return Err(format!("{}: missing {required}", dir.display()));
        }
    }

    Ok(Fixture {
        id: FixtureId {
            cli: cli.to_string(),
            version: version.to_string(),
            scenario: scenario.to_string(),
            cols,
            rows,
        },
        dir: dir.to_path_buf(),
    })
}

/// Load the driver step log of one fixture. Malformed lines are an error
/// naming the file and line, never a short read.
pub fn load_steps(dir: &Path) -> Result<Vec<StepRecord>, String> {
    let path = dir.join("steps.ndjson");
    let raw = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut steps = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let record: StepRecord = serde_json::from_str(line)
            .map_err(|err| format!("{}:{}: {err}", path.display(), index + 1))?;
        steps.push(record);
    }
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_names_parse_into_scenario_and_dimensions() {
        let dir = std::env::temp_dir().join(format!(
            "detection-spike-corpus-test-{}",
            std::process::id()
        ));
        let fixture_dir = dir.join("claude/2.1.201/approval-arrow-key-80x24");
        fs::create_dir_all(&fixture_dir).expect("create fixture dir");
        for required in REQUIRED_FILES {
            fs::write(fixture_dir.join(required), b"x").expect("write artifact");
        }

        let fixtures =
            discover(&dir, &["claude".to_string()]).expect("discovery over the temp corpus");
        assert_eq!(fixtures.len(), 1);
        let id = &fixtures[0].id;
        assert_eq!(id.cli, "claude");
        assert_eq!(id.version, "2.1.201");
        assert_eq!(id.scenario, "approval-arrow-key");
        assert_eq!((id.cols, id.rows), (80, 24));
        assert_eq!(id.to_string(), "claude/2.1.201/approval-arrow-key-80x24");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn missing_required_artifact_is_an_error() {
        let dir = std::env::temp_dir().join(format!(
            "detection-spike-corpus-missing-{}",
            std::process::id()
        ));
        let fixture_dir = dir.join("codex/0.145.0/token-streaming-80x24");
        fs::create_dir_all(&fixture_dir).expect("create fixture dir");
        fs::write(fixture_dir.join("input.bytes"), b"x").expect("write artifact");

        let err = discover(&dir, &["codex".to_string()]).unwrap_err();
        assert!(err.contains("missing"), "got: {err}");

        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn step_records_expose_the_ground_truth_fields() {
        let line = r#"{"hook":"Notification","kind":"permission","label":"permission-dialog","outcome":"ok","seq":4,"step":"wait_hook","t_ns":1,"timeout_ms":30000}"#;
        let record: StepRecord = serde_json::from_str(line).expect("parse step record");
        assert_eq!(record.step, "wait_hook");
        assert_eq!(record.hook.as_deref(), Some("Notification"));
        assert_eq!(record.kind.as_deref(), Some("permission"));
        assert_eq!(record.label.as_deref(), Some("permission-dialog"));

        let line = r#"{"label":"turn-one","step":"type_line","text":"Reply with exactly: ok"}"#;
        let record: StepRecord = serde_json::from_str(line).expect("parse step record");
        assert_eq!(record.text.as_deref(), Some("Reply with exactly: ok"));
    }
}
