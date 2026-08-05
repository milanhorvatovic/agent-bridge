//! `tool.*` — the tool-call lifecycle, and the tool output an adapter
//! surfaces on its own rather than folding into the token stream.
//!
//! The whole lifecycle is published even where a CLI only reliably announces
//! the start of a call: a caller writes its code against started / completed
//! / failed from the beginning, and an adapter that learns to detect the
//! later phases starts filling them in without any consumer changing. Every
//! event in the namespace carries `call_id`, which is what pairs them.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Payload of `tool.call_started` — the CLI began invoking a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallStarted {
    /// Opaque identifier pairing this call's start, end, and result. Where
    /// the CLI supplies its own call identifier the runtime uses it verbatim
    /// rather than synthesizing a second one.
    pub call_id: String,
    /// The tool being invoked, as the CLI names it (for example `"bash"`).
    pub tool: String,
    /// The concrete invocation, for tools that have one to show (a shell
    /// command line, a file path). Absent where the tool's input is
    /// structured or too large to inline.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Payload of `tool.call_completed` — the tool invocation finished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallCompleted {
    /// The `call_id` of the [`ToolCallStarted`] this closes.
    pub call_id: String,
    /// The tool's exit code, for tools that have one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// How long the call took, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Payload of `tool.call_failed` — the tool invocation did not finish.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCallFailed {
    /// The `call_id` of the [`ToolCallStarted`] this closes.
    pub call_id: String,
    /// Why the call failed, in the CLI's own terms (for example
    /// `"timeout"`). Free text: what a CLI can report about a tool failure
    /// is not a set this runtime can close.
    pub reason: String,
}

/// Payload of `tool.result` — the tool's own output, as its own event.
///
/// Separate from the call lifecycle because some adapters surface a tool's
/// textual output as a distinct record while others interleave it into
/// `stream.token`. A consumer that wants tool output without the surrounding
/// narration subscribes to this type; one that wants the session as it read
/// on screen subscribes to the token stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    /// The `call_id` of the [`ToolCallStarted`] this output belongs to.
    pub call_id: String,
    /// The tool's output text.
    pub content: String,
}
