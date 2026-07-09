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
//!   cargo xtask probe        # just the PTY probe, both modes — what the container CI lane runs
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
const PROBE_STEPS: &[(&str, &[&str])] = &[
    (
        "pty-probe",
        &["run", "--quiet", "--package", "agent-bridge-pty-probe"],
    ),
    (
        "pty-probe (env defaults)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-pty-probe",
            "--",
            "--check-env",
        ],
    ),
];

fn main() {
    let task = std::env::args().nth(1).unwrap_or_default();
    let passed = match task.as_str() {
        "ci" => run_ci(),
        "probe" => run_steps(PROBE_STEPS),
        "drift-gate" => drift_gate(),
        other => {
            eprintln!("unknown xtask '{other}'. usage: cargo xtask <ci|probe|drift-gate>");
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
