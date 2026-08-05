# agent-bridge — the AI CLI runtime

[![ci](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/ci.yml/badge.svg)](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/ci.yml)
[![nightly](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/nightly.yml/badge.svg)](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/nightly.yml)

A cross-platform runtime layer that lets tools and agents drive **interactive AI CLIs** (Claude Code and Codex CLI in v1) over a structured, event-normalized JSON-RPC protocol — while the CLIs keep running as the real, subscription-billed, human-visible interactive sessions their vendors expect.

It is infrastructure, not an orchestration platform: sessions are hosted under a PTY, approvals always surface to a human (no silent auto-approval), and events — tokens, tool-call lifecycle, approval prompts with correlation IDs, lifecycle, errors — arrive as a versioned, namespaced taxonomy instead of raw terminal bytes. For CLIs that expose in-session structured channels (Claude Code's hooks and session transcript), those are the primary event sources; the terminal is host, input, interrupt, and fallback.

## The contract, published first

The load-bearing contract of this project is not a binary — it is the **event taxonomy** integrators consume and the **trace format** conformance is judged by. Both are published as versioned artifacts *before* the runtime exists, so integrations can be built against them and the eventual runtime can be held to them:

- [`schema/events.schema.json`](schema/events.schema.json) — the **event envelope**: the fields every runtime event shares (`schema_version`, `type`, `session_id`, `seq`, `ts`, correlation fields, `payload`) with the starter set of event types the committed conformance scenarios exercise.
- [`schema/trace-record.schema.json`](schema/trace-record.schema.json) + [`docs/trace-format.md`](docs/trace-format.md) — the **NDJSON trace-record format**: the line shape of the conformance traces under [`tests/corpus/`](tests/corpus), which every committed golden trace is validated against in CI.

Both schemas are **generated from the Rust types in [`crates/events`](crates/events)**, never hand-written: CI regenerates them on every push and fails on any difference, so the code and the published contract cannot drift apart. The published set is deliberately a seed — the taxonomy grows in that crate, additively, within `schema_version` 1 (new event types, new optional payload fields, new namespaces); breaking shapes bump the version. Publishing early is safe *because* those growth rules are part of the contract: consumers ignore what they do not know.

Tagged pre-releases carry both schemas and the trace-format spec as downloadable artifacts, so a consumer can pin the contract at a version rather than tracking `main`.

## Status

**Validation phase. There is no runtime yet.** Everything below that exists is inspectable in this repository and runs under CI on every commit; everything that does not exist is stated, not implied. No dates are attached to any of it.

| Area | State |
|---|---|
| Event-schema contract | **Published** — generated from `crates/events`, freshness-gated in CI |
| Trace-format contract | **Published** — [`docs/trace-format.md`](docs/trace-format.md) + record schema, golden traces validated against it in CI |
| Cross-platform CI | **Green** — Linux / macOS / Windows matrix plus a Linux container lane on every commit; nightly endurance soaks |
| Platform probes | **Done** — PTY allocation (ConPTY on Windows), a real interactive CLI under a PTY, interrupt-byte vs signal delivery, resize propagation, byte-exact UTF-8 across split reads, post-session cleanup to baseline, streaming perf/soak with a resource monitor ([`tools/`](tools)) |
| Conformance corpus | **Started** — a deterministic scripted fake CLI ([`crates/fake-cli`](crates/fake-cli)), three starter scenarios with golden traces, and captured real-CLI fixtures at pinned versions, scrubbed of identity |
| Stub adapter | **Exists as a stub** — runs every committed scenario through the real launch path (spawn, drain, exit) in CI on all three OSes; deliberately a bare function so it cannot pre-commit the future adapter interface |
| Runtime — PTY host, stream/event pipeline, session management, JSON-RPC surface | **Not built yet** |
| Adapters for real CLIs | **Not built yet** — de-risked by the captured fixtures and a pattern-detection spike, but no adapter exists |
| Conformance-harness comparator | **Not built yet** — until it lands, golden traces are enforced structurally and against the published record schema, not against a live runtime |

## Development

One command, identical to what the PR-tier CI runs:

```
cargo xtask ci
```

[CONTRIBUTING.md](CONTRIBUTING.md) covers the toolchain (rustup reads the pinned `rust-toolchain.toml`; nothing else to install), the CI tiers, the probes, and the capture/scrub tooling. After changing event types in `crates/events`, regenerate the committed artifacts with `cargo run -p agent-bridge-events --bin schema-gen` — CI fails on stale or hand-edited schemas.

## Security

Report vulnerabilities privately per [SECURITY.md](SECURITY.md) — not in public issues.

## License

[MIT](LICENSE)
