//! The generated contract artifacts, and the canonical form they are written
//! in.
//!
//! Everything published under `schema/` is derived from the types in this
//! crate: the envelope schema and the trace-record schema from their
//! `JsonSchema` derives, the taxonomy inventory from the same derived union.
//! Nothing here is hand-written, and CI regenerates all three and fails on
//! any difference — so a type change that forgot to regenerate and a
//! hand-edited artifact fail the same way.

use serde_json::{Map, Value};

use crate::envelope::Event;
use crate::manifest::taxonomy;
use crate::trace::TraceRecord;

/// The instruction stamped into every generated artifact, so a reader of
/// the file — not just of this repository — knows it is generated and how
/// to regenerate it.
const GENERATED_COMMENT: &str = "GENERATED FILE — do not edit by hand. Generated from the Rust types in \
     crates/events by `cargo run -p agent-bridge-events --bin schema-gen`; CI \
     regenerates this file and fails on any difference. The event taxonomy \
     grows within schema_version 1 by additive, non-breaking changes only (new \
     event types, new optional payload fields, new namespaces, new error codes \
     under an existing error type), so consumers must ignore event types, \
     payload fields, and error codes they do not recognize.";

/// The dotted-hierarchical-name pattern both schemas hold event types to.
pub(crate) const EVENT_TYPE_PATTERN: &str = "^[a-z0-9_]+(\\.[a-z0-9_]+)+$";

/// The event types whose records must carry a non-null `approval_id` —
/// the prompt the caller resolves, and the withdrawal that ends it; both
/// generated schemas enforce it by conditional.
const APPROVAL_ID_TYPES: &[&str] = &["prompt.approval_required", "prompt.approval_withdrawn"];

/// One published event type, as the `JsonSchema` derive describes it.
struct PublishedVariant {
    event_type: Value,
    payload: Value,
    description: Option<Value>,
}

/// The derived envelope schema, split into the shared envelope and one entry
/// per published event type.
///
/// The derive emits the flattened event union as a root-level `anyOf`: one
/// entry per published type (an object schema with a `type` const and a
/// `payload` schema) plus the unknown-event fallback arm (a `$ref`, no
/// const). Every expectation here is asserted, so a schemars upgrade that
/// changes the derive's output shape fails generation loudly instead of
/// silently publishing a reshaped contract.
fn derived_event_schema() -> (Value, Vec<PublishedVariant>) {
    let mut value =
        serde_json::to_value(schemars::schema_for!(Event)).expect("a schema serializes infallibly");
    let root = value
        .as_object_mut()
        .expect("a derived root schema is a JSON object");

    let Value::Array(arms) = root
        .remove("anyOf")
        .expect("the derived schema carries the event union as anyOf")
    else {
        panic!("the derived anyOf is an array");
    };

    let mut variants = Vec::new();
    let mut fallback_arms = 0usize;
    for arm in arms {
        let Value::Object(arm) = arm else {
            panic!("every derived union arm is a JSON object");
        };
        let Some(type_schema) = arm
            .get("properties")
            .and_then(|properties| properties.get("type"))
        else {
            // The unknown-event fallback arm — openness is expressed by the
            // top-level `type`/`payload` properties instead, so the arm
            // (and its now-unreferenced definition) is dropped.
            fallback_arms += 1;
            continue;
        };
        variants.push(PublishedVariant {
            event_type: type_schema
                .get("const")
                .expect("every published arm names its type as a const")
                .clone(),
            payload: arm["properties"]
                .get("payload")
                .expect("every published arm carries a payload schema")
                .clone(),
            description: arm.get("description").cloned(),
        });
    }
    assert_eq!(
        fallback_arms, 1,
        "exactly one union arm is the unknown-event fallback"
    );
    assert!(
        !variants.is_empty(),
        "the published taxonomy is never empty"
    );

    let defs = root
        .get_mut("$defs")
        .and_then(Value::as_object_mut)
        .expect("the derived schema carries $defs");
    defs.remove("UnknownEvent")
        .expect("the dropped fallback arm referenced $defs/UnknownEvent");

    (value, variants)
}

/// Every event type this revision publishes, in the order the enum declares
/// them.
pub(crate) fn published_event_types() -> Vec<String> {
    derived_event_schema()
        .1
        .into_iter()
        .map(|variant| match variant.event_type {
            Value::String(event_type) => event_type,
            other => panic!("an event type is a JSON string, found {other}"),
        })
        .collect()
}

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
pub fn event_schema() -> Value {
    let (mut value, variants) = derived_event_schema();
    let root = value
        .as_object_mut()
        .expect("a derived root schema is a JSON object");

    let conditionals: Vec<Value> = variants
        .into_iter()
        .map(|variant| {
            let mut conditional = Map::new();
            if let Some(description) = variant.description {
                conditional.insert("description".to_owned(), description);
            }
            conditional.insert(
                "if".to_owned(),
                serde_json::json!({
                    "properties": { "type": { "const": variant.event_type } },
                    "required": ["type"]
                }),
            );
            // The approval prompt and its withdrawal are the published
            // types with an envelope obligation beyond their payload
            // shape: the `approval_id` must be present and non-null (it
            // is what the caller resolves, and what the withdrawal ends),
            // so their conditionals enforce that too — the prose rule
            // made checkable.
            let then = if APPROVAL_ID_TYPES
                .iter()
                .any(|approval_type| variant.event_type == serde_json::json!(approval_type))
            {
                serde_json::json!({
                    "properties": {
                        "payload": variant.payload,
                        "approval_id": { "type": "string" }
                    },
                    "required": ["approval_id"]
                })
            } else {
                serde_json::json!({ "properties": { "payload": variant.payload } })
            };
            conditional.insert("then".to_owned(), then);
            Value::Object(conditional)
        })
        .collect();
    root.insert("allOf".to_owned(), Value::Array(conditionals));

    // One payload-internal cross-field rule the derive cannot express: a
    // surviving-process count is only ever reported beside an unverified
    // cleanup — the payload doc's invariant, made checkable. The count's
    // own floor of one comes from the field's derive attribute; this
    // conditional adds the pairing.
    let closed = root
        .get_mut("$defs")
        .and_then(Value::as_object_mut)
        .and_then(|defs| defs.get_mut("LifecycleSessionClosed"))
        .and_then(Value::as_object_mut)
        .expect("the closed payload is a published definition");
    let previous = closed.insert(
        "allOf".to_owned(),
        serde_json::json!([{
            "description": "A surviving-process count is only reported beside an unverified cleanup: remaining_processes present requires cleanup_verified to be false.",
            "if": { "required": ["remaining_processes"] },
            "then": {
                "properties": { "cleanup_verified": { "const": false } },
                "required": ["cleanup_verified"]
            }
        }]),
    );
    assert!(
        previous.is_none(),
        "the closed payload grew its own allOf; merge instead of overwriting"
    );

    let properties = root
        .get_mut("properties")
        .and_then(Value::as_object_mut)
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
        .and_then(Value::as_array_mut)
        .expect("the derived schema carries the envelope required list");
    required.push(Value::String("type".to_owned()));
    required.push(Value::String("payload".to_owned()));

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
pub fn trace_record_schema() -> Value {
    let mut value = serde_json::to_value(schemars::schema_for!(TraceRecord))
        .expect("a schema serializes infallibly");
    let root = value
        .as_object_mut()
        .expect("a derived root schema is a JSON object");
    let previous = root.insert(
        "allOf".to_owned(),
        serde_json::json!([{
            "description": "An approval prompt is the record the caller resolves, and its withdrawal names the prompt it ends, so their approval_id must be present and a string; on every other record the field is omitted or null.",
            "if": {
                "properties": { "event_type": { "enum": APPROVAL_ID_TYPES } },
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

/// The taxonomy inventory (`schema/event-taxonomy.json`), as a JSON value.
///
/// Not a schema: the flat list of what this revision publishes, with what
/// the runtime does with each type. It exists because the questions "does
/// this event type exist" and "should it appear in a session's stream" are
/// asked by tools that cannot read Rust — the drift gate that holds the
/// conformance corpus to the taxonomy, and any integrator enumerating what
/// they can subscribe to.
pub fn taxonomy_manifest() -> Value {
    let event_types: Vec<Value> = taxonomy()
        .into_iter()
        .map(|entry| {
            serde_json::json!({
                "event_type": entry.event_type,
                "emit_class": entry.class.as_str(),
            })
        })
        .collect();
    serde_json::json!({
        "$comment": GENERATED_COMMENT,
        "schema_version": crate::SCHEMA_VERSION,
        "emit_classes": {
            "ring": "Broadcast to every subscriber of the session, ordered by seq, and buffered for backfill.",
            "subscription_notification": "Delivered to one subscriber as part of its own subscription; never broadcast, never buffered.",
            "reserved": "Published as a contract; no runtime revision emits it yet."
        },
        "event_types": event_types,
    })
}

fn schema_with_comment(schema: schemars::Schema) -> Value {
    let mut value = serde_json::to_value(schema).expect("a schema serializes infallibly");
    let object = value
        .as_object_mut()
        .expect("a derived root schema is a JSON object");
    object.insert(
        "$comment".to_owned(),
        Value::String(GENERATED_COMMENT.to_owned()),
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
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(&mut out, value, 0);
    out.push('\n');
    out
}

fn write_canonical(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            out.push_str("{\n");
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let last = keys.len() - 1;
            for (position, key) in keys.into_iter().enumerate() {
                push_indent(out, indent + 1);
                // `Value::String` rendering gives the exact JSON string
                // escaping, so keys and string values escape identically.
                out.push_str(&Value::String(key.clone()).to_string());
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
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
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
