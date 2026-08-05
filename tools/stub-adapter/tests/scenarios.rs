//! The committed starter scenarios, run end-to-end through the launch path.
//!
//! Each scenario spawns the real fake-cli binary as a child process — the
//! same execution shape the CI lane uses — so what is asserted here is the
//! launch path itself: spawn, drain, exit, on whichever OS is running the
//! test. Byte counts are asserted exactly for the three starter scenarios
//! because the fake CLI is deterministic by design; a drift in those
//! numbers is a scripted-output change, not flake.

use std::path::PathBuf;
use std::process::Command;

use agent_bridge_stub_adapter::run_scenario;

fn fake_corpus() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/fake")
}

/// Build the fake CLI through the same cargo that runs this test, so a
/// standalone `cargo test -p agent-bridge-stub-adapter` finds the sibling
/// binary [`run_scenario`] spawns instead of failing on a missing build —
/// the same discipline as the interactive probe's record-lane test, for the
/// same reason: the binary under test is always the one from the commit
/// under test.
fn build_fake_cli() {
    let mut profile_dir = std::env::current_exe().expect("the test executable has a path");
    profile_dir.pop(); // the test executable's file name
    if profile_dir.ends_with("deps") {
        profile_dir.pop();
    }
    // Derive the profile from the directory this test runs out of, so a
    // `--release` run builds the binary into the directory the stub then
    // loads it from. Cargo's naming quirk: the `dev` profile outputs into
    // `target/debug`.
    let dir_name = profile_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the profile directory has a UTF-8 name");
    let profile = if dir_name == "debug" { "dev" } else { dir_name };

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args([
            "build",
            "--quiet",
            "--package",
            "agent-bridge-fake-cli",
            "--profile",
            profile,
        ])
        .status()
        .expect("cargo must be runnable");
    assert!(status.success(), "building the fake CLI failed: {status}");
}

#[test]
fn stub_adapter_runs_the_three_starter_scenarios() {
    build_fake_cli();
    // scripted stdout of each starter scenario, in bytes:
    //   clean-exit    — nothing
    //   cold-start    — "fake-cli: session ready\n" + "> "
    //   single-token  — "Hello world."
    let expectations: [(&str, u64); 3] =
        [("clean-exit", 0), ("cold-start", 26), ("single-token", 12)];

    for (name, stdout_bytes) in expectations {
        let scenario = fake_corpus().join(name).join("scenario.json");
        assert!(
            scenario.is_file(),
            "{}: the committed starter scenario is missing",
            scenario.display()
        );
        let report = run_scenario(&scenario)
            .unwrap_or_else(|err| panic!("{name}: the launch path failed: {err}"));
        assert!(
            report.clean(),
            "{name}: expected a clean exit, got {report:?}"
        );
        assert_eq!(
            report.stdout_bytes, stdout_bytes,
            "{name}: scripted stdout byte count changed: {report:?}"
        );
        assert_eq!(
            report.stderr_bytes, 0,
            "{name}: the scripted scenarios write nothing to stderr: {report:?}"
        );
    }
}

#[test]
fn a_missing_scenario_reports_instead_of_panicking() {
    build_fake_cli();
    let missing = fake_corpus().join("no-such-scenario/scenario.json");
    let report = run_scenario(&missing).expect("spawning on a missing file still spawns");
    // The fake CLI exits 2 on an unreadable scenario, with a diagnostic on
    // stderr — the stub must surface that, not mask it.
    assert_eq!(report.exit_code, Some(2), "{report:?}");
    assert!(report.stderr_bytes > 0, "{report:?}");
}
