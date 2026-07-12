//! The smoke driver: every committed corpus scenario runs against the built
//! binary over plain pipes, and the run must reproduce the scenario script
//! exactly — emitted bytes, consumed stdin, and exit status. The expectation
//! is re-derived here from the scenario file itself, independently of the
//! interpreter's own parser, so the two cannot drift in lockstep.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::Value;

/// The scenarios this corpus started with. The corpus only grows — these
/// directories are permanent, and this list is only ever appended to.
const STARTER_SCENARIOS: [&str; 3] = ["cold-start", "single-token", "clean-exit"];

fn fake_corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/fake")
}

/// What a scenario script promises externally: the bytes that must appear on
/// stdout, the stdin bytes the script consumes along the way, and the
/// scripted exit code.
struct Script {
    stdout: Vec<u8>,
    stdin: Vec<u8>,
    exit_code: i32,
}

fn read_script(path: &Path) -> Script {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|err| panic!("{}: cannot read: {err}", path.display()));
    let root: Value = serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("{}: invalid JSON: {err}", path.display()));
    let steps = root["steps"]
        .as_array()
        .unwrap_or_else(|| panic!("{}: \"steps\" must be an array", path.display()));
    let mut script = Script {
        stdout: Vec::new(),
        stdin: Vec::new(),
        exit_code: 0,
    };
    let mut exit_seen = false;
    for (index, step) in steps.iter().enumerate() {
        if let Some(text) = step.get("emit") {
            assert_eq!(
                step.get("channel").and_then(Value::as_str),
                Some("stdout"),
                "{}: the smoke driver only knows the stdout channel",
                path.display()
            );
            script
                .stdout
                .extend_from_slice(text.as_str().expect("emit must be a string").as_bytes());
        } else if let Some(text) = step.get("await_stdin") {
            script.stdin.extend_from_slice(
                text.as_str()
                    .expect("await_stdin must be a string")
                    .as_bytes(),
            );
        } else if let Some(code) = step.get("exit") {
            // Mirror the interpreter's structural rule independently: the
            // scripted exit is the final step, exactly once. Without this,
            // a malformed scenario would surface as a confusing byte-diff
            // failure downstream instead of an authoring diagnostic here —
            // and requiring every exit to be final also makes a second
            // exit impossible.
            assert_eq!(
                index + 1,
                steps.len(),
                "{}: the \"exit\" step must be the final step",
                path.display()
            );
            script.exit_code =
                i32::try_from(code.as_i64().expect("exit must be an integer")).expect("exit code");
            exit_seen = true;
        } else {
            panic!(
                "{}: step the smoke driver does not know: {step}",
                path.display()
            );
        }
    }
    assert!(exit_seen, "{}: no exit step", path.display());
    script
}

/// Run one corpus scenario over pipes and hold the run to its script.
fn run_scenario(name: &str) {
    let dir = fake_corpus_root().join(name);
    let scenario = dir.join("scenario.json");
    let script = read_script(&scenario);

    let mut child = Command::new(env!("CARGO_BIN_EXE_fake-cli"))
        .arg(&scenario)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fake-cli must spawn");
    // All scripted stdin goes in up front: the interpreter consumes exactly
    // the awaited bytes in step order, so early bytes just wait in the pipe.
    child
        .stdin
        .take()
        .expect("stdin must be piped")
        .write_all(&script.stdin)
        .expect("writing scripted stdin must succeed");
    let output = child
        .wait_with_output()
        .expect("collecting the run must succeed");

    assert_eq!(
        output.stdout,
        script.stdout,
        "{name}: emitted bytes must match the script exactly (got {:?}, want {:?})",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&script.stdout),
    );
    assert_eq!(
        output.status.code(),
        Some(script.exit_code),
        "{name}: exit status must match the scripted exit step ({})",
        diagnostics(&output),
    );
    assert!(
        output.stderr.is_empty(),
        "{name}: a passing run must not write diagnostics: {}",
        diagnostics(&output),
    );
}

fn diagnostics(output: &Output) -> String {
    format!("stderr: {}", String::from_utf8_lossy(&output.stderr))
}

#[test]
fn smoke_cold_start() {
    run_scenario("cold-start");
}

#[test]
fn smoke_single_token() {
    run_scenario("single-token");
}

#[test]
fn smoke_clean_exit() {
    run_scenario("clean-exit");
}

/// The starter set is permanent: a missing directory here means a scenario
/// was deleted or renamed, which the corpus rules treat as a contract
/// regression, not a cleanup.
#[test]
fn the_starter_set_never_shrinks() {
    for name in STARTER_SCENARIOS {
        assert!(
            fake_corpus_root()
                .join(name)
                .join("scenario.json")
                .is_file(),
            "starter scenario \"{name}\" is missing from the corpus"
        );
    }
}

/// Every conformance scenario in the fake corpus smokes — scenarios added
/// later are covered the moment they are committed, with no driver change.
/// Version directories of captured-session fixtures (recorded by the
/// interactive probe's `record` lane; no `scenario.json`) share the corpus
/// tree but are not fake-cli scripts, so they are not smokeable here —
/// `trace_check` owns their structural validation.
#[test]
fn every_fake_corpus_scenario_passes_the_smoke_driver() {
    let root = fake_corpus_root();
    let mut found = 0;
    for entry in std::fs::read_dir(&root)
        .unwrap_or_else(|err| panic!("{}: cannot list: {err}", root.display()))
    {
        let path = entry.expect("corpus listing must succeed").path();
        if path.is_dir() && path.join("scenario.json").is_file() {
            run_scenario(
                path.file_name()
                    .expect("scenario directory name")
                    .to_str()
                    .expect("scenario directory names are UTF-8"),
            );
            found += 1;
        }
    }
    assert!(
        found >= STARTER_SCENARIOS.len(),
        "expected at least the {} starter scenarios, found {found}",
        STARTER_SCENARIOS.len()
    );
}
