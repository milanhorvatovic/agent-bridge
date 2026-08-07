# Contributing to agent-bridge

Thanks for your interest. This is an early-stage Rust project; the CI matrix and workspace scaffold are in place, the runtime is being built out. Everything you need is in this repository — contributions and pull requests are self-contained by design.

The conventions themselves — where code goes, which crate may depend on which, naming, errors, stdout discipline, branches and commits — live in **[AGENTS.md](AGENTS.md)**, in one copy, binding on human and AI contributors alike. This file covers the mechanics: toolchain, CI tiers, tooling.

## Development environment

The toolchain is pinned, so your local build and CI resolve to the same compiler. You need:

1. **[rustup](https://rustup.rs/)** — nothing else. On first `cargo` invocation, rustup reads `rust-toolchain.toml` and installs the pinned channel (with `rustfmt` and `clippy`) automatically.
2. **git** — for the drift gate.

No other tools. The dev-task runner (`xtask/`) is pure Rust and carries a single crate (`toml`, which the supply-chain gate needs to read `deny.toml`), so there is no `make` / `just` / shell prerequisite — cargo fetches what it needs — and it runs identically on Linux, macOS, and Windows.

One optional extra, needed only if you are touching dependencies: `cargo-deny`, which puts the supply-chain gate (`cargo xtask deny`) within reach locally. Install it at the version this repository pins — run `cargo xtask deny` and it will print the exact `cargo install` line if yours is missing or is a different version. The pin matters: CI installs one exact version, and this tool has changed its configuration schema before, so a newer local copy can disagree with CI about whether `deny.toml` is even valid. It is deliberately not a prerequisite for `cargo xtask ci` — that command's promise is that it needs nothing but the two tools above — and CI installs it in its own job, so forgetting it costs you a round trip rather than a broken build.

## The one command

Run this before pushing:

```
cargo xtask ci
```

It is format check, `clippy -D warnings`, build, test, the schema-freshness gate, the probe binaries, and the two layout/drift gates. The check sequence lives in one place (`xtask/src/main.rs`); please extend that rather than inventing a parallel script.

**The PR tier runs two lanes beyond it**, each its own CI job, and each separate because it needs something `cargo xtask ci` deliberately does not:

| Lane | Command | Why it is not in `ci` |
|---|---|---|
| Benchmarks | `cargo xtask bench` | Release builds and the committed per-OS baselines; `ci` is the fast lane |
| Supply chain | `cargo xtask deny` | Needs `cargo-deny` installed, and `ci` is meant to need nothing but rustup and git |

So run `cargo xtask deny` when your change touches dependencies, and `cargo xtask bench` when it could affect latency or throughput. This table is the one place that enumerates the difference — elsewhere in the repository you will find pointers here rather than a second copy, because a count restated in six files is a count that will disagree with itself the next time a lane is added.

Individual tasks:

```
cargo xtask probe          # the deterministic probes only (what the container lane runs)
cargo xtask live-probe     # probes that spawn a real CLI — needs credentials, see below
cargo xtask workspace-gate # the crate-layout gate only
cargo xtask drift-gate     # the reserved-pattern and event-taxonomy gate only
cargo xtask deny           # the dependency supply-chain gate (needs cargo-deny)
cargo xtask deny advisories # just the advisory check — what the nightly lane runs
cargo fmt --all            # apply formatting (the CI step only *checks*)

# regenerate the committed schema/ artifacts after changing event types in
# crates/events — they are generated, never hand-written, and CI fails on a
# stale or hand-edited artifact (`schema-gen --check` is the gate)
cargo run -p agent-bridge-events --bin schema-gen
```

## CI

CI runs on every push to `main` and every pull request, across three OSes (`ubuntu-24.04`, `macos-14`, `windows-2022`). The Windows image is a Server image (the closest hosted-runner match to the client target); real Windows-client behaviour is verified separately. Runner images and third-party actions are **pinned** — treat a bump as a reviewed change.

### Tiers

The **PR tier** is the default: fast and credential-free. Almost all of it is deterministic — the same commit gives the same verdict — with one deliberate exception: the supply-chain gate's advisory check reads the RUSTSEC database, so a vulnerability disclosed since your last push can turn a PR red without the diff having changed. That is the point of it, and it is why the advisory check also runs nightly; the licence, ban, and source checks beside it are a pure function of the committed lockfile and cannot move on their own. Everything `cargo xtask ci` runs is in it, plus two lanes that stand on their own. `cargo xtask bench` measures latency and throughput in release builds and holds the latency P99s to the committed per-OS baselines under `tools/perf-probe/baselines/` — a change may not get more than 20% worse than the recorded number. Baselines are updated deliberately: copy a trusted run's report over the baseline file and commit it, so every raise is a reviewed diff. `cargo xtask deny` is the supply-chain gate described below; it runs on one OS because it reads the dependency graph rather than compiling it.

The **live tier** spawns a real interactive CLI. It costs API quota and depends on an upstream service, so it is opt-in per pull request: add the `ci:live` label. Its jobs run serially against one credential, and the credential is logged only as present or absent — never its value. Live assertions check event *shapes and sequences* (a hook fired, a turn completed, the transcript grew), never exact model output, which is not reproducible.

To run the live tier locally you need the CLI on your `PATH` plus either `ANTHROPIC_API_KEY` or a `CLAUDE_CONFIG_DIR` pointing at an authenticated config.

The **nightly tier** (`nightly.yml`) carries what cannot fit a PR's wall-clock budget: two half-hour streaming soaks — a synthetic generated stream and recorded real-CLI sessions replayed at their captured pacing — with a file-descriptor/handle and memory monitor over both, plus the nightly benchmark set, all of it `cargo xtask soak-nightly`. It also re-runs `cargo xtask deny advisories`, which is there for the opposite reason: not because it is slow, but because its answer changes without a commit behind it. A red nightly alerts the maintainer and never blocks a merge; reproduce it locally with the same xtask task. If it refuses to reproduce, suspect the difference rather than the task: a scheduled run always starts from a clean checkout, so a build directory your machine has had for weeks is exactly the thing it does not have — clear `target/` and try again. Its fixture re-record and fuzz lanes attach as the runtime grows.

The **release tier** (`release.yml`) fires on a `v*` tag: it re-runs the schema-freshness gate at the tag and attaches the contract artifacts — the two generated schemas, the taxonomy inventory, and the trace-format spec — to the GitHub release, so integrators can pin a versioned contract. It is deliberately minimal (see `docs/release-tooling.md`); the signed-binaries release tier arrives with the runtime.

### The supply-chain gate

`cargo xtask deny` runs [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) over the resolved dependency tree, against the policy in [`deny.toml`](deny.toml). Four checks:

- **advisories** — nothing known-vulnerable, unmaintained, or yanked, per the [RUSTSEC](https://rustsec.org/) database.
- **licenses** — every dependency carries a permissive license this MIT project may ship. Copyleft is denied by construction: it is simply absent from the allowlist, and anything not on the list fails.
- **bans** — no wildcard version requirements, and duplicate versions of a crate are warned about against a curated list of the ones already known.
- **sources** — everything comes from crates.io. There are no git dependencies; if one ever becomes necessary it is allowed as a single exact repository URL, never as blanket permission to fetch from git.

The first of those runs on the nightly lane as well as on every pull request, because the advisory database is updated daily and a vulnerability disclosed today should fail the default branch tonight without waiting for someone to push. The other three are a pure function of the committed lockfile, so a pull request that was green cannot become red on its own.

If an advisory has no fix available yet, add a suppression to `deny.toml` with the reason it stands and a `review by YYYY-MM-DD` date. The date is not decoration: `cargo xtask deny` fails once it passes, so the entry has to be renewed with a fresh justification or removed. A suppression with no date fails immediately.

Updates arrive on their own through [Dependabot](.github/dependabot.yml), which watches both the Cargo workspace and the pinned GitHub Actions weekly. Minor and patch bumps are grouped into one pull request; majors and security fixes come separately, so the ones that need reading are the ones that stand out.

### Probes

One tool under `tools/` is not a probe: `tools/stub-adapter` runs every committed fake-CLI conformance scenario through the real launch path (spawn the scripted CLI, drain its output, observe its exit) and reports probe-style lines. It is deliberately a bare function rather than a trait implementation, so nothing in it pre-commits the adapter interface the runtime will define.

Probes are throwaway binaries under `tools/` that test the OS, not the runtime — a PTY that cannot be allocated, or an interactive CLI that will not stream, is exactly the kind of thing that only shows at runtime and only on one platform. They print one machine-readable `step=… status=… detail="…"` line per step and exit non-zero with a step-identifying code, so CI asserts the exit status while a human reads the log. They are the one place `println!` is allowed (see `clippy.toml`).

A probe is temporary by design, but it is deleted when a durable part of the runtime covers the same ground on the same platforms — not merely when its findings have been written down — and the deletion happens in the change that provides the replacement, never as a separate cleanup. The PTY layer's own suites retired the allocation, interrupt-delivery, and resize probes that way, and `cargo xtask probe` now runs those suites alongside the probe binaries that remain: the container lane runs only that slice, and whether a terminal can be allocated inside a container is exactly what it exists to answer.

`tools/interactive-probe` additionally carries two commands a maintainer runs by hand:

```
# The four-point hook-channel verification. Runs on any OS; the run that
# matters is on Windows 11 client hardware, where the console is ConPTY and
# the hook channel is a named pipe. A POSIX run is the comparison baseline.
cargo run -p agent-bridge-interactive-probe -- fourpoint --model haiku

# Compare the candidate virtual-terminal libraries on a captured byte stream.
cargo run -p agent-bridge-interactive-probe --features vt-eval -- vt-eval <capture.ndjson>

# Record one scripted session into a fixture directory (byte stream + timing,
# labeled step log, and — for Claude Code — hook payloads and transcript).
# Scenario scripts live under tests/capture-scenarios/<cli>/*.record.json.
cargo run -p agent-bridge-interactive-probe -- record \
  --script tests/capture-scenarios/fake/roundtrip.record.json \
  --out /tmp/fixture --cli-bin target/debug/fake-cli --cli-version fake

# One capture sitting: every scenario for one CLI at one pinned version, both
# terminal sizes, into tests/corpus/<cli>/<version-label>/ — scrubbed, then
# sized against the per-adapter corpus budget. The scrub masks (same-length,
# so timing offsets stay valid): the local username; every --mask needle;
# names auto-derived from git identity (committer name + remote owner), which
# catch the developer's name where it only reaches a fixture through a
# machine-local path; and the temp directory's per-user hash component (macOS
# /var/folders/<bucket>/<hash>/T), also auto-derived, which the CLI paints
# into every cwd and transcript_path — host-specific noise, masked so the
# corpus stays portable. Every needle — auto-derived or --mask — must pass
# the same safety rule (at least 3 bytes and a letter or non-ASCII, so a
# needle cannot corrupt the numeric NDJSON fields, version strings, or prose
# it would otherwise match). An identity that fails it — a very short or an
# all-digit owner/name — therefore cannot be raw-byte-masked by any path and
# must be reviewed and scrubbed by hand.
# Two account-specific tokens the campaign cannot derive
# must be passed as --mask: the logged-in account's email local part (the
# identity in the account footer the TUI paints into the byte stream), and the
# account display name shown in the splash greeting "Welcome back <name>!".
# Once the email local part is masked, the scrub masks the trailing @domain
# automatically — reading it from a control-stripped view, so it clears even
# the domain a differential repaint split across a cursor-move escape (which a
# raw needle cannot reach). After masking, a surviving email-shaped run (a
# forgotten local-part needle) OR an unmasked splash greeting — both seen
# through terminal control sequences — aborts the run, so a forgotten needle
# fails loudly rather than leaking. The claude campaign spends real session
# quota; --dry-run prints the matrix.
cargo xtask capture-campaign --cli claude --bin <versioned-claude> \
  --version-label 2.1.201 --install "npm @anthropic-ai/claude-code@2.1.201" \
  --mask <account-email-local-part> --mask <account-display-name> \
  --model haiku --dry-run

# --only <scenario> records just that one scenario into an existing version
# directory (still scrubbing and sizing the whole adapter) — for adding a
# scenario to a corpus already captured, without re-recording the rest.
cargo xtask capture-campaign --cli claude --bin <versioned-claude> \
  --version-label 2.1.201 --install "npm @anthropic-ai/claude-code@2.1.201" \
  --mask <account-email-local-part> --mask <account-display-name> \
  --only <scenario> --model haiku
```

### Workspace gate

`cargo xtask workspace-gate` reads the manifests and holds the workspace to its own layout contract: the dependency direction between crates, the short-directory/prefixed-package naming rule, central version pinning, and inherited lint levels. Each rule and the reasoning behind it is in [AGENTS.md](AGENTS.md); the allowed dependency edges are written down as data in `xtask/src/main.rs`, which is also where a new crate registers the dependencies it is allowed to have.

It parses manifests rather than compiling anything, so a forbidden edge is reported before it is built against — and its failure messages name the offending edge, crate, or dependency rather than leaving you to find it.

### Drift gate

`cargo xtask drift-gate` fails the build on two kinds of drift. The first: a tracked file re-introducing one of the contract contradictions this project has repeatedly had to correct. The second: the event taxonomy and what asserts against it coming apart — every event type a golden trace under `tests/corpus/` names must appear in the generated inventory (`schema/event-taxonomy.json`), and that inventory must never carry a name belonging to another layer. A scenario asserting an event the runtime has no way to emit would otherwise pass review and then fail forever. The rationale and the exact patterns are documented in `xtask/src/reserved.rs`, the single file the scan exempts, because it is where they are spelled out. If you are *intentionally* writing something the gate flags, add a line to your commit message:

```
WAIVE-DRIFT: <why this is correct here>
```

## Code conventions

In [AGENTS.md](AGENTS.md), and only there — a rule restated in two files is a rule that will eventually say two different things. It covers where code goes, the dependency direction between crates, naming, error types, the stdout discipline, unsafe code, dependencies, and the branch and commit conventions. `cargo fmt` and `cargo clippy -- -D warnings` are the floor beneath all of it, and nothing in the check path may be bash-only: the runner is Rust so that Windows contributors run what everyone else runs.

## Pull requests

- Keep each PR **self-contained**: its description should stand on its own for a reader who has only this repository. State rationale inline rather than pointing at external trackers.
- Keep the diff scoped to one change; open a separate PR for unrelated cleanup.
- Make sure `cargo xtask ci` is green before requesting review.
- **Automated review here hides its findings.** The overview it posts routinely says it generated no comments while carrying real ones inside a collapsed `Suppressed comments` block — on one pull request that block held six findings, three of them behavioural, and the visible comments beside it were all wrong. Open the block; weigh what is in it on the evidence rather than on where it appeared.
- A review claim that code **will not compile** is checkable, and worth checking before acting on it: `cargo xtask ci` answers for this platform and the three-OS matrix for the others. See [AGENTS.md](AGENTS.md#what-the-toolchain-already-gives-you) for the two the reviewer here keeps getting wrong.

## License

By contributing you agree your contributions are licensed under the repository's [MIT license](LICENSE).
