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

/// A string that is not a session id.
///
/// Typed rather than borrowed from the id library so the boundary that
/// parses wire parameters can map it without depending on which library
/// mints the ids.
#[derive(Debug, thiserror::Error)]
#[error("not a session id: {text:?}")]
pub struct InvalidSessionId {
    /// What was offered.
    text: String,
}

impl std::str::FromStr for SessionId {
    type Err = InvalidSessionId;

    /// Parse the id a session once displayed — the round trip the wire
    /// needs, since a caller's `session_id` parameter arrives as the
    /// string `create` handed out. Held to the type's own contract: the
    /// registry only ever mints v4, so a well-formed UUID of any other
    /// version is a value no session ever had — an invalid id, not a
    /// well-formed one that merely fails lookup.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse::<Uuid>()
            .ok()
            .filter(|uuid| uuid.get_version_num() == 4)
            .map(Self)
            .ok_or_else(|| InvalidSessionId { text: text.into() })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_displayed_id_round_trips_through_from_str() {
        // The wire's whole use of an id: create hands out a string, a
        // later call sends it back, and lookup needs the same value.
        let minted = SessionId::new();
        let parsed: SessionId = minted.to_string().parse().expect("must round-trip");
        assert_eq!(parsed, minted);
    }

    #[test]
    fn a_well_formed_uuid_of_another_version_is_not_a_session_id() {
        // The registry only mints v4: nil and time-based values are ids
        // no session ever had, and the wire must call them invalid
        // rather than well-formed-but-unknown.
        for foreign in [
            "00000000-0000-0000-0000-000000000000",
            "c232ab00-9414-11ec-b3c8-9f6bdeced846",
        ] {
            assert!(
                foreign.parse::<SessionId>().is_err(),
                "{foreign} must be rejected"
            );
        }

        let refusal = "not-a-uuid".parse::<SessionId>().expect_err("must refuse");
        assert!(refusal.to_string().contains("not-a-uuid"));
    }
}
