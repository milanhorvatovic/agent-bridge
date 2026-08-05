//! The type-specific fields each event carries, one module per namespace.
//!
//! Every type here is re-exported at the crate root, which is the path
//! consumers use; the modules exist so that a namespace's types — and the
//! prose explaining what that namespace is for — sit together.

pub(crate) mod control;
pub(crate) mod errors;
pub(crate) mod lifecycle;
pub(crate) mod notice;
pub(crate) mod prompt;
pub(crate) mod stream;
pub(crate) mod tool;
