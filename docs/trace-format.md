# The conformance trace format

This document is the normative specification of the **NDJSON event-record format** — the second of the two published contract artifacts, next to the event-envelope schema. Conformance scenarios under `tests/corpus/` declare the event stream a runtime hosting that scenario is expected to emit; those declarations, the runtime's future capture output, and recorded-session replays all share this one file format. There is no per-consumer dialect.

The machine-readable counterpart is [`schema/trace-record.schema.json`](../schema/trace-record.schema.json), generated from the `crates/events` types and validated in CI against every committed golden trace. Where prose and schema could ever disagree, fix the types and regenerate — the artifacts are generated, never hand-written.

## File format

One event record per line, JSON-encoded, UTF-8 throughout. Line endings are LF (`\n`) only — no CRLF — so platform-independent diffing stays deterministic, and a trailing newline at end-of-file is required so concatenation and streaming appends behave correctly. Files use the `.ndjson` extension.

## Required fields on every record

- **`seq`** — integer. Monotonic per session: strictly increasing, gap-free within a single session's stream. Across sessions, `seq` is not coordinated.
- **`monotonic_ns`** — integer. Monotonic-clock reading at emission time, in nanoseconds. Used for inter-event timing analysis and replay pacing. Wall-clock timestamps, where captured, are declared non-deterministic in trace comparisons.
- **`event_type`** — string. Dotted hierarchical event-type name (`lifecycle.session.running`, `stream.token`, `prompt.approval_required`, …). The type names and their payload shapes are the event taxonomy published as [`schema/events.schema.json`](../schema/events.schema.json).
- **`payload`** — object. The event's type-specific fields.

## Optional fields

Present when applicable. For the three correlation-shaped fields, an omitted field and an explicit `null` are equivalent — both mean "not applicable" — and comparisons must treat them so; producers writing through the published types omit. (`schema_version` is the exception: it is either absent or the string `"1"`, never `null`.)

- **`correlation_id`** — string. Ties together related records, for example every event emitted while servicing one caller request.
- **`approval_id`** — string. Correlates the record to one specific pending approval. Required — present and a string — on `prompt.approval_required` records, and the record schema enforces exactly that; on every other record it is omitted or `null`, even while an approval is pending.
- **`session_id`** — string. Required when a single trace captures events across multiple sessions. Single-session traces typically declare it ignored for comparison in the scenario manifest instead.
- **`schema_version`** — string. The version of *this trace-record format*; today's value is `"1"`. Distinct from the event envelope's integer `schema_version` — the two contracts version independently.

Consumers **must ignore unknown top-level fields** — the standard JSON forward-compatibility convention, and what lets producers add optional fields without a version bump.

## Compatibility

- Producers may add new optional fields without bumping `schema_version`.
- Producers must not remove or rename an existing field without a `schema_version` bump and a documented migration path.
- A `schema_version` bump is coordinated: every consumer in this repository ships an update in the same release, never unilaterally.

## Scenario directories

A conformance scenario is a directory under `tests/corpus/<cli>/<scenario>/`:

```
tests/corpus/<cli>/<scenario>/
  scenario.json          # the script the fake CLI executes
  expected.ndjson        # the golden trace: the expected event stream, this format
  manifest.yaml          # tier, OS scope, and the fields comparison must ignore
```

Golden traces are forward contracts. Today CI validates them structurally (line discipline, required fields, gap-free `seq`) and against the published record schema on every commit; the comparator that enforces them against a live runtime's output arrives with the runtime work, consuming the `ignore_fields` each manifest declares. Scenario directories are permanent: new scenarios are added, existing ones are never silently modified or removed — a change to a committed trace is a behavior change and is reviewed as one.

## Sibling input artifacts

Captured real-CLI sessions live one level deeper — `tests/corpus/<cli>/<version>/<scenario>-<cols>x<rows>/`, pinned to the CLI version that produced them — and carry recorded *inputs* in their native shapes rather than golden traces:

```
  input.bytes            # raw terminal byte stream, verbatim
  input.timing.ndjson    # one {offset, monotonic_ns} record per read boundary,
                         # so replay reproduces split-across-reads pacing
  steps.ndjson           # the recording driver's labeled step log
  manifest.yaml          # CLI version, how it was installed, capture date, dimensions
  hook-payloads.ndjson   # (claude) one hook stdin payload per line, verbatim
  transcript.jsonl       # (claude) raw session-transcript lines, verbatim
```

The native input files carry no `schema_version` of ours — they are whatever the recorded CLI produced, which is exactly why they are captured per CLI version. When replay of these inputs lands, its *output* — the emitted event stream — is this format.

## Example

A five-record trace of a short approval session (line breaks within a record shown for readability only; a real file holds one record per line):

```ndjson
{"seq":1,"monotonic_ns":1200,"event_type":"lifecycle.session.created","payload":{"adapter":"fake"},"schema_version":"1"}
{"seq":2,"monotonic_ns":2400,"event_type":"lifecycle.session.running","payload":{},"approval_id":null,"schema_version":"1"}
{"seq":3,"monotonic_ns":5100,"event_type":"stream.token","payload":{"content":"Reading file..."},"correlation_id":"send-1","schema_version":"1"}
{"seq":4,"monotonic_ns":8000,"event_type":"prompt.approval_required","payload":{"prompt":"Allow filesystem write?","options":["y","n"]},"approval_id":"ap-c4d5","schema_version":"1"}
{"seq":5,"monotonic_ns":9900,"event_type":"lifecycle.session.closed","payload":{"exit_code":0},"schema_version":"1"}
```

This example is validated against the published record schema in CI (`crates/events/tests/golden_traces.rs`), so the document cannot show records its own schema rejects.
