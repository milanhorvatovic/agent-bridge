//! Diagnostics that are not errors.
//!
//! Severity lives in the event type, not in a field: these say something a
//! caller wants to know without anything having gone wrong, which is exactly
//! why they are outside the `*.error` namespaces. Routing on the namespace
//! stays a correct way to route on severity.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Payload of `runtime.idle_too_long` — a running session has produced no
/// event for the configured threshold.
///
/// The session is untouched: this is the runtime saying "nothing has happened
/// for a while", which a caller may read as a stuck CLI, a long-running tool,
/// or a human staring at a prompt. Deciding which is the caller's business.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeIdleTooLong {
    /// How long the session has been silent, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_ms: Option<u64>,
    /// The configured threshold that fired, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold_ms: Option<u64>,
}

/// Payload of `runtime.health_changed` — the runtime's health assessment
/// moved.
///
/// Callers poll health when they want a snapshot; they subscribe to this when
/// they want to hear about the transition without polling for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeHealthChanged {
    /// The health the runtime reports now.
    pub status: HealthStatus,
    /// The health it reported before this transition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<HealthStatus>,
    /// What moved it, in a form a human can act on (for example
    /// `"log disk below 5% free"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// How the runtime assesses itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Serving normally.
    Ok,
    /// Serving, with something worth knowing about — the runtime keeps
    /// hosting sessions but some capability is impaired.
    Degraded,
    /// Not serving reliably.
    Unhealthy,
}

/// Payload of `runtime.notice` — a notification the hosted CLI raised
/// through a structured channel, passed through with its kind named.
///
/// This is the catch-all for the notifications a CLI emits that are neither
/// output nor lifecycle: a permission dialog appearing, an idle nudge. The
/// runtime does not interpret them, because interpreting them would mean
/// inventing a taxonomy for every CLI's notification vocabulary; it names the
/// kind and carries the rest verbatim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeNotice {
    /// The kind of notification, in the CLI's own vocabulary (for example
    /// `"permission_prompt"` or `"idle_prompt"`).
    pub notification_type: String,
    /// The notification's human-readable text, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whatever else the notification carried, passed through unmodified.
    /// Absent when it carried nothing else.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub detail: Map<String, Value>,
}

/// Payload of `adapter.version_warning` — the launched CLI's version is
/// outside the range its adapter declares support for.
///
/// The launch proceeds. The adapter's patterns were written against a version
/// range, so a version outside it is a reason to expect detection gaps —
/// which surface as `stream.unrecognized_output` — not a reason to refuse to
/// run.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct AdapterVersionWarning {
    /// The adapter that raised the warning (its source tag).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// The CLI version that was launched, as the CLI reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detected_version: Option<String>,
    /// The version range the adapter declares support for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supported_range: Option<String>,
}
