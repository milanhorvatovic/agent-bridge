//! End-to-end shakedown of the `record` lane against the deterministic fake
//! CLI: the committed `roundtrip` capture scenario runs under a real PTY at
//! the larger capture dimension (120×40), and the full fixture artifact set
//! must come out coherent — the byte stream with its timing sidecar, the
//! labeled step log, and a manifest that accounts for exactly the artifacts
//! a side-channel-free CLI produces. This is the zero-quota proof that the
//! capture rig records the right things, run before any real CLI session is
//! spent on it.
//!
//! Content assertions are substring-based, not byte-exact: the PTY layer is
//! entitled to translate (ONLCR turns LF into CRLF, cooked mode echoes
//! input), and that translation is precisely what the fixtures exist to
//! preserve. What this test owns is that the scripted output arrived, in
//! order, into `input.bytes`, and that every sidecar is consistent with it.

use std::path::PathBuf;
use std::process::Command;

use agent_bridge_interactive_probe::record::{RecordConfig, run};

#[test]
fn record_lane_captures_the_fake_cli_roundtrip() {
    let fake_cli = build_fake_cli();
    let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/capture-scenarios/fake/roundtrip.record.json")
        .canonicalize()
        .expect("the roundtrip capture scenario must exist");
    let out = std::env::temp_dir().join(format!(
        "agent-bridge-record-fake-{}-{}",
        std::process::id(),
        // Two test binaries could share a pid across runs; the timestamp
        // keeps reruns from inheriting a stale fixture directory.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    ));

    let config = RecordConfig {
        script,
        out: out.clone(),
        cols: 120,
        rows: 40,
        cli_bin: Some(fake_cli.to_string_lossy().into_owned()),
        cli_version: Some("fake".to_string()),
        install: Some("workspace build".to_string()),
        ..RecordConfig::default()
    };
    run(&config).unwrap_or_else(|failure| {
        panic!(
            "the record lane must succeed; step {} failed: {}",
            failure.step, failure.detail
        )
    });

    // The byte stream: scripted output present, in scripted order.
    let input = std::fs::read(out.join("input.bytes")).expect("input.bytes must exist");
    let text = String::from_utf8_lossy(&input).into_owned();
    let banner_at = text
        .find("session ready")
        .unwrap_or_else(|| panic!("the banner must be captured: {text:?}"));
    let hello_at = text
        .find("Hello capture world.")
        .unwrap_or_else(|| panic!("the paced stream must be captured: {text:?}"));
    let bye_at = text
        .find("bye")
        .unwrap_or_else(|| panic!("the goodbye must be captured: {text:?}"));
    assert!(
        banner_at < hello_at && hello_at < bye_at,
        "scripted output must arrive in order: banner@{banner_at} hello@{hello_at} bye@{bye_at}"
    );

    // The timing sidecar: one record per read boundary, offsets within the
    // stream and monotonic on both axes. The 5ms-per-byte paced emission
    // guarantees the stream arrived split, so a single-record sidecar would
    // mean the boundaries were lost.
    let timing = std::fs::read_to_string(out.join("input.timing.ndjson"))
        .expect("input.timing.ndjson must exist");
    let records: Vec<serde_json::Value> = timing
        .lines()
        .map(|line| serde_json::from_str(line).expect("timing lines must be JSON"))
        .collect();
    assert!(
        records.len() > 1,
        "paced emission must produce more than one read boundary"
    );
    let mut previous_offset = None;
    let mut previous_t = None;
    for record in &records {
        let offset = record["offset"].as_u64().expect("offset must be a u64");
        let t = record["monotonic_ns"]
            .as_u64()
            .expect("monotonic_ns must be a u64");
        assert!(
            (offset as usize) < input.len(),
            "offset {offset} points past the {}-byte stream",
            input.len()
        );
        if let Some(previous) = previous_offset {
            assert!(offset > previous, "offsets must be strictly increasing");
        }
        if let Some(previous) = previous_t {
            assert!(t >= previous, "timestamps must be monotonic");
        }
        previous_offset = Some(offset);
        previous_t = Some(t);
    }
    assert_eq!(records[0]["offset"], 0, "the stream starts at offset 0");

    // The step log: every scripted step ran, in order, with its label and a
    // clean outcome, on a monotonic clock.
    let steps = std::fs::read_to_string(out.join("steps.ndjson")).expect("steps.ndjson must exist");
    let steps: Vec<serde_json::Value> = steps
        .lines()
        .map(|line| serde_json::from_str(line).expect("step lines must be JSON"))
        .collect();
    let expected = [
        ("wait_text", Some("banner")),
        ("press", Some("answer-key")),
        ("pause", None),
        ("press", None),
        ("wait_text", Some("streamed")),
        ("wait_quiet", Some("stream-over")),
        ("type_line", Some("quit-command")),
        ("wait_text", Some("goodbye")),
    ];
    assert_eq!(steps.len(), expected.len(), "one record per scripted step");
    let mut previous_t = 0u64;
    for (index, (record, (kind, label))) in steps.iter().zip(expected).enumerate() {
        assert_eq!(record["seq"], (index + 1) as u64, "seq must be 1-based");
        assert_eq!(record["step"], *kind, "step {} kind", index + 1);
        assert_eq!(record["outcome"], "ok", "step {} outcome", index + 1);
        match label {
            Some(label) => assert_eq!(record["label"], *label, "step {} label", index + 1),
            None => assert!(
                record.get("label").is_none(),
                "step {} carries no label",
                index + 1
            ),
        }
        let t = record["t_ns"].as_u64().expect("t_ns must be a u64");
        assert!(t >= previous_t, "step timestamps must be monotonic");
        previous_t = t;
    }

    // The manifest: names the session, and accounts for exactly the
    // artifacts a side-channel-free CLI produces.
    let manifest =
        std::fs::read_to_string(out.join("manifest.yaml")).expect("manifest.yaml must exist");
    for line in [
        "cli: fake",
        "cli_version: \"fake\"",
        "install: \"workspace build\"",
        "scenario: roundtrip",
        "cols: 120",
        "rows: 40",
        "tier: ci",
        &format!("os: {}", std::env::consts::OS),
        &format!("  input.bytes: {}", input.len()),
    ] {
        assert!(
            manifest.contains(line),
            "manifest must carry `{line}`:\n{manifest}"
        );
    }
    for absent in ["hook-payloads", "transcript.jsonl"] {
        assert!(
            !manifest.contains(absent),
            "a generic fixture has no {absent}:\n{manifest}"
        );
    }

    // No side-channel files, and no working intermediates, in the fixture.
    for leftover in [
        "hook-payloads.ndjson",
        "hook-payloads.timing.ndjson",
        "transcript.jsonl",
        "capture.ndjson",
        "capture-meta.json",
    ] {
        assert!(
            !out.join(leftover).exists(),
            "{leftover} must not exist in a generic fixture"
        );
    }

    std::fs::remove_dir_all(&out).expect("the fixture directory must be removable");
}

/// Build the fake CLI through the same cargo that runs this test, so the
/// binary under the PTY is always the one from the commit under test — the
/// same discipline as the fake CLI's own PTY-hosted test, for the same reason.
fn build_fake_cli() -> PathBuf {
    let mut profile_dir = std::env::current_exe().expect("the test executable has a path");
    profile_dir.pop(); // the test executable's file name
    if profile_dir.ends_with("deps") {
        profile_dir.pop();
    }
    // Derive the profile from the directory this test runs out of, so a
    // `--release` run builds the binary into the directory it then loads
    // from. Cargo's naming quirk: the `dev` profile outputs into
    // `target/debug`.
    let dir_name = profile_dir
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the profile directory has a UTF-8 name");
    let profile = if dir_name == "debug" { "dev" } else { dir_name };

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = Command::new(cargo);
    build.args([
        "build",
        "--quiet",
        "--package",
        "agent-bridge-fake-cli",
        "--profile",
        profile,
    ]);
    let status = build.status().expect("cargo must be runnable");
    assert!(status.success(), "building the fake CLI failed: {status}");

    let binary = profile_dir.join(format!("fake-cli{}", std::env::consts::EXE_SUFFIX));
    assert!(
        binary.is_file(),
        "built fake-cli not found at {}",
        binary.display()
    );
    binary
}
