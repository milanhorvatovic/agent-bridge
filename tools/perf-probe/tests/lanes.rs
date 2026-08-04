//! The lanes end to end, at test length: real PTYs, real children, the real
//! verification — everything the half-hour runs do, shrunk to seconds. What
//! these tests own is the plumbing between the pieces the unit suites prove
//! separately: that a lane can spawn its child, keep up with it, verify what
//! arrives, and fold an honest report out the other side, on every OS the
//! matrix runs.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use agent_bridge_perf_probe::report::Verdict;
use agent_bridge_perf_probe::{latency, monitor, replay, soak, throughput};

/// Build the fake CLI into this test run's profile directory, where the
/// lanes' sibling-binary lookup finds it. Cross-package binaries are not
/// built for another package's tests by default, and a lane erroring with
/// "binary not found" would read as a lane bug.
///
/// Exactly once per test process: a rebuild replaces the binary on disk, and
/// a second `cargo build` racing a test that is mid-spawn opens a window
/// where the path does not exist — which presented as a one-in-several-runs
/// spawn failure before the `Once`.
fn build_fake_cli() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(build_fake_cli_now);
}

fn build_fake_cli_now() {
    let mut profile_dir = std::env::current_exe().expect("the test executable has a path");
    profile_dir.pop();
    if profile_dir.ends_with("deps") {
        profile_dir.pop();
    }
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

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "agent-bridge-perf-lanes-{}-{name}",
        std::process::id()
    ))
}

#[test]
fn a_short_soak_survives_intact_with_its_resources_accounted() {
    build_fake_cli();
    let monitor_out = temp_path("soak-monitor.ndjson");
    let options = soak::Options {
        duration: Duration::from_secs(6),
        lines_per_second: 300,
        checksum_every: 100,
        monitor_out: Some(monitor_out.clone()),
        monitor_interval: Duration::from_secs(1),
        warmup: Duration::from_secs(2),
        ..soak::Options::default()
    };
    let (report, outcome) = soak::run(&options).expect("the soak lane must run");

    assert!(
        outcome.findings.clean(),
        "a healthy terminal must deliver the stream intact: {}",
        outcome.findings.summary()
    );
    assert_eq!(
        outcome.findings.lines_verified,
        options.lines(),
        "every line the scenario asked for must be verified"
    );
    assert!(
        outcome.findings.checksums_verified >= 10,
        "checkpoints must actually check: {}",
        outcome.findings.summary()
    );
    let assessment = outcome
        .monitor
        .expect("six seconds at a one-second interval has a steady state");
    assert!(
        assessment.within_budget(),
        "a six-second run must not leak: descriptor delta {}, rss growth {}",
        assessment.descriptor_delta,
        assessment.rss_growth_bytes,
    );
    assert!(
        report.exceeded().is_empty(),
        "a clean run reports no exceeded budgets"
    );
    let series = std::fs::read_to_string(&monitor_out).expect("the time series must be on disk");
    assert_eq!(series.lines().count(), assessment.samples);
    std::fs::remove_file(&monitor_out).expect("cleanup");
}

#[test]
fn the_latency_lanes_measure_at_test_scale() {
    build_fake_cli();
    let options = latency::Options {
        samples: 200,
        marker_interval_us: 200,
        discard: 30,
    };
    let (report, outcome) = latency::run(&options).expect("the latency lanes must run");

    assert_eq!(outcome.first_byte_ns.len(), 200);
    assert_eq!(outcome.forwarding_ns.len(), 200);
    for name in ["first_byte_latency", "input_forwarding_latency"] {
        let measurement = report
            .measurements
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("the report must carry {name}"));
        let distribution = measurement
            .distribution
            .unwrap_or_else(|| panic!("{name} must carry its distribution"));
        assert_eq!(distribution.count, 200);
        assert!(
            distribution.p99 > 0 && distribution.p99 >= distribution.p50,
            "{name}: implausible distribution {distribution:?}"
        );
        assert_ne!(
            measurement.verdict,
            Verdict::Unbudgeted,
            "{name} must be judged against its budget"
        );
    }
}

#[test]
fn the_throughput_lane_measures_and_verifies_a_flat_out_stream() {
    build_fake_cli();
    let options = throughput::Options {
        lines: 30_000,
        sessions: 1,
        ..throughput::Options::default()
    };
    let (report, outcome) = throughput::run(&options).expect("the throughput lane must run");
    assert_eq!(outcome.faults(), 0, "the stream must survive intact");
    assert_eq!(outcome.sessions[0].findings.lines_verified, 30_000);
    assert!(
        outcome.slowest_lines_per_sec() > 0,
        "a rate must come out of a completed run"
    );
    assert!(report.exceeded().is_empty() || outcome.slowest_lines_per_sec() < 1000);
}

#[test]
fn concurrent_sessions_each_get_verified_and_measured() {
    build_fake_cli();
    let options = throughput::Options {
        lines: 15_000,
        sessions: 2,
        ..throughput::Options::default()
    };
    let (_, outcome) = throughput::run(&options).expect("the concurrent lane must run");
    assert_eq!(outcome.sessions.len(), 2);
    for session in &outcome.sessions {
        assert!(
            session.findings.clean(),
            "every concurrent stream must survive intact: {}",
            session.findings.summary()
        );
        assert_eq!(session.findings.lines_verified, 15_000);
    }
    // No relation between the aggregate and per-session rates is asserted:
    // on a loaded test machine the sessions can run nearly serially, and the
    // aggregate legitimately lands below a single session's burst rate.
    assert!(outcome.aggregate_lines_per_sec() > 0);
}

/// The committed capture corpus, which is what the real replay lanes loop.
fn corpus_fixture(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus")
        .join(relative)
        .canonicalize()
        .expect("the committed corpus fixture must exist")
}

#[test]
fn a_generated_replay_of_a_real_recording_verifies_on_every_terminal() {
    let fixture = corpus_fixture("claude/2.1.202/token-streaming-80x24");
    let options = replay::Options {
        fixture_dirs: vec![fixture],
        build: replay::BuildOptions {
            mode: replay::Mode::Generated,
            duration: Duration::from_secs(4),
            idle_threshold: Duration::from_millis(500),
            idle_divisor: 20,
            ..replay::BuildOptions::default()
        },
        monitor_out: None,
        monitor_interval: Duration::from_secs(1),
        warmup: Duration::from_secs(1),
    };
    let (report, outcome) = replay::run(&options).expect("the generated replay must run");
    assert_eq!(outcome.faults, 0, "the paced stream must survive intact");
    assert!(outcome.bytes_read > 0);
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("token-streaming")),
        "the report must name its recording sources: {:?}",
        report.notes
    );
}

/// Byte-for-byte replay of the captured stream itself — the strongest claim,
/// on the terminals that are transparent pipes.
#[cfg(unix)]
#[test]
fn a_recorded_replay_arrives_byte_for_byte() {
    let fixture = corpus_fixture("claude/2.1.202/tool-lifecycle-80x24");
    let options = replay::Options {
        fixture_dirs: vec![fixture],
        build: replay::BuildOptions {
            mode: replay::Mode::Recorded,
            duration: Duration::from_secs(4),
            idle_threshold: Duration::from_millis(500),
            idle_divisor: 20,
            ..replay::BuildOptions::default()
        },
        monitor_out: None,
        monitor_interval: Duration::from_secs(1),
        warmup: Duration::from_secs(1),
    };
    let (report, outcome) = replay::run(&options).expect("the recorded replay must run");
    assert_eq!(
        outcome.faults, 0,
        "the captured bytes must cross the terminal untouched: {:?}",
        report.notes
    );
    let divergences = report
        .measurements
        .iter()
        .find(|m| m.name == "byte_divergences")
        .expect("the recorded lane reports byte divergences");
    assert_eq!(divergences.verdict, Verdict::Met);
}

/// The per-workload composition: a lane measuring while a bimodal replay of
/// a real recording streams in a second session. What this test owns is the
/// coexistence — the load spawns, the lane's samples all arrive, the load
/// dies on stop — not the numbers, which only mean something on a quiet
/// machine.
#[test]
fn the_latency_lane_measures_under_bimodal_load() {
    build_fake_cli();
    let fixture = corpus_fixture("claude/2.1.202/token-streaming-80x24");
    let load = replay::BackgroundLoad::start(&[fixture], Duration::from_secs(600))
        .expect("the background load must start");
    let options = latency::Options {
        samples: 120,
        marker_interval_us: 300,
        discard: 20,
    };
    let result = latency::run(&options);
    let stopped = load.stop().expect("the load must die on stop");
    assert!(
        stopped.contains("killed") || stopped.contains("exited"),
        "stopping the load must account for its child: {stopped}"
    );
    let (_, outcome) = result.expect("the latency lanes must run under load");
    assert_eq!(outcome.first_byte_ns.len(), 120);
    assert_eq!(outcome.forwarding_ns.len(), 120);
}

/// The monitor's own soak-shaped property, held at test length: what it
/// samples while a lane runs is what the assessment reads.
#[test]
fn the_monitor_survives_running_alongside_a_lane() {
    build_fake_cli();
    let monitor =
        monitor::Monitor::start(Duration::from_millis(200), None).expect("the monitor must start");
    let options = throughput::Options {
        lines: 5_000,
        sessions: 1,
        ..throughput::Options::default()
    };
    throughput::run(&options).expect("the lane must run under monitoring");
    let samples = monitor.stop().expect("the monitor must stop cleanly");
    // At least the immediate first sample: a fast lane can finish inside the
    // sampling interval, and that is the lane's virtue, not the monitor's
    // failure.
    assert!(!samples.is_empty(), "sampling must have happened");
}
