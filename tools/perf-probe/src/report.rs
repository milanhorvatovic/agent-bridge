//! The machine-readable result of a run.
//!
//! One file per lane, carrying what was measured, how much of it, against
//! which budget, and the verdict. Three readers, and the shape serves all
//! three: a person reading a number, a regression gate comparing two runs,
//! and the write-up that turns a set of runs into a keep-or-downgrade
//! recommendation per budget.
//!
//! Every measurement names the statistic its verdict rests on, because the
//! budgets are stated at P99 and a file that reported only "latency: 3 ms"
//! would leave the reader to guess whether that was a median someone had
//! rounded. Distributions travel whole for the same reason: a P99 inside
//! budget with a maximum three orders of magnitude above it is a finding,
//! not a pass, and only the distribution shows it.
//!
//! What is *not* here is any judgement about the machine. A CI runner shares
//! its host with neighbours; quiet hardware does not. The same budget can be
//! met on one and missed on the other, and both results are true. So a report
//! records where it ran and leaves the interpretation to whoever compares
//! them — see the `host` field, which is the one thing a reader must not
//! ignore when a verdict says "exceeded".

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::stats::Summary;

/// Bumped when a field changes meaning. A gate comparing two files with
/// different schema versions is comparing two different measurements.
pub const SCHEMA: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    /// The lane that produced this — `soak`, `bench-latency`, and so on.
    pub lane: String,
    /// What was streaming: the generated stream, or a named recording.
    pub workload: String,
    pub os: String,
    pub arch: String,
    /// Where this ran, as far as the probe can tell: a CI runner or an
    /// unidentified machine. A verdict of "exceeded" means something
    /// different on each.
    pub host: String,
    /// Wall-clock, for provenance only. No assertion anywhere reads it —
    /// every measured interval is a difference of monotonic readings.
    pub captured_unix_s: u64,
    /// Anything a reader needs in order not to misread the numbers: a
    /// compression factor, a platform behaviour, a shortened lane.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub measurements: Vec<Measurement>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Measurement {
    pub name: String,
    pub unit: String,
    /// The number the budget is checked against, and the number a regression
    /// gate compares between runs.
    pub value: u64,
    /// Which statistic `value` is, when it came from a distribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statistic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distribution: Option<Summary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A budget and the direction that satisfies it. The direction is not
/// decoration: it is what tells a regression gate whether a number that grew
/// got better or worse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Budget {
    /// Latencies and resource growth: at or under the limit.
    AtMost(u64),
    /// Throughput: at or above the floor.
    AtLeast(u64),
}

impl Budget {
    pub fn met_by(self, value: u64) -> bool {
        match self {
            Budget::AtMost(limit) => value <= limit,
            Budget::AtLeast(floor) => value >= floor,
        }
    }

    pub fn limit(self) -> u64 {
        match self {
            Budget::AtMost(value) | Budget::AtLeast(value) => value,
        }
    }

    /// Whether a larger number is a better one, for this budget.
    pub fn higher_is_better(self) -> bool {
        matches!(self, Budget::AtLeast(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Inside the budget on this machine.
    Met,
    /// Outside it. Not automatically a failure of the design — the gate this
    /// feeds accepts a documented downgrade as readily as a pass — but never
    /// something a run may leave unsaid.
    Exceeded,
    /// Measured, with no budget to hold it to.
    Unbudgeted,
}

impl Measurement {
    /// A measurement drawn from a distribution, judged at P99 — the
    /// statistic every latency budget is written against.
    pub fn from_p99(name: &str, unit: &str, summary: Summary, budget: Option<Budget>) -> Self {
        Self::scalar(name, unit, summary.p99, budget)
            .with_statistic("p99")
            .with_distribution(summary)
    }

    pub fn scalar(name: &str, unit: &str, value: u64, budget: Option<Budget>) -> Self {
        let verdict = match budget {
            Some(budget) if budget.met_by(value) => Verdict::Met,
            Some(_) => Verdict::Exceeded,
            None => Verdict::Unbudgeted,
        };
        Self {
            name: name.to_string(),
            unit: unit.to_string(),
            value,
            statistic: None,
            distribution: None,
            budget,
            verdict,
            note: None,
        }
    }

    pub fn with_statistic(mut self, statistic: &str) -> Self {
        self.statistic = Some(statistic.to_string());
        self
    }

    pub fn with_distribution(mut self, summary: Summary) -> Self {
        self.distribution = Some(summary);
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

impl Report {
    pub fn new(lane: &str, workload: &str) -> Self {
        Self {
            schema: SCHEMA,
            lane: lane.to_string(),
            workload: workload.to_string(),
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
            host: host_kind(),
            captured_unix_s: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or_default(),
            notes: Vec::new(),
            measurements: Vec::new(),
        }
    }

    pub fn note(&mut self, note: impl Into<String>) {
        self.notes.push(note.into());
    }

    pub fn add(&mut self, measurement: Measurement) {
        self.measurements.push(measurement);
    }

    /// Every measurement that missed its budget.
    pub fn exceeded(&self) -> Vec<&Measurement> {
        self.measurements
            .iter()
            .filter(|m| m.verdict == Verdict::Exceeded)
            .collect()
    }

    pub fn write(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("{}: {err}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|err| format!("serialising the report failed: {err}"))?;
        std::fs::write(path, format!("{json}\n"))
            .map_err(|err| format!("{}: {err}", path.display()))
    }

    pub fn read(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
        let report: Self =
            serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))?;
        if report.schema != SCHEMA {
            return Err(format!(
                "{}: report schema {} — this build reads schema {SCHEMA}, and two schemas are two \
                 different measurements",
                path.display(),
                report.schema
            ));
        }
        Ok(report)
    }
}

/// What kind of machine this is, as far as the environment admits. The
/// distinction exists because a budget missed on a shared CI runner and a
/// budget missed on quiet hardware are different findings, and only one of
/// them is about the code.
fn host_kind() -> String {
    if std::env::var_os("GITHUB_ACTIONS").is_some() {
        let runner = std::env::var("RUNNER_NAME").unwrap_or_else(|_| "unnamed".to_string());
        format!("ci-runner ({runner})")
    } else if std::env::var_os("CI").is_some() {
        "ci-runner (unidentified)".to_string()
    } else {
        "unidentified machine".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(p99: u64) -> Summary {
        Summary {
            count: 10_000,
            min: 1,
            p50: p99 / 2,
            p90: p99 - 1,
            p99,
            max: p99 * 4,
            mean: p99 / 2,
        }
    }

    #[test]
    fn a_latency_inside_its_budget_is_met() {
        let m = Measurement::from_p99(
            "first_byte_latency",
            "ns",
            summary(40_000_000),
            Some(Budget::AtMost(50_000_000)),
        );
        assert_eq!(m.verdict, Verdict::Met);
        assert_eq!(m.value, 40_000_000);
        assert_eq!(m.statistic.as_deref(), Some("p99"));
    }

    #[test]
    fn a_latency_over_its_budget_is_exceeded_not_silent() {
        let m = Measurement::from_p99(
            "input_forwarding_latency",
            "ns",
            summary(11_000_000),
            Some(Budget::AtMost(10_000_000)),
        );
        assert_eq!(m.verdict, Verdict::Exceeded);
    }

    #[test]
    fn throughput_budgets_read_the_other_way() {
        let floor = Budget::AtLeast(1000);
        assert!(floor.met_by(1000));
        assert!(floor.met_by(5000));
        assert!(!floor.met_by(999));
        assert!(floor.higher_is_better());
        assert!(!Budget::AtMost(10).higher_is_better());
    }

    #[test]
    fn a_measurement_without_a_budget_is_neither_pass_nor_fail() {
        let m = Measurement::scalar("aggregate_throughput", "lines_per_second", 4200, None);
        assert_eq!(m.verdict, Verdict::Unbudgeted);
    }

    #[test]
    fn a_report_round_trips_through_disk() {
        let path = std::env::temp_dir().join(format!(
            "agent-bridge-perf-report-test-{}.json",
            std::process::id()
        ));
        let mut report = Report::new("bench-latency", "synthetic");
        report.note("idle periods compressed 4x");
        report.add(Measurement::from_p99(
            "first_byte_latency",
            "ns",
            summary(3_000_000),
            Some(Budget::AtMost(50_000_000)),
        ));
        report.write(&path).expect("write must succeed");

        let read = Report::read(&path).expect("read must succeed");
        assert_eq!(read.lane, "bench-latency");
        assert_eq!(read.notes, vec!["idle periods compressed 4x".to_string()]);
        assert_eq!(read.measurements.len(), 1);
        assert_eq!(read.measurements[0].value, 3_000_000);
        assert_eq!(
            read.measurements[0].distribution.map(|d| d.count),
            Some(10_000)
        );
        assert!(read.exceeded().is_empty());
        std::fs::remove_file(&path).expect("cleanup");
    }

    #[test]
    fn a_report_from_another_schema_is_refused_rather_than_misread() {
        let path = std::env::temp_dir().join(format!(
            "agent-bridge-perf-schema-test-{}.json",
            std::process::id()
        ));
        let mut report = Report::new("soak", "synthetic");
        report.schema = SCHEMA + 1;
        report.write(&path).expect("write must succeed");
        let err = Report::read(&path).expect_err("a future schema must be refused");
        assert!(err.contains("schema"), "unexpected error: {err}");
        std::fs::remove_file(&path).expect("cleanup");
    }
}
