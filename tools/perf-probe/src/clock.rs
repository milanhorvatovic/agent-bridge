//! Reading the same clock the child reads.
//!
//! Every latency number this probe produces is a subtraction between two
//! processes: the child stamps a line as it writes it, the probe stamps the
//! read that delivered it, and the difference is what the terminal cost. That
//! only means something if both stamps come from one clock, which is why the
//! child's `{ts}` token and this module read the same system counter — see
//! `agent_bridge_fake_cli::clock` for which counter, per platform, and why.
//!
//! The reader thread hands out `Instant`s, because that is what a Rust
//! reader naturally records. [`Anchor`] converts them: it pairs one `Instant`
//! with one counter reading, and every later conversion is that pair plus an
//! elapsed time. The conversion is exact rather than approximate because the
//! two clocks are the same clock — `readings_track_instant` in the fake CLI
//! holds that, and would fail loudly on a platform where it stopped being
//! true.

use std::time::Instant;

pub use agent_bridge_fake_cli::clock::monotonic_ns;

/// A fixed correspondence between this process's `Instant`s and the shared
/// counter, taken once and used for the life of a run.
#[derive(Clone, Copy, Debug)]
pub struct Anchor {
    instant: Instant,
    ns: u64,
}

impl Anchor {
    /// Pair the two clocks. The readings are taken back to back, so the pair
    /// is off by the time between the two calls — tens of nanoseconds, four
    /// orders of magnitude below the budgets being measured.
    pub fn take() -> Self {
        Self {
            instant: Instant::now(),
            ns: monotonic_ns(),
        }
    }

    /// What the shared counter read at `at`. Handles instants from before
    /// the anchor — a reader thread starts before the lane that anchors it.
    pub fn ns_at(&self, at: Instant) -> u64 {
        if at >= self.instant {
            self.ns
                .saturating_add(at.duration_since(self.instant).as_nanos() as u64)
        } else {
            self.ns
                .saturating_sub(self.instant.duration_since(at).as_nanos() as u64)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn conversion_agrees_with_a_reading_taken_beside_it() {
        let anchor = Anchor::take();
        std::thread::sleep(Duration::from_millis(20));
        let at = Instant::now();
        let direct = monotonic_ns();
        let converted = anchor.ns_at(at);
        assert!(
            direct.abs_diff(converted) < 1_000_000,
            "converting an Instant taken beside a direct reading disagreed by {} ns",
            direct.abs_diff(converted)
        );
    }

    #[test]
    fn instants_from_before_the_anchor_convert_backwards() {
        let earlier = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        let anchor = Anchor::take();
        assert!(
            anchor.ns_at(earlier) < anchor.ns,
            "an instant from before the anchor must convert to an earlier reading"
        );
    }
}
