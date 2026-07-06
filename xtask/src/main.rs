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
//!   cargo xtask ci           # format check + clippy + build + test + selftest + drift-gate
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
    ("selftest", &["run", "--quiet", "--package", "ci-selftest"]),
];

fn main() {
    let task = std::env::args().nth(1).unwrap_or_default();
    let passed = match task.as_str() {
        "ci" => run_ci(),
        "drift-gate" => drift_gate(),
        other => {
            eprintln!("unknown xtask '{other}'. usage: cargo xtask <ci|drift-gate>");
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
    let mut passed = true;
    for &(name, args) in STEPS {
        if !cargo(name, args) {
            passed = false;
        }
    }
    // Run the gate regardless of earlier failures so one run reports everything.
    let gate = drift_gate();
    passed && gate
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
    let Some(listing) = git(&["ls-files"]) else {
        eprintln!("xtask: drift-gate: `git ls-files` failed");
        return false;
    };

    let mut violations = Vec::new();
    for path in listing.lines() {
        // This file and the workflow both name the patterns to define them.
        if path == "xtask/src/main.rs" || path == ".github/workflows/ci.yml" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
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
    // A backfill-gap JSON-RPC error code tied to the subscription-attach method
    // in the same file: the gap is a payload field, never that error.
    let attach_error = format!("-{}", "32004");
    if text.contains(&attach_error) && text.contains("session.attach") {
        return Some(format!("{attach_error} paired with session.attach"));
    }
    // The virtual terminal / screen state described as owned by the PTY layer:
    // it is owned by the Stream + Event layer.
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
