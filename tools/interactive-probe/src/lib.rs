//! Interactive-CLI probe — spawns a real interactive TUI under a PTY (ConPTY
//! on Windows), reads the first token of output within a latency budget,
//! drives the session, and shuts it down cleanly.
//!
//! Two lanes share one launch rig:
//!
//! - **stand-in lane** (`standin`): a deterministic, credential-free
//!   raw-terminal fixture child (`interactive-standin`, the package's second
//!   binary) that paints a banner, repaints on request, and exits on command.
//!   This is the PR-tier CI lane on all three OSes.
//! - **live lane** (`probe`): the real Claude Code interactive TUI, launched
//!   with a preset `--session-id` and an injected `--settings` hooks file
//!   whose hook commands call this same binary in `hook-forward` mode to
//!   relay each hook payload to the probe over a local IPC channel (Unix
//!   domain socket on POSIX, named pipe on Windows). Needs the `claude`
//!   binary and credentials, so it runs in the opt-in live CI tier.
//!
//! A third lane, `fourpoint`, extends the live lane with the four-point
//! verification of the hook channel — hooks fire, their payloads reach the
//! listener, the external allow/deny/ask approval round-trip behaves as
//! designed, and a Ctrl+C byte interrupts generation without killing the
//! session. It exists for Windows, where the console is ConPTY and the
//! channel is a named pipe, but it runs everywhere: the POSIX run is the
//! baseline the Windows results are compared against.
//!
//! The probe reports one machine-readable step line per step on stdout and
//! exits non-zero (with a step-specific code) on the first hard failure, the
//! same contract as `pty-probe`: CI asserts the exit status, a human reads
//! the step log.

// This crate legitimately owns stdout — the step-result lines *are* its
// output, and in `hook-forward` mode stdout carries the decision JSON the
// hooked CLI reads — so it is exempt from the workspace-wide stdout-macro
// ban in clippy.toml.
#![allow(clippy::disallowed_macros)]

pub mod capture;
pub mod firsttoken;
pub mod fourpoint;
pub mod hooks;
pub mod pty;
pub mod rig;
pub mod standin;
pub mod utf8;
#[cfg(feature = "vt-eval")]
pub mod vt_eval;

/// Terminal dimensions for every child this probe spawns: the runtime's
/// documented default when a caller supplies none.
pub const COLS: u16 = 80;
pub const ROWS: u16 = 24;

/// A failed probe step: the diagnostic line plus the process exit code that
/// identifies the step to CI. Code ranges per lane: 20+ stand-in, 30+ live
/// probe, 50+ four-point, 60+ virtual-terminal evaluation; 2 is a usage
/// error.
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
    println!("interactive-probe step={step} status={status} detail=\"{clean}\"");
}

pub fn platform_report() -> String {
    format!(
        "os={} arch={} family={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
    )
}
