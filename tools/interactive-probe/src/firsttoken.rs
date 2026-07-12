//! Output-timing measurements. Two deltas matter to this probe:
//!
//! - **launch-side first token**: spawn → first byte readable from the PTY
//!   master. The pre-interaction paint (a trust dialog, a banner) counts —
//!   the question is "does the child talk to our terminal at all, and how
//!   fast", not "when does the model answer". This is the gated
//!   measurement.
//! - **first output after submit**: prompt submitted → next byte out. For a
//!   TUI this is the composer's own repaint, which lands within
//!   milliseconds — *not* the model's first token. Separating a model token
//!   from a repaint needs output detection, which is a later probe's
//!   subject; this number is logged as what it is and gates nothing.
//!
//! The clock is fed timestamps rather than reading time itself, so tests
//! drive it without sleeping.

use std::time::{Duration, Instant};

pub struct FirstTokenClock {
    spawn: Instant,
    first_byte: Option<Instant>,
    submit: Option<Instant>,
    first_after_submit: Option<Instant>,
}

impl FirstTokenClock {
    pub fn new(spawn: Instant) -> Self {
        Self {
            spawn,
            first_byte: None,
            submit: None,
            first_after_submit: None,
        }
    }

    /// Record an output chunk's arrival. The first call fixes the launch
    /// measurement; the first call after `note_submit` fixes the
    /// after-submit one.
    pub fn note_chunk(&mut self, at: Instant) {
        if self.first_byte.is_none() {
            self.first_byte = Some(at);
        }
        if self.submit.is_some() && self.first_after_submit.is_none() {
            self.first_after_submit = Some(at);
        }
    }

    /// Record the prompt-submit instant (the Enter keypress reaching the
    /// child). Only the first submit is measured; a probe session submits
    /// its measured prompt once.
    pub fn note_submit(&mut self, at: Instant) {
        if self.submit.is_none() {
            self.submit = Some(at);
        }
    }

    /// The spawn instant this clock measures from — the zero point every
    /// recorded timestamp (byte chunks, hook arrivals, driver steps) shares,
    /// so a session's artifacts can be interleaved by time after the fact.
    pub fn spawn_instant(&self) -> Instant {
        self.spawn
    }

    /// Spawn → first output byte. The gated first-token measurement.
    pub fn launch_latency(&self) -> Option<Duration> {
        self.first_byte
            .map(|at| at.saturating_duration_since(self.spawn))
    }

    /// Prompt submit → next output byte. See the module note: for a TUI
    /// this is the composer repaint, not the model's first token.
    pub fn first_output_after_submit(&self) -> Option<Duration> {
        match (self.submit, self.first_after_submit) {
            (Some(submit), Some(first)) => Some(first.saturating_duration_since(submit)),
            _ => None,
        }
    }

    /// Prompt submit → `at`, for reporting how long the turn took once the
    /// caller observes its end.
    pub fn since_submit(&self, at: Instant) -> Option<Duration> {
        self.submit
            .map(|submit| at.saturating_duration_since(submit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_latency_is_spawn_to_first_chunk_only() {
        let t0 = Instant::now();
        let mut clock = FirstTokenClock::new(t0);
        assert_eq!(clock.launch_latency(), None);
        clock.note_chunk(t0 + Duration::from_millis(120));
        clock.note_chunk(t0 + Duration::from_millis(900));
        assert_eq!(clock.launch_latency(), Some(Duration::from_millis(120)));
    }

    #[test]
    fn first_output_after_submit_ignores_pre_submit_chunks() {
        let t0 = Instant::now();
        let mut clock = FirstTokenClock::new(t0);
        clock.note_chunk(t0 + Duration::from_millis(100));
        clock.note_submit(t0 + Duration::from_millis(500));
        assert_eq!(clock.first_output_after_submit(), None);
        clock.note_chunk(t0 + Duration::from_millis(650));
        clock.note_chunk(t0 + Duration::from_millis(700));
        assert_eq!(
            clock.first_output_after_submit(),
            Some(Duration::from_millis(150))
        );
    }

    #[test]
    fn only_the_first_submit_is_measured() {
        let t0 = Instant::now();
        let mut clock = FirstTokenClock::new(t0);
        clock.note_submit(t0 + Duration::from_millis(100));
        clock.note_submit(t0 + Duration::from_millis(400));
        clock.note_chunk(t0 + Duration::from_millis(500));
        assert_eq!(
            clock.first_output_after_submit(),
            Some(Duration::from_millis(400))
        );
    }

    #[test]
    fn turn_duration_is_measured_from_the_submit() {
        let t0 = Instant::now();
        let mut clock = FirstTokenClock::new(t0);
        assert_eq!(clock.since_submit(t0 + Duration::from_secs(1)), None);
        clock.note_submit(t0 + Duration::from_millis(500));
        assert_eq!(
            clock.since_submit(t0 + Duration::from_millis(4_500)),
            Some(Duration::from_secs(4))
        );
    }
}
