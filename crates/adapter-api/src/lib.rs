//! The contract one CLI adapter implements.
//!
//! An adapter describes a single interactive CLI: how to launch it (argument
//! vector, environment policy, working directory), what patterns in its output
//! mean, which structured side channels it offers in place of reading the
//! screen, how to ask it to stop, and how to interrogate its version. This is
//! the seam the whole runtime is built around, so it is deliberately narrow
//! and carries nothing terminal-specific — an adapter states intent, and the
//! runtime decides how to carry that intent out.
//!
//! That narrowness is what keeps a second adapter cheap and a non-terminal
//! integration possible later without redesigning the runtime. It is also why
//! pattern matching is split in two: the pattern records and the matching
//! protocol live here — [`matcher`] — while the engine that compiles and runs
//! them lives in the stream crate. An adapter says what to look for; it never
//! runs the search itself.
//!
//! The matcher protocol landed first; the launch and shutdown halves —
//! [`LaunchSpec`] and [`ShutdownHint`], consumed by the session layer's
//! create seam and close path — landed with the session class. The `Adapter`
//! trait itself is still frozen deliberately, in its own change, once there
//! is enough runtime behind it to know the shape is right.

#![forbid(unsafe_code)]

pub mod launch;
pub mod matcher;

pub use launch::{InputStep, LaunchSpec, ShutdownHint, ShutdownSignal};
pub use matcher::{
    Anchor, Captures, DEFAULT_PRIORITY, EmitSpec, MatchOutcome, MatcherId, MatcherKind,
    MatcherSpec, MatcherState, NovelRow, PatternRecord, ScreenDiff, ScreenMatcher, StateLifetime,
    StatefulMatcher, Template, TemplateValue, TextMatcherType, TextWindow,
};
