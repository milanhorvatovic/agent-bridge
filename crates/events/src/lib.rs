//! The event taxonomy seed — the first public contract surface.
//!
//! The runtime this project is building emits **structured events**, never
//! raw terminal bytes: tokens, lifecycle transitions, approval prompts, and
//! errors arrive as versioned, namespaced JSON records. That event shape is
//! the load-bearing contract every future integration depends on, so it is
//! published first — before the runtime exists — as two JSON Schema
//! artifacts generated from the types in this crate:
//!
//! - `schema/events.schema.json` — the **event envelope**: the fields every
//!   runtime event shares, with the starter set of event types the committed
//!   conformance scenarios exercise ([`Event`] / [`EventKind`]).
//! - `schema/trace-record.schema.json` — the **NDJSON trace record**: the
//!   line shape of the conformance traces under `tests/corpus/`
//!   ([`TraceRecord`]; format contract in `docs/trace-format.md`).
//!
//! The two are deliberately distinct shapes: the envelope is what the
//! runtime emits on its wire (discriminant key `"type"`, integer
//! `schema_version`), the trace record is what the conformance corpus
//! stores and compares (key `"event_type"`, string `schema_version`, and
//! only the fields trace comparison needs).
//!
//! **Generated, never hand-written.** The committed artifacts are produced
//! by `cargo run -p agent-bridge-events --bin schema-gen`; CI regenerates
//! them and fails on any difference (`schema-gen --check`), so the schema
//! and the code cannot drift apart. Hand-editing an artifact fails CI.
//!
//! **Seed status and growth.** This crate publishes the subset of the
//! taxonomy that the three committed starter scenarios reference, plus the
//! resilience and approval events a non-trivial contract needs. The full
//! taxonomy grows *here*, additively, within `schema_version` 1: new event
//! types, new optional payload fields, and new namespaces are non-breaking;
//! removing or renaming a field, changing a field's type, or adding a
//! required field bumps `schema_version`. Consumers must ignore unknown
//! event types and unknown fields — that is what makes early publication
//! safe.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One structured event emitted by the runtime.
///
/// Every event shares this envelope; the `type` discriminant names the
/// event and `payload` carries its type-specific fields. This revision
/// describes `schema_version` 1 and publishes the starter subset of the
/// taxonomy; the taxonomy grows within `schema_version` 1 by additive,
/// non-breaking changes only, so a consumer must ignore unknown event types
/// and unknown fields rather than reject them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "agent-bridge event")]
pub struct Event {
    /// Version of the event schema, for the whole event stream. Starts at
    /// 1 and is bumped only on a breaking change (field removed or renamed,
    /// field type changed, required field added, event type renamed or
    /// removed). Additive growth — new event types, new optional payload
    /// fields, new namespaces — keeps the version.
    #[schemars(extend("const" = 1))]
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
    /// latency analysis. Not wall-clock time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monotonic_ns: Option<u64>,
    /// RFC 3339 wall-clock timestamp with millisecond resolution. Not an
    /// ordering key: wall clocks can move backward across corrections.
    #[schemars(extend("format" = "date-time"))]
    pub ts: String,
    /// Correlates the event with one specific pending approval. Carried
    /// (non-null) only on events tied to that approval — required on
    /// `prompt.approval_required`; `null` on unrelated events even while
    /// approvals are pending.
    pub approval_id: Option<String>,
    /// Caller-supplied correlation handle, echoed across the request /
    /// response / event chain it belongs to.
    pub correlation_id: Option<String>,
    /// The namespaced `type` discriminant together with its `payload`.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// The event's namespaced `type` paired with its type-specific `payload`.
///
/// Type names are dotted and hierarchical (`lifecycle.session.created`,
/// `stream.token`, …) so consumers can subscribe by namespace prefix. New
/// types arrive within existing namespaces without a `schema_version` bump;
/// consumers must tolerate types they do not know.
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
    /// Termination has been initiated.
    #[serde(rename = "lifecycle.session.closing")]
    LifecycleSessionClosing(LifecycleSessionClosing),
    /// The session has ended.
    #[serde(rename = "lifecycle.session.closed")]
    LifecycleSessionClosed(LifecycleSessionClosed),
    /// Incremental output: plain text after terminal-control stripping.
    #[serde(rename = "stream.token")]
    StreamToken(StreamToken),
    /// Output the runtime could not classify. Instead of dropping it, the
    /// runtime degrades to "here is the text" — the single most important
    /// resilience event: consumers see everything, classified or not.
    #[serde(rename = "stream.unrecognized_output")]
    StreamUnrecognizedOutput(StreamUnrecognizedOutput),
    /// The CLI is blocked on a human decision. The envelope's `approval_id`
    /// is non-null on this event and correlates the eventual resolution.
    #[serde(rename = "prompt.approval_required")]
    PromptApprovalRequired(PromptApprovalRequired),
}

/// Payload of `lifecycle.session.created`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionCreated {
    /// Name of the adapter hosting the session (its source tag).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
}

/// Payload of `lifecycle.session.launching`. No fields yet; fields arrive
/// additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionLaunching {}

/// Payload of `lifecycle.session.connecting`. No fields yet; fields arrive
/// additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionConnecting {}

/// Payload of `lifecycle.session.running`. No fields yet; fields arrive
/// additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionRunning {}

/// Payload of `lifecycle.session.closing`. No fields yet; fields arrive
/// additively.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionClosing {}

/// Payload of `lifecycle.session.closed`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
pub struct LifecycleSessionClosed {
    /// The CLI process's exit code, when it exited with one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
}

/// Payload of `stream.token`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamToken {
    /// The emitting adapter's source tag (for example `"claude"`), so
    /// consumers can filter when sessions of different CLIs are active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The text chunk, after terminal-control stripping.
    pub content: String,
}

/// Payload of `stream.unrecognized_output`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct StreamUnrecognizedOutput {
    /// The unclassified text, after terminal-control stripping, so the
    /// consumer can decide what to make of it.
    pub content: String,
}

/// Payload of `prompt.approval_required`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PromptApprovalRequired {
    /// The prompt text presented by the CLI.
    pub prompt: String,
    /// The answer options the CLI offers, when they are known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

/// One line of an NDJSON conformance trace.
///
/// Conformance traces record the event stream a scenario is expected to
/// produce, one JSON record per line. This is the storage/comparison shape
/// of an event — not the wire envelope: the discriminant key is
/// `event_type`, `schema_version` is the *trace-format* version as a
/// string, and only the fields trace comparison needs are required. The
/// full format contract (line discipline, comparison rules, sibling input
/// artifacts) is `docs/trace-format.md`. Consumers must ignore unknown
/// top-level fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(title = "agent-bridge trace record")]
pub struct TraceRecord {
    /// Monotonic per-session integer: strictly increasing, gap-free within
    /// one session's stream. Not coordinated across sessions.
    pub seq: u64,
    /// Monotonic clock reading at emission time, in nanoseconds. Used for
    /// inter-event timing analysis and replay pacing.
    pub monotonic_ns: u64,
    /// Dotted hierarchical event-type name, for example
    /// `lifecycle.session.running` or `stream.token`.
    #[schemars(extend("pattern" = "^[a-z0-9_]+(\\.[a-z0-9_]+)+$"))]
    pub event_type: String,
    /// The event's type-specific payload object.
    pub payload: serde_json::Map<String, serde_json::Value>,
    /// Identifier of the originating session. Required when one trace
    /// captures events across multiple sessions; single-session traces
    /// usually declare it ignored for comparison instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Correlates the record with one specific pending approval; `null` on
    /// records that merely coincide with a pending approval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    /// Ties together related records, for example every event emitted while
    /// servicing one caller request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Version of the *trace-record format* (distinct from the event
    /// envelope's integer `schema_version`). Today's value is `"1"`;
    /// producers may add optional fields without bumping it, and must bump
    /// it to remove or rename one.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(extend("const" = "1"))]
    pub schema_version: Option<String>,
}

/// The instruction stamped into every generated artifact, so a reader of
/// the file — not just of this repository — knows it is generated and how
/// to regenerate it.
const GENERATED_COMMENT: &str = "GENERATED FILE — do not edit by hand. Generated from the Rust types in \
     crates/events by `cargo run -p agent-bridge-events --bin schema-gen`; CI \
     regenerates this file and fails on any difference. Seed status: this \
     revision publishes the starter subset of the event taxonomy; the taxonomy \
     grows within schema_version 1 by additive, non-breaking changes only (new \
     event types, new optional payload fields, new namespaces), so consumers \
     must ignore unknown event types and unknown fields.";

/// The event-envelope schema (`schema/events.schema.json`), as a JSON value.
pub fn event_schema() -> serde_json::Value {
    schema_with_comment(schemars::schema_for!(Event))
}

/// The NDJSON trace-record schema (`schema/trace-record.schema.json`), as a
/// JSON value.
pub fn trace_record_schema() -> serde_json::Value {
    schema_with_comment(schemars::schema_for!(TraceRecord))
}

fn schema_with_comment(schema: schemars::Schema) -> serde_json::Value {
    let mut value = serde_json::to_value(schema).expect("a schema serializes infallibly");
    let object = value
        .as_object_mut()
        .expect("a derived root schema is a JSON object");
    object.insert(
        "$comment".to_owned(),
        serde_json::Value::String(GENERATED_COMMENT.to_owned()),
    );
    value
}

/// Render a JSON value in the canonical artifact form: object keys sorted,
/// two-space indentation, LF line endings, one trailing newline.
///
/// The committed artifacts must be byte-identical across regenerations on
/// every OS — that is what lets the freshness gate be a plain byte compare
/// and keeps artifact diffs reviewable. Sorting keys here makes the output
/// independent of serializer insertion order, so determinism is a property
/// of this function, not of any dependency's internals.
pub fn canonical_json(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value, 0);
    out.push('\n');
    out
}

fn write_canonical(out: &mut String, value: &serde_json::Value, indent: usize) {
    match value {
        serde_json::Value::Object(map) if map.is_empty() => out.push_str("{}"),
        serde_json::Value::Object(map) => {
            out.push_str("{\n");
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let last = keys.len() - 1;
            for (position, key) in keys.into_iter().enumerate() {
                push_indent(out, indent + 1);
                // `Value::String` rendering gives the exact JSON string
                // escaping, so keys and string values escape identically.
                out.push_str(&serde_json::Value::String(key.clone()).to_string());
                out.push_str(": ");
                write_canonical(out, &map[key], indent + 1);
                if position < last {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, indent);
            out.push('}');
        }
        serde_json::Value::Array(items) if items.is_empty() => out.push_str("[]"),
        serde_json::Value::Array(items) => {
            out.push_str("[\n");
            let last = items.len() - 1;
            for (position, item) in items.iter().enumerate() {
                push_indent(out, indent + 1);
                write_canonical(out, item, indent + 1);
                if position < last {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, indent);
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}
