# Contributing to agent-bridge

Thanks for your interest. This is an early-stage Rust project; the CI matrix and workspace scaffold are in place, the runtime is being built out. Everything you need is in this repository — contributions and pull requests are self-contained by design.

## Development environment

The toolchain is pinned, so your local build and CI resolve to the same compiler. You need:

1. **[rustup](https://rustup.rs/)** — nothing else. On first `cargo` invocation, rustup reads `rust-toolchain.toml` and installs the pinned channel (with `rustfmt` and `clippy`) automatically.
2. **git** — for the drift gate.

No other tools. The dev-task runner (`xtask/`) is pure Rust with no dependencies, so there is no `make` / `just` / shell prerequisite, and it runs identically on Linux, macOS, and Windows.

## The one command

Run this before pushing:

```
cargo xtask ci
```

It is **exactly what the PR-tier CI runs** — format check, `clippy -D warnings`, build, test, the probe binaries, and the drift gate — so if it is green locally it is green in CI. The check sequence lives in one place (`xtask/src/main.rs`); please extend that rather than inventing a parallel script.

Individual tasks:

```
cargo xtask probe        # the deterministic probes only (what the container lane runs)
cargo xtask live-probe   # probes that spawn a real CLI — needs credentials, see below
cargo xtask drift-gate   # the reserved-pattern gate only
cargo fmt --all          # apply formatting (the CI step only *checks*)
```

## CI

CI runs on every push to `main` and every pull request, across three OSes (`ubuntu-24.04`, `macos-14`, `windows-2022`). The Windows image is a Server image (the closest hosted-runner match to the client target); real Windows-client behaviour is verified separately. Runner images and third-party actions are **pinned** — treat a bump as a reviewed change.

### Tiers

The **PR tier** is the default: fast, deterministic, no external services and no credentials. Everything `cargo xtask ci` runs is in it, plus the benchmark lane: `cargo xtask bench` measures latency and throughput in release builds and holds the latency P99s to the committed per-OS baselines under `tools/perf-probe/baselines/` — a change may not get more than 20% worse than the recorded number. Baselines are updated deliberately: copy a trusted run's report over the baseline file and commit it, so every raise is a reviewed diff.

The **live tier** spawns a real interactive CLI. It costs API quota and depends on an upstream service, so it is opt-in per pull request: add the `ci:live` label. Its jobs run serially against one credential, and the credential is logged only as present or absent — never its value. Live assertions check event *shapes and sequences* (a hook fired, a turn completed, the transcript grew), never exact model output, which is not reproducible.

To run the live tier locally you need the CLI on your `PATH` plus either `ANTHROPIC_API_KEY` or a `CLAUDE_CONFIG_DIR` pointing at an authenticated config.

The **nightly tier** (`nightly.yml`, also runnable as `cargo xtask soak-nightly`) carries what cannot fit a PR's wall-clock budget: two half-hour streaming soaks — a synthetic generated stream and recorded real-CLI sessions replayed at their captured pacing — with a file-descriptor/handle and memory monitor over both, plus the nightly benchmark set. A red nightly alerts the maintainer and never blocks a merge; reproduce it locally with the same xtask task. Its fixture re-record and fuzz lanes, and a release (signed binaries) tier, attach as the runtime grows.

### Probes

Probes are throwaway binaries under `tools/` that test the OS, not the runtime — a PTY that cannot be allocated, or an interactive CLI that will not stream, is exactly the kind of thing that only shows at runtime and only on one platform. They print one machine-readable `step=… status=… detail="…"` line per step and exit non-zero with a step-identifying code, so CI asserts the exit status while a human reads the log. They are the one place `println!` is allowed (see `clippy.toml`).

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

### Drift gate

`cargo xtask drift-gate` fails the build if a tracked file re-introduces one of two contract contradictions this project has repeatedly had to correct. The rationale and the exact patterns are documented in `xtask/src/main.rs`. If you are *intentionally* writing something the gate flags, add a line to your commit message:

```
WAIVE-DRIFT: <why this is correct here>
```

## Code conventions

These apply to human and AI contributors alike:

- `cargo fmt` + `cargo clippy -- -D warnings` are the floor.
- `thiserror` errors in library crates (no `Box<dyn Error>`); `anyhow` allowed in the binary.
- stdout belongs to the JSON-RPC wire; log to files/stderr, not stdout.
- Cross-platform: no bash-only tooling in the check path.

## Pull requests

- Keep each PR **self-contained**: its description should stand on its own for a reader who has only this repository. State rationale inline rather than pointing at external trackers.
- Keep the diff scoped to one change; open a separate PR for unrelated cleanup.
- Make sure `cargo xtask ci` is green before requesting review.

## License

By contributing you agree your contributions are licensed under the repository's [MIT license](LICENSE).
