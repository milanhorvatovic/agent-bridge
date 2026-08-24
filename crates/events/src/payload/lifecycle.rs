//! `lifecycle.*` — where a session is in its life, and where an assistant
//! turn is in its own.
//!
//! The session states form the sequence a caller can rely on: `created` →
//! `launching` → `connecting` → `running`, then `closing` → `closed`, with
//! `awaiting_approval` and `interrupted` as the two states a running session
//! can enter and leave again. The turn events and `compacting` describe what
//! happens *inside* a running session and are reported by the CLI's own
//! structured channels where it exposes them.
//!
//! Most of these payloads are empty today. That is the deliberate shape: the
//! event's meaning is its type, and fields arrive additively when the runtime
//! has something true to put in them — which the compatibility contract
//! allows without a `schema_version` bump.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Payload of `lifecycle.session.created` — the registry entry exists; no
/// terminal has been allocated yet.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionCreated {
    /// Name of the adapter hosting the session (its source tag).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
}

/// Payload of `lifecycle.session.launching` — the terminal is allocated and
/// the CLI process is being started. No fields yet; fields arrive additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionLaunching {}

/// Payload of `lifecycle.session.connecting` — the CLI process is alive; no
/// output has been observed yet. No fields yet; fields arrive additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionConnecting {}

/// Payload of `lifecycle.session.running` — first output observed; the
/// session is live. No fields yet; fields arrive additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionRunning {}

/// Payload of `lifecycle.session.awaiting_approval` — the session is blocked
/// on at least one human decision.
///
/// Paired with `prompt.approval_required`, which carries the prompt itself
/// and the `approval_id` that resolves it. This event is the *state*
/// transition and is deliberately not correlated to any one approval: a
/// session can hold several pending approvals at once, so its envelope
/// `approval_id` stays `null` and the prompt events carry the ids.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionAwaitingApproval {}

/// Payload of `lifecycle.session.interrupted` — an interrupt was forwarded
/// to the CLI and the CLI acknowledged it. Every approval the session was
/// holding is cancelled by the same interrupt. No fields yet; fields arrive
/// additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionInterrupted {}

/// Payload of `lifecycle.session.closing` — termination has been initiated.
/// No fields yet; fields arrive additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionClosing {}

/// Payload of `lifecycle.session.closed` — the session has ended, with what
/// is known about how it ended.
///
/// Every field is optional because what is known depends on how the session
/// ended: a CLI killed after a drain timeout has no exit code to report, and
/// a session that failed before launch has no byte counts.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionClosed {
    /// The CLI process's exit code, when it exited with one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// How long the session lived, in milliseconds, from create to close.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Bytes read from the CLI over the session's lifetime, before any
    /// terminal-control stripping — what the process actually produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_read: Option<u64>,
    /// Bytes written to the CLI's input over the session's lifetime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<u64>,
    /// Whether the shutdown hint worked: `true` when the CLI exited on
    /// its own during the graceful close — while the hint was still being
    /// delivered, or within the drain window that follows it — and
    /// `false` when the close escalated to termination before a voluntary
    /// exit: because the window expired, or because a force-close cut the
    /// wait short (whether the window was armed yet or not), and trailing
    /// output may then be missing. Absent when there was never a graceful
    /// close to answer for: a close that was forced from the start, or a
    /// session that ended by failing.
    //
    // Corrected with the session layer, before anything emitted the field
    // (doc only, no wire change): this previously read inverted — "true
    // when the drain timeout was reached" — against the shutdown-hint
    // contract it mirrors.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drained: Option<bool>,
}

/// Payload of `lifecycle.turn.started` — an assistant turn began.
///
/// A turn is one caller prompt and the work the CLI does in response. The
/// boundary is reported by the CLI's structured channel where it has one;
/// adapters without such a channel do not emit turn events at all, which is
/// why consumers must treat them as informative rather than as a frame they
/// can require around every token. No fields yet; fields arrive additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleTurnStarted {}

/// Payload of `lifecycle.turn.completed` — an assistant turn ended.
///
/// The turn finished on its own. A turn cut short by an interrupt is
/// `lifecycle.session.interrupted` instead — the two are different outcomes
/// and callers routinely need to tell them apart. No fields yet; fields
/// arrive additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleTurnCompleted {}

/// Payload of `lifecycle.session.compacting` — the CLI is compacting its own
/// context.
///
/// Advisory: the session stays live and the runtime keeps following the same
/// output. It is published because a caller watching for a silent stretch
/// otherwise has no way to tell compaction from a stall. No fields yet;
/// fields arrive additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionCompacting {}
