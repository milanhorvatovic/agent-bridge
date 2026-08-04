//! Detection spike — replays captured PTY fixtures through prototype
//! detection pipelines and measures what each pipeline recognizes.
//!
//! The capture half lives in `interactive-probe record`; this crate is the
//! measurement half. It never launches a CLI: every number it produces comes
//! from a deterministic replay of the committed fixtures under
//! `tests/corpus/<cli>/<version>/<scenario>-<cols>x<rows>/`, which is what
//! makes the results reviewable and the lanes cheap enough for the PR tier.
//!
//! Three pipeline configurations exist:
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
//! - **configuration b** (`config_b`): the screen-state pipeline. The same
//!   bytes feed a headless virtual terminal, and classification runs over
//!   the materialized viewport at evaluation points — quiet-period
//!   boundaries derived from the recorded timing and feed quiescence, never
//!   per byte. Repainted content is deduplicated against the previous
//!   evaluation point's screen, a screen-tuned pattern set evaluates each
//!   new row, and a menu-dialog detector reads dialog regions whole,
//!   options and selection included. What the virtual terminal buys over
//!   the stripped stream — and what it still misses — is the quantity
//!   under measurement.
//!
//! - **configuration c** (`config_c`): the structured-side-channel
//!   pipeline. The recorded hook payloads and transcript JSONL — the
//!   channels the planned runtime treats as primary — classify structurally
//!   against a fixed table instead of text needles, with the transcript
//!   read through the tailer's offset contract and hook ↔ transcript
//!   sightings correlated by `tool_use_id`. The byte stream serves only the
//!   fallback surfaces the side channels cannot carry (the trust dialog,
//!   the ask-degraded permission dialog, the interrupted notice), detected
//!   at the same evaluation points configuration (b) uses. Claude-only:
//!   the corpus records side channels for no other CLI. What the typed
//!   channels classify without patterns — and what still leaks to the
//!   fallback screen — is the quantity under measurement.
//!
//! The binary reports one machine-readable step line per replayed fixture on
//! stdout and exits non-zero (with a step-specific code) on the first hard
//! failure, the same contract as the probe binaries: CI asserts the exit
//! status, a human reads the step log.

// This crate legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

pub mod channel;
pub mod config_a;
pub mod config_b;
pub mod config_c;
pub mod corpus;
pub mod dialog;
pub mod metrics;
pub mod pacing;
pub mod patterns;
pub mod screen;
pub mod strip;
pub mod tailer;
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
