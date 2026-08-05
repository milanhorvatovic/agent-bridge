//! `stream.*` — the CLI's output, after the runtime has made sense of as
//! much of it as it can.
//!
//! All three carry text that has already had terminal control sequences
//! stripped; they differ in what the runtime was able to conclude about it.
//! `stream.token` is classified output, `stream.stderr` is output an adapter
//! could attribute to the error channel, and `stream.unrecognized_output` is
//! the honest admission that neither applied.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Payload of `stream.token` — incremental output: plain text after
/// terminal-control stripping.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamToken {
    /// The emitting adapter's source tag (for example `"claude"`), so
    /// consumers can filter when sessions of different CLIs are active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The text chunk, after terminal-control stripping.
    pub content: String,
}

/// Payload of `stream.stderr` — an error-channel line after segmentation.
///
/// Best-effort by nature: a terminal merges the CLI's error output into its
/// standard output, so this event fires only where the adapter declares a
/// way to tell them apart. Without one, error text arrives as
/// [`StreamToken`] instead. A caller that needs a guaranteed error channel
/// uses the `transport.error` / `pty.error` / `adapter.error` /
/// `runtime.error` events, which are always emitted as such.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamStderr {
    /// The error-channel text, after terminal-control stripping.
    pub content: String,
}

/// Payload of `stream.unrecognized_output` — output the runtime could not
/// classify.
///
/// Instead of dropping it, the runtime degrades to "here is the text" — the
/// single most important resilience event: when an adapter's patterns miss a
/// CLI update, consumers still see everything, classified or not.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamUnrecognizedOutput {
    /// The unclassified text, after terminal-control stripping, so the
    /// consumer can decide what to make of it.
    pub content: String,
}
