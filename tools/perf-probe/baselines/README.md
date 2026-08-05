# Benchmark baselines

The regression gate's memory: one report JSON per OS/arch, named
`bench-latency-<os>-<arch>.json` (the `<os>`/`<arch>` values are Rust's
`std::env::consts` spellings — `macos`-`aarch64`, `linux`-`x86_64`,
`windows`-`x86_64` for the current CI matrix). `cargo xtask bench` compares
each run's latency report against the matching file here and fails on a P99
more than 20% worse; anything else — throughput, resource numbers — rides
along as reported drift without failing anything.

While no baseline exists for an OS the gate passes with a notice. That is
the bootstrap state, not a steady state: record a baseline as soon as a
trusted run exists.

**To record or raise a baseline**: take the `bench-latency.json` report from
a run you trust — a green CI run of the `bench` job on the target OS is the
canonical source, since the gate compares runner against runner — copy it
here under the matching name, and commit it with a message saying which run
it came from and why the raise is justified. The gate compares like against
like (same lane, same OS and architecture) and refuses anything else, so a
report from the wrong machine class cannot silently become a baseline.

Deliberate updates only. The gate exists to make "it got slower" a reviewed
decision with a diff, and a baseline refreshed from whatever ran last would
ratchet every drift into the new normal.
