# Security policy

## Reporting a vulnerability

Please report vulnerabilities **privately** — never in a public issue or pull request:

- Preferred: [GitHub private vulnerability reporting](https://github.com/milanhorvatovic/agent-bridge/security/advisories/new) on this repository.
- Alternatively: email the maintainer at <milan.horvatovic@gmail.com> with `[agent-bridge security]` in the subject.

Include what you can of: the affected file or component, a reproduction, the impact you see, and the commit or tag you tested. You will get an acknowledgement within 7 days; the fix, the advisory, and credit (if you want it) are coordinated with you before anything is published.

## Scope

This repository is in its validation phase: there is no runtime, no network service, and no released binary yet. What runs today — the platform probes, the scripted fake CLI, the schema generator, the dev-task runner — executes locally and in CI only. Reports are welcome for all of it; of particular interest:

- anything that lets a committed fixture or capture carry credentials or personal identity past the scrubbing described in [CONTRIBUTING.md](CONTRIBUTING.md),
- CI workflow weaknesses (secret exposure, unpinned or tampered dependencies in the check path),
- unsound process handling in the probes (a spawned process that can escape cleanup and outlive its session).

On that second point, the baseline to report against: third-party actions are pinned by full commit SHA, every crate version is pinned centrally in the root manifest, and [`deny.toml`](deny.toml) holds the resolved dependency tree to known-good advisories, permissive licenses, and crates.io as the only source — checked on every commit, and re-checked nightly for advisories disclosed since. A way past any of that is exactly the kind of report this section is asking for.

The event-schema and trace-format artifacts are data, not code; if you find a way a schema-conformant input breaks a consumer in this repository, that is a valid report too.

## Supported versions

Pre-release (`v0.x`) artifacts are supported at the latest tag only: fixes land on `main` and ship with the next tag rather than being backported.
