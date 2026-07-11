//! Dev-task runner — the single source of truth for the check sequence that
//! both local development and CI run. `cargo xtask ci` runs exactly what the
//! CI workflow runs, so "green locally" and "green in CI" cannot diverge.
//!
//! Deliberately dependency-free (std only): a contributor needs nothing beyond
//! the pinned toolchain and `git`, both of which every dev machine and CI
//! runner already have. Cross-platform (no shell scripts) so Windows, macOS,
//! and Linux run the identical logic.
//!
//! Usage:
//!   cargo xtask ci           # format check + clippy + build + test + probes + drift-gate
//!   cargo xtask probe        # the deterministic probes only — what the container CI lane runs
//!   cargo xtask live-probe   # probes that spawn a real CLI; needs credentials, never on the PR tier
//!   cargo xtask drift-gate   # the reserved-pattern gate only

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
    // The signal, resize, and UTF-8 probes spawn their fixtures
    // (probe-child, resize-child, utf8-child) from a sibling package, which
    // `cargo run --bin <probe>` alone would not build.
    (
        "build the probe fixtures (probe-child, resize-child, utf8-child)",
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
];

fn main() {
    let task = std::env::args().nth(1).unwrap_or_default();
    let passed = match task.as_str() {
        "ci" => run_ci(),
        "probe" => run_steps(PROBE_STEPS),
        "live-probe" => run_live_probes(),
        "drift-gate" => drift_gate(),
        other => {
            eprintln!(
                "unknown xtask '{other}'. usage: cargo xtask <ci|probe|live-probe|drift-gate>"
            );
            exit(2);
        }
    };
    if !passed {
        exit(1);
    }
}

/// Run every step and the drift gate, reporting all failures rather than
/// stopping at the first, so one run surfaces every problem.
fn run_ci() -> bool {
    let checks = run_steps(STEPS);
    let probes = run_steps(PROBE_STEPS);
    // Run the gate regardless of earlier failures so one run reports everything.
    let gate = drift_gate();
    checks && probes && gate
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
    run_steps(LIVE_PROBE_STEPS)
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

/// Reserved-pattern drift gate. Some contradictions in this project's contracts
/// were re-introduced repeatedly after being fixed; this gate fails the build
/// when a tracked file re-pairs one of them, unless the head commit message
/// carries a `WAIVE-DRIFT: <reason>` line (the deliberate, auditable escape).
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
    None
}

fn head_commit_waives() -> bool {
    git(&["log", "-1", "--format=%B"])
        .is_some_and(|msg| msg.lines().any(|line| line.starts_with("WAIVE-DRIFT:")))
}

/// Run a `git` command, returning stdout on success.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}
