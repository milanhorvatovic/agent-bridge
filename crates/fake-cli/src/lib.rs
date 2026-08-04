//! The library half of the fake CLI: the parts a *reader* of its output
//! needs as badly as the binary that writes them.
//!
//! The binary is the scripted stand-in every conformance scenario runs
//! against (see `main.rs` for that contract). Two pieces of it are shared
//! rather than private, for the same reason the probe fixtures share their
//! report-line protocol with the probes that read it: a reader with its own
//! copy of the definition eventually disagrees with the writer about it, and
//! the disagreement shows up as a corruption report nobody can explain.
//!
//! - [`generator`] — the derived-content line shapes and the rolling digest.
//!   Whoever checks a generated stream regenerates it from this module, so
//!   "what line 900 000 should say" has exactly one definition.
//! - [`clock`] — the system monotonic clock behind the `{ts}` token. The
//!   measurement it enables is a subtraction between two processes' readings,
//!   which only means anything if both read the same clock the same way.

pub mod clock;
pub mod exec;
pub mod generator;
pub mod scenario;
