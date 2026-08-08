//! Output bytes in, structured events out.
//!
//! The pipeline that turns what a CLI paints on a terminal into records a
//! client can act on: control-sequence stripping, segmentation into lines and
//! chunks, matching, deduplication of repainted regions, and event emission.
//!
//! Matching runs in two stages for a reason. A literal automaton decides which
//! lines a regular expression is even allowed to run against, so the expensive
//! engine sees a small fraction of the stream; each line is then held to a
//! fast per-line budget, and to a much slower hard timeout that keeps a
//! pathological pattern from stalling a live session. A pattern that misses
//! its budget is a reportable fault, not a hang.
//!
//! Two kinds of input feed this crate, and the preference between them is not
//! a detail. **Structured side channels** win wherever a CLI offers them — a
//! local channel carrying lifecycle and approval payloads, and a transcript
//! file tailed for content — because they carry the CLI's own account of what
//! happened rather than an inference drawn from what it drew. The
//! **reconstructed screen** is the fallback: a virtual terminal replays the
//! byte stream into a grid so the visible state can be matched when nothing
//! structured is available. That reconstruction belongs here, with the rest of
//! the interpretation, and never in the crate that hosts the process — which
//! stays a plain byte pipe precisely so this one can be tested without it.
//!
//! The reconstruction is the stage that exists so far; stripping,
//! segmentation, matching, and the side-channel readers land beside it. See
//! [`screen`].

#![forbid(unsafe_code)]

pub mod screen;

pub use screen::{
    EvalPointScheduler, EvalTrigger, Evaluation, NovelSpan, QUIET_PERIOD, ScreenState,
};
