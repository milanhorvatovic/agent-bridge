//! The event taxonomy — the contract every integration depends on.
//!
//! The runtime this project is building emits **structured events**, never
//! raw terminal bytes: tokens, lifecycle transitions, tool calls, approval
//! prompts, and errors arrive as versioned, namespaced JSON records. That
//! event shape is the load-bearing contract, so it is published first —
//! before the runtime exists — as three artifacts generated from the types
//! in this crate:
//!
//! - `schema/events.schema.json` — the **event envelope**: the fields every
//!   runtime event shares, plus the payload shape of every published event
//!   type ([`Event`] / [`EventKind`]).
//! - `schema/trace-record.schema.json` — the **NDJSON trace record**: the
//!   line shape of the conformance traces under `tests/corpus/`
//!   ([`TraceRecord`]; format contract in `docs/trace-format.md`).
//! - `schema/event-taxonomy.json` — the **taxonomy inventory**: every event
//!   type with what the runtime does with it ([`taxonomy`]), which is what
//!   lets tooling hold the corpus and the runtime to the same list.
//!
//! The first two are deliberately distinct shapes: the envelope is what the
//! runtime emits on its wire (discriminant key `"type"`, integer
//! [`SCHEMA_VERSION`]), the trace record is what the conformance corpus
//! stores and compares (key `"event_type"`, string
//! [`TRACE_SCHEMA_VERSION`], and only the fields trace comparison needs).
//! Both carry a field spelled `schema_version` and the two version
//! *different contracts* — the event stream and the file format — which is
//! why [`TraceRecord::from_event`] exists rather than a cast.
//!
//! **Generated, never hand-written.** The committed artifacts are produced
//! by `cargo run -p agent-bridge-events --bin schema-gen`; CI regenerates
//! them and fails on any difference (`schema-gen --check`), so the schema
//! and the code cannot drift apart. Hand-editing an artifact fails CI.
//!
//! # The shape of an event
//!
//! [`EventKind`] is one flat enum with one variant per event type, each
//! tagged with its full dotted name, rather than a namespace enum holding
//! leaf enums. The wire is the reason: `"lifecycle.session.created"` is a
//! single name on the wire and a single string to match a prefix
//! subscription against, and writing it down once — where serialization
//! reads it — beats assembling it from two enum levels at runtime. The cost
//! is a long enum; the benefit is that the type list and the wire list are
//! the same list.
//!
//! Producers do not build [`Event`] directly. They build an [`EventBody`] —
//! the event and what it correlates with — and the bus stamps the fields
//! only it can get right (`seq` above all: it is gap-free only because one
//! component assigns it).
//!
//! # Growth and tolerance
//!
//! The taxonomy grows *here*, additively, within `schema_version` 1: new
//! event types, new optional payload fields, new namespaces, and new error
//! codes under an existing error type are all non-breaking; removing or
//! renaming a field, changing a field's type, adding a required field, or
//! renaming an event type bumps [`SCHEMA_VERSION`]. Consumers must ignore
//! unknown event types and unknown fields — that is what makes early
//! publication safe, and this crate makes the rule real rather than
//! aspirational: the envelope schema enforces payload shapes for the
//! published types but *admits* any other dotted event type (so additive
//! growth can never break a pinned validator), deserializing an event of an
//! unknown type yields [`EventKind::Unknown`] with the type name and payload
//! preserved instead of an error, and an unrecognized error code is carried
//! verbatim rather than rejected.
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
//!
//! The one place the types are *stricter* than the wire is construction:
//! an approval prompt cannot be built without the `approval_id` that
//! resolves it (see [`EventBody::approval_required`]), because a prompt a
//! caller cannot answer leaves the CLI blocked with no way out.

#![forbid(unsafe_code)]

mod envelope;
mod kind;
mod manifest;
mod payload;
mod schema;
mod trace;

pub use envelope::{Event, EventBody};
pub use kind::{EventKind, UnknownEvent};
pub use manifest::{EmitClass, TaxonomyEntry, taxonomy};
pub use payload::control::*;
pub use payload::errors::*;
pub use payload::lifecycle::*;
pub use payload::notice::*;
pub use payload::prompt::*;
pub use payload::stream::*;
pub use payload::tool::*;
pub use schema::{canonical_json, event_schema, taxonomy_manifest, trace_record_schema};
pub use trace::{TraceError, TraceRecord, read_records, write_record, write_records};

/// Version of the event schema, for the whole event stream.
///
/// An integer, the same for every event and every session, bumped only on a
/// breaking change to the taxonomy. Not the trace-format version — that is
/// [`TRACE_SCHEMA_VERSION`], a string, versioning a different contract.
pub const SCHEMA_VERSION: u32 = 1;

/// Version of the NDJSON trace-record *format*.
///
/// A string, and deliberately not the same field as the event stream's
/// [`SCHEMA_VERSION`] despite the shared name on the wire: a new optional
/// field in the record format says nothing about the events it stores.
pub const TRACE_SCHEMA_VERSION: &str = "1";
