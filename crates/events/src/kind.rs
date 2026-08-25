//! The event's `type` discriminant paired with its `payload`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::payload::{
    control::*, errors::*, lifecycle::*, notice::*, prompt::*, stream::*, tool::*,
};

/// The event's namespaced `type` paired with its type-specific `payload`.
///
/// Type names are dotted and hierarchical (`lifecycle.session.created`,
/// `stream.token`, …) so consumers can subscribe by namespace prefix. New
/// types arrive within existing namespaces without a `schema_version` bump;
/// consumers must tolerate types they do not know — which this enum itself
/// honors: a `type` not in the published set deserializes to
/// [`EventKind::Unknown`] with the type name and payload preserved, never to
/// an error.
///
/// Two names that are deliberately *not* here are as much part of the
/// contract as the ones that are. There is no `runtime.health` event — the
/// health snapshot is something a caller asks for, and the event for a
/// change in it is `runtime.health_changed`. And there is no `session.eof`:
/// the end of a *subscription* is a transport-level notification, not
/// something that happened to the session, and folding it in here would tell
/// every other subscriber that a session ended when it had not.
//
// Deliberately not `#[non_exhaustive]`: every consumer is a crate in this
// workspace, and when a type is added to the taxonomy their matches *should*
// stop compiling. Ignoring a new event type is then a decision someone makes
// rather than a default they get.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "payload")]
pub enum EventKind {
    /// The session exists in the runtime's registry; no terminal has been
    /// allocated yet.
    #[serde(rename = "lifecycle.session.created")]
    LifecycleSessionCreated(LifecycleSessionCreated),
    /// The terminal is allocated and the CLI process is being started.
    #[serde(rename = "lifecycle.session.launching")]
    LifecycleSessionLaunching(LifecycleSessionLaunching),
    /// The CLI process is alive; no output has been observed yet.
    #[serde(rename = "lifecycle.session.connecting")]
    LifecycleSessionConnecting(LifecycleSessionConnecting),
    /// First output observed; the session is live.
    #[serde(rename = "lifecycle.session.running")]
    LifecycleSessionRunning(LifecycleSessionRunning),
    /// The session is blocked on at least one human decision. The prompts
    /// themselves arrive as `prompt.approval_required`.
    #[serde(rename = "lifecycle.session.awaiting_approval")]
    LifecycleSessionAwaitingApproval(LifecycleSessionAwaitingApproval),
    /// An interrupt was forwarded to the CLI and the CLI acknowledged it.
    #[serde(rename = "lifecycle.session.interrupted")]
    LifecycleSessionInterrupted(LifecycleSessionInterrupted),
    /// Termination has been initiated.
    #[serde(rename = "lifecycle.session.closing")]
    LifecycleSessionClosing(LifecycleSessionClosing),
    /// The session has ended.
    #[serde(rename = "lifecycle.session.closed")]
    LifecycleSessionClosed(LifecycleSessionClosed),
    /// The CLI is compacting its own context; the session stays live.
    #[serde(rename = "lifecycle.session.compacting")]
    LifecycleSessionCompacting(LifecycleSessionCompacting),
    /// An assistant turn began.
    #[serde(rename = "lifecycle.turn.started")]
    LifecycleTurnStarted(LifecycleTurnStarted),
    /// An assistant turn ended on its own.
    #[serde(rename = "lifecycle.turn.completed")]
    LifecycleTurnCompleted(LifecycleTurnCompleted),
    /// Incremental output: plain text after terminal-control stripping.
    #[serde(rename = "stream.token")]
    StreamToken(StreamToken),
    /// An error-channel line, for adapters that can tell one apart from
    /// ordinary output.
    #[serde(rename = "stream.stderr")]
    StreamStderr(StreamStderr),
    /// Output the runtime could not classify. Instead of dropping it, the
    /// runtime degrades to "here is the text" — the single most important
    /// resilience event: consumers see everything, classified or not.
    #[serde(rename = "stream.unrecognized_output")]
    StreamUnrecognizedOutput(StreamUnrecognizedOutput),
    /// The CLI is blocked on a human decision. The envelope's `approval_id`
    /// is non-null on this event and correlates the eventual resolution.
    #[serde(rename = "prompt.approval_required")]
    PromptApprovalRequired(PromptApprovalRequired),
    /// The runtime withdrew a pending approval whose announcing source
    /// vanished. The envelope's `approval_id` is non-null and names the
    /// prompt that no longer accepts a resolution.
    #[serde(rename = "prompt.approval_withdrawn")]
    PromptApprovalWithdrawn(PromptApprovalWithdrawn),
    /// The CLI began invoking a tool.
    #[serde(rename = "tool.call_started")]
    ToolCallStarted(ToolCallStarted),
    /// A tool invocation finished.
    #[serde(rename = "tool.call_completed")]
    ToolCallCompleted(ToolCallCompleted),
    /// A tool invocation did not finish.
    #[serde(rename = "tool.call_failed")]
    ToolCallFailed(ToolCallFailed),
    /// A tool's own output, surfaced as its own event.
    #[serde(rename = "tool.result")]
    ToolResult(ToolResult),
    /// The runtime core hit a condition it has to report.
    #[serde(rename = "runtime.error")]
    RuntimeError(RuntimeErrorPayload),
    /// A running session has been silent for longer than the configured
    /// threshold.
    #[serde(rename = "runtime.idle_too_long")]
    RuntimeIdleTooLong(RuntimeIdleTooLong),
    /// The runtime's health assessment moved.
    #[serde(rename = "runtime.health_changed")]
    RuntimeHealthChanged(RuntimeHealthChanged),
    /// A notification the hosted CLI raised through a structured channel.
    #[serde(rename = "runtime.notice")]
    RuntimeNotice(RuntimeNotice),
    /// The JSON-RPC wire could not carry something, or a subscriber could
    /// not keep up with it.
    #[serde(rename = "transport.error")]
    TransportError(TransportErrorPayload),
    /// The terminal, or the process hosted in it, failed.
    #[serde(rename = "pty.error")]
    PtyError(PtyErrorPayload),
    /// The adapter hosting a CLI could not do its job.
    #[serde(rename = "adapter.error")]
    AdapterError(AdapterErrorPayload),
    /// The launched CLI's version is outside the range its adapter declares
    /// support for. The launch proceeds.
    #[serde(rename = "adapter.version_warning")]
    AdapterVersionWarning(AdapterVersionWarning),
    /// A subscriber is re-attaching to a live session.
    #[serde(rename = "session.reconnecting")]
    SessionReconnecting(SessionReconnecting),
    /// The re-attach completed, reporting what backfill delivered.
    #[serde(rename = "session.reconnected")]
    SessionReconnected(SessionReconnected),
    /// Write ownership of a session moved.
    #[serde(rename = "session.writer_changed")]
    SessionWriterChanged(SessionWriterChanged),
    /// An event type this revision does not know. The compatibility
    /// contract lets producers add event types within `schema_version` 1,
    /// so a consumer compiled against this revision must be able to receive
    /// a newer producer's events; this fallback carries the unrecognized
    /// type name and its payload through instead of failing the whole
    /// envelope. Tried last, only after every published type has failed to
    /// match — which makes the typed surface deliberately tolerant: a
    /// published type name over a payload that does not match its shape
    /// also lands here rather than erroring. Shape *enforcement* is the
    /// schema's job ([`event_schema`](crate::event_schema) rejects that
    /// record); this enum's job is to never be the reason a consumer drops
    /// an event.
    #[serde(untagged)]
    Unknown(UnknownEvent),
}

impl EventKind {
    /// The dotted event-type name, exactly as it appears on the wire.
    ///
    /// This is what prefix subscriptions filter on, so it is derived from
    /// the same names serialization uses rather than being a second listing
    /// that could drift from it — a test serializes every type and compares.
    pub fn event_type(&self) -> &str {
        match self {
            Self::LifecycleSessionCreated(_) => "lifecycle.session.created",
            Self::LifecycleSessionLaunching(_) => "lifecycle.session.launching",
            Self::LifecycleSessionConnecting(_) => "lifecycle.session.connecting",
            Self::LifecycleSessionRunning(_) => "lifecycle.session.running",
            Self::LifecycleSessionAwaitingApproval(_) => "lifecycle.session.awaiting_approval",
            Self::LifecycleSessionInterrupted(_) => "lifecycle.session.interrupted",
            Self::LifecycleSessionClosing(_) => "lifecycle.session.closing",
            Self::LifecycleSessionClosed(_) => "lifecycle.session.closed",
            Self::LifecycleSessionCompacting(_) => "lifecycle.session.compacting",
            Self::LifecycleTurnStarted(_) => "lifecycle.turn.started",
            Self::LifecycleTurnCompleted(_) => "lifecycle.turn.completed",
            Self::StreamToken(_) => "stream.token",
            Self::StreamStderr(_) => "stream.stderr",
            Self::StreamUnrecognizedOutput(_) => "stream.unrecognized_output",
            Self::PromptApprovalRequired(_) => "prompt.approval_required",
            Self::PromptApprovalWithdrawn(_) => "prompt.approval_withdrawn",
            Self::ToolCallStarted(_) => "tool.call_started",
            Self::ToolCallCompleted(_) => "tool.call_completed",
            Self::ToolCallFailed(_) => "tool.call_failed",
            Self::ToolResult(_) => "tool.result",
            Self::RuntimeError(_) => "runtime.error",
            Self::RuntimeIdleTooLong(_) => "runtime.idle_too_long",
            Self::RuntimeHealthChanged(_) => "runtime.health_changed",
            Self::RuntimeNotice(_) => "runtime.notice",
            Self::TransportError(_) => "transport.error",
            Self::PtyError(_) => "pty.error",
            Self::AdapterError(_) => "adapter.error",
            Self::AdapterVersionWarning(_) => "adapter.version_warning",
            Self::SessionReconnecting(_) => "session.reconnecting",
            Self::SessionReconnected(_) => "session.reconnected",
            Self::SessionWriterChanged(_) => "session.writer_changed",
            Self::Unknown(unknown) => &unknown.event_type,
        }
    }

    /// The variant's payload as a JSON value — the content half of the
    /// adjacently tagged pair, without the `type` discriminant. For
    /// consumers that mirror or measure the payload alone; a mirror
    /// that stored the tagged wrapper beside a record that already
    /// names the type would say everything twice.
    pub fn payload_value(&self) -> serde_json::Value {
        match serde_json::to_value(self) {
            Ok(mut tagged) => match tagged.get_mut("payload") {
                Some(payload) => payload.take(),
                None => serde_json::Value::Null,
            },
            Err(_) => serde_json::Value::Null,
        }
    }

    /// Serialized size of the payload alone, in bytes, without building
    /// a JSON tree: one serialization pass of the tagged pair, minus
    /// the wrapper the tagging adds. The subtraction leans on exactly
    /// what the round-trip suite already pins — compact output, these
    /// two key names, type first — and a test holds this equal to
    /// measuring [`Self::payload_value`] for every published kind.
    pub fn payload_bytes(&self) -> usize {
        const WRAPPER: usize = r#"{"type":"","payload":}"#.len();
        match serde_json::to_vec(self) {
            Ok(bytes) => bytes
                .len()
                .saturating_sub(WRAPPER + self.event_type().len()),
            // The reading the mirror gives a payload that will not
            // serialize: "null".
            Err(_) => 4,
        }
    }

    /// The first segment of the event type (`"lifecycle"`, `"stream"`, …) —
    /// what a namespace subscription selects on.
    pub fn namespace(&self) -> &str {
        let event_type = self.event_type();
        event_type
            .split_once('.')
            .map_or(event_type, |(namespace, _)| namespace)
    }
}

/// An event of a type this revision does not enumerate: the raw `type`
/// name and its `payload`, carried through so a consumer on an older
/// taxonomy revision keeps receiving a newer producer's events. See
/// [`EventKind::Unknown`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct UnknownEvent {
    /// The event's namespaced type name, as received.
    #[serde(rename = "type")]
    pub event_type: String,
    /// The event's payload object, as received.
    pub payload: serde_json::Map<String, serde_json::Value>,
}
