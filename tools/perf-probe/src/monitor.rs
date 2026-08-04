//! What the probe process is holding, sampled over the life of a run.
//!
//! Two budgets ride on this: no descriptor leak (handles, on Windows) and no
//! more than a bounded amount of resident-memory growth. Both are stated as
//! *growth*, which makes when you start measuring part of the measurement.
//! A process climbs while it starts up — lazily mapped pages, thread stacks,
//! the spawn of the child, the first allocations of the reader — and none of
//! that is a leak. So the assessment ignores a documented warm-up window and
//! compares the first sample after it against the last one. A leak is what
//! keeps growing once the run is in steady state; anything measured from
//! process start would report the startup ramp as the finding.
//!
//! The series is written as it is sampled, not at the end. A soak that dies
//! at minute 27 is exactly the run whose resource curve someone wants to
//! read.
//!
//! Sampling is per-platform by necessity — an open descriptor is a directory
//! entry on Linux, a syscall on macOS, and a differently-shaped question on
//! Windows, where the kernel counts every handle rather than file
//! descriptors. The platform split is one function each, behind one shape,
//! and the Windows number is labelled as handles wherever it is reported:
//! the two are not the same quantity and a report that blurred them would
//! invite a cross-platform comparison that means nothing.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::clock::monotonic_ns;

/// One reading of the probe process's resource use.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Sample {
    /// Nanoseconds since the monitor started.
    pub t_ns: u64,
    /// Open file descriptors (POSIX) or open kernel handles (Windows).
    pub descriptors: u64,
    pub rss_bytes: u64,
}

/// How often to sample when a lane does not say otherwise.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(10);

/// How much of a run's start to leave out of the growth assessment. Long
/// enough to cover process start, the child spawn, and the first minute of
/// streaming — the whole ramp — and short enough to leave the overwhelming
/// majority of a half-hour run inside the window a leak would have to show
/// up in.
pub const DEFAULT_WARMUP: Duration = Duration::from_secs(60);

/// The resident-growth budget: a run may end at most this much above its
/// steady-state baseline.
pub const RSS_GROWTH_BUDGET_BYTES: u64 = 10 * 1024 * 1024;

/// A sampler running alongside a measured run.
pub struct Monitor {
    stop: Arc<AtomicBool>,
    worker: JoinHandle<Result<Vec<Sample>, String>>,
}

impl Monitor {
    /// Start sampling this process every `interval`, appending each sample
    /// to `out` as NDJSON if a path is given.
    pub fn start(interval: Duration, out: Option<PathBuf>) -> Result<Self, String> {
        // Fail here rather than inside the worker: a monitor that cannot
        // read its own process is a broken lane, and finding that out at the
        // end of a thirty-minute run helps nobody.
        let first = read_sample(0)?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || sample_loop(first, interval, out, &stop))
        };
        Ok(Self { stop, worker })
    }

    /// Stop sampling and collect the series.
    pub fn stop(self) -> Result<Vec<Sample>, String> {
        self.stop.store(true, Ordering::Relaxed);
        self.worker
            .join()
            .map_err(|_| "the resource sampler panicked".to_string())?
    }
}

fn sample_loop(
    first: Sample,
    interval: Duration,
    out: Option<PathBuf>,
    stop: &AtomicBool,
) -> Result<Vec<Sample>, String> {
    let mut writer = match out {
        Some(path) => Some((
            std::fs::File::create(&path).map_err(|err| format!("{}: {err}", path.display()))?,
            path,
        )),
        None => None,
    };
    let started_ns = monotonic_ns();
    let mut samples = Vec::new();
    let mut sample = first;
    loop {
        if let Some((file, path)) = writer.as_mut() {
            let line = serde_json::to_string(&sample)
                .map_err(|err| format!("serialising a sample failed: {err}"))?;
            writeln!(file, "{line}").map_err(|err| format!("{}: {err}", path.display()))?;
            file.flush()
                .map_err(|err| format!("{}: {err}", path.display()))?;
        }
        samples.push(sample);
        // Poll the stop flag far more often than the sampling interval, so
        // ending a run does not wait out a ten-second sleep.
        let due = monotonic_ns() + interval.as_nanos() as u64;
        while monotonic_ns() < due {
            if stop.load(Ordering::Relaxed) {
                return Ok(samples);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        sample = read_sample(monotonic_ns() - started_ns)?;
    }
}

/// The growth verdict over a series.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct Assessment {
    pub samples: usize,
    /// Index of the sample the growth is measured from — the first at or
    /// after the warm-up window.
    pub baseline_index: usize,
    pub baseline_descriptors: u64,
    pub final_descriptors: u64,
    pub descriptor_delta: i64,
    pub baseline_rss_bytes: u64,
    pub final_rss_bytes: u64,
    pub rss_growth_bytes: i64,
    /// Highest resident reading anywhere after the baseline. A run that
    /// ballooned and then released would otherwise report a clean delta.
    pub peak_rss_bytes: u64,
    pub descriptors_leaked: bool,
    pub rss_over_budget: bool,
}

impl Assessment {
    pub fn within_budget(&self) -> bool {
        !self.descriptors_leaked && !self.rss_over_budget
    }
}

/// Assess a series against the budgets, ignoring everything inside the
/// warm-up window. `None` if no sample falls outside it — a run too short to
/// have a steady state has no growth to report, and inventing one from two
/// startup samples would be worse than saying so.
pub fn assess(samples: &[Sample], warmup: Duration) -> Option<Assessment> {
    let warmup_ns = warmup.as_nanos() as u64;
    let baseline_index = samples.iter().position(|s| s.t_ns >= warmup_ns)?;
    // A baseline that is also the last sample gives a delta of zero over no
    // elapsed time, which is not a measurement of anything.
    if baseline_index + 1 >= samples.len() {
        return None;
    }
    let baseline = samples[baseline_index];
    let last = samples[samples.len() - 1];
    let after_baseline = &samples[baseline_index..];
    let descriptor_delta = i64::try_from(last.descriptors).unwrap_or(i64::MAX)
        - i64::try_from(baseline.descriptors).unwrap_or(i64::MAX);
    let rss_growth_bytes = i64::try_from(last.rss_bytes).unwrap_or(i64::MAX)
        - i64::try_from(baseline.rss_bytes).unwrap_or(i64::MAX);
    Some(Assessment {
        samples: samples.len(),
        baseline_index,
        baseline_descriptors: baseline.descriptors,
        final_descriptors: last.descriptors,
        descriptor_delta,
        baseline_rss_bytes: baseline.rss_bytes,
        final_rss_bytes: last.rss_bytes,
        rss_growth_bytes,
        peak_rss_bytes: after_baseline
            .iter()
            .map(|s| s.rss_bytes)
            .max()
            .unwrap_or(baseline.rss_bytes),
        // Net zero is the budget: descriptors opened during the run must be
        // closed by the end of it. Ending below the baseline is not a leak.
        descriptors_leaked: descriptor_delta > 0,
        rss_over_budget: rss_growth_bytes > RSS_GROWTH_BUDGET_BYTES as i64,
    })
}

/// Read this process's resource use now.
pub fn read_sample(t_ns: u64) -> Result<Sample, String> {
    Ok(Sample {
        t_ns,
        descriptors: open_descriptors()?,
        rss_bytes: resident_bytes()?,
    })
}

/// The word this platform's number deserves in a report.
pub const DESCRIPTOR_NOUN: &str = if cfg!(windows) {
    "open handles"
} else {
    "open file descriptors"
};

#[cfg(target_os = "linux")]
fn open_descriptors() -> Result<u64, String> {
    let entries = std::fs::read_dir("/proc/self/fd")
        .map_err(|err| format!("/proc/self/fd: {err}"))?
        .count() as u64;
    // Reading the directory needs a descriptor of its own, and it is in the
    // listing it produced. Counting it would put a constant offset on every
    // sample — harmless for a delta, misleading in the series someone reads.
    Ok(entries.saturating_sub(1))
}

#[cfg(target_os = "linux")]
fn resident_bytes() -> Result<u64, String> {
    let statm = std::fs::read_to_string("/proc/self/statm")
        .map_err(|err| format!("/proc/self/statm: {err}"))?;
    let resident_pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("/proc/self/statm: no resident field in {statm:?}"))?
        .parse()
        .map_err(|err| format!("/proc/self/statm: unreadable resident field: {err}"))?;
    // SAFETY: `sysconf` takes an integer name and returns an integer; there
    // are no pointers and no state to get wrong.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err("sysconf(_SC_PAGESIZE) gave no usable page size".to_string());
    }
    Ok(resident_pages * page_size as u64)
}

#[cfg(target_vendor = "apple")]
fn open_descriptors() -> Result<u64, String> {
    // Two calls: the null-buffer form answers with a buffer-size *estimate*
    // (the kernel pads it for growth), so only the byte count a real listing
    // actually writes is an exact count. Sampling against the estimate reads
    // steady when descriptors leak below the padding — precisely the failure
    // this monitor exists to catch.
    let pid = std::process::id() as i32;
    // SAFETY: the null-buffer form is the documented way to size the buffer;
    // nothing is written through the pointer.
    let estimate =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if estimate < 0 {
        return Err(format!(
            "proc_pidinfo(PROC_PIDLISTFDS) failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    // Headroom over the estimate, so descriptors opened between the two
    // calls still fit — and a retry loop behind it, because a listing that
    // exactly fills its buffer may have been silently truncated, and a
    // monitor that undercounts descriptors is a monitor that misses leaks.
    // The loop terminates: capacity doubles past any real descriptor table.
    let mut capacity = (estimate as usize / entry) + 64;
    loop {
        let mut buffer: Vec<libc::proc_fdinfo> = Vec::with_capacity(capacity);
        // SAFETY: the buffer really owns `capacity` entries of spare room,
        // and the call writes at most that many bytes.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                buffer.as_mut_ptr().cast(),
                (capacity * entry) as i32,
            )
        };
        if written < 0 {
            return Err(format!(
                "proc_pidinfo(PROC_PIDLISTFDS) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        if (written as usize) < capacity * entry {
            return Ok(written as u64 / entry as u64);
        }
        capacity *= 2;
    }
}

#[cfg(target_vendor = "apple")]
fn resident_bytes() -> Result<u64, String> {
    let mut info = std::mem::MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_taskinfo>() as i32;
    // SAFETY: the buffer is a correctly sized, zeroed `proc_taskinfo` that
    // outlives the call, and `size` describes exactly it.
    let written = unsafe {
        libc::proc_pidinfo(
            std::process::id() as i32,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return Err(format!(
            "proc_pidinfo(PROC_PIDTASKINFO) wrote {written} of {size} bytes: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: the call above filled the whole struct, as just checked.
    Ok(unsafe { info.assume_init() }.pti_resident_size)
}

#[cfg(windows)]
fn open_descriptors() -> Result<u64, String> {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessHandleCount};

    let mut count: u32 = 0;
    // SAFETY: the pseudo-handle from `GetCurrentProcess` needs no closing,
    // and `count` is a live `u32` the call writes once.
    let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
    if ok == 0 {
        return Err(format!(
            "GetProcessHandleCount failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(u64::from(count))
}

#[cfg(windows)]
fn resident_bytes() -> Result<u64, String> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    // SAFETY: `counters` is a live, correctly sized structure whose `cb`
    // field describes it, which is what the call requires.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    };
    if ok == 0 {
        return Err(format!(
            "GetProcessMemoryInfo failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    // The working set is the Windows spelling of resident memory: the pages
    // actually in physical memory for this process.
    Ok(counters.WorkingSetSize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(t_ns: u64, descriptors: u64, rss_bytes: u64) -> Sample {
        Sample {
            t_ns,
            descriptors,
            rss_bytes,
        }
    }

    #[test]
    fn a_sample_reads_something_plausible() {
        let sample = read_sample(0).expect("this process must be readable");
        assert!(
            sample.descriptors >= 3,
            "a running process holds at least its standard streams, got {}",
            sample.descriptors
        );
        assert!(
            sample.rss_bytes > 512 * 1024,
            "a running process is resident, got {} bytes",
            sample.rss_bytes
        );
    }

    /// The sampler's whole job: descriptors it did not have before must show
    /// up. Planting the leak proves the counter tracks reality rather than
    /// returning a plausible constant. The count is process-global and the
    /// test harness runs sibling tests on other threads, so the plant is
    /// sized well past their few-descriptor churn and the assertions leave
    /// that much slack — the signal under test is 32, the ambient noise is
    /// single digits.
    #[test]
    fn the_sampler_counts_a_planted_leak() {
        const PLANTED: u64 = 32;
        const NOISE_SLACK: u64 = 8;
        let before = read_sample(0).expect("readable").descriptors;
        let openable = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        let planted: Vec<std::fs::File> = (0..PLANTED)
            .map(|_| std::fs::File::open(&openable).expect("open must succeed"))
            .collect();
        let during = read_sample(0).expect("readable").descriptors;
        drop(planted);
        let after = read_sample(0).expect("readable").descriptors;

        assert!(
            during >= before + PLANTED - NOISE_SLACK,
            "{PLANTED} planted descriptors must be visible: {before} -> {during}"
        );
        assert!(
            after <= during - PLANTED + NOISE_SLACK,
            "closing them must be visible too: {during} -> {after}"
        );
    }

    #[test]
    fn growth_is_measured_from_after_the_warm_up_window() {
        let samples = [
            sample(0, 100, 50_000_000), // startup ramp — must not be the baseline
            sample(60_000_000_000, 20, 10_000_000),
            sample(120_000_000_000, 20, 11_000_000),
        ];
        let assessment =
            assess(&samples, Duration::from_secs(60)).expect("a series past the warm-up assesses");
        assert_eq!(assessment.baseline_index, 1);
        assert_eq!(assessment.descriptor_delta, 0);
        assert_eq!(assessment.rss_growth_bytes, 1_000_000);
        assert!(assessment.within_budget());
    }

    #[test]
    fn a_descriptor_that_stays_open_is_a_leak() {
        let samples = [
            sample(60_000_000_000, 20, 10_000_000),
            sample(120_000_000_000, 21, 10_000_000),
        ];
        let assessment = assess(&samples, Duration::from_secs(60)).expect("assessable");
        assert_eq!(assessment.descriptor_delta, 1);
        assert!(assessment.descriptors_leaked);
        assert!(!assessment.within_budget());
    }

    #[test]
    fn ending_below_the_baseline_is_not_a_leak() {
        let samples = [
            sample(60_000_000_000, 25, 12_000_000),
            sample(120_000_000_000, 20, 10_000_000),
        ];
        let assessment = assess(&samples, Duration::from_secs(60)).expect("assessable");
        assert_eq!(assessment.descriptor_delta, -5);
        assert!(!assessment.descriptors_leaked);
        assert!(assessment.within_budget());
    }

    #[test]
    fn growth_past_the_budget_fails() {
        let samples = [
            sample(60_000_000_000, 20, 10_000_000),
            sample(
                120_000_000_000,
                20,
                10_000_000 + RSS_GROWTH_BUDGET_BYTES + 1,
            ),
        ];
        let assessment = assess(&samples, Duration::from_secs(60)).expect("assessable");
        assert!(assessment.rss_over_budget);
        assert!(!assessment.within_budget());
    }

    #[test]
    fn a_spike_that_was_released_still_shows_in_the_peak() {
        let samples = [
            sample(60_000_000_000, 20, 10_000_000),
            sample(90_000_000_000, 20, 900_000_000),
            sample(120_000_000_000, 20, 10_000_000),
        ];
        let assessment = assess(&samples, Duration::from_secs(60)).expect("assessable");
        assert_eq!(assessment.rss_growth_bytes, 0);
        assert_eq!(assessment.peak_rss_bytes, 900_000_000);
    }

    #[test]
    fn a_run_too_short_to_have_a_steady_state_reports_nothing() {
        assert!(assess(&[sample(0, 20, 10_000_000)], Duration::from_secs(60)).is_none());
        // A single sample past the window is a baseline with nothing to
        // compare against.
        assert!(
            assess(
                &[
                    sample(0, 20, 10_000_000),
                    sample(61_000_000_000, 20, 10_000_000)
                ],
                Duration::from_secs(60)
            )
            .is_none()
        );
    }

    #[test]
    fn the_monitor_writes_its_series_as_it_goes() {
        let path = std::env::temp_dir().join(format!(
            "agent-bridge-perf-monitor-test-{}.ndjson",
            std::process::id()
        ));
        let monitor = Monitor::start(Duration::from_millis(60), Some(path.clone()))
            .expect("the monitor must start");
        std::thread::sleep(Duration::from_millis(250));
        let samples = monitor.stop().expect("the monitor must stop cleanly");
        assert!(
            samples.len() >= 3,
            "expected several samples in 250 ms, got {}",
            samples.len()
        );
        let written = std::fs::read_to_string(&path).expect("the series must be on disk");
        assert_eq!(
            written.lines().count(),
            samples.len(),
            "every sample must reach the file as it is taken"
        );
        assert!(
            samples.windows(2).all(|pair| pair[1].t_ns > pair[0].t_ns),
            "samples must carry advancing timestamps"
        );
        std::fs::remove_file(&path).expect("cleanup");
    }
}
