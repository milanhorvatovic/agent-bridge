//! The envelope every event shares, and the draft a producer hands to the
//! bus before the envelope's stamped fields exist.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::kind::EventKind;
use crate::payload::prompt::{ApprovalPrompt, PromptApprovalRequired, PromptApprovalWithdrawn};

/// One structured event emitted by the runtime.
///
/// Every event shares this envelope; the `type` discriminant names the
/// event and `payload` carries its type-specific fields. This revision
/// describes `schema_version` 1; the taxonomy grows within it by additive,
/// non-breaking changes only, so a consumer must ignore unknown event types
/// and unknown fields rather than reject them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "agent-bridge event")]
pub struct Event {
    /// Version of the event schema, for the whole event stream. Starts at
    /// 1 and is bumped only on a breaking change (field removed or renamed,
    /// field type changed, required field added, event type renamed or
    /// removed). Additive growth — new event types, new optional payload
    /// fields, new namespaces — keeps the version. Pre-release exceptions to
    /// this rule are taken as dated decisions, documented in the crate's
    /// module-level Growth and tolerance section.
    #[schemars(extend("const" = crate::SCHEMA_VERSION))]
    pub schema_version: u32,
    /// Identifier of the originating session, or `null` for events that are
    /// not scoped to one session.
    //
    // Required *and* nullable — the field must be present on every event,
    // null when unscoped. `required` alone would drop the null branch from
    // the generated schema, so the nullable type is restated via `extend`.
    #[schemars(required, extend("type" = ["string", "null"]))]
    pub session_id: Option<String>,
    /// The canonical ordering primitive: a per-session integer, starting at
    /// 0 on session create, monotonic and gap-free at generation. Ordering
    /// is by `seq` alone — never by `ts`.
    pub seq: u64,
    /// Optional process-monotonic counter in nanoseconds, for jitter and
    /// latency analysis. Not wall-clock time. Absent when unknown — never
    /// `null`, so "unknown" has exactly one wire spelling.
    //
    // The extend restates "type" as plain integer: the Option would derive
    // ["integer", "null"], but the producer omits the field when it has no
    // reading, and publishing a second spelling of "unknown" would let
    // producers and fixtures drift into both.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(extend("type" = "integer"))]
    pub monotonic_ns: Option<u64>,
    /// RFC 3339 wall-clock timestamp with millisecond resolution. Not an
    /// ordering key: wall clocks can move backward across corrections.
    #[schemars(extend("format" = "date-time"))]
    pub ts: String,
    /// Correlates the event with one specific pending approval. Carried
    /// (non-null) only on events tied to that approval — required on
    /// `prompt.approval_required` and `prompt.approval_withdrawn`;
    /// `null` on unrelated events even while approvals are pending.
    pub approval_id: Option<String>,
    /// Caller-supplied correlation handle, echoed across the request /
    /// response / event chain it belongs to.
    pub correlation_id: Option<String>,
    /// The namespaced `type` discriminant together with its `payload`.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// What a producer builds, before the fields only the bus can fill.
///
/// `schema_version`, `session_id`, `seq`, `ts`, and `monotonic_ns` are
/// stamped in one place when the event is published — `seq` in particular is
/// only gap-free if exactly one component assigns it. What is left is what
/// the producer actually knows: which event this is, and what it is
/// correlated with. Splitting the two is what keeps the stamping site
/// singular.
#[derive(Debug, Clone, PartialEq)]
pub struct EventBody {
    /// The event and its payload.
    pub kind: EventKind,
    /// The pending approval this event belongs to, if any.
    pub approval_id: Option<String>,
    /// The caller request this event belongs to, if any.
    pub correlation_id: Option<String>,
}

impl EventBody {
    /// An event tied to no particular approval — the ordinary case.
    ///
    /// Note what this does *not* do: it does not inherit an `approval_id`
    /// from a session that happens to be awaiting approval. An event carries
    /// one only when it is about that specific approval, so a session with
    /// three pending decisions still emits uncorrelated tokens and lifecycle
    /// events, and a caller matching on `approval_id` gets exactly the
    /// events belonging to the decision it is resolving.
    pub fn new(kind: EventKind) -> Self {
        Self {
            kind,
            approval_id: None,
            correlation_id: None,
        }
    }

    /// An event about one specific pending approval — the tool call it
    /// authorizes, or its resolution.
    pub fn for_approval(kind: EventKind, approval_id: impl Into<String>) -> Self {
        Self {
            kind,
            approval_id: Some(approval_id.into()),
            correlation_id: None,
        }
    }

    /// The approval prompt, and the only way to build one.
    ///
    /// The `approval_id` is an argument rather than a field a caller may
    /// forget, because a prompt the caller cannot resolve would leave the
    /// CLI blocked with no way out. [`PromptApprovalRequired`] is sealed
    /// against construction to make this the single path.
    pub fn approval_required(approval_id: impl Into<String>, prompt: ApprovalPrompt) -> Self {
        Self {
            kind: EventKind::PromptApprovalRequired(PromptApprovalRequired {
                prompt: prompt.prompt,
                tool: prompt.tool,
                options: prompt.options,
            }),
            approval_id: Some(approval_id.into()),
            correlation_id: None,
        }
    }

    /// The runtime withdrew a pending approval whose announcer vanished.
    /// Sealed like [`Self::approval_required`], so a withdrawal without
    /// the id it withdraws is unrepresentable.
    pub fn approval_withdrawn(approval_id: impl Into<String>) -> Self {
        Self {
            kind: EventKind::PromptApprovalWithdrawn(PromptApprovalWithdrawn {}),
            approval_id: Some(approval_id.into()),
            correlation_id: None,
        }
    }

    /// Attach the caller's correlation handle, so every event emitted while
    /// servicing one request can be tied back to it.
    #[must_use]
    pub fn with_correlation_id(mut self, correlation_id: impl Into<String>) -> Self {
        self.correlation_id = Some(correlation_id.into());
        self
    }
}
