//! `prompt.*` — the CLI is blocked on a human decision.
//!
//! This is the one namespace where an envelope field is part of the payload's
//! contract: an approval prompt without the `approval_id` that resolves it is
//! not an approval prompt, it is a dead end. The types make that structural
//! rather than aspirational — see [`PromptApprovalRequired`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Payload of `prompt.approval_required` — the CLI is waiting on a decision
/// only a human can make.
///
/// The envelope's `approval_id` is required on this event and on the
/// withdrawal that can end it ([`PromptApprovalWithdrawn`]), and on no
/// other: it is what the caller answers with, and a prompt nobody can
/// answer leaves the CLI blocked. Events that are not about one specific
/// pending approval carry `null` there, even while approvals are pending.
//
// `#[non_exhaustive]` is the construction seal, not a hint about future
// fields: it makes the struct unbuildable outside this crate, so
// `EventBody::approval_required` — which takes the id as an argument — is
// the only way a producer can emit an approval prompt. Consumers are
// unaffected: deserialization builds it as usual, and every field is public
// to read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct PromptApprovalRequired {
    /// The prompt text presented by the CLI.
    pub prompt: String,
    /// The tool the decision is about (for example `"bash"`), when the CLI
    /// names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The answer options the CLI offers, when they are known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

/// Payload of `prompt.approval_withdrawn` — the runtime withdrew a
/// pending approval it had announced, because the source that announced
/// it vanished before any decision could be delivered. The envelope's
/// `approval_id` names the withdrawn prompt; resolving it answers
/// `-32007` from this point on.
///
/// Runtime-initiated endings are the only ones announced per id: a
/// resolution's caller already knows the outcome it caused, while a
/// withdrawal has no informed actor unless the stream says so. The
/// cancellations an interrupt or close sweeps are likewise not announced
/// per id — the interrupt and closing events carry that meaning for the
/// whole set. No fields yet; fields arrive additively.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[non_exhaustive]
pub struct PromptApprovalWithdrawn {}

/// What the CLI is asking, before it is paired with the id that resolves it.
///
/// The producer side of [`PromptApprovalRequired`]: assemble the question
/// here, then hand it to
/// [`EventBody::approval_required`](crate::EventBody::approval_required)
/// together with the `approval_id`. The split is what makes an
/// approval prompt without an id unrepresentable.
///
/// ```
/// use agent_bridge_events::{ApprovalPrompt, EventBody, EventKind};
///
/// let body = EventBody::approval_required(
///     "a-7f3",
///     ApprovalPrompt::new("Allow filesystem write?")
///         .tool("bash")
///         .options(["y", "n"]),
/// );
/// assert_eq!(body.approval_id.as_deref(), Some("a-7f3"));
/// assert!(matches!(body.kind, EventKind::PromptApprovalRequired(_)));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalPrompt {
    pub(crate) prompt: String,
    pub(crate) tool: Option<String>,
    pub(crate) options: Option<Vec<String>>,
}

impl ApprovalPrompt {
    /// The prompt text the CLI presented.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            tool: None,
            options: None,
        }
    }

    /// Name the tool the decision is about.
    #[must_use]
    pub fn tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }

    /// Record the answer options the CLI offers.
    #[must_use]
    pub fn options<I, S>(mut self, options: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.options = Some(options.into_iter().map(Into::into).collect());
        self
    }
}
