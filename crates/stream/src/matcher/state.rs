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

use std::collections::{BTreeSet, VecDeque};

use agent_bridge_adapter_api::{MatcherId, MatcherState, StateLifetime};

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

/// What a pending tail announced: the occurrence's text as last seen, and
/// the announcing record's rank. Growth re-evaluates against the rank —
/// only a strictly better detection may add to what was announced.
pub(crate) struct PendingAnnouncement {
    pub(crate) text: String,
    pub(crate) priority: u32,
    pub(crate) order: usize,
}

/// One session's matcher state: the stateful cells, their lifetimes, and
/// the sliding window of recent lines.
pub struct SessionMatcherState {
    /// Cell `i` belongs to the engine's `i`-th stateful registration.
    pub(crate) cells: Vec<MatcherState>,
    /// Parallel to `cells`: which boundary clears each.
    lifetimes: Vec<StateLifetime>,
    /// The completed lines before the current one, oldest first, at most
    /// [`TEXT_WINDOW_DEPTH`]. Maintained only when the engine has stateful
    /// matchers — nothing else reads it. A deque so sliding evicts without
    /// shifting, and the evicted entry's buffer is reused for the new line.
    pub(crate) recent: VecDeque<String>,
    /// The text pass's candidate flags, kept here so the per-line hot path
    /// reuses one buffer instead of allocating one per line. Scratch, not
    /// state: no one reads it between evaluations.
    pub(crate) candidate_scratch: Vec<bool>,
    /// Matchers the safety ceiling has disabled — for this session only.
    /// Insertion is the one-shot edge the `pattern_timeout` event fires
    /// on, so membership doubles as "already reported".
    pub(crate) disabled: BTreeSet<MatcherId>,
    /// The last content reported as unrecognized — the dedup that keeps
    /// "never silent" from becoming "always repeating" while a prompt
    /// sits unchanged across quiet periods or repaints. Scoped to that
    /// one occurrence: any *different* completed line retires it, so a
    /// later, distinct appearance of the same unknown prompt reports
    /// again.
    pub(crate) last_unrecognized: Option<String>,
    /// The last pending tail evaluated at an evaluation point, so an
    /// unchanged tail is not re-evaluated every quiet period. Retired the
    /// moment a line completes: the tail it deduplicated no longer exists,
    /// and an identical tail later is a new prompt.
    pub(crate) last_pending: Option<String>,
    /// The pending tail whose evaluation emitted an event, held until the
    /// completed line carrying that occurrence consumes it. A prompt
    /// detected from its unterminated tail *becomes* a completed line the
    /// moment the CLI finally writes the newline — possibly after repaints
    /// interleave other lines — and that line must not announce the same
    /// prompt again under a second id. The announcement carries its
    /// record's rank so a grown tail can reveal a strictly better
    /// detection without re-announcing the one already made.
    pub(crate) pending_emitted: Option<PendingAnnouncement>,
    /// The unknown pending tail already reported as unrecognized, held —
    /// like its recognized sibling above — until the line carrying that
    /// occurrence completes, so one unknown prompt is one report even
    /// when interleaved lines retire the consecutive-content dedup in
    /// between.
    pub(crate) pending_unrecognized: Option<String>,
    /// The identity of the compilation this state belongs to. Cells are
    /// positional, so the engine asserts this on every evaluation.
    engine_id: u64,
}

impl SessionMatcherState {
    pub(crate) fn new(lifetimes: Vec<StateLifetime>, engine_id: u64) -> Self {
        Self {
            cells: lifetimes.iter().map(|_| MatcherState::new()).collect(),
            lifetimes,
            recent: VecDeque::new(),
            candidate_scratch: Vec::new(),
            disabled: BTreeSet::new(),
            last_unrecognized: None,
            last_pending: None,
            pending_emitted: None,
            pending_unrecognized: None,
            engine_id,
        }
    }

    pub(crate) fn engine_id(&self) -> u64 {
        self.engine_id
    }

    /// Whether the safety ceiling has disabled a matcher for this session.
    pub fn is_disabled(&self, id: &MatcherId) -> bool {
        self.disabled.contains(id)
    }

    /// The session closed: every cell clears, both lifetimes. The value is
    /// normally dropped right after — this exists so the clearing is a
    /// testable statement rather than a hope about drop order.
    pub fn on_session_close(&mut self) {
        for cell in &mut self.cells {
            cell.clear();
        }
        self.recent.clear();
        self.disabled.clear();
        self.last_unrecognized = None;
        self.last_pending = None;
        self.pending_emitted = None;
        self.pending_unrecognized = None;
    }

    /// The session moved from running to awaiting an approval: `per_prompt`
    /// cells clear, `per_session` cells persist.
    ///
    /// Only the cells. The window of recent lines is shared input, not
    /// state, and it is not rewound at this boundary — a matcher that must
    /// not carry a detection across the prompt gates on its own (now
    /// cleared) cell, which is exactly what the cell is for. Slicing the
    /// window per registration would buy strictness per_session matchers
    /// would pay for in lost context.
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

    /// Slides the window forward past a completed line, reusing the
    /// evicted entry's buffer once the window is full — this runs per
    /// line, and neither a shift nor an allocation belongs on that path.
    pub(crate) fn push_line(&mut self, line: &str) {
        if self.recent.len() == TEXT_WINDOW_DEPTH
            && let Some(mut recycled) = self.recent.pop_front()
        {
            recycled.clear();
            recycled.push_str(line);
            self.recent.push_back(recycled);
        } else {
            self.recent.push_back(line.to_string());
        }
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
        SessionMatcherState::new(self.stateful_lifetimes(), self.engine_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_slides_and_stays_bounded() {
        let mut state = SessionMatcherState::new(Vec::new(), 0);
        for number in 0..(TEXT_WINDOW_DEPTH + 3) {
            state.push_line(&format!("line {number}"));
        }
        assert_eq!(state.recent.len(), TEXT_WINDOW_DEPTH);
        assert_eq!(state.recent.front().map(String::as_str), Some("line 3"));
        assert_eq!(
            state.recent.back().map(String::as_str),
            Some(&*format!("line {}", TEXT_WINDOW_DEPTH + 2))
        );
    }

    #[test]
    fn lifetime_boundaries_clear_exactly_their_cells() {
        let mut state =
            SessionMatcherState::new(vec![StateLifetime::PerSession, StateLifetime::PerPrompt], 0);
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
