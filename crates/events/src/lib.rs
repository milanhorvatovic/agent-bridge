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
//! safe, and both halves of this crate make the rule real rather than
//! aspirational: the envelope schema enforces payload shapes for the
//! published types but *admits* any other dotted event type (so additive
//! growth can never break a pinned validator), and deserializing an event
//! of an unknown type yields [`EventKind::Unknown`] with the type name and
//! payload preserved instead of an error.
//!
//! **Strictness lives in the schemas; the types are tolerant readers.**
//! The generated artifacts are where invalid shapes are *rejected* — CI
//! validates fixtures against them, and integrators can too. The Rust
//! types deliberately read leniently instead of duplicating that rejection:
//! unknown event types fall back to [`EventKind::Unknown`], and a spelling
//! the schema forbids (an explicit `null` where the contract says "absent",
//! as on [`Event::monotonic_ns`]) deserializes as absence and normalizes
//! away on the next serialize. A consumer holding this crate must never be
//! the component that drops an event a slightly-off producer emitted;
//! flagging that producer is the validator's job.

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
/// consumers must tolerate types they do not know — which this enum itself
/// honors: a `type` not in the published set deserializes to
/// [`EventKind::Unknown`] with the type name and payload preserved, never
/// to an error.
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
    /// An event type this revision does not know. The compatibility
    /// contract lets producers add event types within `schema_version` 1,
    /// so a consumer compiled against this revision must be able to receive
    /// a newer producer's events; this fallback carries the unrecognized
    /// type name and its payload through instead of failing the whole
    /// envelope. Tried last, only after every published type has failed to
    /// match — which makes the typed surface deliberately tolerant: a
    /// published type name over a payload that does not match its shape
    /// also lands here rather than erroring. Shape *enforcement* is the
    /// schema's job ([`event_schema`] rejects that record); this enum's job
    /// is to never be the reason a consumer drops an event.
    #[serde(untagged)]
    Unknown(UnknownEvent),
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
    #[schemars(extend("pattern" = EVENT_TYPE_PATTERN))]
    pub event_type: String,
    /// The event's type-specific payload object.
    pub payload: serde_json::Map<String, serde_json::Value>,
    /// Identifier of the originating session. Required when one trace
    /// captures events across multiple sessions; single-session traces
    /// usually declare it ignored for comparison instead. Omitted and
    /// `null` are equivalent (not applicable); producers writing through
    /// this type omit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Correlates the record with one specific pending approval. Required
    /// — present and a string — on `prompt.approval_required` records (the
    /// generated schema enforces this); on any other record, omitted and
    /// `null` are equivalent, even while an approval is pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    /// Ties together related records, for example every event emitted while
    /// servicing one caller request. Omitted and `null` are equivalent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Version of the *trace-record format* (distinct from the event
    /// envelope's integer `schema_version`). Today's value is `"1"`;
    /// producers may add optional fields without bumping it, and must bump
    /// it to remove or rename one.
    //
    // The extend restates "type" as plain string: the Option would derive
    // ["string", "null"], but a null here has no meaning (omit the field
    // instead) and the const rejects it anyway — publishing the dead null
    // branch would only mislead readers of the artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(extend("type" = "string", "const" = "1"))]
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

/// The dotted-hierarchical-name pattern both schemas hold event types to.
const EVENT_TYPE_PATTERN: &str = "^[a-z0-9_]+(\\.[a-z0-9_]+)+$";

/// The one published type whose records must carry a non-null
/// `approval_id`; both generated schemas enforce it by conditional.
const APPROVAL_REQUIRED_TYPE: &str = "prompt.approval_required";

/// The event-envelope schema (`schema/events.schema.json`), as a JSON value.
///
/// Derived from [`Event`], then reshaped so the artifact states the
/// compatibility contract exactly: the derive produces a *closed* union
/// over the published types (any other `type` fails validation), but the
/// contract's additive-growth rule needs the opposite — a validator pinned
/// to this artifact must keep passing when a newer producer adds event
/// types. So the union becomes a set of per-type conditionals: `type` is
/// any dotted name and `payload` any object at the top level, and each
/// published type's payload shape is enforced by an `if`/`then` on its
/// `type` constant. Unknown types pass the envelope checks; published
/// types are held to their shapes. (A plain union with an unknown-type arm
/// would not do: a published type over a malformed payload would slip
/// through the open arm, and payload enforcement would be lost.)
pub fn event_schema() -> serde_json::Value {
    let mut value =
        serde_json::to_value(schemars::schema_for!(Event)).expect("a schema serializes infallibly");
    let root = value
        .as_object_mut()
        .expect("a derived root schema is a JSON object");

    // The derive emits the flattened EventKind union as a root-level anyOf:
    // one entry per published type (an object schema with a `type` const
    // and a `payload` schema) plus the UnknownEvent fallback arm (a $ref,
    // no const). Every expectation here is asserted, so a schemars upgrade
    // that changes the derive's output shape fails generation loudly
    // instead of silently publishing a reshaped contract.
    let serde_json::Value::Array(variants) = root
        .remove("anyOf")
        .expect("the derived schema carries the EventKind union as anyOf")
    else {
        panic!("the derived anyOf is an array");
    };
    let mut conditionals = Vec::new();
    let mut fallback_arms = 0usize;
    for variant in variants {
        let serde_json::Value::Object(variant) = variant else {
            panic!("every derived union arm is a JSON object");
        };
        let Some(type_schema) = variant
            .get("properties")
            .and_then(|properties| properties.get("type"))
        else {
            // The UnknownEvent fallback arm — openness is expressed by the
            // top-level `type`/`payload` properties instead, so the arm
            // (and its now-unreferenced definition) is dropped.
            fallback_arms += 1;
            continue;
        };
        let type_const = type_schema
            .get("const")
            .expect("every published arm names its type as a const")
            .clone();
        let payload_schema = variant["properties"]
            .get("payload")
            .expect("every published arm carries a payload schema")
            .clone();
        let mut conditional = serde_json::Map::new();
        if let Some(description) = variant.get("description") {
            conditional.insert("description".to_owned(), description.clone());
        }
        conditional.insert(
            "if".to_owned(),
            serde_json::json!({
                "properties": { "type": { "const": type_const } },
                "required": ["type"]
            }),
        );
        // The approval prompt is the one published type with an envelope
        // obligation beyond its payload shape: its `approval_id` must be
        // present and non-null (it is what the caller resolves), so its
        // conditional enforces that too — the prose rule made checkable.
        let then = if type_const == serde_json::json!(APPROVAL_REQUIRED_TYPE) {
            serde_json::json!({
                "properties": {
                    "payload": payload_schema,
                    "approval_id": { "type": "string" }
                },
                "required": ["approval_id"]
            })
        } else {
            serde_json::json!({ "properties": { "payload": payload_schema } })
        };
        conditional.insert("then".to_owned(), then);
        conditionals.push(serde_json::Value::Object(conditional));
    }
    assert_eq!(
        fallback_arms, 1,
        "exactly one union arm is the UnknownEvent fallback"
    );
    assert!(
        !conditionals.is_empty(),
        "the published taxonomy is never empty"
    );
    root.insert("allOf".to_owned(), serde_json::Value::Array(conditionals));

    let defs = root
        .get_mut("$defs")
        .and_then(serde_json::Value::as_object_mut)
        .expect("the derived schema carries $defs");
    defs.remove("UnknownEvent")
        .expect("the dropped fallback arm referenced $defs/UnknownEvent");

    let properties = root
        .get_mut("properties")
        .and_then(serde_json::Value::as_object_mut)
        .expect("the derived schema carries the envelope properties");
    properties.insert(
        "type".to_owned(),
        serde_json::json!({
            "type": "string",
            "pattern": EVENT_TYPE_PATTERN,
            "description": "Namespaced event-type name: dotted and hierarchical, so consumers can subscribe by prefix. The types published in this revision are enumerated in the allOf conditionals; other dotted names are valid — new types arrive within schema_version 1, and consumers must not reject them."
        }),
    );
    properties.insert(
        "payload".to_owned(),
        serde_json::json!({
            "type": "object",
            // Explicit although it is the JSON Schema default: the sibling
            // trace-record artifact carries the flag (derived from its map
            // type), and the two published contracts should read the same
            // way in tooling that surfaces it.
            "additionalProperties": true,
            "description": "The event's type-specific fields. Shapes for the published types are enforced by the allOf conditionals; unknown fields inside any payload must be ignored."
        }),
    );
    let required = root
        .get_mut("required")
        .and_then(serde_json::Value::as_array_mut)
        .expect("the derived schema carries the envelope required list");
    required.push(serde_json::Value::String("type".to_owned()));
    required.push(serde_json::Value::String("payload".to_owned()));

    let schema =
        serde_json::from_value(value).expect("the reshaped schema is still a valid schema");
    schema_with_comment(schema)
}

/// The NDJSON trace-record schema (`schema/trace-record.schema.json`), as a
/// JSON value.
///
/// Derived from [`TraceRecord`], plus the one cross-field rule a per-field
/// derive cannot express: a `prompt.approval_required` record must carry
/// its `approval_id` as a string — the same conditional the envelope
/// schema enforces, so the two artifacts state one approval contract.
pub fn trace_record_schema() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(TraceRecord))
        .expect("a schema serializes infallibly");
    let root = value
        .as_object_mut()
        .expect("a derived root schema is a JSON object");
    let previous = root.insert(
        "allOf".to_owned(),
        serde_json::json!([{
            "description": "An approval prompt is the record the caller resolves, so its approval_id must be present and a string; on every other record the field is omitted or null.",
            "if": {
                "properties": { "event_type": { "const": APPROVAL_REQUIRED_TYPE } },
                "required": ["event_type"]
            },
            "then": {
                "properties": { "approval_id": { "type": "string" } },
                "required": ["approval_id"]
            }
        }]),
    );
    assert!(
        previous.is_none(),
        "the derived trace-record schema grew its own allOf; merge instead of overwriting"
    );
    let schema =
        serde_json::from_value(value).expect("the extended schema is still a valid schema");
    schema_with_comment(schema)
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
