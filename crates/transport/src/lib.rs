//! JSON-RPC 2.0 over stdio.
//!
//! Length-prefixed framing — a `Content-Length` header, a blank line, then
//! exactly that many bytes of body and nothing after it — plus the method
//! surface the first external consumer drives the runtime through, the
//! error-code table those methods answer with, and the stdio discipline the
//! rest of the workspace is built to respect.
//!
//! That discipline is a single rule with a wide blast radius: **stdout carries
//! protocol frames and nothing else.** Logs go to a file or to stderr; a
//! diagnostic print anywhere in the process corrupts the wire for the client
//! reading it. The lint in `clippy.toml` bans the stdout macros across the
//! whole workspace, [`stdio::capture_stdout`] captures the real stdout for the
//! framer alone and repoints descriptor 1 at the log, and the framing tests
//! would catch corruption if any slipped through.
//!
//! # Shape
//!
//! [`serve`] is the loop the binary calls after startup: it reads frames off
//! one stream, dispatches each through the method table, writes responses back
//! through the core's bounded die-loudly writer, and streams a subscribed
//! session's events as `session.event` notifications — all until stdin closes,
//! `runtime.shutdown` arrives, or the parent stops reading. It is generic over
//! its streams, so the binary passes its captured stdio and the tests pass an
//! in-process duplex against the same code. [`Client`] is the framed client
//! the tests drive it with and the conformance harness reuses, so both sides
//! of the wire share one framing implementation.

// Not `forbid(unsafe_code)`, unlike most of the workspace: capturing the
// process stdout for the framer means duplicating and redirecting the raw
// descriptor, below what the standard library exposes. The unsafety is
// confined to `stdio`, behind a safe function.

mod client;
mod dispatch;
mod error;
mod framing;
mod method;
mod notify;
mod outbound;
mod rpc;
mod serve;
pub mod stdio;
mod timestamp;

pub use client::{Client, Message};
pub use dispatch::{RuntimeContext, RuntimeInfoRef};
pub use error::JsonRpcError;
pub use framing::{FrameError, FrameReader, encode};
pub use serve::{ServeControl, ServeOutcome, serve};
pub use stdio::{StdoutRedirect, capture_stdout};
pub use timestamp::rfc3339_now;

/// The transport's wire defaults, promoted from the design's config schema so
/// a caller that does not tune them still gets the contract values.
pub mod defaults {
    use std::time::Duration;

    /// The maximum inbound frame body — `transport.max_frame_bytes`, 16 MiB.
    /// Bounds a denial-of-service via an unbounded `Content-Length`, and is
    /// also the outbound writer's buffer capacity so a single legal frame can
    /// never trip its overflow ceiling.
    pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

    /// The stdin-drain grace — `transport.stdin_drain_seconds`, 30 s — a
    /// closing runtime gives its sessions to exit before their remainder is
    /// forced.
    pub const DRAIN_GRACE: Duration = Duration::from_secs(30);

    /// How long the outbound writer tolerates a non-reading parent making zero
    /// progress before die-loudly fires. Not a design-named knob; a few
    /// seconds is well past any scheduling hiccup and well short of a wedge.
    pub const STDOUT_DEADLINE: Duration = Duration::from_secs(5);
}
