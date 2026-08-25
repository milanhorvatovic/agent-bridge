//! The NDJSON conformance-trace format: the record shape, and the line
//! discipline that makes a trace file comparable byte for byte.
//!
//! A trace record and an event are the same thing in two serializations, not
//! two things: the wire envelope names the discriminant `type` and versions
//! the *event stream*, while a stored record names it `event_type` and
//! versions the *file format*. [`TraceRecord::from_event`] and
//! [`TraceRecord::to_kind`] are that mapping, written once here so no
//! consumer re-derives it.

use std::io::{BufRead, Write};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::envelope::Event;
use crate::kind::{EventKind, UnknownEvent};

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
    #[schemars(extend("pattern" = crate::schema::EVENT_TYPE_PATTERN))]
    pub event_type: String,
    /// The event's type-specific payload object.
    pub payload: Map<String, Value>,
    /// Identifier of the originating session. Required when one trace
    /// captures events across multiple sessions; single-session traces
    /// usually declare it ignored for comparison instead. Omitted and
    /// `null` are equivalent (not applicable); producers writing through
    /// this type omit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Correlates the record with one specific pending approval. Required
    /// — present and a string — on `prompt.approval_required` and
    /// `prompt.approval_withdrawn` records (the generated schema enforces
    /// this); on any other record, omitted and `null` are equivalent,
    /// even while an approval is pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    /// Ties together related records, for example every event emitted while
    /// servicing one caller request. Omitted and `null` are equivalent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<String>,
    /// Version of the *trace-record format*: today's value is the string
    /// `"1"`, distinct from the event envelope's integer `schema_version` —
    /// the two contracts version independently, so a change to one says
    /// nothing about the other. Producers may add optional fields without
    /// bumping it, and must bump it to remove or rename one.
    //
    // The extend restates "type" as plain string: the Option would derive
    // ["string", "null"], but a null here has no meaning (omit the field
    // instead) and the const rejects it anyway — publishing the dead null
    // branch would only mislead readers of the artifact.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(extend("type" = "string", "const" = crate::TRACE_SCHEMA_VERSION))]
    pub schema_version: Option<String>,
}

impl TraceRecord {
    /// The stored spelling of an emitted event.
    ///
    /// `None` when the event carries no `monotonic_ns`: the record format
    /// requires one, because replay pacing and inter-event timing are what
    /// traces are compared on. An event without a reading cannot be stored
    /// as a conformant record, and inventing a zero for it would put a lie
    /// in the corpus.
    pub fn from_event(event: &Event) -> Option<Self> {
        Some(Self {
            seq: event.seq,
            monotonic_ns: event.monotonic_ns?,
            event_type: event.kind.event_type().to_owned(),
            payload: payload_object(&event.kind),
            session_id: event.session_id.clone(),
            approval_id: event.approval_id.clone(),
            correlation_id: event.correlation_id.clone(),
            schema_version: Some(crate::TRACE_SCHEMA_VERSION.to_owned()),
        })
    }

    /// The typed event this record describes.
    ///
    /// Reassembles through the same tolerant path the wire uses: a record
    /// naming a type this revision does not publish — or one whose payload
    /// does not fit the published shape — resolves to
    /// [`EventKind::Unknown`] rather than failing, so a comparator reading a
    /// newer corpus keeps working.
    pub fn to_kind(&self) -> EventKind {
        let document = serde_json::json!({
            "type": self.event_type,
            "payload": self.payload,
        });
        serde_json::from_value(document).unwrap_or_else(|_| {
            EventKind::Unknown(UnknownEvent {
                event_type: self.event_type.clone(),
                payload: self.payload.clone(),
            })
        })
    }
}

/// The `payload` half of an event's adjacent `type` / `payload` pair.
fn payload_object(kind: &EventKind) -> Map<String, Value> {
    let mut document = serde_json::to_value(kind).expect("an event serializes infallibly");
    let payload = document.get_mut("payload").map(Value::take);
    let Some(Value::Object(payload)) = payload else {
        // Every payload in the taxonomy is a struct, so adjacent tagging
        // always pairs the type name with a JSON object.
        unreachable!("an event payload serializes to a JSON object");
    };
    payload
}

/// What can go wrong reading a trace file.
///
/// Every variant carries the line it happened on: a comparator pointed at a
/// corpus of hundreds of records is useless if it can only say that
/// something, somewhere, was malformed.
//
// Hand-written rather than derived with `thiserror`: this crate is the root
// of the dependency graph — every other crate in the workspace inherits what
// it depends on — and four `Display` arms are a smaller price than a
// proc-macro dependency there. The house rule that motivates `thiserror`
// (typed, matchable errors; never a stringly-typed `Box<dyn Error>`) is met
// either way.
#[derive(Debug)]
#[non_exhaustive]
pub enum TraceError {
    /// The file could not be read, or held bytes that are not UTF-8.
    Io(std::io::Error),
    /// A line was not a valid trace record.
    Record {
        /// One-based line number.
        line: usize,
        /// What the JSON parser objected to.
        source: serde_json::Error,
    },
    /// A line ended with CRLF. The format is LF-only so that traces compare
    /// byte for byte on every platform, and a CRLF file would compare equal
    /// on the machine that wrote it and unequal everywhere else.
    CarriageReturn {
        /// One-based line number.
        line: usize,
    },
    /// A line held no record. NDJSON has no blank-line convention, so this
    /// is a truncated or mis-assembled file rather than a formatting choice.
    BlankLine {
        /// One-based line number.
        line: usize,
    },
    /// The last line was not terminated. The format requires a trailing
    /// newline, so an unterminated final line means the file was cut off
    /// mid-write — the one corruption a reader can still see.
    MissingTrailingNewline {
        /// One-based line number.
        line: usize,
    },
}

impl std::fmt::Display for TraceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(source) => write!(f, "cannot read the trace: {source}"),
            Self::Record { line, source } => {
                write!(f, "line {line} is not a trace record: {source}")
            }
            Self::CarriageReturn { line } => {
                write!(f, "line {line} ends with CRLF; the trace format is LF-only")
            }
            Self::BlankLine { line } => write!(f, "line {line} is blank"),
            Self::MissingTrailingNewline { line } => write!(
                f,
                "line {line} is not terminated; the trace format requires a trailing newline"
            ),
        }
    }
}

impl std::error::Error for TraceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Record { source, .. } => Some(source),
            Self::CarriageReturn { .. }
            | Self::BlankLine { .. }
            | Self::MissingTrailingNewline { .. } => None,
        }
    }
}

/// Append one record as a trace line: compact JSON, then LF.
///
/// Writing record by record is what the runtime's capture path does — a
/// trace file is complete after every line, so a run that is killed leaves a
/// valid prefix rather than an unreadable file.
pub fn write_record<W: Write>(writer: &mut W, record: &TraceRecord) -> std::io::Result<()> {
    // A record is a struct of strings, integers, and a JSON object: nothing
    // in it can fail to serialize, so the only error left is the write.
    let line = serde_json::to_string(record).expect("a trace record serializes infallibly");
    writer.write_all(line.as_bytes())?;
    writer.write_all(b"\n")
}

/// Write a whole trace. Equivalent to [`write_record`] per record, including
/// the trailing newline the format requires at end of file.
pub fn write_records<'a, W, I>(writer: &mut W, records: I) -> std::io::Result<()>
where
    W: Write,
    I: IntoIterator<Item = &'a TraceRecord>,
{
    for record in records {
        write_record(writer, record)?;
    }
    Ok(())
}

/// Read a trace, one record per line.
///
/// Unknown top-level fields are ignored, per the format's
/// forward-compatibility rule; violations of the line discipline are
/// reported per line and do not stop the read, so one malformed record in a
/// corpus file does not hide the rest.
pub fn read_records<R: BufRead>(
    reader: R,
) -> impl Iterator<Item = Result<TraceRecord, TraceError>> {
    Records {
        reader,
        line: 0,
        buffer: String::new(),
        finished: false,
    }
}

struct Records<R> {
    reader: R,
    line: usize,
    buffer: String,
    finished: bool,
}

impl<R: BufRead> Iterator for Records<R> {
    type Item = Result<TraceRecord, TraceError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        self.buffer.clear();
        match self.reader.read_line(&mut self.buffer) {
            Err(err) => {
                // A read that failed once fails again; reporting the same
                // error forever would turn a broken file into a hang.
                self.finished = true;
                Some(Err(TraceError::Io(err)))
            }
            Ok(0) => {
                self.finished = true;
                None
            }
            Ok(_) => {
                self.line += 1;
                Some(self.parse_buffered())
            }
        }
    }
}

impl<R: BufRead> Records<R> {
    fn parse_buffered(&self) -> Result<TraceRecord, TraceError> {
        let line = self.line;
        let Some(record) = self.buffer.strip_suffix('\n') else {
            return Err(TraceError::MissingTrailingNewline { line });
        };
        if record.ends_with('\r') {
            return Err(TraceError::CarriageReturn { line });
        }
        if record.trim().is_empty() {
            return Err(TraceError::BlankLine { line });
        }
        serde_json::from_str(record).map_err(|source| TraceError::Record { line, source })
    }
}
