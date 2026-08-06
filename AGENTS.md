# House rules

The conventions this repository is built on, in one tool-neutral file. Written for AI coding agents and equally binding on humans — an agent that reads this should not need to infer a convention from surrounding code, and a reviewer should not have to explain one twice.

[README.md](README.md) says what the project is; [CONTRIBUTING.md](CONTRIBUTING.md) covers the toolchain, the CI tiers, and the probe and capture tooling. This file is the rules.

## The one command

```
cargo xtask ci
```

Run it before pushing. It is format, `clippy -D warnings`, build, test, the schema-freshness gate, the probes, and the two gates below — everything the PR tier checks except the supply-chain gate, which is the next paragraph. The sequence lives in one place, [`xtask/src/main.rs`](xtask/src/main.rs); extend that rather than adding a parallel script, and never add a check to CI that a contributor cannot run locally with the same command.

If the change touches dependencies, `cargo xtask deny` as well — see [Dependencies](#dependencies). It is kept out of `cargo xtask ci` because it needs a tool installed, and that command's promise is that it needs nothing beyond `rustup` and `git`.

## Where code goes

Each crate is one layer, and the boundaries exist to be kept. If a change does not obviously belong to exactly one of these, that is a design question worth raising in the pull request rather than a filing decision to make quietly.

| Crate | Owns |
|---|---|
| [`crates/events`](crates/events) | The event taxonomy: the envelope every event shares, every event type and its payload, the NDJSON trace-record format, and the generated contract artifacts under `schema/` |
| [`crates/pty`](crates/pty) | Process hosting — allocate a pseudo-terminal, spawn a child in it, move bytes, resize, interrupt, terminate, contain descendants |
| [`crates/adapter-api`](crates/adapter-api) | The contract one CLI adapter implements: launch spec, pattern records, side-channel declaration, shutdown, version probe |
| [`crates/stream`](crates/stream) | Output bytes to structured events: control-sequence stripping, segmentation, matching, repaint dedup, side-channel readers, the reconstructed-screen fallback |
| [`crates/session`](crates/session) | One live session: the state machine, single-writer ownership, pending approvals, interrupt orchestration, shutdown |
| [`crates/core`](crates/core) | The runtime: session registry, the event bus and its single sequence-stamping point, bounded subscriber queues, replay, health |
| [`crates/transport`](crates/transport) | JSON-RPC 2.0 over stdio: framing, method surface, error codes |
| [`crates/harness`](crates/harness) | The conformance runner: scenario loading, fixture playback, trace comparison |
| [`crates/agent-bridge`](crates/agent-bridge) | The binary: configuration, logging init, wiring, the diagnostic subcommand. The only artifact distributed |
| [`crates/fake-cli`](crates/fake-cli) | The deterministic scripted stand-in every conformance scenario runs against |
| [`tools/`](tools) | Probes and reference binaries. They test the operating system, not the runtime, and nothing in the runtime may depend on them |
| [`xtask/`](xtask) | The dev-task runner behind `cargo xtask` |

Most of these are still empty. That is deliberate: the boundaries were agreed once, up front, so no later change has to invent one under deadline.

## Dependency direction

Bytes enter at `pty`, meaning is added by `stream`, state by `session`, ordering by `core`, and only the binary sees all of it at once. The direction is acyclic and one-way, and the complete allowed edge set is written down as data in `INTERNAL_DEPENDENCIES` in [`xtask/src/main.rs`](xtask/src/main.rs), where `cargo xtask workspace-gate` checks it on every run.

The edge that matters most is the one that never appears: `pty` must not depend on `adapter-api`. The moment the byte pipe knows which CLI it is hosting, adapter-shaped assumptions leak into the layer that has to stay a plain pipe, and a second adapter stops being cheap.

Adding a crate means adding it to that table with the dependencies it is allowed to have. A member missing from the table fails the gate rather than passing unchecked — stating the allowed edges is the point of adding a crate, not paperwork after the fact.

## Naming

Directories are short, package names carry the prefix: `crates/pty` is package `agent-bridge-pty`. The prefix is not for a registry — no library here is published — it is so a log line or a backtrace frame says which crate it came from. Two documented exceptions: the binary is the product and carries the bare name `agent-bridge`, and `xtask` is named for the `cargo xtask` alias that invokes it. The workspace gate enforces the rest.

## Errors

Typed [`thiserror`](https://docs.rs/thiserror) enums in the library crates. No `Box<dyn Error>`, and no `anyhow` outside the binary: the transport has to map a failure onto a specific protocol error code, and a session has to decide whether a failure is recoverable — neither can be done against an error that has been flattened into a string.

## stdout belongs to the protocol

The runtime speaks JSON-RPC over stdout. One stray line on it corrupts the wire for whatever client is reading, and the failure surfaces far from its cause. So the stdout print macros are banned workspace-wide by [`clippy.toml`](clippy.toml), and diagnostics go to stderr or a file — `tracing` once logging lands, line-atomic `eprintln!` in the tools until then. Partial-line `eprint!` is banned alongside the stdout macros because interleaved half-lines are unreadable.

A crate that legitimately owns its own stdout — the probes and reference tools, the scripted fake CLI, the transport's framer, and a future hook helper whose contract with the CLI is to print a decision — opts out with a crate-level or module-level `#![allow(clippy::disallowed_macros)]` carrying a comment saying why. Add the narrowest allow that works; do not loosen the ban.

## Unsafe code

Library crates carry `#![forbid(unsafe_code)]`. The exceptions are the crates that talk to the operating system directly — `pty` and `supervisor-ref` for process groups and job objects, `transport` for keeping a framed write whole — and each says so in a comment where the attribute would otherwise be. Removing a `forbid` is a visible, reviewable diff, which is the property that makes putting it there worthwhile.

## Dependencies

Versions are pinned once in the root [`Cargo.toml`](Cargo.toml) under `[workspace.dependencies]`; members inherit with `dep.workspace = true` and never name a version of their own. The workspace gate enforces that, so a version change stays a one-line diff in one file. Lint levels work the same way: `[workspace.lints]` in the root, `[lints] workspace = true` in every member.

Adding a dependency is a decision, not a reflex. Prefer the standard library, say in the pull request why the crate is worth its maintenance surface, and keep `xtask` dependency-free — a contributor should need nothing but `rustup` and `git`.

What gets added is also checked. [`deny.toml`](deny.toml) is the policy — no known-vulnerable, unmaintained, or yanked crates; permissive licenses only, so copyleft fails by not being on the allowlist; no wildcard version requirements; crates.io and nothing else — and `cargo xtask deny` is how you run it, the same way CI does. It needs `cargo install cargo-deny --locked` once. An advisory with no fix yet may be suppressed in `deny.toml`, but only with a reason and a `review by YYYY-MM-DD` date that the gate itself enforces: once the date passes, the build fails until someone looks again. Keeping the pinned set current is Dependabot's job, not a pre-release scramble; see [`.github/dependabot.yml`](.github/dependabot.yml).

## Contracts are generated, never hand-written

The published artifacts under [`schema/`](schema) — the two JSON Schemas and the taxonomy inventory — are produced from the Rust types in `crates/events`. Change the types and regenerate (`cargo run -p agent-bridge-events --bin schema-gen`); CI regenerates them itself and fails on any difference, so a hand-edited artifact and a forgotten regeneration fail the same way. The same principle applies to the crate-level documentation: those doc comments are where a crate's contract is stated, so a change that alters what a crate is responsible for updates its doc comment in the same commit, not later.

## Two gates worth knowing about

`cargo xtask ci` ends with both; they can also be run alone.

- **`cargo xtask workspace-gate`** — the layout contract: dependency direction, package naming, central version pinning, inherited lints. Its failure messages name the offending edge, crate, or dependency.
- **`cargo xtask drift-gate`** — three ways contracts here have drifted apart before. It scans tracked files for contradictions this project has repeatedly had to correct — two of them design contradictions, the third a claim that the single pre-push command covers the whole of CI, which stopped being true when the supply-chain gate became its own job and was written down in seven files by then — and it holds the event taxonomy to what asserts against it: every event type a golden trace names must be in the generated inventory, which in turn must never carry the two names that belong to other layers. The patterns and the reasoning are documented in `xtask/src/main.rs`. If you are writing one of them deliberately, add a `WAIVE-DRIFT: <why>` line to the commit message.

A third, `cargo xtask deny`, guards the dependency tree rather than this repository's own contracts, which is why it sits under [Dependencies](#dependencies) and outside `cargo xtask ci`.

## Branches, commits, and pull requests

Branches are `<type>/<slug>`: `feature/`, `fix/`, `chore/`, `docs/`, `test/`, `spike/`. Commit subjects follow [Conventional Commits](https://www.conventionalcommits.org/) — an imperative subject of at most 72 characters, and a body that explains **why**, in prose. The diff already shows what changed; a body that lists the changed files says nothing a reader could not see.

Every pull request must stand on its own for someone who has only this repository in front of them: state the rationale inline rather than pointing at an external tracker. Keep the diff to one change, and make sure `cargo xtask ci` is green before requesting review.
