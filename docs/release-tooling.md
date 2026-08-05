# Release tooling — decision note

**Decision (2026-08): hand-rolled tag workflow now; adopt dedicated release tooling only when binaries ship.**

## The need today

The only release artifacts this repository currently publishes are the three generated contract files (`schema/events.schema.json`, `schema/event-taxonomy.json`, `schema/trace-record.schema.json`) and the trace-format specification (`docs/trace-format.md`), attached to a `v0.0.x` pre-release tag so integrators can pin a versioned contract. That is artifact attachment, nothing more: no compiled binaries, no signing, no installers, no package-manager channels.

[`.github/workflows/release.yml`](../.github/workflows/release.yml) covers exactly that with a tag-triggered `gh release create` — no third-party actions, no credentials beyond the workflow's own token, nothing to configure. Adopting a release-tooling framework for four files would be tooling ahead of need.

## The direction for when binaries ship

The runtime, when it lands, distributes as a single self-contained binary per platform (macOS signed + notarized, Linux static, Windows signed), shipped inside per-platform archives with checksums — a shape both major candidates automate:

- **`dist`** (the `cargo-dist` lineage) — native to Cargo workspaces: reads the workspace, builds per-target archives, generates installers and the release pipeline. The natural fit for a Rust repository, **with one caveat to re-check at adoption time**: its maintenance situation has changed hands before, so verify the project is actively maintained before adopting it.
- **GoReleaser** — mature and actively maintained, with Rust build support; more of its configuration surface is general-purpose rather than Cargo-aware. The fallback if `dist` does not pass the maintenance check.

The recorded direction is **`dist` first, GoReleaser as fallback**, decided properly at the revisit point below — not now, when neither would be exercised by a real release.

## Revisit

- **Trigger**: the first release that ships a compiled runtime binary (equivalently: when signing certificates and per-platform archives become real work items).
- **Owner**: the repository maintainer.
- **Until then**: the hand-rolled workflow is deliberately minimal; resist growing it feature-by-feature into an unowned bespoke release system — that accretion is exactly what the revisit exists to preempt.
