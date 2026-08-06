//! Dev-task runner — the single source of truth for the check sequence that
//! both local development and CI run. Every check CI performs is a task in
//! here, so "green locally" and "green in CI" cannot drift apart: `cargo
//! xtask ci` is the PR tier bar one job, and that one is `cargo xtask deny`,
//! kept separate only because it needs a tool installed rather than because
//! it is run differently.
//!
//! Deliberately dependency-free (std only): a contributor needs nothing beyond
//! the pinned toolchain and `git`, both of which every dev machine and CI
//! runner already have — which is exactly why the check that *does* need
//! something installed sits outside `ci` instead of quietly costing that
//! promise. Cross-platform (no shell scripts) so Windows, macOS, and Linux
//! run the identical logic.
//!
//! Usage:
//!   cargo xtask ci           # format check + clippy + build + test + schema freshness + probes + gates
//!   cargo xtask probe        # the deterministic probes only — what the container CI lane runs
//!   cargo xtask live-probe   # probes that spawn a real CLI; needs credentials, never on the PR tier
//!   cargo xtask drift-gate   # the reserved-pattern gate only
//!   cargo xtask workspace-gate
//!                            # the crate-layout gate only: dependency direction,
//!                            # package naming, central version pinning, inherited lints
//!   cargo xtask deny [check]…
//!                            # the dependency supply-chain gate: advisories, licenses,
//!                            # bans, and sources over the resolved tree (deny.toml), plus
//!                            # the review dates on any advisory suppression. Needs
//!                            # `cargo install cargo-deny --locked`, which is why it is a
//!                            # separate step from `ci` rather than part of it.
//!   cargo xtask bench        # release-built latency + throughput benchmarks, then the
//!                            # regression gate against the committed per-OS baseline —
//!                            # the PR benchmark lane
//!   cargo xtask soak-nightly # the half-hour endurance lanes (synthetic + bimodal replay)
//!                            # with the resource monitor, plus the nightly benchmark set —
//!                            # the nightly workflow's payload, runnable locally as-is
//!   cargo xtask capture-campaign --cli <name> --bin <path> --version-label <label>
//!                            --install <text> [--model <name>] [--mask <text>]...
//!                            [--only <scenario>] [--dry-run]
//!                            # record every capture scenario for one CLI at one pinned
//!                            # version, both terminal sizes, into tests/corpus/ — then
//!                            # scrub and hold the corpus to its size budget. Maintainer-run,
//!                            # never on any CI tier: the claude campaign spends session quota.
//!                            # One sitting per CLI = one invocation per pinned version.
//!                            # --mask adds scrub needles beyond the username and the names
//!                            # auto-derived from git identity (those distinctive enough to
//!                            # mask safely — >=3 bytes with a letter). The claude TUI paints two
//!                            # account settings the campaign cannot derive: the account
//!                            # email (pass its local part) and the display name in the
//!                            # "Welcome back <name>!" splash — pass both, or the scrub aborts.

use std::path::{Path, PathBuf};
use std::process::{Command, exit};

/// The check sequence, in order. Every entry is a `cargo` subcommand; the
/// gate ties them to what CI enforces so the two stay identical.
const STEPS: &[(&str, &[&str])] = &[
    ("format", &["fmt", "--all", "--", "--check"]),
    (
        "clippy",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    ),
    ("build", &["build", "--workspace"]),
    ("test", &["test", "--workspace"]),
    // The committed schema/ artifacts are generated from the event types,
    // never hand-written. This gate regenerates them in memory and fails on
    // any byte difference — so an event-type change that forgot to commit
    // regenerated artifacts, and a hand-edit of an artifact, both fail CI.
    (
        "schema freshness",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-events",
            "--bin",
            "schema-gen",
            "--",
            "--check",
        ],
    ),
];

/// The probe binaries *run* (not just build) — a PTY that cannot be
/// allocated on a platform is exactly what they exist to catch, and that
/// only shows at runtime. Split out of STEPS so the container CI lane can
/// run just this slice as `cargo xtask probe`.
///
/// Every probe here is deterministic and credential-free: the interactive
/// lane drives a stand-in fixture, not a real CLI. Probes that need a real
/// CLI and credentials live in `LIVE_PROBE_STEPS`.
const PROBE_STEPS: &[(&str, &[&str])] = &[
    (
        "pty-probe",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-pty-probe",
            "--bin",
            "pty-probe",
        ],
    ),
    (
        "pty-probe (env defaults)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-pty-probe",
            "--bin",
            "pty-probe",
            "--",
            "--check-env",
        ],
    ),
    // `cargo run --bin X` builds only X, but the interactive probe spawns
    // its stand-in fixture as a sibling binary. Build both first: the
    // `cargo xtask probe` lane runs only these PROBE_STEPS, not the
    // workspace-wide `build` from STEPS, so without this step the fixture
    // would be missing and the probe would fail to spawn it.
    (
        "interactive-probe (build both bins)",
        &[
            "build",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bins",
        ],
    ),
    (
        "interactive-probe (stand-in fixture)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bin",
            "interactive-probe",
            "--",
            "standin",
        ],
    ),
    // The signal, resize, UTF-8, and cleanup probes spawn their fixtures
    // (probe-child, resize-child, utf8-child, tree-child) from a sibling
    // package, which `cargo run --bin <probe>` alone would not build.
    (
        "build the probe fixtures (probe-child, resize-child, utf8-child, tree-child)",
        &["build", "--quiet", "--package", "agent-bridge-probe-child"],
    ),
    // Both interrupt-delivery scenarios: to a raw-mode child (the mode
    // interactive CLIs run in) 0x03 is data and a process-group SIGINT is a
    // separate, distinct path; to a cooked-mode child the terminal itself
    // turns the same byte into the interrupt.
    (
        "signal-probe (raw-mode child)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-signal-probe",
            "--bin",
            "signal-probe",
            "--",
            "raw",
        ],
    ),
    (
        "signal-probe (cooked-mode child)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-signal-probe",
            "--bin",
            "signal-probe",
            "--",
            "cooked",
        ],
    ),
    // Both resize scenarios: the steady grow-and-shrink pair proves resize
    // propagation is observed and repeatable with the dimension env pinned
    // at spawn-time values; the early scenario characterizes the
    // resize-before-ready launch race as a typed, recorded outcome.
    (
        "resize-probe (steady grow/shrink pair)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-resize-probe",
            "--bin",
            "resize-probe",
            "--",
            "steady",
        ],
    ),
    (
        "resize-probe (early resize before ready)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-resize-probe",
            "--bin",
            "resize-probe",
            "--",
            "early",
        ],
    ),
    // Both UTF-8 scenarios: the sweep respawns the fixture once per
    // read-buffer size (down to a single byte) and holds the reassembled
    // multi-byte corpus to the fixture's checksum trailer; the invalid lane
    // proves bytes that can never be UTF-8 surface as exactly-located spans
    // — or, on Windows, as a recorded ConPTY substitution — never as
    // silence.
    (
        "utf8-probe (read-buffer sweep over the multi-byte corpus)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-utf8-probe",
            "--bin",
            "utf8-probe",
            "--",
            "sweep",
        ],
    ),
    (
        "utf8-probe (invalid bytes between valid sequences)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-utf8-probe",
            "--bin",
            "utf8-probe",
            "--",
            "invalid",
        ],
    ),
    // Both cleanup scenarios: the clean exit proves an ended session leaves
    // nothing behind — process group / job object empty, PTY released,
    // fd/handle counts back to baseline, no ConPTY console host — with the
    // setsid escapee detected as outside the group rather than pretended
    // away; the terminate scenario proves the polite-then-forced escalation
    // empties the tree even when the fixture ignores the polite signal.
    (
        "cleanup-probe (clean exit of a process tree)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-cleanup-probe",
            "--bin",
            "cleanup-probe",
            "--",
            "clean",
        ],
    ),
    (
        "cleanup-probe (terminate escalation past a stubborn tree)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-cleanup-probe",
            "--bin",
            "cleanup-probe",
            "--",
            "terminate",
        ],
    ),
    // The detection spike's replay is deterministic and credential-free —
    // it reads only the committed corpus — but the binary's corpus walk and
    // report path are exercised end-to-end here, beyond what the library
    // integration lanes in `cargo test` cover.
    (
        "detection-spike (text-matching replay over the corpus)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-detection-spike",
            "--bin",
            "detection-spike",
            "--",
            "replay",
            "--config",
            "a",
        ],
    ),
    // The screen-state configuration walks the same corpus through the
    // virtual-terminal path; a separate entry so a failure names the
    // configuration that broke.
    (
        "detection-spike (screen-state replay over the corpus)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-detection-spike",
            "--bin",
            "detection-spike",
            "--",
            "replay",
            "--config",
            "b",
        ],
    ),
    // The side-channel configuration replays the recorded hook payloads and
    // transcripts (claude-only — no other corpus records them); same
    // per-configuration split so a failure names its configuration.
    (
        "detection-spike (side-channel replay over the corpus)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-detection-spike",
            "--bin",
            "detection-spike",
            "--",
            "replay",
            "--config",
            "c",
        ],
    ),
    // The metrics collection replays all three configurations in one run
    // and folds in the committed effort log, so this entry exercises the
    // whole measurement path — aggregation, drift computation, the log's
    // validation against the corpus-known matcher ids, and the JSON
    // report write (into the untracked build directory).
    (
        "detection-spike (metrics collection over the corpus)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-detection-spike",
            "--bin",
            "detection-spike",
            "--",
            "metrics",
            "--out",
            "target/detection-spike-metrics.json",
        ],
    ),
    // The stub adapter runs every committed fake-CLI conformance scenario
    // through the real launch path — spawn the scripted CLI, drain its
    // output, observe its exit — on every OS and in the container lane. The
    // build step first: this lane spawns the fake-cli binary as a child,
    // which `cargo run --bin stub-adapter` alone would not build.
    (
        "stub-adapter (build it and the scripted CLI it launches)",
        &[
            "build",
            "--quiet",
            "--package",
            "agent-bridge-stub-adapter",
            "--package",
            "agent-bridge-fake-cli",
        ],
    ),
    (
        "stub-adapter (conformance scenarios through the launch path)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-stub-adapter",
            "--bin",
            "stub-adapter",
        ],
    ),
    // The perf probe's smoke pair: a seconds-long soak and a seconds-long
    // recorded-pacing replay, so the endurance plumbing — spawn, verify,
    // monitor, teardown — is exercised on every OS and in the container lane
    // on every push. The measured runs live elsewhere: `cargo xtask bench`
    // is the PR benchmark lane and `cargo xtask soak-nightly` the half-hour
    // lanes; this entry only proves the machinery still works.
    (
        "perf-probe (build the measurement binaries)",
        &[
            "build",
            "--quiet",
            "--package",
            "agent-bridge-perf-probe",
            "--package",
            "agent-bridge-fake-cli",
        ],
    ),
    (
        "perf-probe (streaming soak smoke)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-perf-probe",
            "--bin",
            "perf-probe",
            "--",
            "soak",
            "--seconds",
            "10",
            "--rate",
            "500",
            "--monitor-interval-secs",
            "2",
            "--warmup-secs",
            "3",
        ],
    ),
    (
        "perf-probe (recorded-pacing replay smoke)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-perf-probe",
            "--bin",
            "perf-probe",
            "--",
            "replay",
            "--seconds",
            "8",
            "--fixture",
            "tests/corpus/claude/2.1.202/token-streaming-80x24",
            "--idle-threshold-ms",
            "500",
            "--idle-divisor",
            "20",
        ],
    ),
];

/// Probes that spawn a **real** interactive CLI. They need the CLI on PATH
/// and working credentials, so they never run on the PR tier: `cargo xtask
/// live-probe` is invoked only by the opt-in live CI lane and by a
/// maintainer locally.
///
/// Both lanes pin the cheapest model: they assert event shapes and
/// sequences, never model output, so a larger model would buy nothing and
/// cost quota. The four-point lane runs here — on POSIX it is the baseline
/// the Windows-client results are compared against, and running it every
/// time the label goes on keeps that baseline from going stale.
const LIVE_PROBE_STEPS: &[(&str, &[&str])] = &[
    (
        "interactive-probe (real CLI)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bin",
            "interactive-probe",
            "--",
            "probe",
            "--model",
            "haiku",
        ],
    ),
    (
        "interactive-probe (four-point hook-channel verification)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bin",
            "interactive-probe",
            "--",
            "fourpoint",
            "--model",
            "haiku",
        ],
    ),
    // The clean-shutdown observation the deterministic cleanup lanes cannot
    // make: a typed /exit into the real CLI, asserting SessionEnd fires AND
    // the child process actually terminates AND the PTY tears down, with
    // the SessionEnd-to-exit interval reported.
    (
        "interactive-probe (real-CLI /exit cleanup)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bin",
            "interactive-probe",
            "--",
            "cleanup",
            "--model",
            "haiku",
        ],
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let task = args.first().map(String::as_str).unwrap_or_default();
    let passed = match task {
        "ci" => run_ci(),
        "probe" => run_steps(PROBE_STEPS),
        "live-probe" => run_live_probes(),
        "drift-gate" => drift_gate(),
        "workspace-gate" => workspace_gate(),
        "deny" => run_deny(&args[1..]),
        "capture-campaign" => run_capture_campaign(&args[1..]),
        "bench" => run_bench(),
        "soak-nightly" => run_soak_nightly(),
        other => {
            eprintln!(
                "unknown xtask '{other}'. usage: cargo xtask <ci|probe|live-probe|drift-gate|workspace-gate|deny|capture-campaign|bench|soak-nightly>"
            );
            exit(2);
        }
    };
    if !passed {
        exit(1);
    }
}

/// The recorded real-CLI sessions the bimodal replay lanes loop: bursty
/// token streaming, a tool-call lifecycle, and a long idle around thinking —
/// the three shapes that make real CLI traffic unlike a steady synthetic
/// stream. One list, so the nightly lanes and anyone re-running them locally
/// replay the same workload.
const BIMODAL_FIXTURES: [&str; 3] = [
    "tests/corpus/claude/2.1.202/token-streaming-120x40",
    "tests/corpus/claude/2.1.202/tool-lifecycle-120x40",
    "tests/corpus/claude/2.1.202/idle-notification-120x40",
];

/// Run one owned-args cargo step — the dynamic sibling of the static step
/// tables, for lanes whose arguments depend on the platform or compose from
/// lists.
fn cargo_owned(name: &str, args: &[String]) -> bool {
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    cargo(name, &borrowed)
}

/// The release-built perf binaries every measured lane spawns. Release on
/// purpose: the verifier runs inside the measured loop, and debug-build
/// overhead would be measured as if it were the terminal's.
fn build_perf_release() -> bool {
    cargo(
        "perf-probe (release build)",
        &[
            "build",
            "--release",
            "--quiet",
            "--package",
            "agent-bridge-perf-probe",
            "--package",
            "agent-bridge-fake-cli",
        ],
    )
}

fn perf_probe_args(tail: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "run",
        "--release",
        "--quiet",
        "--package",
        "agent-bridge-perf-probe",
        "--bin",
        "perf-probe",
        "--",
    ]
    .map(String::from)
    .to_vec();
    args.extend(tail.iter().map(|arg| (*arg).to_string()));
    args
}

/// The PR benchmark lane: full-sample latency and short throughput runs in
/// release, then the latency report held against the committed per-OS
/// baseline — the regression gate. Absolute budget verdicts stay in the
/// report JSON (shared runners are too noisy to enforce them); what fails a
/// push is getting *worse* than the recorded baseline. The baseline is
/// committed and updated deliberately from a trusted run's report so every
/// raise is a reviewed diff; until one is recorded for this OS the gate
/// passes with a notice saying exactly that.
fn run_bench() -> bool {
    if !build_perf_release() {
        return false;
    }
    let latency_report = "target/perf/bench-latency.json";
    let baseline = format!(
        "tools/perf-probe/baselines/bench-latency-{}-{}.json",
        std::env::consts::OS,
        std::env::consts::ARCH,
    );
    let mut passed = cargo_owned(
        "perf-probe (latency benchmark pair)",
        &perf_probe_args(&["bench-latency", "--out", latency_report]),
    );
    passed &= cargo_owned(
        "perf-probe (latency regression gate)",
        &perf_probe_args(&[
            "compare",
            "--baseline",
            &baseline,
            "--current",
            latency_report,
        ]),
    );
    passed &= cargo_owned(
        "perf-probe (single-session throughput)",
        &perf_probe_args(&[
            "bench-throughput",
            "--lines",
            "300000",
            "--out",
            "target/perf/bench-throughput-1.json",
        ]),
    );
    passed &= cargo_owned(
        "perf-probe (concurrent throughput)",
        &perf_probe_args(&[
            "bench-throughput",
            "--lines",
            "150000",
            "--sessions",
            "4",
            "--out",
            "target/perf/bench-throughput-4.json",
        ]),
    );
    passed
}

/// The nightly endurance lanes: the two half-hour soaks — synthetic and
/// bimodal-recorded — with the resource monitor over both, then the
/// benchmark set for the nightly record. The bimodal lane replays the
/// recordings at full fidelity (no idle compression: the lane's length is
/// fixed by its duration, so shortening idle would buy nothing and cost the
/// realism the lane exists for). On terminals that pipe rather than
/// re-render, a byte-for-byte recorded-content pass runs as well.
fn run_soak_nightly() -> bool {
    if !build_perf_release() {
        return false;
    }
    let mut passed = cargo_owned(
        "perf-probe (30-minute synthetic soak)",
        &perf_probe_args(&[
            "soak",
            "--minutes",
            "30",
            "--monitor-out",
            "target/perf/soak-monitor.ndjson",
            "--out",
            "target/perf/soak.json",
        ]),
    );

    let mut replay_args = vec!["replay", "--minutes", "30"];
    for fixture in BIMODAL_FIXTURES {
        replay_args.extend(["--fixture", fixture]);
    }
    replay_args.extend([
        "--monitor-out",
        "target/perf/replay-monitor.ndjson",
        "--out",
        "target/perf/replay-generated.json",
    ]);
    passed &= cargo_owned(
        "perf-probe (30-minute bimodal replay soak)",
        &perf_probe_args(&replay_args),
    );

    if !cfg!(windows) {
        let mut recorded_args = vec!["replay", "--minutes", "5", "--content", "recorded"];
        for fixture in BIMODAL_FIXTURES {
            recorded_args.extend(["--fixture", fixture]);
        }
        recorded_args.extend(["--out", "target/perf/replay-recorded.json"]);
        passed &= cargo_owned(
            "perf-probe (byte-for-byte recorded replay)",
            &perf_probe_args(&recorded_args),
        );
    }

    passed &= cargo_owned(
        "perf-probe (nightly latency benchmark)",
        &perf_probe_args(&["bench-latency", "--out", "target/perf/nightly-latency.json"]),
    );
    // The same latency pair while a bimodal replay streams in a second
    // session: the per-workload half of the latency verdict. A budget met
    // only on an otherwise-idle path would be a promise about a runtime
    // that never hosts more than one quiet session.
    let mut loaded_latency = vec!["bench-latency"];
    for fixture in BIMODAL_FIXTURES {
        loaded_latency.extend(["--load", fixture]);
    }
    loaded_latency.extend(["--out", "target/perf/nightly-latency-loaded.json"]);
    passed &= cargo_owned(
        "perf-probe (latency under bimodal load)",
        &perf_probe_args(&loaded_latency),
    );
    // The aggregate-versus-per-session curve: one point per concurrency
    // level, each a full verified run.
    for sessions in ["1", "2", "4", "8"] {
        let out = format!("target/perf/nightly-throughput-{sessions}.json");
        passed &= cargo_owned(
            &format!("perf-probe (throughput at {sessions} session(s))"),
            &perf_probe_args(&[
                "bench-throughput",
                "--lines",
                "500000",
                "--sessions",
                sessions,
                "--out",
                &out,
            ]),
        );
    }
    // One curve point re-measured under the bimodal load, so the capacity
    // numbers also exist for the workload shape the runtime actually hosts.
    let mut loaded_throughput = vec!["bench-throughput", "--lines", "300000", "--sessions", "4"];
    for fixture in BIMODAL_FIXTURES {
        loaded_throughput.extend(["--load", fixture]);
    }
    loaded_throughput.extend(["--out", "target/perf/nightly-throughput-4-loaded.json"]);
    passed &= cargo_owned(
        "perf-probe (throughput under bimodal load)",
        &perf_probe_args(&loaded_throughput),
    );
    passed
}

/// Run every step and the drift gate, reporting all failures rather than
/// stopping at the first, so one run surfaces every problem.
fn run_ci() -> bool {
    let checks = run_steps(STEPS);
    let probes = run_steps(PROBE_STEPS);
    // Run the gates regardless of earlier failures so one run reports
    // everything.
    let layout = workspace_gate();
    let drift = drift_gate();
    checks && probes && layout && drift
}

/// The live-CLI probes. Credential presence is logged — never the value —
/// and its absence fails loudly here rather than surfacing as a confusing
/// authentication error inside the spawned CLI.
fn run_live_probes() -> bool {
    let credential = ["ANTHROPIC_API_KEY", "CLAUDE_CONFIG_DIR"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    match credential {
        Some(name) => eprintln!("── xtask: live-probe: credential present ({name} is set) ──"),
        None => {
            eprintln!(
                "xtask: live-probe: credential absent — set ANTHROPIC_API_KEY (CI) or \
                 CLAUDE_CONFIG_DIR (local). The live lane cannot run without one."
            );
            return false;
        }
    }
    // The stand-in fixture's build step is not needed here, but the real-CLI
    // probe binary is: `cargo run` builds it.
    let steps = run_steps(LIVE_PROBE_STEPS);
    // Run the capture smoke even if a probe failed: one live run should
    // report everything it can, and the smoke's one extra session is the
    // cheapest part of the lane.
    let smoke = live_capture_smoke();
    steps && smoke
}

/// One scripted capture session against the real CLI — the path the capture
/// campaign depends on, exercised whenever the live tier runs so it cannot
/// rot between capture sittings. Cheapest cell of the campaign matrix
/// (token streaming, default size, haiku-class model), recorded into
/// `target/` where nothing will mistake it for a committed fixture.
fn live_capture_smoke() -> bool {
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        eprintln!("xtask: live-probe: `git rev-parse --show-toplevel` failed");
        return false;
    };
    let root = PathBuf::from(top.trim_end());
    let script = root.join("tests/capture-scenarios/claude/token-streaming.record.json");
    let out = root.join("target/live-capture-smoke");
    let args: Vec<String> = [
        "run",
        "--quiet",
        "--package",
        "agent-bridge-interactive-probe",
        "--bin",
        "interactive-probe",
        "--",
        "record",
        "--model",
        "haiku",
        "--expect-cli",
        "claude",
    ]
    .map(str::to_string)
    .into_iter()
    .chain([
        "--script".to_string(),
        script.to_string_lossy().into_owned(),
        "--out".to_string(),
        out.to_string_lossy().into_owned(),
    ])
    .collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    cargo("interactive-probe (scripted capture smoke)", &refs)
}

/// The two terminal sizes every capture scenario is recorded at: the
/// runtime's default, and a larger size that makes a TUI paint (wrap,
/// reflow, redraw) differently. Classification must hold at both.
const CAPTURE_DIMS: [(u16, u16); 2] = [(80, 24), (120, 40)];

/// The per-adapter corpus budget, in bytes. The campaign reports against it
/// after every run so an over-budget corpus is discovered at capture time —
/// when trimming is a re-record away — not at review time.
///
/// Raised from the original 1 MiB once the first full claude sitting
/// measured reality: 3 pinned versions × 2 terminal sizes × 9 scenarios is
/// ~2.96 MiB, and the raw PTY byte streams alone — the irreducible replay
/// core; everything else in a fixture derives from them — are already more
/// than 1 MiB across three versions, so 1 MiB was infeasible for the corpus
/// the campaign records, not a trim target. Full fidelity is kept
/// deliberately: the transcripts' setup-noise records (the largest trimmable
/// chunk) are exactly what the side-channel tailer must learn to skip, so
/// dropping them would flatter the detection metrics they exist to test.
/// Set to 3.5 MiB — comfortably above the measured corpus so a re-record's
/// ordinary size drift does not trip a false over-budget.
const CORPUS_BUDGET_BYTES: u64 = 3_670_016;

/// One capture sitting: every `tests/capture-scenarios/<cli>/*.record.json`
/// scenario, at both capture sizes, recorded through the interactive
/// probe's `record` lane into `tests/corpus/<cli>/<version-label>/`. Stops
/// at the first failed scenario — for the claude CLI every session costs
/// quota, and a systematically broken setup must not burn the rest of the
/// matrix discovering itself. After recording, the fixtures are scrubbed
/// (the local username is masked, same-length so the timing offsets stay
/// valid; a credential hit aborts) and the whole adapter corpus is sized
/// against its budget.
fn run_capture_campaign(args: &[String]) -> bool {
    let Some(campaign) = CampaignArgs::parse(args) else {
        eprintln!(
            "usage: cargo xtask capture-campaign --cli <name> --bin <path> \
             --version-label <label> --install <text> [--model <name>] \
             [--mask <text>]... [--only <scenario>] [--dry-run]"
        );
        return false;
    };
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        eprintln!("xtask: capture-campaign: `git rev-parse --show-toplevel` failed");
        return false;
    };
    let root = PathBuf::from(top.trim_end());

    let scenarios_dir = root.join("tests/capture-scenarios").join(&campaign.cli);
    let mut scripts: Vec<PathBuf> = match std::fs::read_dir(&scenarios_dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                // Real regular files only — `is_file` would follow a
                // symlink, and the campaign's inputs stay confined to the
                // scenario tree the same way its scrub walk does. A
                // directory or any symlink named *.record.json would
                // otherwise be handed to the record lane as a script and
                // fail there, confusingly.
                std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".record.json"))
            })
            .collect(),
        Err(err) => {
            eprintln!(
                "xtask: capture-campaign: no scenario directory at {} ({err}) — write the \
                 <name>.record.json scripts before running the campaign",
                scenarios_dir.display()
            );
            return false;
        }
    };
    scripts.sort();
    // `--only <name>` records a single scenario into an existing version
    // directory — for adding a scenario to a corpus already captured, without
    // re-recording (and re-spending session quota on) the rest of the matrix.
    // The whole version directory is still scrubbed and sized afterwards, so
    // the addition lands held to the same guarantees as a full sitting.
    if let Some(only) = &campaign.only {
        scripts.retain(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".record.json"))
                == Some(only.as_str())
        });
        if scripts.is_empty() {
            eprintln!(
                "xtask: capture-campaign: --only {only} matched no scenario in {} \
                 (expected {only}.record.json)",
                scenarios_dir.display()
            );
            return false;
        }
    }
    if scripts.is_empty() {
        eprintln!(
            "xtask: capture-campaign: {} holds no *.record.json scenarios",
            scenarios_dir.display()
        );
        return false;
    }

    let version_dir = root
        .join("tests/corpus")
        .join(&campaign.cli)
        .join(&campaign.version_label);
    eprintln!(
        "── xtask: capture-campaign: {} scenarios × {} sizes for cli={} version={} into {} ──",
        scripts.len(),
        CAPTURE_DIMS.len(),
        campaign.cli,
        campaign.version_label,
        version_dir.display(),
    );
    for script in &scripts {
        for (cols, rows) in CAPTURE_DIMS {
            eprintln!(
                "  {} @ {cols}x{rows} -> {}",
                script.file_name().unwrap_or_default().to_string_lossy(),
                version_dir
                    .join(fixture_dir_name(script, cols, rows))
                    .display(),
            );
        }
    }
    if campaign.dry_run {
        eprintln!("capture-campaign: dry run — nothing recorded.");
        return true;
    }

    for script in &scripts {
        for (cols, rows) in CAPTURE_DIMS {
            let out = version_dir.join(fixture_dir_name(script, cols, rows));
            let mut record_args: Vec<String> = [
                "run",
                "--quiet",
                "--package",
                "agent-bridge-interactive-probe",
                "--bin",
                "interactive-probe",
                "--",
                "record",
            ]
            .map(str::to_string)
            .to_vec();
            record_args.extend([
                "--script".to_string(),
                script.to_string_lossy().into_owned(),
                "--out".to_string(),
                out.to_string_lossy().into_owned(),
                "--cols".to_string(),
                cols.to_string(),
                "--rows".to_string(),
                rows.to_string(),
                "--cli-bin".to_string(),
                campaign.bin.clone(),
            ]);
            record_args.extend(["--install".to_string(), campaign.install.clone()]);
            // A script misfiled under this CLI's scenario directory fails
            // inside the record lane with both names stated.
            record_args.extend(["--expect-cli".to_string(), campaign.cli.clone()]);
            // The claude profile reports its own --version; every other CLI
            // gets the campaign's label stamped into its manifests.
            if campaign.cli != "claude" {
                record_args.extend(["--cli-version".to_string(), campaign.version_label.clone()]);
            }
            if let Some(model) = &campaign.model {
                record_args.extend(["--model".to_string(), model.clone()]);
            }
            let name = format!(
                "record {} @ {cols}x{rows}",
                script.file_name().unwrap_or_default().to_string_lossy()
            );
            let refs: Vec<&str> = record_args.iter().map(String::as_str).collect();
            if !cargo(&name, &refs) {
                eprintln!(
                    "capture-campaign: stopping at the first failure — the remaining matrix \
                     would spend sessions against the same broken setup"
                );
                return false;
            }
        }
    }

    let mut masks = campaign.masks.clone();
    for needle in derived_identity_needles() {
        if !masks.contains(&needle) {
            // Report the source and length, never the needle bytes: the
            // needle *is* the identity being scrubbed, and campaign logs get
            // pasted into issues/PRs — printing it verbatim would re-expose
            // exactly what the corpus masks.
            eprintln!(
                "capture-campaign: auto-mask needle derived from the environment (git \
                 identity or temp-dir hash), {} bytes",
                needle.len()
            );
            masks.push(needle);
        }
    }
    if !scrub_fixtures(&version_dir, &masks) {
        return false;
    }
    report_corpus_budget(&root.join("tests/corpus").join(&campaign.cli))
}

struct CampaignArgs {
    cli: String,
    bin: String,
    version_label: String,
    /// How the pinned binary was obtained (e.g. `npm
    /// @anthropic-ai/claude-code@2.1.201`). Required: the version-drift
    /// measurement needs "which release, obtained how" in every manifest,
    /// and the campaign is the one place that knows it.
    install: String,
    model: Option<String>,
    /// Extra scrub needles beyond the username, masked the same way (a
    /// same-length `x`-run, raw bytes). The claude TUI paints two account
    /// settings the campaign cannot derive on its own, so the invoker names
    /// them: the logged-in account email (its local part) and the display
    /// name in the "Welcome back <name>!" splash. The post-scrub email and
    /// greeting sweeps abort — and remove the offending fixture — if either
    /// needle was forgotten.
    masks: Vec<Vec<u8>>,
    /// Record only this one scenario (by `<name>.record.json` stem) into the
    /// version directory, instead of the whole scenario set. For extending an
    /// already-captured corpus without re-recording the rest.
    only: Option<String>,
    dry_run: bool,
}

impl CampaignArgs {
    fn parse(args: &[String]) -> Option<Self> {
        let mut cli = None;
        let mut bin = None;
        let mut version_label = None;
        let mut install = None;
        let mut model = None;
        let mut masks: Vec<Vec<u8>> = Vec::new();
        let mut only = None;
        let mut dry_run = false;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--cli" => cli = Some(iter.next()?.clone()),
                "--bin" => bin = Some(iter.next()?.clone()),
                "--version-label" => version_label = Some(iter.next()?.clone()),
                "--install" => install = Some(iter.next()?.clone()),
                "--model" => model = Some(iter.next()?.clone()),
                "--only" => only = Some(iter.next()?.clone()),
                "--mask" => {
                    let needle = iter.next()?.clone().into_bytes();
                    // Same safety rule as the username needle: a mask is a
                    // raw-byte replacement across every artifact, and a
                    // short or digits-and-punctuation needle would also hit
                    // numeric NDJSON fields, version strings, and prose.
                    if !safe_mask_needle(&needle) {
                        eprintln!(
                            "xtask: capture-campaign: --mask needs at least 3 bytes and a \
                             letter (or non-ASCII); refusing an unsafe needle"
                        );
                        return None;
                    }
                    masks.push(needle);
                }
                "--dry-run" => dry_run = true,
                other => {
                    eprintln!("xtask: capture-campaign: unknown option {other}");
                    return None;
                }
            }
        }
        let (cli, bin, version_label, install) = (cli?, bin?, version_label?, install?);
        // Both become path components under tests/corpus/ — hold them to
        // characters that stay put in directory names on every OS, and
        // anchor the first and last character to an alphanumeric so `.`,
        // `..`, and leading/trailing separators cannot slip through as a
        // traversal or an awkward dot-directory. `--cli` is stricter: it
        // must match a record script's kebab-case `cli` field, where a dot
        // is a parse error — accepting one here would only defer the
        // mismatch to every scenario in the sitting.
        for (name, value, charset, dot_ok) in [
            ("--cli", &cli, "[a-z0-9-]", false),
            ("--version-label", &version_label, "[a-z0-9.-]", true),
        ] {
            let body_clean = value.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || (dot_ok && c == '.')
            });
            let anchored = value
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
                && value
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_ascii_alphanumeric());
            if !(body_clean && anchored) {
                eprintln!(
                    "xtask: capture-campaign: {name} must be {charset} and start/end alphanumeric, got \"{value}\""
                );
                return None;
            }
        }
        Some(Self {
            cli,
            bin,
            version_label,
            install,
            model,
            masks,
            only,
            dry_run,
        })
    }
}

/// `token-streaming.record.json` at 80×24 → `token-streaming-80x24`.
fn fixture_dir_name(script: &Path, cols: u16, rows: u16) -> String {
    let stem = script
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".record.json"))
        .unwrap_or("scenario");
    format!("{stem}-{cols}x{rows}")
}

/// Scrub committed fixtures of machine-local identity. The local username
/// (HOME's last component) and every `--mask` needle are masked with a
/// same-length run of `x`, so byte offsets recorded in the timing sidecars
/// stay valid; a fixture that contains the ANTHROPIC_API_KEY or
/// OPENAI_API_KEY value is a leak and aborts the campaign outright —
/// masking a credential and committing anyway would hide the evidence that
/// the capture setup is wrong. After masking, any email-shaped byte-run still present aborts
/// too: the claude TUI paints the logged-in account email into the byte
/// stream, so a surviving address means a `--mask` was forgotten (or the
/// scenario elicited an address), and only a human can tell which.
fn scrub_fixtures(dir: &Path, extra_masks: &[Vec<u8>]) -> bool {
    eprintln!("── xtask: capture-campaign: scrub {} ──", dir.display());
    // A maskable username needs some length and at least one letter: the
    // mask is a raw byte replacement across every artifact, and an
    // all-digit name (say "123") would also match inside the numeric
    // fields of the NDJSON sidecars and corrupt them.
    let username =
        maskable_username(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")));
    if username.is_none() {
        eprintln!(
            "capture-campaign: no safely maskable username (HOME unset, too short, or only \
             digits and punctuation — such a needle would also hit numeric NDJSON fields and \
             ordinary prose); skipping the mask, review the fixtures by hand"
        );
    }
    // One guarded credential per CLI the sittings record. Codex authenticates
    // through CODEX_HOME's auth.json, whose tokens never reach the PTY, but
    // an API key exported in the campaign's shell is the same leak class the
    // claude guard exists for — if either value lands in a fixture, the
    // setup is wrong, and masking it would hide exactly that.
    let api_keys: Vec<(&str, String)> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|key| key.len() >= 8)
                .map(|key| (name, key))
        })
        .collect();

    let mut files = Vec::new();
    if let Err(err) = collect_files(dir, &mut files) {
        eprintln!(
            "capture-campaign: ABORT — the scrub could not walk the fixture tree, so it cannot \
             claim the fixtures are clean: {err}"
        );
        return false;
    }
    // Deterministic order, so which file aborts a run (a credential hit)
    // and the order of diagnostics do not vary by platform or filesystem.
    files.sort();
    // Pass 1 — credential guard, then mask and write each file. A credential
    // hit is a hard abort; the recorded file on disk holds the key, so it is
    // removed before returning, the same reason the sweeps in pass 2 remove
    // what they flag.
    let mut masked_files = 0usize;
    for path in &files {
        let Ok(mut bytes) = std::fs::read(path) else {
            eprintln!("capture-campaign: unreadable fixture {}", path.display());
            return false;
        };
        if let Some((name, _)) = api_keys
            .iter()
            .find(|(_, key)| find_subsequence(&bytes, key.as_bytes()).is_some())
        {
            remove_leaking_fixture(path);
            eprintln!(
                "capture-campaign: ABORT — {} contained the {name} value (removed). \
                 The capture setup leaked a credential into a fixture; unset the key, then \
                 re-record.",
                path.display()
            );
            return false;
        }
        let mut changed = false;
        for needle in username
            .as_deref()
            .into_iter()
            .chain(extra_masks.iter().map(Vec::as_slice))
        {
            let mut from = 0usize;
            while let Some(at) = find_subsequence(&bytes[from..], needle) {
                let at = from + at;
                bytes[at..at + needle.len()].fill(MASK_BYTE);
                from = at + needle.len();
                changed = true;
            }
        }
        // The account address is painted split across cursor-move escapes, so
        // once the `--mask` needle has masked the identifying local part, a
        // raw needle cannot reach the `@domain` that trails it — the escape
        // sits between the characters. Mask it here from the control-stripped
        // view instead, so no email-shaped string survives regardless of how
        // the repaint fragmented it.
        changed |= mask_email_domains(&mut bytes);
        if changed {
            if std::fs::write(path, &bytes).is_err() {
                eprintln!("capture-campaign: rewriting {} failed", path.display());
                return false;
            }
            masked_files += 1;
        }
    }
    // Pass 2 — sweep every masked file for identity a needle missed, and
    // remove each offender *before* returning, so an aborted run never leaves
    // a still-leaking fixture on disk for an accidental commit (a removed
    // file also makes its fixture directory trace_check-invalid, a second
    // guard). Every leaker is reported, not just the first, so one re-run
    // surfaces all the missing needles at once.
    let mut leaked = false;
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("capture-campaign: unreadable fixture {}", path.display());
            return false;
        };
        // Report the shape and length of what survived, never its bytes: the
        // survivor is the identity itself, and this diagnostic can be pasted
        // into an issue/PR — the maintainer knows their own account email and
        // display name, so the category plus a length is enough to act on
        // without the log re-exposing what the corpus is scrubbing.
        if let Some(found) = surviving_email(&bytes) {
            remove_leaking_fixture(path);
            eprintln!(
                "capture-campaign: {} still held an email-shaped run ({} bytes) after masking \
                 (removed). Mask the account email's local part with --mask <local-part>; \
                 the @domain a name-mask leaves behind is cleared automatically.",
                path.display(),
                found.len(),
            );
            leaked = true;
        } else if let Some(found) = surviving_greeting(&bytes) {
            remove_leaking_fixture(path);
            eprintln!(
                "capture-campaign: {} still showed the account display name ({} bytes) in the \
                 splash greeting after masking (removed). Mask it with --mask <display-name>.",
                path.display(),
                found.len(),
            );
            leaked = true;
        }
    }
    if leaked {
        eprintln!(
            "capture-campaign: ABORT — removed the leaking fixture(s) above; add the missing \
             --mask needle(s) and re-run."
        );
        return false;
    }
    eprintln!(
        "capture-campaign: scrubbed {} files ({masked_files} carried a needle and were masked).",
        files.len()
    );
    true
}

/// Remove a fixture a scrub check flagged as still leaking, so an aborted
/// run cannot leave it on disk for an accidental commit. Best-effort: a
/// failure to remove is reported (so a human deletes it by hand) but does
/// not change the abort the caller is already returning.
fn remove_leaking_fixture(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        eprintln!(
            "capture-campaign: could not remove leaking {} ({err}) — delete it by hand \
             before committing",
            path.display()
        );
    }
}

/// The first email-shaped byte-run in `haystack`, or `None`. Hand-rolled
/// (this crate is deliberately dependency-free) and deliberately narrow: an
/// `@` with a `[A-Za-z0-9._%+-]` local part on its left and a dotted domain
/// whose final label is alphabetic (≥ 2 chars) on its right. The narrowness
/// is what keeps the sweep quiet on the bytes fixtures actually carry —
/// npm scopes (`@anthropic-ai/...`, no dotted domain), versioned installs
/// (`...@2.1.201`, numeric final label), and the `@` that terminates an
/// ANSI insert-character sequence (no local part or no dotted domain) all
/// pass — while anything a human would read as an address aborts.
///
/// The local-part walk may over-extend left into adjacent characters, so
/// the returned range *contains* an address, possibly with a run-on prefix.
/// The sweep only names what it aborts on; a wider range is more context,
/// not a wrong answer.
///
/// The `@` is sought at or after `from` (so a caller can resume past a match
/// it chose to ignore), but the local part still walks freely left of it.
fn find_email(haystack: &[u8], from: usize) -> Option<std::ops::Range<usize>> {
    for i in from..haystack.len() {
        if haystack[i] != b'@' {
            continue;
        }
        let is_local = |b: u8| b.is_ascii_alphanumeric() || b"._%+-".contains(&b);
        let mut start = i;
        while start > 0 && is_local(haystack[start - 1]) {
            start -= 1;
        }
        if start == i {
            continue;
        }
        let is_domain = |b: u8| b.is_ascii_alphanumeric() || b == b'.' || b == b'-';
        let mut end = i + 1;
        while end < haystack.len() && is_domain(haystack[end]) {
            end += 1;
        }
        // An address at the end of a sentence drags the full stop into the
        // domain walk; trimmed here so `user@mail.com.` still aborts the
        // campaign instead of slipping past on an empty final label.
        while end > i + 1 && matches!(haystack[end - 1], b'.' | b'-') {
            end -= 1;
        }
        let domain = &haystack[i + 1..end];
        let mut labels = domain.split(|&b| b == b'.');
        let Some(last) = labels.next_back() else {
            continue;
        };
        let tld_like = last.len() >= 2 && last.iter().all(u8::is_ascii_alphabetic);
        let rest_nonempty = {
            let mut any = false;
            let mut all_full = true;
            for label in labels {
                any = true;
                all_full &= !label.is_empty();
            }
            any && all_full
        };
        if tld_like && rest_nonempty {
            return Some(start..end);
        }
    }
    None
}

/// The byte a mask leaves in place of a needle. A local part carrying a run
/// of this byte is an already-scrubbed remnant, not identity, so the email
/// sweep skips it (see `surviving_email`).
const MASK_BYTE: u8 = b'x';

/// How many consecutive `MASK_BYTE`s at the end of a local part mark it as a
/// scrubbed remnant. It equals `safe_mask_needle`'s minimum length, so every
/// needle the campaign is allowed to mask leaves a recognizable run.
const MASK_RUN_LEN: usize = 3;

/// Is this address's local part already scrubbed — i.e. is what remains just
/// the `@domain` trailing a masked needle, rather than surviving identity?
///
/// The run must sit at the *end* of the local part, immediately before the
/// `@`. That is where masking a needle always leaves it: the needle covers
/// the identifying text right up to the `@`, so `<needle>@domain` becomes
/// `xxxx@domain`, and stripping terminal framing can only glue extra
/// characters onto the *front* (`fre` + `xxxx@…`). Testing for a run
/// *anywhere* in the local part would instead whitelist a genuine unmasked
/// address that merely happens to contain `xxx` (`xxxuser@example.com`) —
/// a false negative in a leak check, the one direction that must not fail.
fn local_part_is_scrubbed(local: &[u8]) -> bool {
    local
        .iter()
        .rev()
        .take_while(|&&byte| byte == MASK_BYTE)
        .count()
        >= MASK_RUN_LEN
}

/// An email-shaped run that survived masking, rendered as text for the
/// diagnostic, or `None` if the fixture is clean.
///
/// The scan runs on a *control-stripped* view of the bytes, because the
/// claude TUI paints the account footer through differential repaints: an
/// address can be interrupted mid-domain by a cursor-move escape
/// (`...@gma\x1b[28Gil.com`) that a raw contiguous scan reads straight past
/// — which is exactly how the account name leaked through the first sitting
/// while a raw sweep reported clean. Stripping the framing first makes the
/// split address contiguous again.
///
/// A match whose local part carries a `MASK_RUN_LEN` run of `MASK_BYTE` is
/// the `@domain` a name-mask leaves behind, not identity, so it is skipped
/// and the scan resumes past it — a real address elsewhere in the same file
/// still aborts. The run is tested rather than "all mask bytes" because
/// stripping the framing can glue a neighbouring footer word onto the front
/// of the masked local part (`fre` + `xxxxxxxxxxxxxxxx@…`), and that prefix
/// must not turn a scrubbed remnant back into an abort.
fn surviving_email(bytes: &[u8]) -> Option<String> {
    let rendered = strip_terminal_framing(bytes);
    let mut cursor = 0;
    while let Some(range) = find_email(&rendered, cursor) {
        let at = range.start
            + rendered[range.clone()]
                .iter()
                .position(|&b| b == b'@')
                .expect("find_email only returns ranges containing an @");
        if local_part_is_scrubbed(&rendered[range.start..at]) {
            cursor = range.end;
            continue;
        }
        return Some(String::from_utf8_lossy(&rendered[range]).into_owned());
    }
    None
}

/// The account display name still showing in the splash greeting after
/// masking, or `None`. The claude TUI paints `Welcome back <name>!` on the
/// startup splash, where `<name>` is the account's chosen display name — an
/// account setting the campaign cannot derive (unlike the username or the
/// git-owner), so it only leaves the corpus if the maintainer passes it as a
/// `--mask` needle. This sweep is the backstop that makes forgetting it a
/// loud abort rather than a silent leak, the same role the email sweep plays
/// for the account address; it was added after a display name slipped past a
/// purely token-based verification (nothing searched for a token nobody knew
/// to look for). The scan runs on the control-stripped view because the
/// greeting, like the footer, can be repaint-split; a name that is a run of
/// `MASK_BYTE`s is already scrubbed and ignored.
fn surviving_greeting(bytes: &[u8]) -> Option<String> {
    const MARKER: &[u8] = b"Welcome back ";
    let rendered = strip_terminal_framing(bytes);
    let mut from = 0;
    while let Some(rel) = find_subsequence(&rendered[from..], MARKER) {
        let name_start = from + rel + MARKER.len();
        // The greeting ends at `!`; the name is what precedes it. Bound the
        // scan so a missing `!` (truncated paint) cannot run to end of file.
        let tail = &rendered[name_start..];
        let name_end = tail
            .iter()
            .take(64)
            .position(|&b| b == b'!')
            .unwrap_or(tail.len().min(64));
        let name = &tail[..name_end];
        let all_masked = !name.is_empty() && name.iter().all(|&b| b == MASK_BYTE);
        if !name.is_empty() && !all_masked {
            return Some(String::from_utf8_lossy(name).into_owned());
        }
        from = name_start;
    }
    None
}

/// A copy of `bytes` with terminal control framing removed: an ESC-led
/// sequence (CSI `\x1b[`…final, OSC `\x1b]`…BEL/ST, or any other two-byte
/// `\x1b X`) and every C0 control or DEL is dropped; printable bytes,
/// including UTF-8 continuation bytes, pass through. This is a detection aid
/// only — masking still edits the original bytes in place so the recorded
/// timing offsets stay valid. Dropping newlines can concatenate unrelated
/// text, which can only ever make the sweep *more* eager to abort; erring
/// toward a spurious abort (a maintainer looks) beats erring toward a silent
/// leak (identity ships).
fn strip_terminal_framing(bytes: &[u8]) -> Vec<u8> {
    render_indexed(bytes).0
}

/// `strip_terminal_framing`, but also returning, for each kept byte, its
/// index in the original `bytes`. The map is what lets a masker edit the
/// original bytes behind an on-screen run it found in the stripped view (the
/// account address, split across cursor-move escapes), masking the printable
/// characters while leaving the escapes — and the total length — untouched.
fn render_indexed(bytes: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut out = Vec::with_capacity(bytes.len());
    let mut src = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            i += 1;
            let Some(&intro) = bytes.get(i) else { break };
            match intro {
                b'[' => {
                    // CSI: parameter/intermediate bytes, then a final in @-~.
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += usize::from(i < bytes.len()); // consume the final byte
                }
                b']' => {
                    // OSC: runs until BEL, or ST (`\x1b\\`).
                    i += 1;
                    while i < bytes.len() && bytes[i] != 0x07 {
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    i += usize::from(bytes.get(i) == Some(&0x07)); // consume BEL
                }
                // Any other `\x1b X` (a two-byte escape) drops both bytes.
                _ => i += 1,
            }
            continue;
        }
        if b >= 0x20 && b != 0x7f {
            out.push(b);
            src.push(i);
        }
        i += 1;
    }
    (out, src)
}

/// Mask the `@domain` of every already-scrubbed email address in `bytes`, in
/// place, returning whether anything changed. Finds each address in the
/// control-stripped view and masks the on-screen `@` and domain characters
/// through the source-index map, so the escapes a repaint left between them
/// (and the total byte length, which the timing sidecars index) are
/// preserved. Only addresses whose local part is already a `MASK_BYTE` run
/// are touched: the identity is the local part, masked by a `--mask` needle
/// first, and an address whose local part is *not* masked is left intact for
/// the email sweep to abort on — so a forgotten needle still fails loudly.
fn mask_email_domains(bytes: &mut [u8]) -> bool {
    let (rendered, src) = render_indexed(bytes);
    let mut changed = false;
    let mut cursor = 0;
    while let Some(range) = find_email(&rendered, cursor) {
        let at = range.start
            + rendered[range.clone()]
                .iter()
                .position(|&b| b == b'@')
                .expect("find_email only returns ranges containing an @");
        if local_part_is_scrubbed(&rendered[range.start..at]) {
            for j in at..range.end {
                if bytes[src[j]] != MASK_BYTE {
                    bytes[src[j]] = MASK_BYTE;
                    changed = true;
                }
            }
        }
        cursor = range.end;
    }
    changed
}

/// Is this needle safe to mask by raw-byte replacement? Shared rule for the
/// username and every `--mask`: long enough to be distinctive, and carrying
/// a letter (or any non-ASCII byte — a name in a non-Latin script), so it
/// cannot collide with the numeric NDJSON fields and version strings that
/// digits-and-punctuation needles would corrupt.
fn safe_mask_needle(needle: &[u8]) -> bool {
    let name_like = needle
        .iter()
        .any(|byte| byte.is_ascii_alphabetic() || !byte.is_ascii());
    needle.len() >= 3 && name_like
}

/// Any single fixture file above this is flagged for a truncation review —
/// the per-file complement to the per-adapter total.
const FIXTURE_REVIEW_BYTES: u64 = 100 * 1024;

/// Size the whole per-adapter corpus against its budget, and name every
/// file large enough to need a truncation review. Over budget is a loud
/// warning, not a hard failure: the maintainer trimming scenarios needs the
/// fixtures on disk to decide what to cut.
fn report_corpus_budget(adapter_dir: &Path) -> bool {
    let mut files = Vec::new();
    if let Err(err) = collect_files(adapter_dir, &mut files) {
        eprintln!(
            "capture-campaign: the corpus could not be walked, so the budget report would \
             under-count: {err}"
        );
        return false;
    }
    // Deterministic order, so the large-file warnings read the same on
    // every platform and diff cleanly between sittings.
    files.sort();
    let mut total = 0u64;
    for path in &files {
        // `collect_files` only returns real regular files, so this cannot
        // follow a link — symlink_metadata keeps the walk's stance uniform.
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            continue;
        };
        total += meta.len();
        if meta.len() > FIXTURE_REVIEW_BYTES {
            eprintln!(
                "capture-campaign: {} is {} bytes (> {FIXTURE_REVIEW_BYTES}) — review for truncation",
                path.display(),
                meta.len(),
            );
        }
    }
    let verdict = if total <= CORPUS_BUDGET_BYTES {
        "within budget"
    } else {
        "OVER BUDGET — trim before committing"
    };
    eprintln!(
        "── xtask: capture-campaign: {} holds {total} bytes across {} files ({verdict}, budget {CORPUS_BUDGET_BYTES}) ──",
        adapter_dir.display(),
        files.len(),
    );
    true
}

/// Regular files under `dir`, recursively — never through a symlink. The
/// scrub rewrites what this returns in place and the budget sums its
/// sizes; following a symlink would let either walk out of the fixture
/// tree it is supposed to be confined to. Anything the walk cannot soundly
/// cover is an error, not a skip: an unreadable entry, a symlink (whose
/// target text can itself carry a machine-local path), or a special file
/// silently left out would let the scrub claim fixtures are clean without
/// having covered them, and the budget under-count what is committed.
fn collect_files(dir: &Path, into: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|err| format!("listing {} failed: {err}", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("reading an entry of {} failed: {err}", dir.display()))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|err| format!("inspecting {} failed: {err}", path.display()))?;
        if meta.is_dir() {
            collect_files(&path, into)?;
        } else if meta.is_file() {
            into.push(path);
        } else {
            return Err(format!(
                "{} is neither a real directory nor a real regular file (a symlink or special \
                 entry) — the walk refuses entries it cannot soundly cover",
                path.display()
            ));
        }
    }
    Ok(())
}

/// The username worth masking out of committed fixtures: the home path's
/// final component, when long and lettered enough to be a safe raw-byte
/// needle. Trailing separators on the home path are irrelevant — path
/// parsing skips empty components on every platform — and a home that is
/// just a root has no component at all, which the caller turns into a loud
/// warning rather than a silent skip.
///
/// The needle is the component's *bytes*, not a String: on Unix a home path
/// need not be UTF-8, and a needle lossily converted through replacement
/// characters could never match the raw bytes a fixture actually carries —
/// it would claim a mask it did not perform. Windows environment values are
/// Unicode in practice; the lossy fallback there is exact for real inputs.
///
/// "Safe" means the needle contains something name-like: an ASCII letter,
/// or any non-ASCII byte — which is what a username in any non-Latin
/// script is made of, so those mask too. Needles of only ASCII digits and
/// punctuation are refused: they collide with numeric NDJSON fields,
/// version strings, and ordinary prose.
fn maskable_username(home: Option<std::ffi::OsString>) -> Option<Vec<u8>> {
    let name = PathBuf::from(home?).file_name()?.to_os_string();
    #[cfg(unix)]
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(name.as_os_str()).to_vec();
    #[cfg(not(unix))]
    let bytes = name.to_string_lossy().into_owned().into_bytes();
    safe_mask_needle(&bytes).then_some(bytes)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn run_steps(steps: &[(&str, &[&str])]) -> bool {
    let mut passed = true;
    for &(name, args) in steps {
        if !cargo(name, args) {
            passed = false;
        }
    }
    passed
}

/// Run one `cargo` subcommand, streaming its output; returns whether it passed.
fn cargo(name: &str, args: &[&str]) -> bool {
    eprintln!("── xtask: {name} ──");
    match Command::new("cargo").args(args).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("xtask: {name} failed ({status})");
            false
        }
        Err(err) => {
            eprintln!("xtask: {name} could not start: {err}");
            false
        }
    }
}

/// The drift gate: two ways this project's contracts have drifted apart
/// before, each now a failed build rather than a review someone has to think
/// to make. Both are waived the same way — a `WAIVE-DRIFT: <reason>` line in
/// the head commit message, the deliberate and auditable escape.
///
/// The first is the reserved patterns below: contradictions that were fixed
/// and then re-introduced, which a grep can recognize. The second is the
/// event taxonomy drifting from what asserts against it — the generated
/// inventory in `schema/event-taxonomy.json` versus the event types the
/// golden traces name, plus the two names the taxonomy must never carry.
fn drift_gate() -> bool {
    eprintln!("── xtask: drift-gate ──");
    // `git ls-files` lists only files under the current directory and returns
    // paths relative to it, so a run from a subdirectory would silently scan
    // a subset of the repo. Anchor the listing and every read to the
    // repository root instead.
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        eprintln!("xtask: drift-gate: `git rev-parse --show-toplevel` failed");
        return false;
    };
    let root = std::path::PathBuf::from(top.trim_end());
    let Some(listing) = git(&["-C", top.trim_end(), "ls-files"]) else {
        eprintln!("xtask: drift-gate: `git ls-files` failed");
        return false;
    };

    let mut violations = Vec::new();
    for path in listing.lines() {
        // Only this file is exempt: it names the patterns to define them.
        if path == "xtask/src/main.rs" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(path)) else {
            continue; // binary or unreadable file — nothing to scan
        };
        if let Some(reason) = reserved_pattern_hit(&text) {
            violations.push(format!("{path}: {reason}"));
        }
    }
    violations.extend(taxonomy_drift(&root));

    if violations.is_empty() {
        eprintln!("drift-gate: clean.");
        return true;
    }
    for v in &violations {
        eprintln!("drift-gate: {v}");
    }
    if head_commit_waives() {
        eprintln!(
            "drift-gate: reserved pattern present but waived by the head commit (WAIVE-DRIFT)."
        );
        return true;
    }
    eprintln!(
        "drift-gate: FAILED. If this pairing is intentional, add a \
         'WAIVE-DRIFT: <reason>' line to the head commit message."
    );
    false
}

/// Returns a description if `text` re-pairs a reserved contradiction, else `None`.
fn reserved_pattern_hit(text: &str) -> Option<String> {
    // The protocol this runtime will expose reconnects a client to a live
    // session via a `session.attach` request; output missed while detached is
    // reported inside the replay payload (a gap marker), never as a dedicated
    // JSON-RPC error. A recurring design error re-introduced an error code
    // (-32004) for that gap — a file pairing the two is re-importing the
    // contradiction.
    let attach_error = format!("-{}", "32004");
    if text.contains(&attach_error) && text.contains("session.attach") {
        return Some(format!("{attach_error} paired with session.attach"));
    }
    // The runtime will reconstruct a terminal screen (a "virtual terminal")
    // from the output stream so clients get render-state, not raw bytes. That
    // reconstruction belongs to the stream/event layer; a recurring design
    // error assigned it to the PTY layer (the process-hosting layer), which
    // must stay a plain byte pipe.
    let lower = text.to_lowercase();
    if (lower.contains("virtual terminal") || lower.contains("screen state"))
        && lower.contains("pty layer")
    {
        return Some("virtual-terminal / screen-state described as PTY-layer-owned".to_string());
    }
    // `cargo xtask ci` is the check sequence, not the whole of CI. That was
    // true until the supply-chain gate became a PR-tier job of its own, and
    // the claim of equality had by then been written into seven places — the
    // house rules, the contributor guide, the README, the cargo alias, the
    // crate description, this file's own header, and a pull-request checkbox
    // a contributor is asked to tick. One change falsified all seven at once,
    // which is what makes it a contract worth gating rather than remembering.
    // The honest statement, and the one worth keeping, is that every check CI
    // runs is *a* task here — not that one task is all of CI.
    for line in lower.lines() {
        let names_the_command = line.contains("xtask ci");
        if (names_the_command && CI_EQUALITY_CLAIMS.iter().any(|claim| line.contains(claim)))
            || CI_EQUALITY_CLAIMS_STANDALONE
                .iter()
                .any(|claim| line.contains(claim))
        {
            return Some(
                "`cargo xtask ci` claimed to be the whole CI run — the supply-chain gate is a \
                 PR-tier job it deliberately does not include"
                    .to_string(),
            );
        }
    }
    None
}

/// The ways the "one command is all of CI" claim has been phrased. Matched
/// per line rather than per file on purpose: "exactly what" is ordinary
/// English that appears in this repository for unrelated reasons, and only
/// means this when it shares a line with the command it makes a claim about.
const CI_EQUALITY_CLAIMS: &[&str] = &[
    "exactly what ci runs",
    "exactly what the ci",
    "exactly what the pr tier",
    "exactly what the pr-tier",
    "identical to what ci",
    "identical to what the pr",
    "cannot diverge",
];

/// The same claim in the phrasing that does not name the command, because the
/// command sat in a fenced block above it. These read as a promise about the
/// whole of CI wherever they appear, so they need no companion token — and
/// they are the form the claim survived in after the explicit phrasings were
/// corrected, which is why matching only the explicit ones was half a fix.
const CI_EQUALITY_CLAIMS_STANDALONE: &[&str] = &[
    "green locally means green in ci",
    "green locally it is green in ci",
    "green locally and green in ci",
];

/// The generated event taxonomy, versus what asserts against it.
///
/// `schema/event-taxonomy.json` is generated from the event types in
/// `crates/events` and the freshness gate keeps it that way, so it is the one
/// place that knows which events exist. Two things are held to it: the two
/// names the taxonomy must never carry, and every event type the committed
/// golden traces name. A scenario asserting an event the runtime has no way
/// to emit would pass review and then fail forever, and a scenario
/// misspelling a real event type looks exactly the same.
fn taxonomy_drift(root: &Path) -> Vec<String> {
    let manifest = root.join("schema").join("event-taxonomy.json");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return vec![
            "schema/event-taxonomy.json: cannot be read — generate it with \
             `cargo run -p agent-bridge-events --bin schema-gen`"
                .to_string(),
        ];
    };
    let published = event_types_at_depth(&text, INVENTORY_ENTRY_DEPTH);
    if published.is_empty() {
        return vec![
            "schema/event-taxonomy.json: names no event types — the generator or this parser \
             changed shape"
                .to_string(),
        ];
    }

    let mut violations = Vec::new();
    // Asking the runtime how it is doing is a request for a snapshot; the
    // event is the transition in that answer. Publishing both names would be
    // two silently different answers to one question.
    if published.iter().any(|event| event == "runtime.health") {
        violations.push(
            "schema/event-taxonomy.json: `runtime.health` is a request for a snapshot, never an \
             event type — the transition is `runtime.health_changed`"
                .to_string(),
        );
    }
    if !published
        .iter()
        .any(|event| event == "runtime.health_changed")
    {
        violations.push(
            "schema/event-taxonomy.json: `runtime.health_changed` is missing — a health snapshot \
             nothing announces a change to has to be polled for"
                .to_string(),
        );
    }
    // A subscription ending says nothing about the session, which usually
    // keeps running; as an event it would tell every other subscriber that
    // something happened to the session when nothing did.
    if published.iter().any(|event| event == "session.eof") {
        violations.push(
            "schema/event-taxonomy.json: the end of a subscription is a transport notification, \
             not an event type"
                .to_string(),
        );
    }

    let corpus = root.join("tests").join("corpus");
    let mut files = Vec::new();
    if let Err(err) = collect_files(&corpus, &mut files) {
        violations.push(format!("tests/corpus: {err}"));
        return violations;
    }
    for trace in files.iter().filter(|path| {
        path.file_name()
            .is_some_and(|name| name == "expected.ndjson")
    }) {
        let shown = trace
            .strip_prefix(root)
            .unwrap_or(trace.as_path())
            .display();
        let Ok(text) = std::fs::read_to_string(trace) else {
            violations.push(format!("{shown}: cannot be read"));
            continue;
        };
        let mut unknown: Vec<String> = event_types_at_depth(&text, TRACE_RECORD_DEPTH)
            .into_iter()
            .filter(|event| !published.contains(event))
            .collect();
        unknown.sort();
        unknown.dedup();
        for event in unknown {
            violations.push(format!("{shown}: `{event}` is not in the event taxonomy"));
        }
    }
    violations
}

/// How deep an inventory entry's keys sit: the root object, the
/// `event_types` array, then the entry itself.
const INVENTORY_ENTRY_DEPTH: usize = 3;

/// How deep a trace record's own keys sit: each NDJSON line is one object.
const TRACE_RECORD_DEPTH: usize = 1;

/// Every value of an `"event_type"` key that sits exactly `depth` levels of
/// nesting deep.
///
/// A string scan rather than a JSON parse, because `xtask` is deliberately
/// dependency-free — but a depth-aware one, because a trace record's
/// `payload` is whatever the event carried. A payload can legally hold its
/// own nested `"event_type"` key (an error's `detail` and a notice's
/// passthrough are arbitrary objects) or a string whose text is itself JSON,
/// and a plain substring search would read either as a second event type and
/// fail the gate on a valid trace. So strings are stepped over whole, and
/// only keys at the requested depth count.
///
/// Escapes inside a string are stepped over rather than decoded: the values
/// this reads are dotted ASCII names, so an escaped spelling of one simply
/// fails to match — the safe direction, since the result is a reported
/// unknown type rather than a silent pass.
fn event_types_at_depth(text: &str, depth: usize) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut level = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                level += 1;
                index += 1;
            }
            b'}' | b']' => {
                level = level.saturating_sub(1);
                index += 1;
            }
            b'"' => {
                let Some((name, after_name)) = json_string(bytes, index) else {
                    // An unterminated string means this is not the
                    // machine-written JSON the caller thinks it is. Stop with
                    // what was read rather than resync into nonsense: a short
                    // inventory fails the caller's emptiness and reserved-name
                    // checks, which is the loud outcome.
                    return found;
                };
                let colon = skip_ascii_whitespace(bytes, after_name);
                if bytes.get(colon) != Some(&b':') {
                    index = after_name;
                    continue;
                }
                index = skip_ascii_whitespace(bytes, colon + 1);
                if level != depth || name != "event_type" || bytes.get(index) != Some(&b'"') {
                    continue;
                }
                let Some((event_type, after_value)) = json_string(bytes, index) else {
                    return found;
                };
                found.push(event_type.to_string());
                index = after_value;
            }
            _ => index += 1,
        }
    }
    found
}

/// The contents of the JSON string opening at `start`, and the index just
/// past its closing quote.
fn json_string(bytes: &[u8], start: usize) -> Option<(&str, usize)> {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            // A backslash escapes exactly one character, and every character
            // JSON lets it escape is ASCII — so stepping over both bytes
            // keeps the scan on a character boundary.
            b'\\' => index += 2,
            b'"' => {
                return Some((
                    std::str::from_utf8(&bytes[start + 1..index]).ok()?,
                    index + 1,
                ));
            }
            _ => index += 1,
        }
    }
    None
}

fn skip_ascii_whitespace(bytes: &[u8], from: usize) -> usize {
    let mut index = from;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn head_commit_waives() -> bool {
    git(&["log", "-1", "--format=%B"])
        .is_some_and(|msg| msg.lines().any(|line| line.starts_with("WAIVE-DRIFT:")))
}

/// The internal dependency direction, as data: every workspace package and
/// the complete set of *workspace* packages it may depend on. External crates
/// are not this gate's business.
///
/// The direction is acyclic and one-way. Bytes enter at `pty`, meaning is
/// added by `stream`, state by `session`, ordering by `core`, and only the
/// binary is allowed to see all of it at once. Written down here, a violation
/// is a failed build; left as prose, it is a review comment somebody has to
/// think to make. The edge this exists to prevent above all others is `pty`
/// growing a dependency on `adapter-api` — the moment the byte pipe knows
/// which CLI it is hosting, every adapter-shaped assumption is free to leak
/// into the layer that must stay a plain pipe, and the runtime loses the
/// property that makes a second adapter cheap.
///
/// A workspace member missing from this table fails the gate rather than
/// passing unchecked. Adding a crate is exactly the moment its allowed
/// dependencies should be stated, and a table that silently ignores what it
/// does not recognize enforces nothing.
const INTERNAL_DEPENDENCIES: &[(&str, &[&str])] = &[
    // The runtime, bottom of the layer model upward.
    ("agent-bridge-events", &[]),
    ("agent-bridge-pty", &[]),
    ("agent-bridge-adapter-api", &["agent-bridge-events"]),
    (
        "agent-bridge-stream",
        &["agent-bridge-adapter-api", "agent-bridge-events"],
    ),
    (
        "agent-bridge-session",
        &[
            "agent-bridge-events",
            "agent-bridge-pty",
            "agent-bridge-stream",
        ],
    ),
    (
        "agent-bridge-core",
        &["agent-bridge-events", "agent-bridge-session"],
    ),
    (
        "agent-bridge-transport",
        &["agent-bridge-core", "agent-bridge-events"],
    ),
    (
        "agent-bridge-harness",
        &["agent-bridge-events", "agent-bridge-transport"],
    ),
    (
        "agent-bridge",
        &[
            "agent-bridge-adapter-api",
            "agent-bridge-core",
            "agent-bridge-events",
            "agent-bridge-harness",
            "agent-bridge-pty",
            "agent-bridge-session",
            "agent-bridge-stream",
            "agent-bridge-transport",
        ],
    ),
    // Test, tooling, and reference members. They sit outside the layer model —
    // nothing in the runtime may depend on them — so their edges are recorded
    // as they actually are rather than derived from a layer.
    ("agent-bridge-fake-cli", &[]),
    ("agent-bridge-supervisor-ref", &[]),
    ("agent-bridge-detection-spike", &[]),
    ("agent-bridge-probe-child", &[]),
    ("agent-bridge-pty-probe", &[]),
    ("agent-bridge-stub-adapter", &[]),
    ("xtask", &[]),
    (
        "agent-bridge-interactive-probe",
        &["agent-bridge-probe-child"],
    ),
    (
        "agent-bridge-cleanup-probe",
        &["agent-bridge-interactive-probe", "agent-bridge-probe-child"],
    ),
    (
        "agent-bridge-resize-probe",
        &["agent-bridge-interactive-probe", "agent-bridge-probe-child"],
    ),
    (
        "agent-bridge-signal-probe",
        &["agent-bridge-interactive-probe", "agent-bridge-probe-child"],
    ),
    (
        "agent-bridge-utf8-probe",
        &["agent-bridge-interactive-probe", "agent-bridge-probe-child"],
    ),
    (
        "agent-bridge-perf-probe",
        &["agent-bridge-fake-cli", "agent-bridge-interactive-probe"],
    ),
];

/// The dependency-table names a manifest can use. A dependency declared in
/// any of them is a real compile-time edge, so all three are gated alike:
/// a test-only shortcut across a layer boundary is still a layer boundary
/// crossed, and it is how the direction erodes in practice.
const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// The layout contract, checked against the manifests themselves.
///
/// Four things, all of them properties the workspace claims in prose and
/// would otherwise only be true by everyone remembering:
///
/// 1. **Dependency direction** — every internal edge appears in
///    [`INTERNAL_DEPENDENCIES`], and every package in that table still exists.
/// 2. **Naming** — a short directory (`crates/pty`) carrying a prefixed
///    package (`agent-bridge-pty`), so a backtrace frame or a log line names
///    the crate it came from.
/// 3. **Central pinning** — no member declares its own version for a
///    dependency the workspace already pins, which is what keeps a version
///    change a one-line diff in the root manifest.
/// 4. **Inherited lints** — every member takes the workspace lint levels,
///    so a lint is never quietly weaker in one crate than in the rest.
///
/// Reported together: one run should surface everything wrong, not the first
/// thing wrong.
fn workspace_gate() -> bool {
    eprintln!("── xtask: workspace-gate ──");
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        eprintln!("xtask: workspace-gate: `git rev-parse --show-toplevel` failed");
        return false;
    };
    let root = PathBuf::from(top.trim_end());

    let Ok(root_manifest) = std::fs::read_to_string(root.join("Cargo.toml")) else {
        eprintln!("xtask: workspace-gate: cannot read the workspace Cargo.toml");
        return false;
    };
    let members = workspace_members(&root_manifest);
    if members.is_empty() {
        eprintln!("xtask: workspace-gate: the workspace declares no members");
        return false;
    }
    let pinned = workspace_pinned_names(&root_manifest);

    // Read every member first: the direction check needs to know which
    // dependency names are workspace packages at all, and that is only known
    // once all the manifests have been seen.
    let mut manifests = Vec::new();
    for member in &members {
        let path = root.join(member).join("Cargo.toml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("xtask: workspace-gate: cannot read {}", path.display());
            return false;
        };
        manifests.push((member.clone(), parse_manifest(&text)));
    }

    let mut violations = Vec::new();
    for (member, facts) in &manifests {
        if facts.package.is_empty() {
            violations.push(format!("{member}/Cargo.toml declares no package name"));
            continue;
        }
        let directory = member.rsplit('/').next().unwrap_or(member);
        let expected = expected_package_name(directory);
        if facts.package != expected {
            violations.push(format!(
                "{member} is package `{}`, expected `{expected}` — directories are short, \
                 package names carry the `agent-bridge-` prefix",
                facts.package
            ));
        }
        if !facts.inherits_lints {
            violations.push(format!(
                "{member} does not inherit the workspace lints — add `[lints]` with \
                 `workspace = true`"
            ));
        }
    }

    let packages: Vec<&str> = manifests
        .iter()
        .map(|(_, facts)| facts.package.as_str())
        .collect();

    for (member, facts) in &manifests {
        let Some((_, allowed)) = INTERNAL_DEPENDENCIES
            .iter()
            .find(|(name, _)| *name == facts.package)
        else {
            violations.push(format!(
                "{member} (`{}`) is not in the dependency-direction table — add it to \
                 INTERNAL_DEPENDENCIES in xtask/src/main.rs with the internal dependencies it \
                 is allowed to have",
                facts.package
            ));
            continue;
        };
        for dep in &facts.dependencies {
            if packages.contains(&dep.name.as_str()) && !allowed.contains(&dep.name.as_str()) {
                violations.push(format!(
                    "{} -> {} is a forbidden dependency direction (see INTERNAL_DEPENDENCIES \
                     in xtask/src/main.rs)",
                    facts.package, dep.name
                ));
            }
            if pinned.contains(&dep.name) && !dep.inherits_workspace {
                violations.push(format!(
                    "{member} declares its own version of `{}`, which the workspace already \
                     pins — use `{} = {{ workspace = true }}`",
                    dep.name, dep.name
                ));
            }
        }
    }

    // A stale row is a rule nobody is following any more; it should be
    // deleted with the crate, not left to imply coverage that is not there.
    for (name, _) in INTERNAL_DEPENDENCIES {
        if !packages.contains(name) {
            violations.push(format!(
                "`{name}` is in the dependency-direction table but is not a workspace member \
                 — remove the stale row from INTERNAL_DEPENDENCIES in xtask/src/main.rs"
            ));
        }
    }

    if violations.is_empty() {
        eprintln!(
            "workspace-gate: clean ({} members, direction + naming + pinning + lints).",
            members.len()
        );
        return true;
    }
    for violation in &violations {
        eprintln!("workspace-gate: {violation}");
    }
    eprintln!("workspace-gate: FAILED.");
    false
}

/// The package name a member directory must carry.
fn expected_package_name(directory: &str) -> String {
    match directory {
        // The binary is the product, so it carries the project's name with no
        // prefix to add.
        "agent-bridge" => "agent-bridge".to_string(),
        // The dev-task runner is named for the `cargo xtask` alias that
        // invokes it — a cargo-wide convention worth more than local
        // consistency, and it is never linked into the product.
        "xtask" => "xtask".to_string(),
        other => format!("agent-bridge-{other}"),
    }
}

/// What the gate needs from one member manifest.
struct ManifestFacts {
    package: String,
    dependencies: Vec<DependencyEntry>,
    inherits_lints: bool,
}

/// One declared dependency: its name, and whether it takes the workspace pin
/// rather than naming a version of its own.
struct DependencyEntry {
    name: String,
    inherits_workspace: bool,
}

fn parse_manifest(text: &str) -> ManifestFacts {
    let mut package = String::new();
    let mut dependencies: Vec<DependencyEntry> = Vec::new();
    let mut inherits_lints = false;

    for_each_entry(text, |table, key, value| {
        if table == "package" && key == "name" {
            package = unquote(value).to_string();
            return;
        }
        if table == "lints" && key == "workspace" {
            inherits_lints = value.trim() == "true";
            return;
        }
        let Some(dep_table) = dependency_table(table) else {
            return;
        };
        // `[dependencies.serde]` names the dependency in the table header and
        // its fields in the keys; `[dependencies]` names it in the key.
        let (name, field, field_value) = match dep_table {
            Some(name) => (name.to_string(), key, value),
            None => match key.split_once('.') {
                // `serde.workspace = true`
                Some((name, field)) => (unquote(name).to_string(), field, value),
                // `serde = { workspace = true }`
                None => (unquote(key).to_string(), "", value),
            },
        };
        // An inline table carries its fields in the value; a dotted key or a
        // sub-table carries one field per entry. Both spellings mean the same
        // thing, so both are read the same way.
        let inherits = match field {
            "" => value_declares_workspace(field_value),
            "workspace" => field_value.trim() == "true",
            _ => false,
        };
        match dependencies.iter_mut().find(|dep| dep.name == name) {
            Some(existing) => existing.inherits_workspace |= inherits,
            None => dependencies.push(DependencyEntry {
                name,
                inherits_workspace: inherits,
            }),
        }
    });

    ManifestFacts {
        package,
        dependencies,
        inherits_lints,
    }
}

/// Whether `table` is a dependency table, and if it is a per-dependency
/// sub-table, which dependency it belongs to. Handles the platform-specific
/// form (`target.'cfg(unix)'.dependencies`) by looking at the tail of the
/// path rather than matching the whole of it.
fn dependency_table(table: &str) -> Option<Option<&str>> {
    let segments = table_segments(table);
    let last = segments.last()?;
    if DEPENDENCY_TABLES.contains(last) {
        return Some(None);
    }
    let parent = segments.get(segments.len().checked_sub(2)?)?;
    DEPENDENCY_TABLES.contains(parent).then_some(Some(*last))
}

/// Split a table path on its dots, leaving quoted segments intact — a
/// `cfg(…)` predicate is one segment however it is spelled inside.
fn table_segments(table: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut quote = None;
    let mut start = 0;
    for (index, byte) in table.bytes().enumerate() {
        match (quote, byte) {
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(open), byte) if byte == open => quote = None,
            (None, b'.') => {
                segments.push(table[start..index].trim().trim_matches(['\'', '"']));
                start = index + 1;
            }
            _ => {}
        }
    }
    segments.push(table[start..].trim().trim_matches(['\'', '"']));
    segments
}

/// Whether an inline dependency value takes the workspace pin. `version` and
/// `workspace` are mutually exclusive in cargo, so finding the latter is
/// enough — there is no need to also prove the former is absent.
fn value_declares_workspace(value: &str) -> bool {
    let Some(inner) = value.trim().strip_prefix('{') else {
        // A bare `serde = "1"`: a version of its own by definition.
        return false;
    };
    inner
        .split(',')
        .filter_map(|field| field.split_once('='))
        .any(|(key, value)| {
            key.trim() == "workspace" && value.trim().trim_end_matches('}').trim() == "true"
        })
}

/// The workspace members, in declaration order.
fn workspace_members(root_manifest: &str) -> Vec<String> {
    let mut members = Vec::new();
    for_each_entry(root_manifest, |table, key, value| {
        if table == "workspace" && key == "members" {
            members = value
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(|entry| unquote(entry).to_string())
                .collect();
        }
    });
    members
}

/// The dependency names the workspace pins centrally.
fn workspace_pinned_names(root_manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    for_each_entry(root_manifest, |table, key, _| {
        if table == "workspace.dependencies" {
            let name = key.split_once('.').map_or(key, |(name, _)| name);
            let name = unquote(name).to_string();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    });
    names
}

/// Walk the key assignments of a manifest, calling `visit(table, key, value)`
/// for each one with the table path it appeared under.
///
/// Deliberately not a TOML parser — it reads the subset these manifests are
/// written in, which is the subset a reviewer reads too. Values spanning
/// several lines (a feature list, a long inline table) are joined before they
/// are visited, so a continuation line is never mistaken for a key of its own;
/// comments and quoting are respected for the same reason.
fn for_each_entry(text: &str, mut visit: impl FnMut(&str, &str, &str)) {
    let mut table = String::new();
    let mut pending: Option<(String, String)> = None;
    let mut depth = 0i32;

    for raw in text.lines() {
        let line = strip_comment(raw);
        let trimmed = line.trim();

        if let Some((key, mut value)) = pending.take() {
            value.push(' ');
            value.push_str(trimmed);
            depth += bracket_delta(&line);
            if depth > 0 {
                pending = Some((key, value));
            } else {
                depth = 0;
                visit(&table, &key, &value);
            }
            continue;
        }

        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('[') {
            table = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim().to_string(), value.trim().to_string());
        depth = bracket_delta(&line);
        if depth > 0 {
            pending = Some((key, value));
        } else {
            depth = 0;
            visit(&table, &key, &value);
        }
    }
}

/// Everything before an unquoted `#`.
fn strip_comment(line: &str) -> String {
    let mut quote = None;
    for (index, byte) in line.bytes().enumerate() {
        match (quote, byte) {
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(open), byte) if byte == open => quote = None,
            (None, b'#') => return line[..index].to_string(),
            _ => {}
        }
    }
    line.to_string()
}

/// How far a line opens or closes brackets and braces, ignoring quoted text —
/// the signal that a value continues onto the next line.
fn bracket_delta(line: &str) -> i32 {
    let mut quote = None;
    let mut delta = 0;
    for byte in line.bytes() {
        match (quote, byte) {
            (None, b'\'' | b'"') => quote = Some(byte),
            (Some(open), byte) if byte == open => quote = None,
            (None, b'[' | b'{') => delta += 1,
            (None, b']' | b'}') => delta -= 1,
            _ => {}
        }
    }
    delta
}

fn unquote(value: &str) -> &str {
    value.trim().trim_matches(['\'', '"'])
}

/// Run a `git` command, returning stdout on success.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The owner segment of a git remote URL — the `owner` in `owner/repo` — for
/// both `scheme://host/owner/repo` and scp-style `user@host:owner/repo`
/// forms, or `None` when the URL has no host-then-path shape. The `.git`
/// suffix and any port ride along on the repo/host parts this ignores.
fn remote_owner_from_url(url: &str) -> Option<&str> {
    let after_host = match url.trim().split_once("://") {
        Some((_scheme, rest)) => {
            let (host, path) = rest.split_once('/')?;
            // A hostless URL (`file:///path`) has no owner to speak of; its
            // first path segment is just a directory, not a repo owner.
            if host.is_empty() {
                return None;
            }
            path
        }
        // scp-like `user@host:owner/repo` — everything after the first colon.
        // `git remote get-url` can also return a Windows local-clone path
        // (`C:/Users/…` or `C:\repos\…`); its single-letter drive "host"
        // must not be read as an scp host, or the scrub would derive a
        // bogus, over-generic owner needle (`Users`, `repos`) and mask it
        // everywhere. A drive letter is one alphabetic char, and a real
        // scp path carries an `owner/repo` slash a bare drive path may not.
        None => {
            let (host, path) = url.trim().split_once(':')?;
            let is_drive_letter = matches!(host.as_bytes(), [c] if c.is_ascii_alphabetic());
            if is_drive_letter || !path.contains('/') {
                return None;
            }
            path
        }
    };
    let owner = after_host.trim_start_matches('/').split('/').next()?;
    (!owner.is_empty()).then_some(owner)
}

/// Scrub needles the campaign derives from its own environment, so a
/// machine-local token that only ever reaches a fixture through a path is
/// masked without the maintainer having to remember it. Two sources: the
/// repository's git identity — the hook command is the probe's absolute
/// `current_exe`, which sits under `~/…/<remote-owner>/<repo>/…`, so the
/// remote owner and committer name are needles (a name has no shape the
/// email sweep can catch, so an un-derived one would leak silently, exactly
/// how the repo-owner name slipped through the first sitting) — and the
/// temp directory, whose opaque per-user hash component (macOS
/// `/var/folders/<bucket>/<hash>/T`) the CLI paints into every `cwd` and
/// `transcript_path`. That hash is not personal identity, but it is
/// machine-local host-specific noise that makes the corpus needlessly
/// host-bound; masking it keeps the fixtures portable. Additive to the
/// explicit `--mask` list; each needle still passes `safe_mask_needle`.
fn derived_identity_needles() -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut consider = |text: &str| {
        let needle = text.trim().as_bytes().to_vec();
        if safe_mask_needle(&needle) && !out.contains(&needle) {
            out.push(needle);
        }
    };
    if let Some(name) = git(&["config", "user.name"]) {
        consider(&name);
    }
    if let Some(url) = git(&["remote", "get-url", "origin"])
        && let Some(owner) = remote_owner_from_url(&url)
    {
        consider(owner);
    }
    for hash in hash_like_path_components(&std::env::temp_dir()) {
        consider(&hash);
    }
    out
}

/// The shortest temp-directory path component treated as a per-user hash to
/// mask. The macOS `_CS_DARWIN_USER_TEMP_DIR` hash is ~28–30 chars; ordinary
/// components (`var`, `folders`, `tmp`, `T`, `AppData`, `Local`, `Temp`) are
/// all shorter, so this bar masks the hash without touching structure.
const TEMP_HASH_MIN_LEN: usize = 16;

/// The long, purely-alphanumeric path components of `path` — the per-user
/// temp-dir hash on macOS, and nothing on Linux (`/tmp`) or the short,
/// dictionary-like segments of any temp path. Pulled out of the derivation
/// so the length/charset rule is unit-tested rather than exercised only on
/// whatever machine happens to run the campaign.
fn hash_like_path_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .filter(|text| {
            text.len() >= TEMP_HASH_MIN_LEN && text.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(str::to_owned)
        .collect()
}

/// The dependency supply-chain gate: `cargo-deny` over the resolved
/// dependency tree, run here exactly as CI runs it, plus the one rule about
/// suppressions that `cargo-deny` has no way to express itself.
///
/// Kept out of `cargo xtask ci` deliberately. That task is the one a
/// contributor must be able to run with nothing but the pinned toolchain and
/// `git`, and this one needs a binary installed separately — folding it in
/// would quietly add a prerequisite to the command whose whole promise is
/// that it has none. It is a separate step in the pre-push routine instead.
///
/// Arguments are forwarded, so `cargo xtask deny advisories` runs just that
/// check, the same way the scheduled lane does.
fn run_deny(args: &[String]) -> bool {
    eprintln!("── xtask: deny ──");
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        eprintln!("xtask: deny: `git rev-parse --show-toplevel` failed");
        return false;
    };
    let root = PathBuf::from(top.trim_end());

    // Checked before shelling out, and independently of which checks were
    // asked for: an expired suppression is a problem with this repository's
    // own policy, and it should be reported even on a run that would
    // otherwise pass.
    let suppressions = advisory_suppressions_are_current(&root);

    if !cargo_deny_installed() {
        eprintln!(
            "xtask: deny: cargo-deny is not installed. It is a development tool rather than a \
             workspace dependency, so it is installed once per machine:\n    cargo install \
             cargo-deny --locked"
        );
        return false;
    }
    let argv = deny_args(&root, args);
    let checks = cargo_owned("cargo-deny", &argv);
    checks && suppressions
}

/// The `cargo deny` invocation, anchored at the repository root.
///
/// The anchoring is the point. Left to the working directory, a run from
/// inside a crate directory checks *that crate's* subtree and prints the same
/// reassuring summary line — a narrower graph, an unchanged verdict, and a
/// dependency two crates over that nobody looked at. A gate whose answer
/// depends on where it was invoked from is worse than no gate, because it is
/// believed. The layout and drift gates anchor themselves for the same
/// reason; this one is a shell-out, so the anchor has to be handed to the
/// tool rather than applied by it.
fn deny_args(root: &Path, forwarded: &[String]) -> Vec<String> {
    let mut argv = vec![
        "deny".to_string(),
        "--manifest-path".to_string(),
        root.join("Cargo.toml").to_string_lossy().into_owned(),
        "--config".to_string(),
        root.join("deny.toml").to_string_lossy().into_owned(),
        "check".to_string(),
    ];
    argv.extend(forwarded.iter().cloned());
    argv
}

/// Whether `cargo deny` can be invoked at all. Asked separately, and with its
/// output discarded, so that "the tool is missing" is reported as the
/// actionable thing it is rather than surfacing as cargo's generic
/// no-such-command failure at the end of a run.
fn cargo_deny_installed() -> bool {
    Command::new("cargo")
        .args(["deny", "--version"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The marker a suppression's reason must carry, and the thing that makes a
/// suppression temporary rather than permanent.
const REVIEW_MARKER: &str = "review by ";

/// Hold every advisory suppression in `deny.toml` to a review date that has
/// not yet passed.
///
/// A suppression silences a known-vulnerable, unmaintained, or yanked crate.
/// That is sometimes the only available answer — an advisory with no fixed
/// version published yet — but it is never a permanent one, and a suppression
/// nobody revisits is indistinguishable from not having noticed. `cargo-deny`
/// accepts only an id and a free-text reason for these entries, with no notion
/// of expiry, so the date lives in the reason (`review by YYYY-MM-DD`) and
/// this gate is what makes it mean something: once that date passes, the build
/// fails until somebody looks again and either removes the entry or moves the
/// date forward with a fresh justification.
fn advisory_suppressions_are_current(root: &Path) -> bool {
    let path = root.join("deny.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        eprintln!("xtask: deny: cannot read {}", path.display());
        return false;
    };
    let today = today_utc();
    let mut violations = Vec::new();
    for entry in advisory_suppression_entries(&text) {
        match review_date(&entry) {
            None => violations.push(format!(
                "{entry}\n    has no `{REVIEW_MARKER}YYYY-MM-DD` in its reason — every \
                 suppression states when it gets looked at again"
            )),
            Some(date) if date.as_str() < today.as_str() => violations.push(format!(
                "{entry}\n    was due for review on {date} (today is {today}) — remove it if the \
                 advisory is addressed, or set a new date and say why it still stands"
            )),
            Some(_) => {}
        }
    }
    if violations.is_empty() {
        return true;
    }
    eprintln!("xtask: deny: advisory suppressions need attention:");
    for violation in &violations {
        eprintln!("  - {violation}");
    }
    false
}

/// The `ignore = [ … ]` entries of `deny.toml`'s `[advisories]` table, one
/// string per entry.
///
/// Hand-rolled, like the manifest reading the workspace gate does, because
/// this crate stays dependency-free. It reads only the shape this file is
/// actually written in — an `ignore` array in the `[advisories]` table, one
/// entry per line — and a line is an entry only if it carries an `id` key, so
/// the surrounding comments (including the worked example in `deny.toml`,
/// which is commented out precisely so it is not mistaken for a real
/// suppression) are skipped.
fn advisory_suppression_entries(text: &str) -> Vec<String> {
    let mut entries = Vec::new();
    let mut in_advisories = false;
    let mut in_ignore = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && !in_ignore {
            in_advisories = trimmed.starts_with("[advisories]");
            continue;
        }
        if in_advisories && !in_ignore && trimmed.starts_with("ignore") && trimmed.contains('[') {
            in_ignore = true;
            // `ignore = []` opens and closes on one line and holds nothing.
            if trimmed.contains(']') {
                in_ignore = false;
            }
            continue;
        }
        if in_ignore {
            if trimmed.starts_with(']') {
                in_ignore = false;
                continue;
            }
            // The `id` key specifically, not the letters: a reason saying
            // "avoid" or "idle" is not an entry boundary.
            if trimmed.contains("id =") || trimmed.contains("id=") {
                entries.push(trimmed.trim_end_matches(',').to_string());
            }
        }
    }
    entries
}

/// The `YYYY-MM-DD` following the review marker in a suppression entry, if it
/// is there and well-formed. A malformed date reads as no date at all, which
/// fails the gate — the alternative would be to compare nonsense against today
/// and let it pass.
fn review_date(entry: &str) -> Option<String> {
    let at = entry.find(REVIEW_MARKER)? + REVIEW_MARKER.len();
    let date: String = entry[at..].chars().take(10).collect();
    let bytes = date.as_bytes();
    let shaped = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9]
            .iter()
            .all(|&i| bytes[i].is_ascii_digit());
    shaped.then_some(date)
}

/// Today's UTC date as `YYYY-MM-DD`, so it compares with a review date as
/// plain text — the reason that format is the one asked for.
fn today_utc() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64 / 86_400)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since the Unix epoch to a civil year/month/day, by Howard Hinnant's
/// `civil_from_days`. Written out because this crate takes no dependencies,
/// and a date crate would be a lot of supply-chain surface to add inside the
/// very gate that exists to keep supply-chain surface down.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    // Shift the epoch to 0000-03-01, which puts the leap day at the end of the
    // year and makes the month arithmetic below branchless.
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097; // [0, 146096]
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365; // [0, 399]
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100); // [0, 365]
    let month_prime = (5 * day_of_year + 2) / 153; // [0, 11], where 0 is March
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as u32, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every manifest spelling the workspace actually uses, in one file: an
    /// inline table, a bare version, a dotted key, a per-dependency
    /// sub-table, a platform-specific section, a value spanning several
    /// lines, and comments that contain the punctuation the reader tracks.
    const SAMPLE_MANIFEST: &str = r#"
# A comment with a [bracket] and a "quote" in it.
[package]
name = "agent-bridge-example"
edition.workspace = true

[lints]
workspace = true

[dependencies]
serde = { workspace = true }
regex = "1"
tokio.workspace = true
windows-sys = { version = "0.61", features = [
    # The console handles.
    "Win32_System_Console",
    "Win32_Foundation",
] }

[dependencies.schemars]
workspace = true

[dev-dependencies]
jsonschema = { workspace = true }

[target.'cfg(unix)'.dependencies]
libc = { workspace = true }

[[bin]]
name = "example"
path = "src/main.rs"
"#;

    #[test]
    fn manifest_facts_are_read_from_every_dependency_spelling() {
        let facts = parse_manifest(SAMPLE_MANIFEST);
        assert_eq!(facts.package, "agent-bridge-example");
        assert!(facts.inherits_lints);

        let mut names: Vec<&str> = facts
            .dependencies
            .iter()
            .map(|dep| dep.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "jsonschema",
                "libc",
                "regex",
                "schemars",
                "serde",
                "tokio",
                "windows-sys",
            ],
            "a dependency the gate cannot see is a dependency it cannot hold to the direction"
        );

        let inherits = |name: &str| {
            facts
                .dependencies
                .iter()
                .find(|dep| dep.name == name)
                .is_some_and(|dep| dep.inherits_workspace)
        };
        // Inline table, dotted key, and sub-table all mean "take the
        // workspace pin"; a bare version string means the opposite, which is
        // the case the gate exists to catch.
        assert!(inherits("serde"));
        assert!(inherits("tokio"));
        assert!(inherits("schemars"));
        assert!(inherits("libc"));
        assert!(!inherits("regex"));
        assert!(!inherits("windows-sys"));
    }

    #[test]
    fn a_multiline_value_is_not_read_as_further_keys() {
        // The feature list under `windows-sys` spans four lines. Read line by
        // line, its entries would look like keys of the dependency table and
        // invent dependencies named after Windows API groups.
        let facts = parse_manifest(SAMPLE_MANIFEST);
        assert!(
            !facts
                .dependencies
                .iter()
                .any(|dep| dep.name.contains("Win32")),
            "a continuation line was mistaken for a key"
        );
    }

    #[test]
    fn dependency_tables_are_recognized_by_their_tail() {
        assert_eq!(dependency_table("dependencies"), Some(None));
        assert_eq!(dependency_table("dev-dependencies"), Some(None));
        assert_eq!(dependency_table("build-dependencies"), Some(None));
        // Platform-specific sections are dependency tables too — a
        // Windows-only edge crosses a layer boundary just as a portable one
        // does, and it is only compiled on the platform least likely to be
        // the author's.
        assert_eq!(
            dependency_table("target.'cfg(windows)'.dependencies"),
            Some(None)
        );
        assert_eq!(dependency_table("dependencies.serde"), Some(Some("serde")));
        assert_eq!(
            dependency_table("target.'cfg(unix)'.dependencies.libc"),
            Some(Some("libc"))
        );
        assert_eq!(dependency_table("package"), None);
        assert_eq!(dependency_table("lints"), None);
        // `[features]` names other dependencies for a living; reading it as
        // a dependency table would report edges that do not exist.
        assert_eq!(dependency_table("features"), None);
    }

    #[test]
    fn the_root_manifest_yields_its_members_and_its_pins() {
        // The real one: the gate is worth nothing if it cannot read the file
        // it is actually pointed at, and a members list is the one value in
        // the workspace that is always spread over many lines.
        let root = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("Cargo.toml"),
        )
        .expect("the workspace manifest sits one directory above xtask");

        let members = workspace_members(&root);
        assert!(members.contains(&"xtask".to_string()));
        assert!(members.contains(&"crates/pty".to_string()));
        assert!(
            members
                .iter()
                .all(|member| !member.contains(['[', ']', '"'])),
            "members: {members:?}"
        );

        let pinned = workspace_pinned_names(&root);
        assert!(pinned.contains(&"serde".to_string()));
        assert!(pinned.contains(&"windows-sys".to_string()));
        assert!(
            !pinned.contains(&"Win32_System_Console".to_string()),
            "a feature name leaked out of a multi-line value and would be gated as a dependency"
        );
    }

    #[test]
    fn package_names_follow_the_directory_they_live_in() {
        assert_eq!(expected_package_name("pty"), "agent-bridge-pty");
        assert_eq!(
            expected_package_name("adapter-api"),
            "agent-bridge-adapter-api"
        );
        // The two documented exceptions: the product binary carries the bare
        // project name, and the dev-task runner is named for the cargo alias.
        assert_eq!(expected_package_name("agent-bridge"), "agent-bridge");
        assert_eq!(expected_package_name("xtask"), "xtask");
    }

    #[test]
    fn the_direction_table_is_acyclic() {
        // A cycle in the table would make the gate pass a workspace cargo
        // itself would refuse to build, and the table is hand-maintained.
        // Depth-first from every package; the layer model has no cycles, so
        // neither may its written form.
        fn reaches(from: &str, target: &str, depth: usize) -> bool {
            if depth == 0 {
                return true; // deeper than the table is wide: treat as a cycle
            }
            INTERNAL_DEPENDENCIES
                .iter()
                .find(|(name, _)| *name == from)
                .is_some_and(|(_, allowed)| {
                    allowed
                        .iter()
                        .any(|dep| *dep == target || reaches(dep, target, depth - 1))
                })
        }
        for (package, _) in INTERNAL_DEPENDENCIES {
            assert!(
                !reaches(package, package, INTERNAL_DEPENDENCIES.len()),
                "`{package}` can reach itself through the dependency-direction table"
            );
        }
    }

    #[test]
    fn temp_hash_component_is_masked_but_structure_is_not() {
        // The macOS temp path yields exactly the per-user hash; the
        // structural segments (var, folders, the `_3` bucket which is not
        // all-alphanumeric, T) stay. Linux `/tmp` yields nothing. The hash
        // here is a placeholder of the right shape, not any real machine's.
        assert_eq!(
            hash_like_path_components(Path::new("/var/folders/_9/aaaabbbbccccdddd1111222233/T")),
            vec!["aaaabbbbccccdddd1111222233".to_string()],
        );
        assert!(hash_like_path_components(Path::new("/tmp")).is_empty());
    }

    #[test]
    fn remote_owner_is_parsed_from_every_url_shape() {
        // The owner is what a machine-local path carries (`~/…/<owner>/<repo>`)
        // and what the campaign auto-masks. Both git URL grammars must yield
        // it, including the SSH host-alias form where the alias itself
        // repeats the owner (`github.com-owner`) — the parse must return the
        // path owner, not the host.
        for (url, owner) in [
            ("git@github.com:acme/widget.git", "acme"),
            ("git@github.com-acme:acme/widget.git", "acme"),
            ("https://github.com/acme/widget.git", "acme"),
            ("https://github.com:443/acme/widget", "acme"),
            ("ssh://git@github.com/acme/widget.git", "acme"),
        ] {
            assert_eq!(remote_owner_from_url(url), Some(owner), "{url}");
        }
    }

    #[test]
    fn remote_owner_declines_urls_with_no_owner() {
        // No host-then-path shape, so nothing safe to treat as an owner; the
        // deriver must add nothing rather than mask a stray directory name.
        for url in ["file:///srv/repos/widget.git", "not-a-url", "https://", ""] {
            assert_eq!(remote_owner_from_url(url), None, "{url}");
        }
    }

    #[test]
    fn remote_owner_declines_windows_local_paths() {
        // `git remote get-url` returns a bare filesystem path for a local
        // clone. A Windows drive-letter path must not be parsed as scp-style
        // (`C:owner/…`) — that derived `Users`/`repos` needles that the scrub
        // would then mask across every fixture. Both slash directions, and a
        // bare `host:repo` with no owner slash, decline.
        for url in [
            "C:/Users/dev/widget.git",
            r"C:\Users\dev\widget.git",
            "D:/repos/widget",
            "server:widget.git", // no owner/repo slash
        ] {
            assert_eq!(remote_owner_from_url(url), None, "{url}");
        }
    }

    #[test]
    fn trailing_separators_do_not_defeat_the_username_mask() {
        // A reviewed claim held that a trailing separator on HOME makes
        // file_name return None and silently skips the mask. Path parsing
        // skips empty trailing components on every platform; this test runs
        // on all three CI OSes so that stays a checked fact, not a belief.
        for home in ["/Users/alice", "/Users/alice/", "/Users/alice//"] {
            assert_eq!(
                maskable_username(Some(home.into())).as_deref(),
                Some(b"alice".as_slice()),
                "{home}"
            );
        }
        #[cfg(windows)]
        for home in [r"C:\Users\alice", r"C:\Users\alice\", "C:/Users/alice/"] {
            assert_eq!(
                maskable_username(Some(home.into())).as_deref(),
                Some(b"alice".as_slice()),
                "{home}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_username_masks_by_its_exact_bytes() {
        // A Unix home path need not be UTF-8. The needle must be the
        // component's exact bytes — a lossily-converted needle could never
        // match what a fixture actually carries, claiming a mask that never
        // happened.
        use std::os::unix::ffi::OsStringExt;
        let raw = b"/Users/al\x80ce".to_vec(); // invalid UTF-8 in the name
        let needle = maskable_username(Some(std::ffi::OsString::from_vec(raw)))
            .expect("a lettered non-UTF-8 name must still mask");
        assert_eq!(needle, b"al\x80ce");
    }

    #[test]
    fn non_latin_usernames_mask_by_their_utf8_bytes() {
        // Requiring an ASCII letter would skip every CJK or Cyrillic
        // username and leak exactly the identity the scrub exists to
        // remove. Any non-ASCII byte is name-like enough to be a safe
        // needle — it cannot collide with a numeric field.
        for name in ["\u{7530}\u{4e2d}", "\u{0438}\u{0432}\u{0430}\u{043d}"] {
            let home = format!("/Users/{name}");
            assert_eq!(
                maskable_username(Some(home.clone().into())).as_deref(),
                Some(name.as_bytes()),
                "{home}"
            );
        }
    }

    #[test]
    fn email_sweep_finds_addresses_wherever_they_hide() {
        // The first real claude sitting found the TUI paints the logged-in
        // account email into the raw byte stream, wrapped in ANSI color
        // sequences. The sweep must see through that framing, and through
        // JSON, or a forgotten --mask commits identity to the public corpus.
        for (haystack, expect) in [
            (
                &b"plain someone@example.com text"[..],
                "someone@example.com",
            ),
            (
                &b"\x1b[38;2;153;153;153msomeone@example.com's Organization\x1b[54G"[..],
                "someone@example.com",
            ),
            (&br#"{"email":"a.b+tag@mail.co"}"#[..], "a.b+tag@mail.co"),
            (
                &b"reach me at user@mail.com. Next sentence."[..],
                "user@mail.com",
            ),
        ] {
            let range = find_email(haystack, 0).expect("address must be found");
            // Suffix, not equality: the local-part walk may drag framing
            // bytes (an SGR sequence's `153m`) into the front of the range.
            assert!(
                haystack[range.clone()].ends_with(expect.as_bytes()),
                "found {:?} in {:?}",
                String::from_utf8_lossy(&haystack[range]),
                String::from_utf8_lossy(haystack),
            );
        }
    }

    #[test]
    fn email_sweep_stays_quiet_on_fixture_bytes_that_merely_carry_an_at() {
        // Every fixture legitimately carries `@`s that are not addresses:
        // the manifest's npm install line (scope has no dotted domain; the
        // version pin's final label is numeric), ANSI insert-character
        // sequences ending in `@`, and the x-runs the mask itself leaves
        // behind. Aborting on those would make the sweep impossible to
        // live with, and it would get disabled instead of heeded.
        for haystack in [
            &b"npm @anthropic-ai/claude-code@2.1.201"[..],
            &b"install: \"npm @anthropic-ai/claude-code@2.1.201\""[..],
            &b"\x1b[4@after"[..],
            &b"xxxxxxxxxxxxxxxxxxxxxxxxxx's Organization"[..],
            &b"bare @ sign"[..],
            &b"trailing@"[..],
        ] {
            assert_eq!(
                find_email(haystack, 0),
                None,
                "{}",
                String::from_utf8_lossy(haystack)
            );
        }
    }

    #[test]
    fn the_sweep_sees_an_address_split_across_a_repaint_escape() {
        // The exact shape that leaked in the first sitting: the account
        // footer's domain interrupted by a differential-repaint cursor move,
        // so the address is not contiguous in the raw bytes. A raw scan
        // reads past it; the framing-stripped sweep must catch it.
        let raw = b"\x1b[7G\x1b[38;2;153;153;153msomeone@gma\x1b[28Gil.com's Organization";
        assert_eq!(find_email(raw, 0), None, "raw scan cannot see the split");
        assert_eq!(
            surviving_email(raw).as_deref(),
            Some("someone@gmail.com"),
            "the framing-stripped sweep reassembles it"
        );
    }

    #[test]
    fn the_sweep_ignores_the_domain_a_name_mask_leaves_behind() {
        // After masking the identifying local part, the residual `@domain`
        // still forms a syntactic address, but the local part carries a run
        // of mask bytes and no identity — the sweep must not abort on it, or
        // a correctly-scrubbed fixture could never be committed. This holds
        // when the domain is itself split across a repaint escape, and when
        // stripping the framing glues a neighbouring footer word (`fre`, off
        // the end of "free") onto the front of the masked run — the real
        // false positive that a live capture hit.
        for clean in [
            &b"\x1b[7Gxxxxxxxxxxxxxxxx@gmail.com's Organization"[..],
            &b"\x1b[7Gxxxxxxxxxxxxxxxx@gma\x1b[28Gil.com's Organization"[..],
            &b"terminal fre\x1b[2Cxxxxxxxxxxxxxxxx@gmail.com's Organization"[..],
        ] {
            assert_eq!(
                surviving_email(clean),
                None,
                "{}",
                String::from_utf8_lossy(clean)
            );
        }
    }

    #[test]
    fn a_mask_run_only_counts_where_masking_leaves_it_before_the_at() {
        // The scrubbed-remnant test looks for mask bytes at the END of the
        // local part, because that is the only place masking a needle can
        // leave them. Accepting a run anywhere would whitelist a genuine
        // address that merely contains "xxx" — a missed leak, the failure
        // direction that matters.
        assert!(
            local_part_is_scrubbed(b"xxxxxxxxxxxxxxxx"),
            "a fully masked local part is a remnant"
        );
        assert!(
            local_part_is_scrubbed(b"frexxxxxxxxxxxxxxxx"),
            "framing glued onto the front still leaves the run before the @"
        );
        assert!(
            !local_part_is_scrubbed(b"xxxuser"),
            "a real address that happens to start with xxx is NOT scrubbed"
        );
        assert!(!local_part_is_scrubbed(b"user"), "no mask bytes at all");
        // End to end: the address must be reported, not silently skipped.
        assert_eq!(
            surviving_email(b"contact xxxuser@example.com now").as_deref(),
            Some("xxxuser@example.com"),
        );
    }

    #[test]
    fn the_sweep_finds_a_real_address_past_a_masked_remnant() {
        // A masked remnant earlier in the file must not shadow a genuine
        // address the maintainer forgot to mask further along.
        let bytes = b"xxxx@gmail.com ... later real@example.org";
        assert_eq!(surviving_email(bytes).as_deref(), Some("real@example.org"));
    }

    #[test]
    fn the_greeting_sweep_catches_an_unmasked_display_name() {
        // The exact shape that slipped past token-based verification: the
        // account display name in the startup splash greeting, wrapped in
        // SGR sequences and box-drawing. A placeholder name (not any real
        // account's) carrying a non-ASCII byte, as a real one can.
        let raw = "\u{2502}\x1b[1mWelcome back Zoë!\x1b[0m\u{2502}".as_bytes();
        assert_eq!(surviving_greeting(raw).as_deref(), Some("Zoë"));
    }

    #[test]
    fn the_greeting_sweep_ignores_a_masked_name_and_absent_greetings() {
        // A masked name (the scrubbed state) must not abort, and a fixture
        // with no greeting at all is clean.
        assert_eq!(
            surviving_greeting(b"\x1b[1mWelcome back xxxxx!\x1b[0m"),
            None
        );
        assert_eq!(surviving_greeting(b"no greeting here, just prose"), None);
        // A greeting with an empty name (nothing between marker and `!`) is
        // not a leak either.
        assert_eq!(surviving_greeting(b"Welcome back !"), None);
    }

    #[test]
    fn domain_masker_clears_a_repaint_split_domain_in_place() {
        // The exact shape a raw needle cannot reach: the account address with
        // the local part already masked and the domain split across a
        // cursor-move escape. The masker must clear the on-screen @domain,
        // leave the escape and total length intact, and leave no email-shaped
        // run behind.
        let mut bytes = b"footer xxxxxxxxxxxxxxxx@gma\x1b[28Gil.com's Org".to_vec();
        let before = bytes.len();
        assert!(mask_email_domains(&mut bytes));
        assert_eq!(bytes.len(), before, "masking must preserve byte length");
        assert!(bytes.windows(5).any(|w| w == b"\x1b[28G"), "escape kept");
        assert_eq!(surviving_email(&bytes), None, "no email-shaped run remains");
    }

    #[test]
    fn domain_masker_leaves_an_unmasked_address_for_the_sweep() {
        // If the local part was never masked (a forgotten --mask), the domain
        // masker must not touch it — the email sweep has to still see a real
        // address and abort, rather than the domain being quietly masked into
        // a shape the sweep no longer recognises.
        let mut bytes = b"contact realname@example.com now".to_vec();
        assert!(!mask_email_domains(&mut bytes));
        assert_eq!(
            surviving_email(&bytes).as_deref(),
            Some("realname@example.com")
        );
    }

    #[test]
    fn framing_stripper_drops_escapes_and_keeps_utf8_text() {
        // CSI, OSC (both BEL- and ST-terminated), a lone two-byte escape,
        // and a C0 control all vanish; printable ASCII and multi-byte UTF-8
        // survive. Reading the survivors off the input: a b c d e f é.
        let framed = b"a\x1b[1;2mb\x1b]0;x\x07c\x1b]8;;u\x1b\\d\x1bXe\nf\xc3\xa9";
        assert_eq!(strip_terminal_framing(framed), "abcdef\u{e9}".as_bytes());
    }

    #[test]
    fn unmaskable_homes_are_none_so_the_caller_warns_loudly() {
        // No home, a bare root, an empty value, a too-short name, and
        // needles of only ASCII digits and punctuation all decline the
        // mask — digits collide with numeric NDJSON fields, and something
        // like "1-2" or "..." with version strings and prose. Each reaches
        // the caller's explicit warning path instead of masking unsafely.
        for home in [
            None,
            Some("/"),
            Some(""),
            Some("/Users/123"),
            Some("/Users/ab"),
            Some("/Users/1-2"),
            Some("/Users/..."),
        ] {
            assert_eq!(
                maskable_username(home.map(std::ffi::OsString::from)),
                None,
                "{home:?}"
            );
        }
    }

    /// Trace records carrying everything that would break a scan that just
    /// looked for the key: a payload with its own nested `event_type`, a
    /// payload string whose text is JSON, a string full of braces, an escaped
    /// quote, and a record that puts its payload before its own name.
    const AWKWARD_TRACE: &str = concat!(
        r#"{"seq":1,"monotonic_ns":1,"event_type":"runtime.notice","payload":{"#,
        r#""notification_type":"permission_prompt","detail":{"event_type":"vendor.private"},"#,
        r#""message":"{\"event_type\": \"quoted.thing\"}"},"schema_version":"1"}"#,
        "\n",
        r#"{"seq":2,"monotonic_ns":2,"event_type":"stream.token","payload":{"content":"}}} \" {"}}"#,
        "\n",
        r#"{"payload":{"content":"x"},"event_type":"stream.stderr","seq":3,"monotonic_ns":3}"#,
        "\n",
    );

    #[test]
    fn only_a_records_own_event_type_counts() {
        // The nested names are payload data — an error's `detail` and a
        // notice's passthrough carry whatever the CLI sent — so counting them
        // would fail the gate on a trace that is entirely valid.
        assert_eq!(
            event_types_at_depth(AWKWARD_TRACE, TRACE_RECORD_DEPTH),
            ["runtime.notice", "stream.token", "stream.stderr"]
        );
    }

    /// The generated inventory's shape, abbreviated: the entries sit inside
    /// the `event_types` array, three levels in.
    const SAMPLE_INVENTORY: &str = r#"{
  "$comment": "GENERATED FILE — do not edit by hand.",
  "emit_classes": {
    "ring": "Broadcast to every subscriber of the session."
  },
  "event_types": [
    {
      "emit_class": "ring",
      "event_type": "stream.token"
    },
    {
      "emit_class": "reserved",
      "event_type": "session.writer_changed"
    }
  ],
  "schema_version": 1
}
"#;

    #[test]
    fn inventory_entries_are_read_at_their_own_depth() {
        assert_eq!(
            event_types_at_depth(SAMPLE_INVENTORY, INVENTORY_ENTRY_DEPTH),
            ["stream.token", "session.writer_changed"]
        );
        // Read at the record depth it yields nothing, which is what makes the
        // depth load-bearing rather than incidental: the two inputs put the
        // same key in different places, and each is read where its own is.
        assert!(event_types_at_depth(SAMPLE_INVENTORY, TRACE_RECORD_DEPTH).is_empty());
    }

    /// The committed `deny.toml` shape, plus the two things that must not be
    /// read as suppressions: the commented-out worked example, and the
    /// `ignore` key of a different table.
    const SAMPLE_DENY: &str = r#"
[advisories]
db-urls = ["https://github.com/rustsec/advisory-db"]
# ignore = [
#   { id = "RUSTSEC-0000-0000", reason = "the worked example, review by 2020-01-01" },
# ]
ignore = [
  { id = "RUSTSEC-1111-1111", reason = "no fixed version yet, review by 2030-01-01" },
  { id = "RUSTSEC-2222-2222", reason = "test-scope only" },
]

[licenses.private]
ignore = false
"#;

    #[test]
    fn suppressions_are_read_without_the_commented_example() {
        let entries = advisory_suppression_entries(SAMPLE_DENY);
        assert_eq!(entries.len(), 2, "got: {entries:?}");
        assert!(entries[0].contains("RUSTSEC-1111-1111"));
        assert!(entries[1].contains("RUSTSEC-2222-2222"));
        // The `[licenses.private]` table also has an `ignore` key, and it is a
        // bare boolean rather than an array — reading it as one would take
        // the rest of the file as suppression entries.
        assert!(!entries.iter().any(|entry| entry.contains("false")));
    }

    #[test]
    fn an_empty_suppression_list_holds_nothing() {
        assert!(advisory_suppression_entries("[advisories]\nignore = []\n").is_empty());
    }

    #[test]
    fn a_review_date_is_read_only_when_well_formed() {
        assert_eq!(
            review_date(r#"{ id = "X", reason = "…, review by 2030-01-02" }"#).as_deref(),
            Some("2030-01-02")
        );
        // No marker, and a marker followed by something that is not a date,
        // both read as "no review date" — which fails the gate rather than
        // being compared against today as nonsense.
        assert_eq!(review_date(r#"{ id = "X", reason = "…" }"#), None);
        assert_eq!(review_date("review by soon-ish"), None);
        assert_eq!(review_date("review by 2030-1-2"), None);
    }

    #[test]
    fn days_since_the_epoch_become_the_expected_civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // A leap day, and the day either side of it, since the shifted-era
        // arithmetic exists precisely to get these right.
        assert_eq!(civil_from_days(19_781), (2024, 2, 28));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
        // 2000 was a leap year and 1900 was not; the century rules are the
        // part a hand-written conversion gets wrong.
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(-25_508), (1900, 3, 1));
    }

    /// Each reserved pattern, in the phrasing that actually got written down
    /// and had to be corrected. These went untested until the third pattern
    /// was added; a gate nobody tests is a grep nobody has checked.
    #[test]
    fn each_reserved_pattern_catches_its_recurrence() {
        assert!(
            reserved_pattern_hit("the gap is reported as -32004 on session.attach").is_some(),
            "the attach error-code pairing must be caught"
        );
        assert!(
            reserved_pattern_hit("the PTY layer reconstructs a virtual terminal").is_some(),
            "virtual-terminal ownership must be caught"
        );
        assert!(
            reserved_pattern_hit("`cargo xtask ci` is exactly what the PR tier runs").is_some(),
            "the one-command-is-all-of-CI claim must be caught"
        );
    }

    /// Every phrasing the claim has actually taken across the repository, so
    /// a reworded reintroduction is caught rather than only the exact
    /// sentence that happened to be corrected.
    #[test]
    fn every_recorded_phrasing_of_the_ci_equality_claim_is_caught() {
        for text in [
            "run `cargo xtask ci` before pushing — it is exactly what CI runs.",
            "- [ ] `cargo xtask ci` is green locally (it is exactly what the PR tier runs).",
            "`cargo xtask ci` runs exactly what the CI workflow runs",
            "One command, identical to what the PR-tier CI runs: cargo xtask ci",
            "It is **exactly what the PR-tier CI runs** — cargo xtask ci",
            "cargo xtask ci — so green locally and green in CI cannot diverge",
            // The two that survived correcting the explicit phrasings,
            // because the command sat in a fenced block above rather than on
            // the line making the promise.
            "…and the two gates below — so green locally means green in CI.",
            "…and the two layout/drift gates — so if it is green locally it is green in CI.",
        ] {
            assert!(
                reserved_pattern_hit(text).is_some(),
                "this phrasing must be caught: {text}"
            );
        }
    }

    /// The rule is line-scoped precisely so ordinary prose survives it. These
    /// are the shapes that must stay legal, including the honest replacement
    /// the claim was corrected to.
    #[test]
    fn the_ci_equality_rule_leaves_honest_prose_alone() {
        for text in [
            // The corrected statement: every check is *a* task, not one task
            // is all of CI.
            "Every check CI runs is a `cargo xtask` task, so local and CI cannot drift apart.",
            "`cargo xtask ci` is the PR tier, less the supply-chain gate.",
            // "exactly what" used for something else entirely, on its own
            // line — the false positive a whole-file rule would produce.
            "A PTY that cannot be allocated is exactly what the probes exist to catch.",
            "cargo xtask ci",
        ] {
            assert!(
                reserved_pattern_hit(text).is_none(),
                "this must stay legal: {text}"
            );
        }
    }

    /// The gate must read the whole workspace no matter where it was invoked
    /// from. Without the anchor, running it inside a crate directory checked
    /// only that crate's dependencies and still printed a green summary —
    /// a false pass, and the failure mode a supply-chain gate can least
    /// afford.
    #[test]
    fn the_deny_invocation_is_anchored_at_the_repository_root() {
        let root = Path::new("/repo");
        let argv = deny_args(root, &[]);
        let manifest = argv
            .iter()
            .position(|a| a == "--manifest-path")
            .map(|i| argv[i + 1].as_str());
        assert_eq!(manifest, Some("/repo/Cargo.toml"));
        let config = argv
            .iter()
            .position(|a| a == "--config")
            .map(|i| argv[i + 1].as_str());
        assert_eq!(config, Some("/repo/deny.toml"));
        // Both anchors must precede the subcommand: cargo-deny takes them as
        // options of the tool, not of `check`.
        let check = argv
            .iter()
            .position(|a| a == "check")
            .expect("a subcommand");
        assert!(
            argv.iter().position(|a| a == "--manifest-path").unwrap() < check
                && argv.iter().position(|a| a == "--config").unwrap() < check,
            "the anchors belong before the subcommand: {argv:?}"
        );
    }

    #[test]
    fn deny_forwards_a_named_check_after_the_subcommand() {
        let argv = deny_args(Path::new("/repo"), &["advisories".to_string()]);
        assert_eq!(argv.last().map(String::as_str), Some("advisories"));
        // …and the anchors survive the forwarding, which is what the nightly
        // lane depends on.
        assert!(argv.iter().any(|a| a == "/repo/deny.toml"), "{argv:?}");
    }

    #[test]
    fn today_is_a_sortable_date_string() {
        let today = today_utc();
        assert_eq!(today.len(), 10);
        // The comparison the gate makes is plain text, so the format has to
        // be zero-padded and year-first for it to mean anything.
        assert!(review_date(&format!("review by {today}")).is_some());
        assert!(today.as_str() > "2020-01-01");
    }
}
