//! The error payloads, one type per source layer.
//!
//! Errors are split by *origin* — `pty.error`, `adapter.error`,
//! `runtime.error` as events, and `transport.error` as a wire condition —
//! rather than pooled into one type with a `kind` string, so a consumer
//! routes by component and never has to parse its way to that decision. Each
//! payload has the same three fields: a machine-readable `code`, a
//! human-readable `message`, and optional structured `detail` whose shape
//! depends on the code.
//!
//! Three of the four are sequenced events, subscribable by namespace:
//! something happened *in* a session or the runtime, so it belongs in the
//! event stream. `transport.error` is the exception — a condition of the
//! wire *carrying* that stream (a frame too large, stdout blocked, a
//! subscriber disconnected for lag), scoped to no session and delivered
//! out-of-band as a `transport.error` transport notification, a sibling of
//! `session.event` and `session.eof` rather than a taxonomy event. Its
//! payload lives here because it is shared vocabulary: the bus speaks it to
//! name a lag disconnect and the transport speaks it on the wire.
//!
//! The code sets are open by contract: a new code under an existing error
//! type is an additive change that keeps `schema_version`. Each code type
//! therefore carries an `Unknown` variant holding the code verbatim, so a
//! consumer compiled against this revision reads a newer runtime's errors
//! instead of failing on them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Structured detail accompanying an error code. Absent when the code says
/// everything there is to say.
type Detail = Map<String, Value>;

/// Payload of `transport.error` — the JSON-RPC wire could not carry
/// something, or a subscriber could not keep up with it.
///
/// Unlike its three sibling error payloads this is not a taxonomy event:
/// a transport condition is scoped to no session, so it rides the wire as a
/// `transport.error` transport notification rather than the sequenced event
/// stream. The payload is defined here as shared vocabulary — the bus names
/// a lag disconnect with it, the transport frames it — carried by that
/// notification, never wrapped in an [`Event`](crate::Event).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TransportErrorPayload {
    /// What went wrong, as a machine-readable code.
    pub code: TransportErrorCode,
    /// What went wrong, in a form a human can act on.
    pub message: String,
    /// Structured context for the code — the fields depend on which code it
    /// is. Absent when there is nothing to add.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub detail: Detail,
}

/// Payload of `pty.error` — the terminal, or the process hosted in it,
/// failed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PtyErrorPayload {
    /// What went wrong, as a machine-readable code.
    pub code: PtyErrorCode,
    /// What went wrong, in a form a human can act on.
    pub message: String,
    /// Structured context for the code — the fields depend on which code it
    /// is. Absent when there is nothing to add.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub detail: Detail,
}

/// Payload of `adapter.error` — the adapter hosting a CLI could not do its
/// job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct AdapterErrorPayload {
    /// What went wrong, as a machine-readable code.
    pub code: AdapterErrorCode,
    /// What went wrong, in a form a human can act on.
    pub message: String,
    /// Structured context for the code — the fields depend on which code it
    /// is. Absent when there is nothing to add.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub detail: Detail,
}

/// Payload of `runtime.error` — the runtime core itself hit a condition it
/// has to report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeErrorPayload {
    /// What went wrong, as a machine-readable code.
    pub code: RuntimeErrorCode,
    /// What went wrong, in a form a human can act on.
    pub message: String,
    /// Structured context for the code — the fields depend on which code it
    /// is. Absent when there is nothing to add.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub detail: Detail,
}

/// The codes `transport.error` publishes in this revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum TransportErrorCode {
    /// A frame exceeded the transport's size cap.
    FrameTooLarge,
    /// A frame could not be parsed as the protocol requires.
    MalformedFrame,
    /// A subscriber fell far enough behind that the runtime disconnected it
    /// rather than let its backlog grow without bound. The session itself
    /// continues.
    SubscriberLagging,
    /// The caller stopped reading the runtime's output. There is no recovery
    /// from a parent that does not read: the runtime reports this and exits.
    StdoutBlocked,
    /// A code this revision does not know, carried verbatim. New codes are
    /// additive, so a consumer must read them rather than reject them.
    #[serde(untagged)]
    Unknown(String),
}

/// The codes `pty.error` publishes in this revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PtyErrorCode {
    /// A pseudo-terminal could not be allocated for the session.
    PtyAllocFailed,
    /// A write to the CLI's input blocked long enough to report.
    StdinBlocked,
    /// Undecodable bytes were replaced to keep the stream flowing — output
    /// is preserved, but altered.
    EncodingReplacement,
    /// Replacements arrived in a burst, which usually means the CLI is
    /// emitting something that is not text at all.
    EncodingBurst,
    /// The CLI process could not be started in the allocated terminal.
    ChildExecFailed,
    /// The operation could not be carried out because the CLI process is
    /// already gone. Distinct from the terminal failing: nothing is wrong
    /// with it, there is simply nobody left in it.
    ChildExitedEarly,
    /// A signal could not be delivered to the CLI's process group. Also how
    /// a platform reports a signal it has no equivalent for, because the
    /// honest answer is that the delivery did not happen.
    SignalDeliveryFailed,
    /// The terminal was resized before the CLI had taken possession of it.
    /// The geometry was applied; what is uncertain is whether anything was
    /// there to be notified, so the resize is worth reissuing.
    EarlyResize,
    /// The terminal itself failed an operation, for a reason that is not the
    /// CLI having exited. A session cannot continue on it.
    PtyIoFailed,
    /// A code this revision does not know, carried verbatim. New codes are
    /// additive, so a consumer must read them rather than reject them.
    #[serde(untagged)]
    Unknown(String),
}

/// The codes `adapter.error` publishes in this revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AdapterErrorCode {
    /// A pattern the adapter declared could not be compiled.
    PatternCompileFailed,
    /// Matching took longer than the adapter's budget allows.
    PatternTimeout,
    /// The launched CLI's version is one the adapter cannot drive. The
    /// softer case — a version outside the declared range that the adapter
    /// will still attempt — is `adapter.version_warning`, not an error.
    VersionMismatch,
    /// A code this revision does not know, carried verbatim. New codes are
    /// additive, so a consumer must read them rather than reject them.
    #[serde(untagged)]
    Unknown(String),
}

/// The codes `runtime.error` publishes in this revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RuntimeErrorCode {
    /// A session did not stop when asked and the supervisor closed it by
    /// force.
    SupervisorForceClose,
    /// Two operations raced over the same session registry entry.
    RegistryRace,
    /// Logs cannot be written because the disk is full. The runtime keeps
    /// serving sessions; the diagnostic record is what is lost, and saying
    /// so is the point of the event.
    LogDiskFull,
    /// A log file could not be rotated.
    LogRotationFailed,
    /// Configuration was rejected as invalid.
    ConfigInvalid,
    /// A code this revision does not know, carried verbatim. New codes are
    /// additive, so a consumer must read them rather than reject them.
    #[serde(untagged)]
    Unknown(String),
}
