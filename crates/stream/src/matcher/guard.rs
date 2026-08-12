//! The safety ceiling: a wall-clock bound on each individual matcher
//! evaluation, enforced at runtime.
//!
//! This is the *other* budget, and the distinction is load-bearing enough
//! to spell out. The evaluation chain per line has a microsecond-scale
//! performance budget, held by a benchmark in the CI lane — a regression
//! there is a review problem. The ceiling here is three orders of
//! magnitude looser, applied per individual evaluation at runtime, and its
//! purpose is blast-radius control: one pathological matcher must cost the
//! session that matcher, never the session. The two never share a
//! constant, a config key, or a code path, so they cannot drift back into
//! being one number.
//!
//! Enforcement is detection, not preemption: elapsed time is checked after
//! an evaluation returns. The expression engine's linear-time guarantee is
//! what rules out an evaluation that never returns, so the ceiling's real
//! targets are the code kinds — a stateful or screen matcher that blocks —
//! and any future expression engine without that guarantee. On a breach
//! the evaluation's result is discarded (a detection that took that long
//! is not one to act on), the matcher joins the session's disabled set,
//! and `adapter.error` with `pattern_timeout` fires exactly once for that
//! (session, matcher) — not once per subsequent line. Other sessions keep
//! the matcher: their evaluations were never the slow ones.

use std::time::Duration;

use agent_bridge_adapter_api::MatcherId;
use agent_bridge_events::{AdapterErrorCode, AdapterErrorPayload, EventBody, EventKind};

/// The default ceiling. Configurable per runtime as
/// `stream.pattern_eval_timeout_ms`; tightening it is a config change, not
/// a code change.
pub const DEFAULT_EVAL_TIMEOUT: Duration = Duration::from_millis(50);

/// The ceiling, held where the engine can consult it per evaluation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct EvalGuard {
    ceiling: Duration,
}

impl EvalGuard {
    pub(crate) fn new(ceiling: Duration) -> Self {
        Self { ceiling }
    }

    /// Whether an evaluation that took `elapsed` breached the ceiling.
    ///
    /// `>=` rather than `>` so a zero ceiling — the test seam that makes
    /// every evaluation trip — works even on a clock coarse enough to
    /// report a zero elapsed time.
    pub(crate) fn breached(&self, elapsed: Duration) -> bool {
        elapsed >= self.ceiling
    }
}

impl Default for EvalGuard {
    fn default() -> Self {
        Self::new(DEFAULT_EVAL_TIMEOUT)
    }
}

/// The one-shot breach event: `adapter.error { pattern_timeout }`, naming
/// the matcher and what its evaluation cost.
pub(crate) fn pattern_timeout_event(id: &MatcherId, elapsed: Duration) -> EventBody {
    let mut detail = serde_json::Map::new();
    detail.insert("matcher".to_string(), id.as_str().into());
    detail.insert(
        "elapsed_ms".to_string(),
        u64::try_from(elapsed.as_millis())
            .unwrap_or(u64::MAX)
            .into(),
    );
    EventBody::new(EventKind::AdapterError(AdapterErrorPayload {
        code: AdapterErrorCode::PatternTimeout,
        message: format!(
            "matcher `{id}` exceeded the per-evaluation ceiling and is disabled for this session"
        ),
        detail,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mocked-clock variant: the breach decision is a pure function of
    /// two durations, so the clock needs no mocking machinery — these are
    /// the exact readings the real path would take.
    #[test]
    fn the_breach_decision_is_exact_at_the_boundary() {
        let guard = EvalGuard::new(Duration::from_millis(50));
        assert!(!guard.breached(Duration::from_millis(49)));
        assert!(
            guard.breached(Duration::from_millis(50)),
            ">= is the contract"
        );
        assert!(guard.breached(Duration::from_millis(51)));

        let zero = EvalGuard::new(Duration::ZERO);
        assert!(
            zero.breached(Duration::ZERO),
            "the zero-ceiling test seam trips on a coarse clock's zero reading"
        );
    }

    #[test]
    fn the_breach_event_names_the_matcher_and_the_cost() {
        let event = pattern_timeout_event(&MatcherId::new("slowpoke"), Duration::from_millis(72));
        let EventKind::AdapterError(payload) = &event.kind else {
            panic!("expected adapter.error, got {:?}", event.kind);
        };
        assert_eq!(payload.code, AdapterErrorCode::PatternTimeout);
        assert!(payload.message.contains("slowpoke"));
        assert!(payload.message.contains("disabled for this session"));
        assert_eq!(
            payload
                .detail
                .get("matcher")
                .and_then(|value| value.as_str()),
            Some("slowpoke")
        );
        assert_eq!(
            payload
                .detail
                .get("elapsed_ms")
                .and_then(serde_json::Value::as_u64),
            Some(72)
        );
    }
}
