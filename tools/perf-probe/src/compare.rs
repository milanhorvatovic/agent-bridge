//! Did a change make it worse? Two reports from the same lane on the same
//! platform, compared measurement by measurement.
//!
//! The gate is *relative*, and that is a considered position: shared CI
//! runners differ machine to machine and minute to minute, so an absolute
//! budget enforced there would fail on the neighbours' noise. What a PR can
//! be held to is not "under 10 ms" but "not suddenly worse than the recorded
//! baseline" — and the threshold below is sized to runner noise, so anything
//! past it is signal that the change itself moved the number.
//!
//! Only latency percentiles gate. Throughput and resource numbers ride along
//! as reported drift for a reader to see, because their variance on shared
//! hardware is wider than any threshold that would still catch real
//! regressions; their absolute budgets are enforced where the numbers are
//! trustworthy, on the soak lanes and quiet hardware.
//!
//! The baseline is a committed file, updated deliberately. A gate that
//! compared against "whatever the last run produced" would ratchet every
//! slow drift into the new normal; a committed baseline makes raising it a
//! reviewed change with a diff. The bootstrap case — no baseline recorded
//! yet — passes loudly: the gate cannot hold a run to a number nobody has
//! recorded, and pretending otherwise would just be a differently shaped
//! silence.

use std::path::Path;

use crate::report::Report;
use crate::{human_ns, print_step};

/// How much worse a gated number may get before the comparison fails, in
/// percent. Sized to shared-runner noise: smaller regressions are
/// indistinguishable from neighbour load, larger ones are signal.
pub const REGRESSION_THRESHOLD_PERCENT: u64 = 20;

/// A measurement is gated when it is a latency percentile: statistic
/// present, nanosecond unit, and nothing declaring that bigger is better.
/// A percentile with no budget at all still gates — deliberately, so a
/// latency distribution someone adds without deciding its absolute budget
/// is regression-guarded from its first run; only an explicit
/// higher-is-better budget opts a measurement out.
fn gated(measurement: &crate::report::Measurement) -> bool {
    measurement.statistic.is_some()
        && measurement.unit == "ns"
        && measurement.budget.is_none_or(|b| !b.higher_is_better())
}

#[derive(Debug)]
pub struct Comparison {
    pub regressions: Vec<String>,
    pub improvements: Vec<String>,
    pub drift: Vec<String>,
}

impl Comparison {
    pub fn passed(&self) -> bool {
        self.regressions.is_empty()
    }
}

/// Compare a run against its baseline. `Err` means the two files are not
/// comparable at all — different lanes, different platforms — which is a
/// wiring mistake, not a regression.
pub fn compare(baseline: &Report, current: &Report) -> Result<Comparison, String> {
    if baseline.lane != current.lane {
        return Err(format!(
            "comparing lane {} against baseline lane {} — two lanes are two measurements",
            current.lane, baseline.lane
        ));
    }
    if baseline.os != current.os || baseline.arch != current.arch {
        return Err(format!(
            "comparing {}/{} against a {}/{} baseline — cross-platform latency deltas are \
             not evidence of anything",
            current.os, current.arch, baseline.os, baseline.arch
        ));
    }

    let mut comparison = Comparison {
        regressions: Vec::new(),
        improvements: Vec::new(),
        drift: Vec::new(),
    };
    for current_m in &current.measurements {
        let Some(baseline_m) = baseline
            .measurements
            .iter()
            .find(|m| m.name == current_m.name && m.statistic == current_m.statistic)
        else {
            comparison
                .drift
                .push(format!("{}: no baseline recorded", current_m.name));
            continue;
        };
        if baseline_m.unit != current_m.unit {
            return Err(format!(
                "{}: baseline is in {} and the current run in {} — raw numbers across \
                 units are not comparable; re-record the baseline",
                current_m.name, baseline_m.unit, current_m.unit
            ));
        }
        if !gated(current_m) {
            if baseline_m.value != current_m.value {
                comparison.drift.push(format!(
                    "{}: {} -> {} {} (ungated)",
                    current_m.name, baseline_m.value, current_m.value, current_m.unit
                ));
            }
            continue;
        }
        // Integer arithmetic on purpose: the comparison is a gate, and a
        // gate should not owe its verdict to floating-point rounding.
        let limit = baseline_m
            .value
            .saturating_mul(100 + REGRESSION_THRESHOLD_PERCENT)
            / 100;
        let improvement_mark = baseline_m
            .value
            .saturating_mul(100 - REGRESSION_THRESHOLD_PERCENT)
            / 100;
        if current_m.value > limit {
            comparison.regressions.push(format!(
                "{}: {} against a baseline of {} — past the {REGRESSION_THRESHOLD_PERCENT}% threshold ({})",
                current_m.name,
                human_ns(current_m.value),
                human_ns(baseline_m.value),
                human_ns(limit),
            ));
        } else if current_m.value < improvement_mark {
            comparison.improvements.push(format!(
                "{}: {} against a baseline of {} — worth noting in the change's description",
                current_m.name,
                human_ns(current_m.value),
                human_ns(baseline_m.value),
            ));
        }
    }
    // The other direction: a baselined measurement the current run no
    // longer carries. For a gated number that is not drift — removing or
    // renaming a measurement must not be a way past the gate, so its
    // disappearance fails the comparison until the baseline is deliberately
    // re-recorded. Ungated numbers disappearing are reported as drift.
    for baseline_m in &baseline.measurements {
        let present = current
            .measurements
            .iter()
            .any(|m| m.name == baseline_m.name && m.statistic == baseline_m.statistic);
        if present {
            continue;
        }
        if gated(baseline_m) {
            comparison.regressions.push(format!(
                "{}: baselined but absent from this run — a gated measurement cannot \
                 disappear silently; re-record the baseline if the removal is deliberate",
                baseline_m.name,
            ));
        } else {
            comparison
                .drift
                .push(format!("{}: no longer measured", baseline_m.name));
        }
    }
    Ok(comparison)
}

/// The gate as the command line runs it: compare the report at
/// `current_path` against the committed baseline at `baseline_path`, print
/// every finding, and say whether the gate passed. A missing baseline file
/// passes, loudly.
pub fn run(baseline_path: &Path, current_path: &Path) -> Result<bool, String> {
    let current = Report::read(current_path)?;
    if !baseline_path.exists() {
        print_step(
            "compare",
            "pass",
            &format!(
                "no baseline at {} — nothing to hold {} against; record one deliberately \
                 from a trusted run",
                baseline_path.display(),
                current.lane,
            ),
        );
        return Ok(true);
    }
    let baseline = Report::read(baseline_path)?;
    let comparison = compare(&baseline, &current)?;
    for finding in &comparison.regressions {
        print_step("compare", "fail", &format!("regression: {finding}"));
    }
    for finding in &comparison.improvements {
        print_step("compare", "pass", &format!("improvement: {finding}"));
    }
    for finding in &comparison.drift {
        print_step("compare", "pass", &format!("drift: {finding}"));
    }
    if comparison.passed() {
        print_step(
            "compare",
            "pass",
            &format!(
                "{} within {REGRESSION_THRESHOLD_PERCENT}% of its baseline",
                current.lane
            ),
        );
    }
    Ok(comparison.passed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Budget, Measurement, Report};
    use crate::stats::Summary;

    fn latency_report(p99_ns: u64) -> Report {
        let summary = Summary {
            count: 10_000,
            min: p99_ns / 10,
            p50: p99_ns / 2,
            p90: p99_ns * 9 / 10,
            p99: p99_ns,
            max: p99_ns * 3,
            mean: p99_ns / 2,
        };
        let mut report = Report::new("bench-latency", "synthetic");
        report.add(Measurement::from_p99(
            "first_byte_latency",
            "ns",
            summary,
            Some(Budget::AtMost(50_000_000)),
        ));
        report.add(Measurement::scalar(
            "aggregate_throughput",
            "lines_per_second",
            100_000,
            None,
        ));
        report
    }

    #[test]
    fn within_the_threshold_passes() {
        let comparison =
            compare(&latency_report(1_000_000), &latency_report(1_150_000)).expect("comparable");
        assert!(comparison.passed());
        assert!(comparison.improvements.is_empty());
    }

    #[test]
    fn past_the_threshold_fails_and_names_the_numbers() {
        let comparison =
            compare(&latency_report(1_000_000), &latency_report(1_300_000)).expect("comparable");
        assert!(!comparison.passed());
        assert!(
            comparison.regressions[0].contains("first_byte_latency"),
            "{:?}",
            comparison.regressions
        );
    }

    #[test]
    fn exactly_the_threshold_passes() {
        // The wording is "worse than 20%", not "20% or worse".
        let comparison =
            compare(&latency_report(1_000_000), &latency_report(1_200_000)).expect("comparable");
        assert!(comparison.passed());
    }

    #[test]
    fn a_marked_improvement_is_reported_not_silent() {
        let comparison =
            compare(&latency_report(1_000_000), &latency_report(500_000)).expect("comparable");
        assert!(comparison.passed());
        assert!(!comparison.improvements.is_empty());
    }

    #[test]
    fn ungated_numbers_drift_without_failing() {
        let baseline = latency_report(1_000_000);
        let mut current = latency_report(1_000_000);
        current
            .measurements
            .iter_mut()
            .find(|m| m.name == "aggregate_throughput")
            .expect("present")
            .value = 40_000;
        let comparison = compare(&baseline, &current).expect("comparable");
        assert!(comparison.passed(), "throughput does not gate");
        assert!(
            comparison
                .drift
                .iter()
                .any(|d| d.contains("aggregate_throughput")),
            "{:?}",
            comparison.drift
        );
    }

    #[test]
    fn a_gated_measurement_cannot_disappear_silently() {
        // Removing or renaming a baselined latency must fail the gate:
        // deletion would otherwise be the cheapest way past it. An ungated
        // number disappearing is only drift.
        let baseline = latency_report(1_000_000);
        let mut current = latency_report(1_000_000);
        current
            .measurements
            .retain(|m| m.name != "first_byte_latency");
        let comparison = compare(&baseline, &current).expect("comparable");
        assert!(!comparison.passed());
        assert!(
            comparison.regressions[0].contains("absent"),
            "{:?}",
            comparison.regressions
        );

        let mut current = latency_report(1_000_000);
        current
            .measurements
            .retain(|m| m.name != "aggregate_throughput");
        let comparison = compare(&baseline, &current).expect("comparable");
        assert!(
            comparison.passed(),
            "an ungated number disappearing is drift"
        );
        assert!(
            comparison
                .drift
                .iter()
                .any(|d| d.contains("no longer measured")),
            "{:?}",
            comparison.drift
        );
    }

    #[test]
    fn a_unit_change_is_refused_not_compared_raw() {
        // ns against ms compared as raw numbers would pass a thousandfold
        // regression; a unit change means the baseline must be re-recorded.
        let baseline = latency_report(1_000_000);
        let mut current = latency_report(1_000_000);
        current
            .measurements
            .iter_mut()
            .find(|m| m.name == "first_byte_latency")
            .expect("present")
            .unit = "ms".to_string();
        let err = compare(&baseline, &current).expect_err("must refuse");
        assert!(err.contains("not comparable"), "unexpected error: {err}");
    }

    #[test]
    fn cross_platform_comparison_is_refused() {
        let baseline = latency_report(1_000_000);
        let mut current = latency_report(1_000_000);
        current.os = "somewhere-else".to_string();
        let err = compare(&baseline, &current).expect_err("must refuse");
        assert!(err.contains("cross-platform"), "unexpected error: {err}");
    }

    #[test]
    fn cross_lane_comparison_is_refused() {
        let baseline = latency_report(1_000_000);
        let mut current = latency_report(1_000_000);
        current.lane = "soak".to_string();
        assert!(compare(&baseline, &current).is_err());
    }
}
