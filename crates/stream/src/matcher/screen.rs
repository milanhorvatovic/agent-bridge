//! The screen kind's seat in the pipeline: a reconstructed screen, its
//! evaluation-point scheduler, and the engine's screen pass, driven as one.
//!
//! Screen matchers evaluate at *evaluation points* — once per burst of
//! output, at whichever of the quiet-period boundary and feed quiescence a
//! burst reaches first — never per byte and never per line. The scheduler
//! that decides those points is a pure state machine over injected
//! instants, and this slot deliberately stays one too: no channel, no
//! timer, no runtime. The owning task feeds bytes as they arrive, arms its
//! own timer from [`ScreenSlot::deadline`], and calls [`ScreenSlot::poll`]
//! when it fires (or [`ScreenSlot::on_quiescent`] when the feed drains);
//! matches come back as return values on the caller's own thread. A
//! delivery channel would only add a hop between two things the same task
//! already owns.
//!
//! The whole slot is inert for sessions that keep no screen — the
//! effective `tui_aware` decision is made once, at construction, by the
//! screen state itself — and a snapshot is only ever materialized when a
//! screen matcher is registered to read it.

use std::time::Instant;

use agent_bridge_events::EventBody;

use super::engine::MatcherEngine;
use crate::screen::{EvalPointScheduler, ScreenState};

/// One session's screen pass: the reconstructed screen and the scheduler
/// that decides when the engine looks at it.
#[derive(Debug)]
pub struct ScreenSlot {
    state: ScreenState,
    scheduler: EvalPointScheduler,
}

impl ScreenSlot {
    /// A slot for one session. `tui_aware_effective` is the session's
    /// resolved setting — adapter override if set, else the runtime
    /// default; when it is `false` the slot keeps no screen and every call
    /// is a cheap no-op.
    pub fn new(cols: u16, rows: u16, tui_aware_effective: bool) -> Self {
        Self {
            state: ScreenState::new(cols, rows, tui_aware_effective),
            scheduler: EvalPointScheduler::new(),
        }
    }

    /// A slot whose quiet period is not the standard one — for adapters
    /// whose CLI paints on a different rhythm, and for tests that would
    /// rather not wait.
    pub fn with_scheduler(
        cols: u16,
        rows: u16,
        tui_aware_effective: bool,
        scheduler: EvalPointScheduler,
    ) -> Self {
        Self {
            state: ScreenState::new(cols, rows, tui_aware_effective),
            scheduler,
        }
    }

    /// Raw output bytes, as read from the terminal — the same tee the
    /// reconstructed screen has always consumed.
    pub fn feed(&mut self, now: Instant, bytes: &[u8]) {
        self.state.feed(bytes);
        self.scheduler.on_feed(now, bytes.len());
    }

    /// The terminal was resized under the session.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.state.resize(cols, rows);
    }

    /// When [`poll`](Self::poll) would next fire — arm a timer from this
    /// rather than polling on a tick. `None` means no burst is open.
    pub fn deadline(&self) -> Option<Instant> {
        self.scheduler.deadline()
    }

    /// Fires the screen pass if the quiet window has elapsed.
    pub fn poll(&mut self, engine: &MatcherEngine, now: Instant) -> Vec<EventBody> {
        match self.scheduler.poll(now) {
            Some(_) => self.evaluation_point(engine),
            None => Vec::new(),
        }
    }

    /// The feed reports nothing pending: the burst, if one is open, ends
    /// here and the screen pass fires now.
    pub fn on_quiescent(&mut self, engine: &MatcherEngine) -> Vec<EventBody> {
        match self.scheduler.on_quiescent() {
            Some(_) => self.evaluation_point(engine),
            None => Vec::new(),
        }
    }

    fn evaluation_point(&mut self, engine: &MatcherEngine) -> Vec<EventBody> {
        // Order matters twice here. The engine check comes first so a
        // session with no screen matchers never pays for a render — the
        // one call that walks the whole grid. And `evaluate()` runs before
        // `render()` because evaluation consumes the damage flags; calling
        // it is what makes the next point's diff mean "since the last
        // point".
        if !engine.has_screen_matchers() {
            return Vec::new();
        }
        let evaluation = self.state.evaluate();
        if evaluation.damaged.is_empty() && evaluation.novel.is_empty() {
            return Vec::new();
        }
        let Some(snapshot) = self.state.render() else {
            // Not `tui_aware`, or the screen outgrew what a session may
            // keep: there is no screen to match against.
            return Vec::new();
        };
        engine.evaluate_screen(&snapshot, &evaluation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::QUIET_PERIOD;
    use agent_bridge_adapter_api::{
        Captures, EmitSpec, MatchOutcome, MatcherId, ScreenDiff, ScreenMatcher, Template,
        TemplateValue,
    };
    use agent_bridge_events::{EventKind, ScreenSnapshot};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts its invocations, then matches a dialog row when one is
    /// novel — the probe for both the cadence and the gate.
    struct DialogProbe {
        id: MatcherId,
        calls: Arc<AtomicUsize>,
    }

    impl ScreenMatcher for DialogProbe {
        fn id(&self) -> &MatcherId {
            &self.id
        }

        fn evaluate(
            &self,
            _snapshot: &ScreenSnapshot,
            diff: &ScreenDiff<'_>,
        ) -> Option<MatchOutcome> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            diff.novel
                .iter()
                .find(|row| row.text.contains("Do you want to proceed?"))
                .map(|row| MatchOutcome::with_captures(Captures::new().with("prompt", row.text)))
        }
    }

    fn probe_engine(calls: &Arc<AtomicUsize>) -> MatcherEngine {
        let emits = EmitSpec {
            event_type: "prompt.approval_required".to_string(),
            fields: BTreeMap::from([
                (
                    "approval_id".to_string(),
                    TemplateValue::One(Template::Uuid4),
                ),
                (
                    "prompt".to_string(),
                    TemplateValue::One(Template::Group("prompt".to_string())),
                ),
            ]),
        };
        MatcherEngine::builder()
            .screen(
                Box::new(DialogProbe {
                    id: MatcherId::new("dialog_probe"),
                    calls: Arc::clone(calls),
                }),
                emits,
            )
            .compile()
            .expect("compiles")
    }

    #[test]
    fn screen_matchers_fire_at_the_quiet_period_boundary_not_per_feed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = probe_engine(&calls);
        let mut slot = ScreenSlot::new(80, 24, true);
        let start = Instant::now();

        // A dialog painted in two feeds: still one burst, still zero
        // evaluations until the quiet window closes.
        slot.feed(start, b"Do you want ");
        slot.feed(start, b"to proceed?");
        assert!(slot.poll(&engine, start).is_empty(), "mid-burst: too early");
        assert_eq!(calls.load(Ordering::Relaxed), 0, "never evaluated per feed");

        let events = slot.poll(&engine, start + QUIET_PERIOD);
        assert_eq!(calls.load(Ordering::Relaxed), 1, "one point per burst");
        assert_eq!(events.len(), 1);
        let EventKind::PromptApprovalRequired(payload) = &events[0].kind else {
            panic!("expected an approval, got {:?}", events[0].kind);
        };
        assert_eq!(payload.prompt, "Do you want to proceed?");
        assert!(events[0].approval_id.is_some());

        // Quiet with nothing new written: no further points fire.
        assert!(slot.poll(&engine, start + 2 * QUIET_PERIOD).is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn feed_quiescence_ends_the_burst_without_waiting() {
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = probe_engine(&calls);
        let mut slot = ScreenSlot::new(80, 24, true);
        let start = Instant::now();

        slot.feed(start, b"Do you want to proceed?");
        let events = slot.on_quiescent(&engine);
        assert_eq!(events.len(), 1, "quiescence is an evaluation point");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(
            slot.on_quiescent(&engine).is_empty(),
            "no reopened burst, no point"
        );
    }

    #[test]
    fn a_session_without_tui_aware_keeps_the_matchers_blind() {
        let calls = Arc::new(AtomicUsize::new(0));
        let engine = probe_engine(&calls);
        let mut slot = ScreenSlot::new(80, 24, false);
        let start = Instant::now();

        slot.feed(start, b"Do you want to proceed?");
        assert!(slot.poll(&engine, start + QUIET_PERIOD).is_empty());
        assert!(slot.on_quiescent(&engine).is_empty());
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "no screen kept, so the screen kind never runs"
        );
    }

    #[test]
    fn no_screen_matchers_means_no_render() {
        let engine = MatcherEngine::builder().compile().expect("empty compiles");
        let mut slot = ScreenSlot::new(80, 24, true);
        let start = Instant::now();

        slot.feed(start, b"painted text");
        assert!(slot.poll(&engine, start + QUIET_PERIOD).is_empty());
        assert_eq!(
            slot.state.renders(),
            0,
            "a snapshot nobody reads is never materialized"
        );
    }

    /// Two screen matchers over the same paint resolve like every other
    /// kind: ascending priority, then registration order.
    struct EagerProbe {
        id: MatcherId,
        priority: u32,
    }

    impl ScreenMatcher for EagerProbe {
        fn id(&self) -> &MatcherId {
            &self.id
        }

        fn priority(&self) -> u32 {
            self.priority
        }

        fn evaluate(
            &self,
            _snapshot: &ScreenSnapshot,
            _diff: &ScreenDiff<'_>,
        ) -> Option<MatchOutcome> {
            Some(MatchOutcome::new())
        }
    }

    #[test]
    fn the_screen_pass_resolves_by_priority_then_order() {
        let emits_tool = |tool: &str| EmitSpec {
            event_type: "tool.call_started".to_string(),
            fields: BTreeMap::from([
                ("call_id".to_string(), TemplateValue::One(Template::Uuid4)),
                (
                    "tool".to_string(),
                    TemplateValue::One(Template::Literal(tool.to_string())),
                ),
            ]),
        };
        let engine = MatcherEngine::builder()
            .screen(
                Box::new(EagerProbe {
                    id: MatcherId::new("first_default"),
                    priority: 100,
                }),
                emits_tool("first"),
            )
            .screen(
                Box::new(EagerProbe {
                    id: MatcherId::new("late_but_urgent"),
                    priority: 10,
                }),
                emits_tool("urgent"),
            )
            .compile()
            .expect("compiles");

        let mut slot = ScreenSlot::new(80, 24, true);
        let start = Instant::now();
        slot.feed(start, b"anything at all");
        let events = slot.poll(&engine, start + QUIET_PERIOD);
        let EventKind::ToolCallStarted(payload) = &events[0].kind else {
            panic!("expected tool.call_started");
        };
        assert_eq!(payload.tool, "urgent");
    }
}
