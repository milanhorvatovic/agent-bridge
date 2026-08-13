//! What a subscription receives: everything, one exact type, or a dotted
//! namespace.
//!
//! The predicate is a string comparison over the wire-form event-type name,
//! which is the payoff of the taxonomy's hierarchical names: `"tool.*"`
//! needs no registry of which types exist under `tool` — any type whose name
//! sits under that prefix matches, including types added after this
//! subscriber was written. That is the compatibility contract's read side:
//! a prefix subscriber receives additive growth without redeploying.

use agent_bridge_events::Event;

/// Which events a subscription receives.
///
/// Matching is over [`EventKind::event_type`](agent_bridge_events::EventKind::event_type),
/// the dotted wire name, so a filter means the same thing here as in a
/// client's subscription request.
#[derive(Debug, Clone)]
pub enum EventFilter {
    /// Every event on the channel.
    All,
    /// Exactly one event type, by its full dotted name — `"stream.token"`.
    /// No wildcard reading: `"tool.*"` as an exact filter matches nothing,
    /// because no event type is named that.
    Exact(String),
    /// A dotted-name prefix — everything under one namespace, or one
    /// sub-tree of it. The spellings `"tool"`, `"tool."`, and `"tool.*"`
    /// are equivalent: each is normalized to the segment boundary, so
    /// `"tool"` matches `tool.call_started` but never `toolbox.opened`.
    /// An empty prefix (or bare `"*"`) is a prefix of every name and
    /// matches everything, same as [`EventFilter::All`].
    ///
    /// Only that trailing `*` is a wildcard. One anywhere else —
    /// `"*.error"`, `"tool.*.result"` — is taken literally, and since no
    /// published type name contains a `*`, such a filter matches nothing;
    /// subscribing with one is logged as a warning so the dead
    /// subscription is loud instead of silent.
    Prefix(String),
}

impl EventFilter {
    /// Whether an event of this dotted type passes the filter.
    pub fn matches(&self, event_type: &str) -> bool {
        match self {
            Self::All => true,
            Self::Exact(name) => event_type == name,
            Self::Prefix(prefix) => {
                let base = prefix.strip_suffix('*').unwrap_or(prefix);
                let base = base.strip_suffix('.').unwrap_or(base);
                if base.is_empty() {
                    // "" (and so "*") is a prefix of every name; without
                    // this arm the boundary check below would demand a
                    // leading dot and match nothing, inverting the intent.
                    return true;
                }
                match event_type.strip_prefix(base) {
                    // The boundary check is what separates a namespace
                    // prefix from a raw string prefix: the remainder must
                    // start a new dotted segment (or be nothing, when a
                    // name equals the prefix exactly).
                    Some(rest) => rest.is_empty() || rest.starts_with('.'),
                    None => false,
                }
            }
        }
    }
}

/// One subscriber's filter set: empty means everything, otherwise any match
/// admits the event.
///
/// A session subscription carries exactly one [`EventFilter`]; a global
/// subscription carries one per requested namespace, with the empty set as
/// its documented "all namespaces" default. Holding both as a set lets the
/// session and global channels share one fanout path.
#[derive(Debug)]
pub(crate) struct FilterSet {
    filters: Vec<EventFilter>,
}

impl FilterSet {
    pub(crate) fn new(filters: Vec<EventFilter>) -> Self {
        for filter in &filters {
            // A `*` outside the recognized trailing position makes a filter
            // that can never match — a subscription that looks live and
            // delivers nothing forever. The API has no error channel for
            // it (a filter is data, not a fallible call), so the next best
            // thing to failing is being loud.
            let dead = match filter {
                EventFilter::All => None,
                EventFilter::Exact(name) => name.contains('*').then_some(name),
                EventFilter::Prefix(prefix) => prefix
                    .strip_suffix('*')
                    .unwrap_or(prefix)
                    .contains('*')
                    .then_some(prefix),
            };
            if let Some(pattern) = dead {
                tracing::warn!(
                    pattern = %pattern,
                    "filter contains a non-trailing `*` and can never match a published event type"
                );
            }
        }
        Self { filters }
    }

    pub(crate) fn admits(&self, event: &Event) -> bool {
        self.filters.is_empty()
            || self
                .filters
                .iter()
                .any(|filter| filter.matches(event.kind.event_type()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_admits_everything() {
        assert!(EventFilter::All.matches("stream.token"));
        assert!(EventFilter::All.matches("lifecycle.session.created"));
    }

    #[test]
    fn exact_is_exact() {
        let filter = EventFilter::Exact("stream.token".into());
        assert!(filter.matches("stream.token"));
        assert!(!filter.matches("stream.stderr"));
        assert!(!filter.matches("stream.token.extra"));
        assert!(!EventFilter::Exact("tool.*".into()).matches("tool.result"));
    }

    #[test]
    fn prefix_spellings_are_equivalent() {
        for spelling in ["tool", "tool.", "tool.*"] {
            let filter = EventFilter::Prefix(spelling.into());
            assert!(filter.matches("tool.call_started"), "{spelling}");
            assert!(filter.matches("tool.result"), "{spelling}");
            assert!(!filter.matches("stream.token"), "{spelling}");
            assert!(!filter.matches("toolbox.opened"), "{spelling}");
        }
    }

    #[test]
    fn prefix_respects_segment_boundaries_at_depth() {
        let filter = EventFilter::Prefix("lifecycle.session".into());
        assert!(filter.matches("lifecycle.session.created"));
        assert!(!filter.matches("lifecycle.sessionish.created"));
        assert!(!filter.matches("lifecycle.turn.started"));
    }

    #[test]
    fn empty_prefix_is_a_prefix_of_every_name() {
        assert!(EventFilter::Prefix(String::new()).matches("stream.token"));
        assert!(EventFilter::Prefix("*".into()).matches("runtime.error"));
    }
}
