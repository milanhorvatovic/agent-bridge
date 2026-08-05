//! Performance and endurance measurement over the PTY read path.
//!
//! Five lanes, one question each:
//!
//! - **soak** — does half an hour of continuous streaming arrive intact?
//!   A generated stream, checked line by line as it comes, so a failure says
//!   which line and when rather than "something went wrong".
//! - **replay** — does the same hold for a *real* workload? Recorded CLI
//!   sessions, replayed at their captured pacing. Real terminal traffic is
//!   bursty around tool calls and idle around thinking, and a measurement
//!   that only ever saw an even stream would pass a runtime that cannot
//!   survive the real shape. This lane is why the soak lane is not enough.
//! - **bench-latency** — what does the terminal cost, in first-byte delivery
//!   and in input forwarding? Both are differences between two processes'
//!   readings of one system clock, so neither includes a clock-sync estimate.
//! - **bench-throughput** — how much can one session sustain, and what
//!   happens to that number when sessions run alongside each other? The
//!   second half is the point: a per-session figure measured alone says
//!   nothing about what a runtime hosting many of them can promise.
//! - **compare** — did a change make any of it worse?
//!
//! Every lane writes the same machine-readable report and every budget
//! verdict is `met` or `exceeded`, never absent. A budget that cannot be met
//! is a finding to publish, and the one outcome that helps nobody is a run
//! that measured something and said nothing about it.
//!
//! This is measurement scaffolding. Nothing here becomes runtime code; the
//! numbers and the method are the durable output.

// This crate legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

pub mod clock;
pub mod compare;
pub mod latency;
pub mod lines;
pub mod monitor;
pub mod replay;
pub mod report;
pub mod session;
pub mod soak;
pub mod stats;
pub mod throughput;
pub mod verify;

/// One machine-readable step line, the same contract as the sibling probes:
/// CI asserts the exit status, a human reads the step log. Details are
/// flattened to one line so the log stays parseable.
pub fn print_step(step: &str, status: &str, detail: &str) {
    let clean = detail.replace(['\r', '\n'], " ").replace('"', "'");
    println!("perf-probe step={step} status={status} detail=\"{clean}\"");
}

pub fn platform_report() -> String {
    format!(
        "os={} arch={} family={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::consts::FAMILY,
    )
}

/// Bytes as a person reads them. Resource numbers appear in step lines that
/// someone skims; `11534336` and `11.0 MiB` carry the same information and
/// only one of them gets noticed.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

/// Nanoseconds as a person reads them, at whatever scale the number is.
pub fn human_ns(ns: u64) -> String {
    match ns {
        n if n >= 1_000_000_000 => format!("{:.2} s", n as f64 / 1e9),
        n if n >= 1_000_000 => format!("{:.2} ms", n as f64 / 1e6),
        n if n >= 1_000 => format!("{:.2} µs", n as f64 / 1e3),
        n => format!("{n} ns"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_counts_are_rendered_at_a_readable_scale() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(10 * 1024 * 1024), "10.0 MiB");
        assert_eq!(human_bytes(3 * (1 << 30)), "3.0 GiB");
    }

    #[test]
    fn durations_are_rendered_at_a_readable_scale() {
        assert_eq!(human_ns(900), "900 ns");
        assert_eq!(human_ns(1_500), "1.50 µs");
        assert_eq!(human_ns(50_000_000), "50.00 ms");
        assert_eq!(human_ns(1_800_000_000_000), "1800.00 s");
    }
}
