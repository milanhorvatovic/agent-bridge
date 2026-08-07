The house rules for this repository live in **[AGENTS.md](../AGENTS.md)** — read that file.

It is deliberately the only copy. Rules duplicated per tool drift apart, and the version an agent happens to read stops matching the version CI enforces; a pointer cannot drift.

The three lines below are the one exception, because a pointer only works on a reader that follows it. Each is copied word for word out of the section named at the end, and `cargo xtask drift-gate` fails the build if any of them stops appearing there — a copy that cannot drift rather than a second source. They are here at all because each has been raised against this repository as a defect in code the three-OS matrix was compiling green at the time:

- **`size_of`, `size_of_val`, `align_of`, and `align_of_val` are prelude items.** Calling `size_of::<T>()` with no import is correct, and adding `use std::mem::size_of` on top of it fails the build — unused imports are denied workspace-wide.
- **Let-chains (`if let Some(x) = f() && x.ok()`) are stable in this edition,** and clippy's `collapsible_if` will ask you for one where a nested `if` would do. Rewriting a let-chain back into nested `if`s trades a build failure for a lint failure.
- whether something compiles is a question `cargo xtask ci` answers, and the three-OS matrix answers for the platforms you are not on

Source: [AGENTS.md § What the toolchain already gives you](../AGENTS.md#what-the-toolchain-already-gives-you).
