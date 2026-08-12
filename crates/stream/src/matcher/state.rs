//! Per-session matcher state: the cells stateful matchers think in, owned
//! here and never by the adapter.
//!
//! One value of [`SessionMatcherState`] exists per (session, engine)
//! pairing, held by the session's stream task and handed to every
//! evaluation. Keeping it beside the session rather than inside the shared
//! engine is what makes the ownership rule cheap to honor: sessions never
//! contend for each other's state, closing a session drops its cells with
//! it, and the two lifetime boundaries are plain method calls the session
//! actor invokes on the transitions it already owns.
//!
//! The cells are indexed, not keyed: cell `i` belongs to the engine's
//! `i`-th stateful registration, assigned at [`MatcherEngine::new_session`]
//! time. The pairing is positional because both sides come from the same
//! compilation — a state object is only ever used with the engine that
//! created it.

use agent_bridge_adapter_api::{MatcherState, StateLifetime};

use super::engine::MatcherEngine;

/// How many completed lines back a stateful matcher's window reaches.
///
/// Multi-line detections this engine is built for — a prompt above its
/// option rows, a marker above its output — sit within a few lines of each
/// other; a matcher assembling something longer keeps it in its own state
/// cell, which is what the cell is for. Deeper history multiplies the
/// per-line copy cost for every session that registers any stateful
/// matcher, so the window stays shallow until a real matcher needs more.
pub const TEXT_WINDOW_DEPTH: usize = 8;

/// One session's matcher state: the stateful cells, their lifetimes, and
/// the sliding window of recent lines.
pub struct SessionMatcherState {
    /// Cell `i` belongs to the engine's `i`-th stateful registration.
    pub(crate) cells: Vec<MatcherState>,
    /// Parallel to `cells`: which boundary clears each.
    lifetimes: Vec<StateLifetime>,
    /// The completed lines before the current one, oldest first, at most
    /// [`TEXT_WINDOW_DEPTH`]. Maintained only when the engine has stateful
    /// matchers — nothing else reads it.
    pub(crate) recent: Vec<String>,
}

impl SessionMatcherState {
    pub(crate) fn new(lifetimes: Vec<StateLifetime>) -> Self {
        Self {
            cells: lifetimes.iter().map(|_| MatcherState::new()).collect(),
            lifetimes,
            recent: Vec::new(),
        }
    }

    /// The session closed: every cell clears, both lifetimes. The value is
    /// normally dropped right after — this exists so the clearing is a
    /// testable statement rather than a hope about drop order.
    pub fn on_session_close(&mut self) {
        for cell in &mut self.cells {
            cell.clear();
        }
        self.recent.clear();
    }

    /// The session moved from running to awaiting an approval: `per_prompt`
    /// cells clear, `per_session` cells persist.
    pub fn on_awaiting_approval(&mut self) {
        for (cell, lifetime) in self.cells.iter_mut().zip(&self.lifetimes) {
            if *lifetime == StateLifetime::PerPrompt {
                cell.clear();
            }
        }
    }

    /// How many cells currently hold state — what the lifetime tests
    /// assert on, and a shape-only number safe to log.
    pub fn occupied_cells(&self) -> usize {
        self.cells.iter().filter(|cell| !cell.is_empty()).count()
    }

    /// Slides the window forward past a completed line.
    pub(crate) fn push_line(&mut self, line: &str) {
        if self.recent.len() == TEXT_WINDOW_DEPTH {
            self.recent.remove(0);
        }
        self.recent.push(line.to_string());
    }
}

impl std::fmt::Debug for SessionMatcherState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SessionMatcherState({} of {} cells occupied, {} recent lines)",
            self.occupied_cells(),
            self.cells.len(),
            self.recent.len()
        )
    }
}

impl MatcherEngine {
    /// A fresh state object for one session of this engine's adapter.
    ///
    /// Pass it to every evaluation for that session, call the two
    /// transition methods on the boundaries the session actor owns, and
    /// drop it with the session.
    pub fn new_session(&self) -> SessionMatcherState {
        SessionMatcherState::new(self.stateful_lifetimes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_slides_and_stays_bounded() {
        let mut state = SessionMatcherState::new(Vec::new());
        for number in 0..(TEXT_WINDOW_DEPTH + 3) {
            state.push_line(&format!("line {number}"));
        }
        assert_eq!(state.recent.len(), TEXT_WINDOW_DEPTH);
        assert_eq!(state.recent.first().map(String::as_str), Some("line 3"));
        assert_eq!(
            state.recent.last().map(String::as_str),
            Some(&*format!("line {}", TEXT_WINDOW_DEPTH + 2))
        );
    }

    #[test]
    fn lifetime_boundaries_clear_exactly_their_cells() {
        let mut state =
            SessionMatcherState::new(vec![StateLifetime::PerSession, StateLifetime::PerPrompt]);
        state.cells[0].get_or_insert_with(|| 1u32);
        state.cells[1].get_or_insert_with(|| 2u32);
        assert_eq!(state.occupied_cells(), 2);

        state.on_awaiting_approval();
        assert!(
            !state.cells[0].is_empty(),
            "per_session survives the prompt"
        );
        assert!(state.cells[1].is_empty(), "per_prompt resets on the prompt");

        state.cells[1].get_or_insert_with(|| 3u32);
        state.on_session_close();
        assert_eq!(state.occupied_cells(), 0, "close clears both lifetimes");
    }
}
