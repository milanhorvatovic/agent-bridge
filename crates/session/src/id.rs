//! The two identities a session record carries: its own, and its writer's.

use uuid::Uuid;

/// A session's identity: a UUIDv4, assigned by the registry at create and
/// never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(Uuid);

impl SessionId {
    /// Mint a fresh id. Random rather than sequential so an id says nothing
    /// about creation order and cannot be guessed from a neighbour's.
    #[expect(
        clippy::new_without_default,
        reason = "minting an id is an act, not a default value a struct fills in"
    )]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The identity of the transport peer that owns a session's write side.
///
/// State only in v1: the single transport peer always writes the
/// sessions it creates, so this field is carried — initialized to the
/// creator, cleared when the transport drops — but nothing acquires or
/// releases it until the multi-client transport lands. Carried
/// now so the record's shape does not have to change under a frozen trait.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SubscriberId(pub String);

impl std::fmt::Display for SubscriberId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
