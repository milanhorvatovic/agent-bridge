//! Detection spike — replays captured PTY fixtures through prototype
//! detection pipelines and measures what each pipeline recognizes.
//!
//! The capture half lives in `interactive-probe record`; this crate is the
//! measurement half. It never launches a CLI: every number it produces comes
//! from a deterministic replay of the committed fixtures under
//! `tests/corpus/<cli>/<version>/<scenario>-<cols>x<rows>/`, which is what
//! makes the results reviewable and the lanes cheap enough for the PR tier.
//!
//! One pipeline configuration exists so far:
//!
//! - **configuration a** (`config_a`): the text-matching pipeline. Raw bytes
//!   are stripped of terminal control sequences, segmented into lines, and
//!   evaluated against a prototype set of literal and regex matchers behind
//!   an Aho-Corasick prefilter — the same algorithms the planned runtime
//!   names for its matcher engine, so the measured cost and the measured
//!   misses resemble the real thing. TUI repaints are deliberately *not*
//!   deduplicated and cursor-move fragmentation is deliberately *not*
//!   repaired here: how badly those hurt line-anchored matching is the
//!   quantity under measurement, not a defect to engineer away.
//!
//! The `screen` module is the front half of the screen-state configuration
//! that lands next: it feeds the same recorded bytes into a headless
//! virtual terminal and materializes the viewport at evaluation points,
//! where a later step classifies it.
//!
//! The structured-side-channel configuration lands as a later step of the
//! same spike.
//!
//! The binary reports one machine-readable step line per replayed fixture on
//! stdout and exits non-zero (with a step-specific code) on the first hard
//! failure, the same contract as the probe binaries: CI asserts the exit
//! status, a human reads the step log.

// This crate legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

pub mod config_a;
pub mod corpus;
pub mod metrics;
pub mod pacing;
pub mod patterns;
pub mod screen;
pub mod strip;
pub mod utf8;

/// A failed replay step: the diagnostic line plus the process exit code that
/// identifies the step to CI. Code ranges: 90 corpus discovery, 91 fixture
/// load, 92 pattern compilation, 93 report writing; 2 is a usage error.
pub struct Failure {
    pub step: &'static str,
    pub code: i32,
    pub detail: String,
}

impl Failure {
    pub fn new(step: &'static str, code: i32, detail: impl Into<String>) -> Self {
        Self {
            step,
            code,
            detail: detail.into(),
        }
    }
}

/// One machine-readable step line. Details are normalized to a single line
/// so the log stays parseable.
pub fn print_step(step: &str, status: &str, detail: &str) {
    let clean = detail.replace(['\r', '\n'], " ").replace('"', "'");
    println!("detection-spike step={step} status={status} detail=\"{clean}\"");
}
