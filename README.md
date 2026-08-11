# agent-bridge — the AI CLI runtime

[![ci](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/ci.yml/badge.svg)](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/ci.yml)
[![nightly](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/nightly.yml/badge.svg)](https://github.com/milanhorvatovic/agent-bridge/actions/workflows/nightly.yml)

A cross-platform runtime layer that lets tools and agents drive **interactive AI CLIs** (Claude Code and Codex CLI in v1) over a structured, event-normalized JSON-RPC protocol — while the CLIs keep running as the real, subscription-billed, human-visible interactive sessions their vendors expect.

It is infrastructure, not an orchestration platform: sessions are hosted under a PTY, approvals always surface to a human (no silent auto-approval), and events — tokens, tool-call lifecycle, approval prompts with correlation IDs, lifecycle, errors — arrive as a versioned, namespaced taxonomy instead of raw terminal bytes. For CLIs that expose in-session structured channels (Claude Code's hooks and session transcript), those are the primary event sources; the terminal is host, input, interrupt, and fallback.

## The contract, published first

The load-bearing contract of this project is not a binary — it is the **event taxonomy** integrators consume and the **trace format** conformance is judged by. Both are published as versioned artifacts *before* the runtime exists, so integrations can be built against them and the eventual runtime can be held to them:

- [`schema/events.schema.json`](schema/events.schema.json) — the **event envelope**: the fields every runtime event shares (`schema_version`, `type`, `session_id`, `seq`, `ts`, correlation fields, `payload`), and the payload shape of every event type the runtime will emit — session and turn lifecycle, tokens and unclassified output, tool calls, approval prompts, errors split by originating layer, and the reconnect and notice events.
- [`schema/event-taxonomy.json`](schema/event-taxonomy.json) — the **taxonomy inventory**: every event type with what the runtime does with it (broadcast and replayable, delivered to one re-attaching subscriber, or published but not yet emitted). It is what lets tooling — and an integrator writing a client — enumerate the surface without reading Rust.
- [`schema/trace-record.schema.json`](schema/trace-record.schema.json) + [`docs/trace-format.md`](docs/trace-format.md) — the **NDJSON trace-record format**: the line shape of the conformance traces under [`tests/corpus/`](tests/corpus), which every committed golden trace is validated against in CI.

All three are **generated from the Rust types in [`crates/events`](crates/events)**, never hand-written: CI regenerates them on every push and fails on any difference, so the code and the published contract cannot drift apart. The taxonomy grows in that crate, additively, within `schema_version` 1 (new event types, new optional payload fields, new namespaces, new error codes); breaking shapes bump the version. Publishing before the runtime exists is safe *because* those growth rules are part of the contract, and the artifacts encode them: the schemas enforce payload shapes for the published event types while *admitting* unknown ones, so a validator pinned to this version keeps passing as the taxonomy grows — consumers ignore what they do not know.

Tagged pre-releases carry all three artifacts and the trace-format spec as downloads, so a consumer can pin the contract at a version rather than tracking `main`.

## Status

**The runtime has begun: its first layer exists, and the rest does not yet.** Everything below that exists is inspectable in this repository and runs under CI on every commit; everything that does not exist is stated, not implied. No dates are attached to any of it.

| Area | State |
|---|---|
| Event-schema contract | **Published** — the full event taxonomy and its inventory, generated from `crates/events`, freshness-gated in CI and held to the conformance corpus by `cargo xtask drift-gate` |
| Trace-format contract | **Published** — [`docs/trace-format.md`](docs/trace-format.md) + record schema, golden traces validated against it in CI |
| Cross-platform CI | **Green** — Linux / macOS / Windows matrix plus a Linux container lane on every commit; nightly endurance soaks |
| Platform probes | **Done** — PTY allocation (ConPTY on Windows), a real interactive CLI under a PTY, interrupt-byte vs signal delivery, resize propagation, byte-exact UTF-8 across split reads, post-session cleanup to baseline, streaming perf/soak with a resource monitor ([`tools/`](tools)). A probe retires once part of the runtime covers the same ground; the allocation, interrupt-delivery, and resize probes have gone, and their findings are re-asserted against the PTY layer's API on every commit |
| Conformance corpus | **Started** — a deterministic scripted fake CLI ([`crates/fake-cli`](crates/fake-cli)), three starter scenarios with golden traces, and captured real-CLI fixtures at pinned versions, scrubbed of identity |
| Stub adapter | **Exists as a stub** — runs every committed scenario through the real launch path (spawn, drain, exit) in CI on all three OSes; deliberately a bare function so it cannot pre-commit the future adapter interface |
| Crate layout | **Stood up** — the runtime's layers exist as documented crates under [`crates/`](crates), most of them still empty; the dependency direction between them, the naming rule, and central version pinning are checked on every run by `cargo xtask workspace-gate` |
| Dependency supply chain | **Gated** — advisories, licenses, bans, and sources checked against the pinned tree by `cargo xtask deny` ([`deny.toml`](deny.toml)) on every commit, with advisories repeated nightly so a newly disclosed vulnerability fails the default branch on its own; updates arrive through [Dependabot](.github/dependabot.yml) |
| PTY host | **Built** — [`crates/pty`](crates/pty): allocate a terminal and spawn a child in it, read without ever splitting a character, write against a deadline, resize, deliver an interrupt as the byte a terminal sends rather than as a signal, contain every descendant in a process group or job object, and terminate. One interface over a POSIX and a Windows ConPTY backend, exercised against real terminals on all three platforms |
| Reconstructed screen | **Built** — [`crates/stream`](crates/stream): a headless terminal keeps what a CLI has drawn, so a menu-rendered dialog is a dialog rather than the fragments each repaint wrote. Fed on every byte and rendered only when asked; a line that has only been redrawn or has scrolled up a row is recognised as the line it already was, so it is not emitted twice; a session whose CLI prints lines keeps no screen and pays nothing. Replayed against the recorded real-CLI fixtures at both captured widths, against the same recordings re-cut at boundaries they never had, and against the requirement that no recorded session ever reports the same line twice |
| Stream reader | **Built** — [`crates/stream`](crates/stream): the per-session reader between the hosted terminal and everything that interprets it. Decoded text moves the moment it decodes — sub-line, never waiting for a newline — while the raw bytes feed the reconstructed screen in parallel; undecodable bytes become U+FFFD with a typed incident naming where and how much, coalesced into a burst when they arrive too fast to be worth reporting one by one; a consumer that stops reading stops the drain at a bounded buffer, so backpressure lands on the child instead of in runtime memory. Every byte in is accounted for as text out or replacement reported — an equation asserted across the suite and against real terminals |
| Runtime — the rest of the stream/event pipeline, session management, JSON-RPC surface | **Not built yet** |
| Adapters for real CLIs | **Not built yet** — de-risked by the captured fixtures and a pattern-detection spike, but no adapter exists |
| Conformance-harness comparator | **Not built yet** — until it lands, golden traces are enforced structurally and against the published record schema, not against a live runtime |

## Development

One command, run before pushing:

```
cargo xtask ci
```

If the change touches dependencies, `cargo xtask deny` as well. That command is not part of `cargo xtask ci` because it needs `cargo-deny` installed, and `ci` is meant to need nothing but rustup and git; [CONTRIBUTING.md](CONTRIBUTING.md) lists the lanes the PR tier runs beyond it.

[CONTRIBUTING.md](CONTRIBUTING.md) covers the toolchain (rustup reads the pinned `rust-toolchain.toml`; nothing else is required), the CI tiers, the probes, and the capture/scrub tooling. [AGENTS.md](AGENTS.md) carries the house rules — where code goes, which crate may depend on which, and the conventions CI enforces — in one copy, for human and AI contributors alike. After changing event types in `crates/events`, regenerate the committed artifacts with `cargo run -p agent-bridge-events --bin schema-gen` — CI fails on stale or hand-edited schemas.

## Security

Report vulnerabilities privately per [SECURITY.md](SECURITY.md) — not in public issues.

## License

[MIT](LICENSE)
