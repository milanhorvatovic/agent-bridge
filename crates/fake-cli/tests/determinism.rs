//! The determinism guarantee and the `await_stdin` failure paths, asserted
//! against the built binary — the binary contract is the public surface, so
//! the tests exercise it the way a scenario runner will: spawn, feed stdin,
//! read stdout, check the exit status and the stderr diagnostic.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};

fn fake_cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fake-cli"))
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn corpus_scenario(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus/fake")
        .join(name)
        .join("scenario.json")
}

/// Spawn the binary on a scenario, write `input` to stdin, close stdin, and
/// collect the run.
fn run_with_input(scenario: &PathBuf, input: &[u8]) -> Output {
    let mut child = spawn(scenario);
    child
        .stdin
        .take()
        .expect("stdin must be piped")
        .write_all(input)
        .expect("writing scripted stdin must succeed");
    child
        .wait_with_output()
        .expect("collecting the run must succeed")
}

fn spawn(scenario: &PathBuf) -> Child {
    fake_cli()
        .arg(scenario)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("fake-cli must spawn")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The determinism contract: the same scenario produces byte-identical
/// stdout on every run. The paced corpus scenario is the interesting run —
/// per-byte writes with sleeps between them are where nondeterminism would
/// creep in if pacing ever touched content.
#[test]
fn run_twice_byte_identical() {
    let scenario = corpus_scenario("single-token");
    let first = run_with_input(&scenario, b"");
    let second = run_with_input(&scenario, b"");
    assert!(
        first.status.success(),
        "first run failed: {}",
        stderr_text(&first)
    );
    assert!(
        second.status.success(),
        "second run failed: {}",
        stderr_text(&second)
    );
    assert_eq!(
        first.stdout, second.stdout,
        "the same scenario must emit identical bytes on every run"
    );
}

/// The generated stream carries the determinism contract too — and it is
/// the harder case, because its content is computed rather than copied out
/// of the scenario file. A generator that drifted by so much as one line
/// would make every corruption report from a soak run unreadable.
#[test]
fn generated_streams_are_byte_identical_across_runs() {
    let scenario = fixture("generate-checksummed.json");
    let first = run_with_input(&scenario, b"");
    let second = run_with_input(&scenario, b"");
    assert!(
        first.status.success(),
        "first run failed: {}",
        stderr_text(&first)
    );
    assert_eq!(
        first.stdout, second.stdout,
        "a generated stream is derived from line numbers, so it cannot vary"
    );
    assert_eq!(
        first.stdout.iter().filter(|byte| **byte == b'\n').count(),
        300 + 6,
        "300 payload lines plus one checksum line per 50 of them"
    );
}

/// The `{ts}` token is the one place a scenario's bytes may differ between
/// runs, and this is the test that says so out loud: same scenario, two
/// runs, different bytes — and both readings inside the window the test
/// itself observed.
#[test]
fn the_ts_token_is_the_only_scripted_content_that_varies() {
    let scenario = fixture("timestamped-marker.json");
    let before = agent_bridge_fake_cli::clock::monotonic_ns();
    let first = run_with_input(&scenario, b"");
    let second = run_with_input(&scenario, b"");
    let after = agent_bridge_fake_cli::clock::monotonic_ns();
    assert!(
        first.status.success(),
        "first run failed: {}",
        stderr_text(&first)
    );
    assert_ne!(
        first.stdout, second.stdout,
        "two readings of a running clock cannot be equal"
    );
    for output in [&first, &second] {
        let text = String::from_utf8(output.stdout.clone()).expect("the marker is ASCII");
        let stamp: u64 = text
            .trim_end()
            .rsplit(' ')
            .next()
            .expect("the marker has a timestamp field")
            .parse()
            .unwrap_or_else(|_| panic!("the timestamp must be a number: {text:?}"));
        assert!(
            (before..=after).contains(&stamp),
            "{stamp} is outside the window {before}..={after} the runs happened in — \
             the child's clock and the reader's must be the same clock"
        );
    }
}

#[test]
fn await_stdin_mismatch_nonzero_with_diagnostic() {
    let output = run_with_input(&fixture("await-mismatch.json"), b"n\n");
    assert!(
        !output.status.success(),
        "diverging input must fail the run"
    );
    let diagnostic = stderr_text(&output);
    assert!(
        diagnostic.contains("await_stdin") && diagnostic.contains("mismatch"),
        "the diagnostic must name the failing step kind and class: {diagnostic}"
    );
    assert!(
        diagnostic.contains("y\\n") && diagnostic.contains('n'),
        "the diagnostic must carry expected and got: {diagnostic}"
    );
}

#[test]
fn await_stdin_timeout_nonzero_with_diagnostic() {
    // Spawn without writing and keep stdin open until the child gives up:
    // a closed pipe would exercise the end-of-input path, not the timeout.
    let mut child = spawn(&fixture("await-timeout.json"));
    let held_open = child.stdin.take().expect("stdin must be piped");
    let output = child
        .wait_with_output()
        .expect("collecting the run must succeed");
    drop(held_open);
    assert!(!output.status.success(), "silence must fail the run");
    let diagnostic = stderr_text(&output);
    assert!(
        diagnostic.contains("await_stdin") && diagnostic.contains("timed out"),
        "the diagnostic must name the failing step kind and class: {diagnostic}"
    );
}

#[test]
fn await_stdin_eof_nonzero_with_diagnostic() {
    let output = run_with_input(&fixture("await-mismatch.json"), b"");
    assert!(
        !output.status.success(),
        "stdin closing before the expected input must fail the run"
    );
    let diagnostic = stderr_text(&output);
    assert!(
        diagnostic.contains("stdin closed"),
        "the diagnostic must name the end-of-input cause: {diagnostic}"
    );
}

#[test]
fn await_stdin_match_releases_the_script() {
    let output = run_with_input(&fixture("await-then-emit.json"), b"go\n");
    assert!(
        output.status.success(),
        "matching input must let the script run to its exit step: {}",
        stderr_text(&output)
    );
    assert_eq!(
        output.stdout, b"ready\ndone\n",
        "emits on both sides of the await must reach stdout in order"
    );
    assert!(
        output.stderr.is_empty(),
        "a passing run must not write diagnostics: {}",
        stderr_text(&output)
    );
}

#[test]
fn missing_argument_is_a_usage_error() {
    let output = fake_cli()
        .stdin(Stdio::null())
        .output()
        .expect("fake-cli must spawn");
    assert!(!output.status.success());
    assert!(
        stderr_text(&output).contains("usage"),
        "the diagnostic must state the usage: {}",
        stderr_text(&output)
    );
}

#[test]
fn unreadable_scenario_is_a_load_error() {
    let output = fake_cli()
        .arg("no-such-scenario.json")
        .stdin(Stdio::null())
        .output()
        .expect("fake-cli must spawn");
    assert!(!output.status.success());
    assert!(
        stderr_text(&output).contains("cannot read scenario"),
        "the diagnostic must name the load failure: {}",
        stderr_text(&output)
    );
}
