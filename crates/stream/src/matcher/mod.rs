//! The pattern-matcher engine: compiled multi-pattern evaluation over the
//! post-strip feed.
//!
//! Adapters declare matchers — YAML records for the text kinds, code for the
//! stateful and screen kinds — and this module runs them. At registration a
//! pack is loaded ([`loader`]), validated, and compiled: every literal and
//! every regex with an extractable prefix goes into one Aho-Corasick
//! automaton, so on the hot path a regex only ever runs on a line the
//! automaton has already flagged. Compile once, evaluate everywhere: the
//! compiled artifacts are shared by every session of the adapter that
//! registered them.
//!
//! Two ceilings bound the engine, and they are deliberately different
//! numbers enforced in different places. The evaluation *chain* per line is
//! benchmarked in CI against a fifty-microsecond P99 — a regression there is
//! a review problem, not a runtime decision. Each *individual* evaluation
//! runs under a fifty-millisecond wall-clock guard at runtime — a breach
//! there disables that matcher for that session and is reported once, so one
//! pathological pattern can never wedge a live session. Neither number backs
//! the other, and no code path shares them.

mod engine;
mod loader;
mod template;

pub use engine::{CompileError, EngineBuilder, EngineStats, MatcherEngine};
pub use loader::{LoadError, load_dir, parse_pack};
