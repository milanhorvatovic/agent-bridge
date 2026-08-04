//! The system-wide monotonic clock, read as a plain number.
//!
//! `std::time::Instant` deliberately hides its counter: an `Instant` compares
//! only against another `Instant` from the same process. The `{ts}` token
//! exists to be compared across the process boundary — whoever reads this
//! process's output subtracts the reading embedded in a line from its own
//! reading of when that line arrived, and the difference is how long the
//! terminal took to deliver it. Both sides therefore read the same
//! system-wide counter directly, and neither needs a clock-synchronisation
//! protocol: there is only one clock.
//!
//! Each platform's source is the one `Instant` is itself built on —
//! `CLOCK_MONOTONIC` on Linux, `CLOCK_UPTIME_RAW` on Apple, the performance
//! counter on Windows — so a reading here and an `Instant` taken beside it
//! advance together, which is what lets a reader anchor the two once and
//! convert freely afterwards. `readings_track_instant` holds that property;
//! if a platform's `Instant` ever moves to a different source, that test
//! fails rather than the latency numbers quietly drifting.
//!
//! This is the one place the fake CLI talks to the OS rather than to std.
//! A timestamp that cannot be compared across the boundary would not be a
//! less precise measurement — it would be no measurement at all.

/// Nanoseconds from an unspecified but process-independent epoch. Only
/// differences are meaningful, and only between readings taken on the same
/// machine and the same boot.
#[cfg(unix)]
pub fn monotonic_ns() -> u64 {
    #[cfg(target_vendor = "apple")]
    const SOURCE: libc::clockid_t = libc::CLOCK_UPTIME_RAW;
    #[cfg(not(target_vendor = "apple"))]
    const SOURCE: libc::clockid_t = libc::CLOCK_MONOTONIC;

    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a live, correctly typed `timespec` that outlives the
    // call, and the call only writes through that pointer.
    let rc = unsafe { libc::clock_gettime(SOURCE, &mut ts) };
    // A supported clock id cannot fail here, and a silent zero would turn a
    // latency measurement into fiction — so this is an assertion, not a
    // fallback. The OS error rides along: if this ever trips, the errno is
    // the whole diagnosis.
    assert_eq!(
        rc,
        0,
        "clock_gettime({SOURCE}) failed: {}",
        std::io::Error::last_os_error()
    );
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64)
}

/// See the `unix` sibling. The performance counter's frequency is fixed for
/// the life of the boot, so it is read once; the tick-to-nanosecond scaling
/// goes through `u128` because a counter that has been running for years
/// overflows a `u64` the moment it is multiplied by a billion.
#[cfg(windows)]
pub fn monotonic_ns() -> u64 {
    use std::sync::OnceLock;

    use windows_sys::Win32::System::Performance::{
        QueryPerformanceCounter, QueryPerformanceFrequency,
    };

    static FREQUENCY: OnceLock<u64> = OnceLock::new();
    let frequency = *FREQUENCY.get_or_init(|| {
        let mut hz: i64 = 0;
        // SAFETY: `hz` is a live `i64` that outlives the call, which only
        // writes through the pointer.
        let ok = unsafe { QueryPerformanceFrequency(&mut hz) };
        assert!(
            ok != 0 && hz > 0,
            "QueryPerformanceFrequency failed: {}",
            std::io::Error::last_os_error()
        );
        hz as u64
    });
    let mut counter: i64 = 0;
    // SAFETY: as above — one live `i64`, written by the call.
    let ok = unsafe { QueryPerformanceCounter(&mut counter) };
    assert!(
        ok != 0,
        "QueryPerformanceCounter failed: {}",
        std::io::Error::last_os_error()
    );
    ((counter as u128 * 1_000_000_000) / u128::from(frequency)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn readings_advance() {
        let first = monotonic_ns();
        std::thread::sleep(Duration::from_millis(2));
        assert!(
            monotonic_ns() > first,
            "the monotonic clock must advance across a sleep"
        );
    }

    /// The property every cross-process latency number rests on: this clock
    /// and `Instant` are the same clock wearing different epochs, so a
    /// reader can anchor them once and convert between them forever after.
    /// A platform whose `Instant` moved to a different source would show up
    /// here as diverging elapsed times.
    #[test]
    fn readings_track_instant() {
        let instant_start = Instant::now();
        let clock_start = monotonic_ns();
        std::thread::sleep(Duration::from_millis(50));
        let by_instant = instant_start.elapsed().as_nanos() as u64;
        let by_clock = monotonic_ns() - clock_start;
        let skew = by_instant.abs_diff(by_clock);
        // The two pairs of readings are taken microseconds apart, so a
        // millisecond of slack is generous for sampling and stingy for a
        // genuinely different clock source (a suspend-counting clock against
        // a suspend-ignoring one diverges without bound).
        assert!(
            skew < 1_000_000,
            "clock and Instant disagree by {skew} ns over a 50 ms sleep"
        );
    }
}
