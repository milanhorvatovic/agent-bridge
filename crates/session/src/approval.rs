//! The approval-correlation state: which decisions are pending, keyed by
//! the id that resolves each one.
//!
//! The load-bearing shape is a *set*, not a slot: a single
//! assistant turn issuing parallel tool calls produces several concurrent
//! hook approvals with distinct `tool_use_id`s, all pending before any
//! resolves. `AwaitingApproval` therefore means "≥ 1 pending", each
//! resolution matches exactly one id, and a mis-routed approval — the worst
//! bug class this layer can have — is made structurally impossible
//! by looking entries up by id and refusing ids that match nothing. The
//! one-dialog-at-a-time rule survives only where it is physically true: the
//! screen-detected fallback path, where a TUI renders one dialog at a time.
//!
//! Entries keep their insertion order so event assertions are deterministic;
//! the map is a small ordered vector rather than a hash map because a
//! session plausibly holds a handful of pending approvals, never thousands.

use std::time::Instant;

use tokio::sync::oneshot;

use crate::error::SessionError;

/// The id that resolves one pending approval.
///
/// Hook-sourced approvals carry the CLI's own `tool_use_id` verbatim —
/// the runtime never synthesizes a second id for a correlation the CLI
/// already made. Screen-detected prompts get a runtime-minted
/// UUIDv4. Pairing is always `(session_id, approval_id)`; cross-session
/// uniqueness is incidental and never relied on.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ApprovalId(pub String);

impl std::fmt::Display for ApprovalId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How an announced approval is identified, fixed by its source: a hook
/// carries the CLI's own `tool_use_id` verbatim — the correlation is the
/// CLI's to keep — while a screen detection carries nothing, and the
/// runtime mints its UUIDv4 at the announcement. The announcing surface
/// cannot supply a screen id at all, which is what keeps the mint a
/// contract rather than a convention and a hook-id collision impossible
/// to introduce from outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalIdentity {
    /// A structured hook announcement: the CLI's correlation id, verbatim.
    Hook(ApprovalId),
    /// A screen-detected prompt: the runtime mints the id.
    Screen,
}

/// Where a pending approval came from — recorded because the two sources
/// carry different invariants (set semantics for hooks, single-active for
/// the screen) and Phase 2 routes resolutions differently by source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalSource {
    /// A structured hook announcement; the id is the CLI's `tool_use_id`.
    Hook,
    /// A prompt detected on the reconstructed screen; the id is
    /// runtime-minted.
    Screen,
}

/// A caller's answer to one pending approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Let the tool call proceed.
    Allow,
    /// Refuse it; `reason` rides back to the model when given.
    Deny {
        /// Why, for the model to act on.
        reason: Option<String>,
    },
    /// Defer to the CLI's own interactive prompt.
    Ask,
}

/// What a parked entry's resolver receives — the caller's decision, or the
/// cancellation an interrupt or close forces on the whole set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResolution {
    /// The caller allowed it.
    Allow,
    /// The caller denied it, with the reason to carry back.
    Deny {
        /// Why, for the model to act on.
        reason: Option<String>,
    },
    /// The caller deferred to the CLI's own prompt.
    Ask,
    /// Nobody decided: an interrupt or close cancelled every pending
    /// approval, so the source must not leave its reply dangling to a
    /// timeout.
    Cancelled,
}

impl From<ApprovalDecision> for ApprovalResolution {
    fn from(decision: ApprovalDecision) -> Self {
        match decision {
            ApprovalDecision::Allow => ApprovalResolution::Allow,
            ApprovalDecision::Deny { reason } => ApprovalResolution::Deny { reason },
            ApprovalDecision::Ask => ApprovalResolution::Ask,
        }
    }
}

/// One pending approval: its source, when it arrived, and the channel the
/// waiting source is parked on.
pub(crate) struct PendingApproval {
    pub(crate) source: ApprovalSource,
    /// Bookkeeping only — there is deliberately no idle expiry (an
    /// operator may sit at a prompt indefinitely), so nothing reads this
    /// to time anything out; it exists for diagnostics.
    #[allow(dead_code, reason = "diagnostic bookkeeping until a consumer lands")]
    pub(crate) since: Instant,
    /// The Phase-2 hook listener (or a test) awaits this for the decision.
    pub(crate) resolver: oneshot::Sender<ApprovalResolution>,
}

/// The `tool_use_id`-keyed pending set. `AwaitingApproval` ⇔ not empty.
#[derive(Default)]
pub(crate) struct PendingApprovals {
    entries: Vec<(ApprovalId, PendingApproval)>,
}

impl PendingApprovals {
    /// Park a new pending approval.
    ///
    /// Refuses a duplicate id outright, and a second screen-sourced entry
    /// while one pends (the screen path's retained one-dialog-at-a-time
    /// rule) — in both cases the set is untouched.
    pub(crate) fn insert(
        &mut self,
        id: ApprovalId,
        entry: PendingApproval,
    ) -> Result<(), SessionError> {
        if self.entries.iter().any(|(existing, _)| *existing == id) {
            return Err(SessionError::ApprovalAlreadyPending);
        }
        if entry.source == ApprovalSource::Screen
            && self
                .entries
                .iter()
                .any(|(_, pending)| pending.source == ApprovalSource::Screen)
        {
            return Err(SessionError::ScreenApprovalContractViolation);
        }
        self.entries.push((id, entry));
        Ok(())
    }

    /// Resolve exactly the entry matching `id`, delivering `resolution` to
    /// its parked source. A stale or unknown id is rejected with the set —
    /// and every pending prompt — untouched.
    pub(crate) fn resolve(
        &mut self,
        id: &ApprovalId,
        resolution: ApprovalResolution,
    ) -> Result<(), SessionError> {
        let position = self
            .entries
            .iter()
            .position(|(existing, _)| existing == id)
            .ok_or(SessionError::ApprovalIdMismatch)?;
        let (_, entry) = self.entries.remove(position);
        // A dropped receiver means the source stopped caring; the approval
        // is resolved either way, so the send result carries nothing.
        let _ = entry.resolver.send(resolution);
        Ok(())
    }

    /// Cancel every pending approval: each parked source receives
    /// [`ApprovalResolution::Cancelled`], in insertion order, and the set
    /// empties.
    pub(crate) fn cancel_all(&mut self) {
        for (_, entry) in self.entries.drain(..) {
            let _ = entry.resolver.send(ApprovalResolution::Cancelled);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(source: ApprovalSource) -> (PendingApproval, oneshot::Receiver<ApprovalResolution>) {
        let (resolver, rx) = oneshot::channel();
        (
            PendingApproval {
                source,
                since: Instant::now(),
                resolver,
            },
            rx,
        )
    }

    fn id(text: &str) -> ApprovalId {
        ApprovalId(text.to_string())
    }

    #[test]
    fn two_hook_approvals_coexist_and_resolve_independently_by_id() {
        // The multi-pending contract in miniature: both pend at once, resolving one
        // leaves the other, and each parked source hears its own verdict.
        let mut set = PendingApprovals::default();
        let (first, mut first_rx) = pending(ApprovalSource::Hook);
        let (second, mut second_rx) = pending(ApprovalSource::Hook);
        set.insert(id("tool-a"), first).unwrap();
        set.insert(id("tool-b"), second).unwrap();
        assert_eq!(set.len(), 2);

        set.resolve(&id("tool-b"), ApprovalResolution::Allow)
            .unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(second_rx.try_recv().unwrap(), ApprovalResolution::Allow);
        assert!(first_rx.try_recv().is_err(), "the other entry still pends");

        set.resolve(
            &id("tool-a"),
            ApprovalResolution::Deny {
                reason: Some("not in this repo".into()),
            },
        )
        .unwrap();
        assert!(set.is_empty());
        assert!(matches!(
            first_rx.try_recv().unwrap(),
            ApprovalResolution::Deny { .. }
        ));
    }

    #[test]
    fn a_stale_id_is_rejected_and_every_prompt_stays_pending() {
        let mut set = PendingApprovals::default();
        let (entry, mut rx) = pending(ApprovalSource::Hook);
        set.insert(id("tool-a"), entry).unwrap();

        let refusal = set
            .resolve(&id("tool-gone"), ApprovalResolution::Allow)
            .unwrap_err();
        assert!(matches!(refusal, SessionError::ApprovalIdMismatch));
        assert_eq!(set.len(), 1, "the set is untouched");
        assert!(rx.try_recv().is_err(), "the parked source heard nothing");
    }

    #[test]
    fn a_second_screen_prompt_is_a_contract_violation_set_untouched() {
        let mut set = PendingApprovals::default();
        let (first, _first_rx) = pending(ApprovalSource::Screen);
        set.insert(id("s-1"), first).unwrap();

        let (second, mut second_rx) = pending(ApprovalSource::Screen);
        let refusal = set.insert(id("s-2"), second).unwrap_err();
        assert!(matches!(
            refusal,
            SessionError::ScreenApprovalContractViolation
        ));
        assert_eq!(set.len(), 1);
        // The refused entry's resolver was dropped with the refusal, which
        // its source observes as a closed channel — never as a decision.
        assert!(matches!(
            second_rx.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn a_screen_prompt_may_pend_beside_hook_approvals() {
        // The single-active rule is the *screen path's*, not the session's:
        // a hook approval that degraded to the TUI dialog coexists with
        // other pending hook approvals.
        let mut set = PendingApprovals::default();
        let (hook, _hook_rx) = pending(ApprovalSource::Hook);
        let (screen, _screen_rx) = pending(ApprovalSource::Screen);
        set.insert(id("tool-a"), hook).unwrap();
        set.insert(id("d3b0…"), screen).unwrap();
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn a_duplicate_id_is_refused_without_disturbing_the_original() {
        let mut set = PendingApprovals::default();
        let (first, mut first_rx) = pending(ApprovalSource::Hook);
        set.insert(id("tool-a"), first).unwrap();

        let (duplicate, _rx) = pending(ApprovalSource::Hook);
        let refusal = set.insert(id("tool-a"), duplicate).unwrap_err();
        assert!(matches!(refusal, SessionError::ApprovalAlreadyPending));
        assert_eq!(set.len(), 1);
        assert!(first_rx.try_recv().is_err(), "the original still pends");
    }

    #[test]
    fn cancel_all_delivers_cancelled_to_every_parked_source_in_order() {
        // An interrupt cancels the whole set — no reply dangles to
        // its timeout, and delivery order is insertion order so event
        // assertions stay deterministic.
        let mut set = PendingApprovals::default();
        let (first, mut first_rx) = pending(ApprovalSource::Hook);
        let (second, mut second_rx) = pending(ApprovalSource::Hook);
        set.insert(id("tool-a"), first).unwrap();
        set.insert(id("tool-b"), second).unwrap();

        set.cancel_all();
        assert!(set.is_empty());
        assert_eq!(first_rx.try_recv().unwrap(), ApprovalResolution::Cancelled);
        assert_eq!(second_rx.try_recv().unwrap(), ApprovalResolution::Cancelled);
    }
}
