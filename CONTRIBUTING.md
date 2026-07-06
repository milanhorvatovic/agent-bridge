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

It is **exactly what CI runs** — format check, `clippy -D warnings`, build, test, the self-test binary, and the drift gate — so if it is green locally it is green in CI. The check sequence lives in one place (`xtask/src/main.rs`); please extend that rather than inventing a parallel script.

Individual tasks:

```
cargo xtask drift-gate   # the reserved-pattern gate only
cargo fmt --all          # apply formatting (the CI step only *checks*)
```

## CI

CI runs on every push to `main` and every pull request, across three OSes (`ubuntu-24.04`, `macos-14`, `windows-2022`). The Windows image is a Server image (the closest hosted-runner match to the client target); real Windows-client behaviour is verified separately. Runner images and third-party actions are **pinned** — treat a bump as a reviewed change.

The current tier is the PR tier: fast, deterministic, no external services and no credentials. As the runtime grows, additional tiers attach — an opt-in live-CLI tier, a nightly tier (soak + fuzz), and a release tier (signed binaries) — each gated so ordinary PRs stay cheap.

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
