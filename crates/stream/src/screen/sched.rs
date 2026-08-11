//! When the screen is worth looking at.
//!
//! Reading the screen on every byte would be both wasteful and wrong: a CLI
//! paints a dialog over several writes, and a matcher that ran between two of
//! them would be matching against half a dialog. So the screen is examined at
//! **evaluation points**, and there are two ways to reach one — the output
//! went quiet for long enough that whatever was being drawn is finished, or
//! the source ran out of bytes to give.
//!
//! One evaluation point per burst of output, whichever signal arrives first.
//! Both signals mean the same thing about the screen — nothing more is coming
//! for now — so firing twice would ask the same question twice and get the
//! same answer, and the second answer would be an evaluation point with no
//! change behind it.
//!
//! Nothing here reads a clock. The caller supplies the time on every call and
//! asks [`EvalPointScheduler::deadline`] when to wake up next, which is what
//! lets the whole of this behavior be tested at exact instants instead of by
//! sleeping and hoping.

use std::time::{Duration, Instant};

/// How long output must stay quiet before the screen is worth looking at.
///
/// This is the security floor — the minimum quiet a prompt must be followed
/// by before it can be trusted — used as a sampling cadence. **It is not a
/// guarantee that a paint has finished**, and an earlier version of this
/// comment claimed it was. Recorded sessions show gaps of up to 400 ms
/// *inside* a burst of painting, between spinner frames and key echo, with
/// the settled-for-good boundary nearer 500 ms; a component that samples the
/// screen for measurement rather than for detection reasonably picks the
/// larger number.
///
/// This one picks the floor, and the trade is deliberate. A shorter window
/// samples more often, so a dialog is noticed sooner, and the cost of a
/// sample that lands mid-paint is absorbed: a half-drawn screen matches
/// nothing and the next sample sees the rest, while the repaint filter keeps
/// the extra looks from turning into extra events. Measured over the
/// recorded approval sessions, the shorter window produced 108 evaluation
/// points against 29 and **never once showed the dialog's question without
/// its answers** — which is the failure the larger window would be buying
/// protection from. That is an observation about recordings, not a promise:
/// a screen matcher must still tolerate a partial paint, because nothing
/// here can rule one out.
pub const QUIET_PERIOD: Duration = Duration::from_millis(100);

/// Why an evaluation point fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvalTrigger {
    /// Output stopped for [`QUIET_PERIOD`] after arriving.
    QuietPeriod,
    /// The source reported it has nothing pending.
    FeedQuiescence,
}

/// Decides when the screen should be examined.
///
/// A burst opens on the first bytes after an evaluation point and closes at
/// the next one. Between those, more bytes only push the quiet deadline out.
#[derive(Debug)]
pub struct EvalPointScheduler {
    quiet_period: Duration,
    /// When the most recent bytes arrived, while a burst is open. `None`
    /// between bursts, which is what makes both signals edge-triggered:
    /// nothing was written, so there is nothing new to look at.
    latest_write: Option<Instant>,
}

impl EvalPointScheduler {
    /// A scheduler using the standard [`QUIET_PERIOD`].
    pub fn new() -> Self {
        Self::with_quiet_period(QUIET_PERIOD)
    }

    /// A scheduler using a different quiet window — for an adapter whose CLI
    /// paints on a different rhythm, and for tests that would rather not
    /// wait.
    pub fn with_quiet_period(quiet_period: Duration) -> Self {
        Self {
            quiet_period,
            latest_write: None,
        }
    }

    /// Records that `bytes` bytes reached the screen at `now`.
    ///
    /// An empty read is not activity: a source that reports zero bytes has
    /// told us nothing changed, and treating it as the start of a burst would
    /// schedule an evaluation point for a screen nobody wrote to.
    pub fn on_feed(&mut self, now: Instant, bytes: usize) {
        if bytes > 0 {
            self.latest_write = Some(now);
        }
    }

    /// Fires an evaluation point if the quiet window has elapsed.
    ///
    /// Call it when [`deadline`](Self::deadline) comes due, or on any
    /// convenient tick — asking early is free and asking late only delays the
    /// point.
    pub fn poll(&mut self, now: Instant) -> Option<EvalTrigger> {
        let latest_write = self.latest_write?;
        (now.duration_since(latest_write) >= self.quiet_period).then(|| {
            self.latest_write = None;
            EvalTrigger::QuietPeriod
        })
    }

    /// Records that the source has nothing pending, firing an evaluation
    /// point if anything has been written since the last one.
    ///
    /// Takes no time because it needs none: the point is *now* by
    /// construction, and there is no window to measure.
    pub fn on_quiescent(&mut self) -> Option<EvalTrigger> {
        self.latest_write
            .take()
            .map(|_| EvalTrigger::FeedQuiescence)
    }

    /// When [`poll`](Self::poll) would next fire, so a caller with a timer
    /// can sleep exactly that long instead of polling.
    ///
    /// `None` means no burst is open and no timer is needed — the next
    /// [`on_feed`](Self::on_feed) is what starts one.
    pub fn deadline(&self) -> Option<Instant> {
        self.latest_write
            .map(|latest_write| latest_write + self.quiet_period)
    }
}

impl Default for EvalPointScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{EvalPointScheduler, EvalTrigger, QUIET_PERIOD};
    use std::time::{Duration, Instant};

    /// A fixed origin every case measures from, so every instant in a test is
    /// written as an offset and nothing depends on how long the test took.
    fn origin() -> Instant {
        Instant::now()
    }

    fn after(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn a_quiet_window_after_output_is_an_evaluation_point() {
        let start = origin();
        let mut scheduler = EvalPointScheduler::new();
        scheduler.on_feed(start, 12);
        assert_eq!(scheduler.poll(after(start, 99)), None);
        assert_eq!(
            scheduler.poll(after(start, 100)),
            Some(EvalTrigger::QuietPeriod)
        );
    }

    #[test]
    fn more_output_pushes_the_window_out_rather_than_firing_twice() {
        // A dialog painted over several writes must be examined once, after
        // the last of them — not once per write.
        let start = origin();
        let mut scheduler = EvalPointScheduler::new();
        scheduler.on_feed(start, 4);
        scheduler.on_feed(after(start, 60), 4);
        assert_eq!(scheduler.poll(after(start, 120)), None);
        assert_eq!(
            scheduler.poll(after(start, 160)),
            Some(EvalTrigger::QuietPeriod)
        );
    }

    #[test]
    fn the_quiet_window_fires_once_per_burst() {
        let start = origin();
        let mut scheduler = EvalPointScheduler::new();
        scheduler.on_feed(start, 4);
        assert!(scheduler.poll(after(start, 100)).is_some());
        assert_eq!(scheduler.poll(after(start, 5_000)), None);
    }

    #[test]
    fn a_drained_source_is_an_evaluation_point_without_waiting() {
        let start = origin();
        let mut scheduler = EvalPointScheduler::new();
        scheduler.on_feed(start, 4);
        assert_eq!(scheduler.on_quiescent(), Some(EvalTrigger::FeedQuiescence));
    }

    #[test]
    fn draining_twice_is_one_evaluation_point() {
        let start = origin();
        let mut scheduler = EvalPointScheduler::new();
        scheduler.on_feed(start, 4);
        assert!(scheduler.on_quiescent().is_some());
        assert_eq!(scheduler.on_quiescent(), None);
    }

    #[test]
    fn draining_closes_the_burst_the_quiet_window_was_measuring() {
        // Both signals say the same thing about the screen. Once one has
        // said it, the other has nothing to add.
        let start = origin();
        let mut scheduler = EvalPointScheduler::new();
        scheduler.on_feed(start, 4);
        assert!(scheduler.on_quiescent().is_some());
        assert_eq!(scheduler.poll(after(start, 1_000)), None);
    }

    #[test]
    fn a_source_that_never_wrote_has_nothing_to_evaluate() {
        let start = origin();
        let mut scheduler = EvalPointScheduler::new();
        assert_eq!(scheduler.on_quiescent(), None);
        assert_eq!(scheduler.poll(after(start, 10_000)), None);
        scheduler.on_feed(start, 0);
        assert_eq!(scheduler.poll(after(start, 10_000)), None);
    }

    #[test]
    fn the_deadline_is_the_quiet_window_from_the_last_write() {
        let start = origin();
        let mut scheduler = EvalPointScheduler::new();
        assert_eq!(scheduler.deadline(), None);
        scheduler.on_feed(start, 4);
        assert_eq!(scheduler.deadline(), Some(start + QUIET_PERIOD));
        scheduler.on_feed(after(start, 30), 4);
        assert_eq!(scheduler.deadline(), Some(after(start, 30) + QUIET_PERIOD));
        assert!(scheduler.poll(after(start, 130)).is_some());
        assert_eq!(scheduler.deadline(), None, "a closed burst needs no timer");
    }
}
