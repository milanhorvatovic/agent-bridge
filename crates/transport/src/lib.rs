//! JSON-RPC 2.0 over stdio.
//!
//! Length-prefixed framing — a `Content-Length` header, a blank line, then
//! exactly that many bytes of body and nothing after it — plus the method
//! surface, the error-code table, and the stdio discipline the rest of the
//! workspace is built to respect.
//!
//! That discipline is a single rule with a wide blast radius: **stdout carries
//! protocol frames and nothing else.** Logs go to a file or to stderr; a
//! diagnostic print anywhere in the process corrupts the wire for the client
//! reading it. The lint in `clippy.toml` therefore bans the stdout macros
//! across the whole workspace, and the framer in this crate is one of the few
//! places that carries an explicit exemption, scoped to the module that owns
//! the write.
//!
//! Empty for now — framing lands first, then the methods that ride on it.

// Not `forbid(unsafe_code)`, unlike most of the workspace: owning the process
// stdio handles can mean going below the standard library's wrappers to keep
// framing writes whole.

#[cfg(test)]
mod tests {
    /// A placeholder so this crate builds and runs a test binary from the day
    /// it exists, rather than the day it first has behavior — a test harness
    /// that has never run is not a test harness. Delete it with the first real
    /// test.
    #[test]
    fn test_harness_is_wired() {}
}
