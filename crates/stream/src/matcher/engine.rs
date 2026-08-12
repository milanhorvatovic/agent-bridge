//! The compiled matcher engine: one automaton pass decides which
//! expressions run, priority decides which match wins.
//!
//! Compilation happens once, at adapter registration, and the result is
//! shared by every session of that adapter. Every substring needle and every
//! regex with an extractable mandatory prefix enters one Aho-Corasick
//! automaton; on the hot path the automaton scans the line once and flags
//! candidates, and only a flagged record's expression actually runs. A regex
//! whose pattern offers no mandatory literal — an alternation from the first
//! character, say — runs on every completed line, which is the documented
//! cost of writing one.
//!
//! Resolution is deterministic by construction: records are evaluated in
//! ascending priority, ties broken by record order, and the first match
//! wins the line. Same pack, same line, same event — replayable output is a
//! property the conformance harness asserts, so it is not left to iteration
//! order anywhere in here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use agent_bridge_adapter_api::{
    Anchor, Captures, EmitSpec, MatcherId, NovelRow, PatternRecord, ScreenDiff, ScreenMatcher,
    StateLifetime, StatefulMatcher, TextMatcherType, TextWindow,
};
use agent_bridge_events::{
    AdapterErrorCode, AdapterErrorPayload, EventBody, EventKind, ScreenSnapshot,
    StreamUnrecognizedOutput,
};
use aho_corasick::AhoCorasick;
use regex::Regex;

use super::guard::{EvalGuard, pattern_timeout_event};
use super::state::SessionMatcherState;
use super::template::{groups_read, render_event, validate_emit_spec};

/// Why a pattern set was rejected at registration. One bad record rejects
/// the set — see [`CompileError::to_adapter_error`] for the event the
/// rejection becomes.
#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    /// The expression does not compile; the message is the regex engine's.
    #[error("record `{record}`: pattern does not compile: {message}")]
    BadRegex { record: String, message: String },
    /// An emit template reads a capture group the expression never defines.
    #[error(
        "record `{record}`: emit template reads `matches.{group}` but the pattern has no \
             group `{group}`"
    )]
    UnknownGroup { record: String, group: String },
    /// The emit spec fails the emit table — possible here because compiled
    /// sets can be built from code, not only from loaded packs.
    #[error("record `{record}`: {message}")]
    BadEmit { record: String, message: String },
    /// A matcher with nothing to look for. The loader rejects this too,
    /// but compiled sets can be built from code — and an empty needle
    /// matches everywhere, which is the opposite of a matcher.
    #[error("record `{record}`: `matcher.source` is empty")]
    EmptySource { record: String },
    /// A matcher with no name. The id is how every diagnostic — the
    /// timeout event, the disable set, this very error family — refers to
    /// a matcher; blank, none of them could say who they mean.
    #[error("a matcher id must be non-blank")]
    BlankId,
    /// Two matchers share an id.
    #[error("matcher id `{id}` is registered twice")]
    DuplicateId { id: String },
    /// The automaton itself would not build — a pathological needle set.
    #[error("prefilter automaton: {message}")]
    Automaton { message: String },
}

impl CompileError {
    /// The registration-rejection event: `adapter.error` with
    /// `pattern_compile_failed`, naming the record so the pack author reads
    /// the failure from the event stream alone.
    pub fn to_adapter_error(&self) -> AdapterErrorPayload {
        let mut detail = serde_json::Map::new();
        if let Self::BadRegex { record, .. }
        | Self::UnknownGroup { record, .. }
        | Self::BadEmit { record, .. }
        | Self::EmptySource { record } = self
        {
            detail.insert("record".to_string(), record.as_str().into());
        }
        if let Self::DuplicateId { id } = self {
            detail.insert("record".to_string(), id.as_str().into());
        }
        AdapterErrorPayload {
            code: AdapterErrorCode::PatternCompileFailed,
            message: self.to_string(),
            detail,
        }
    }
}

/// Counters the engine keeps about its own work. Snapshot, not a live view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineStats {
    /// How many times a regex actually ran — the number the prefilter
    /// exists to keep small, and the number its tests assert on.
    pub regex_evaluations: u64,
}

/// How one text record matches, after compilation.
enum TextKind {
    Substring { needle: String },
    Regex { regex: Regex, reads_groups: bool },
}

/// One compiled record, in evaluation order. Priority and registration
/// order stay attached because the winner of a line is resolved across
/// kinds — a text match still has to beat every stateful match.
struct TextRecord {
    id: MatcherId,
    priority: u32,
    order: usize,
    anchored: bool,
    kind: TextKind,
    emits: EmitSpec,
}

/// One registered stateful matcher and the event a win emits. The priority
/// is read once, here — a matcher that changes its answer between calls
/// would otherwise make resolution unrepeatable.
struct StatefulRegistration {
    matcher: Box<dyn StatefulMatcher>,
    id: MatcherId,
    priority: u32,
    order: usize,
    lifetime: StateLifetime,
    emits: EmitSpec,
}

/// One registered screen matcher. Kept sorted by (priority, order) — the
/// screen pass has its own cadence, so its resolution never crosses into
/// the per-line pass.
struct ScreenRegistration {
    matcher: Box<dyn ScreenMatcher>,
    id: MatcherId,
    emits: EmitSpec,
}

/// Collects what an adapter registers, then compiles it as a set.
///
/// Registration order matters — it is the priority tiebreak — and it runs
/// across kinds: a pack registered before a stateful matcher outranks it at
/// equal priority.
#[derive(Default)]
pub struct EngineBuilder {
    records: Vec<(PatternRecord, usize)>,
    stateful: Vec<(Box<dyn StatefulMatcher>, EmitSpec, usize)>,
    screen: Vec<(Box<dyn ScreenMatcher>, EmitSpec, usize)>,
    eval_timeout: Option<Duration>,
    next_order: usize,
}

impl EngineBuilder {
    /// Appends pack records, keeping their order.
    #[must_use]
    pub fn records(mut self, records: Vec<PatternRecord>) -> Self {
        for record in records {
            let order = self.next_order;
            self.next_order += 1;
            self.records.push((record, order));
        }
        self
    }

    /// Registers a stateful matcher and the event its wins emit. The
    /// matcher brings its own captures at match time, so the emit spec's
    /// group templates are checked against what it actually captured only
    /// in the sense every template is: an unfilled group renders empty.
    ///
    /// One registration, one spec, one event type — a lifecycle that emits
    /// different types from one detector needs the emit-set selection the
    /// outcome type reserves room for, landing with the lifecycle
    /// scenarios that first need it. Id continuity needs no such wait: a
    /// matcher generates its id, keeps it in its cell, and captures it on
    /// every event it emits.
    #[must_use]
    pub fn stateful(mut self, matcher: Box<dyn StatefulMatcher>, emits: EmitSpec) -> Self {
        let order = self.next_order;
        self.next_order += 1;
        self.stateful.push((matcher, emits, order));
        self
    }

    /// Registers a screen matcher — the code path for the screen kind,
    /// which has no data-record form yet. It participates only in the
    /// screen pass, for sessions that keep a reconstructed screen.
    #[must_use]
    pub fn screen(mut self, matcher: Box<dyn ScreenMatcher>, emits: EmitSpec) -> Self {
        let order = self.next_order;
        self.next_order += 1;
        self.screen.push((matcher, emits, order));
        self
    }

    /// Overrides the per-evaluation safety ceiling — the seam the
    /// runtime's configuration will drive once the binary grows one, and
    /// the test seam today: a zero ceiling makes every guarded evaluation
    /// trip, which exercises the disable path without needing a
    /// pathological matcher.
    #[must_use]
    pub fn eval_timeout(mut self, ceiling: Duration) -> Self {
        self.eval_timeout = Some(ceiling);
        self
    }

    /// Compiles the set: expressions, prefilter automaton, evaluation
    /// order. Failure rejects the whole registration.
    pub fn compile(self) -> Result<MatcherEngine, CompileError> {
        let mut ids = std::collections::BTreeSet::new();
        let mut text = Vec::with_capacity(self.records.len());
        for (record, order) in self.records {
            if record.name.trim().is_empty() {
                return Err(CompileError::BlankId);
            }
            validate_emit_spec(&record.emits).map_err(|message| CompileError::BadEmit {
                record: record.name.clone(),
                message,
            })?;
            if !ids.insert(record.name.clone()) {
                return Err(CompileError::DuplicateId { id: record.name });
            }
            if record.matcher.source.is_empty() {
                return Err(CompileError::EmptySource {
                    record: record.name,
                });
            }
            let anchored = record.matcher.anchor == Some(Anchor::LineStart);
            let kind = match record.matcher.kind {
                TextMatcherType::Substring => {
                    if let Some(group) = groups_read(&record.emits).next() {
                        return Err(CompileError::UnknownGroup {
                            record: record.name,
                            group: group.to_string(),
                        });
                    }
                    TextKind::Substring {
                        needle: record.matcher.source.clone(),
                    }
                }
                TextMatcherType::Regex => {
                    // The anchor is enforced by the expression itself: the
                    // wrapped pattern can only match at the line's first
                    // byte, so there is no separate position check to get
                    // out of sync with what the regex found.
                    let effective = if anchored {
                        format!("^(?:{})", record.matcher.source)
                    } else {
                        record.matcher.source.clone()
                    };
                    let regex = Regex::new(&effective).map_err(|error| CompileError::BadRegex {
                        record: record.name.clone(),
                        message: error.to_string(),
                    })?;
                    let defined: Vec<&str> = regex.capture_names().flatten().collect();
                    for group in groups_read(&record.emits) {
                        if !defined.contains(&group) {
                            return Err(CompileError::UnknownGroup {
                                record: record.name.clone(),
                                group: group.to_string(),
                            });
                        }
                    }
                    let reads_groups = groups_read(&record.emits).next().is_some();
                    TextKind::Regex {
                        regex,
                        reads_groups,
                    }
                }
            };
            text.push(TextRecord {
                id: MatcherId::new(record.name),
                priority: record.priority,
                order,
                anchored,
                kind,
                emits: record.emits,
            });
        }
        text.sort_by_key(|record| (record.priority, record.order));

        let mut stateful = Vec::with_capacity(self.stateful.len());
        for (matcher, emits, order) in self.stateful {
            let id = matcher.id().clone();
            if id.as_str().trim().is_empty() {
                return Err(CompileError::BlankId);
            }
            validate_emit_spec(&emits).map_err(|message| CompileError::BadEmit {
                record: id.as_str().to_string(),
                message,
            })?;
            if !ids.insert(id.as_str().to_string()) {
                return Err(CompileError::DuplicateId {
                    id: id.as_str().to_string(),
                });
            }
            stateful.push(StatefulRegistration {
                priority: matcher.priority(),
                lifetime: matcher.state_lifetime(),
                matcher,
                id,
                order,
                emits,
            });
        }
        // Sorted like the other kinds: the trait promises ascending
        // priority with registration order breaking ties, and evaluation
        // order is observable — two breaching matchers report their
        // timeouts in it. Sessions key their cells to this order, so the
        // sort must happen here, before any session exists.
        stateful.sort_by_key(|registration| (registration.priority, registration.order));

        let mut screen_sortable = Vec::with_capacity(self.screen.len());
        for (matcher, emits, order) in self.screen {
            let id = matcher.id().clone();
            if id.as_str().trim().is_empty() {
                return Err(CompileError::BlankId);
            }
            validate_emit_spec(&emits).map_err(|message| CompileError::BadEmit {
                record: id.as_str().to_string(),
                message,
            })?;
            if !ids.insert(id.as_str().to_string()) {
                return Err(CompileError::DuplicateId {
                    id: id.as_str().to_string(),
                });
            }
            let priority = matcher.priority();
            screen_sortable.push((priority, order, ScreenRegistration { matcher, id, emits }));
        }
        screen_sortable.sort_by_key(|(priority, order, _)| (*priority, *order));
        let screen: Vec<ScreenRegistration> = screen_sortable
            .into_iter()
            .map(|(_, _, registration)| registration)
            .collect();

        let mut needles: Vec<String> = Vec::new();
        let mut needle_owner = Vec::new();
        let mut every_line = Vec::new();
        for (index, record) in text.iter().enumerate() {
            match &record.kind {
                TextKind::Substring { needle } => {
                    needles.push(needle.clone());
                    needle_owner.push(index);
                }
                TextKind::Regex { regex, .. } => match literal_prefix(regex.as_str()) {
                    Some(prefix) => {
                        needles.push(prefix);
                        needle_owner.push(index);
                    }
                    None => every_line.push(index),
                },
            }
        }
        let automaton = if needles.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::new(&needles).map_err(|error| CompileError::Automaton {
                    message: error.to_string(),
                })?,
            )
        };

        Ok(MatcherEngine {
            text,
            automaton,
            needle_owner,
            every_line,
            stateful,
            screen,
            guard: EvalGuard::new(
                self.eval_timeout
                    .unwrap_or(super::guard::DEFAULT_EVAL_TIMEOUT),
            ),
            prompt_shape: Regex::new(
                r"[\[(]\s*[^\s\[\]()/]{1,10}(?:\s*/\s*[^\s\[\]()/]{1,10})+\s*[\])]\s*:?\s*$",
            )
            .expect("a fixed expression, exercised by every test that builds an engine"),
            regex_evaluations: AtomicU64::new(0),
            id: NEXT_ENGINE_ID.fetch_add(1, Ordering::Relaxed),
        })
    }
}

/// Every compilation gets a distinct identity, so a session state object
/// can prove it is being used with the engine that created it.
static NEXT_ENGINE_ID: AtomicU64 = AtomicU64::new(0);

/// The compiled engine for one adapter registration.
pub struct MatcherEngine {
    /// Every text record, already in evaluation order: ascending priority,
    /// ties by record order.
    text: Vec<TextRecord>,
    automaton: Option<AhoCorasick>,
    /// Automaton pattern index → index into `text`.
    needle_owner: Vec<usize>,
    /// Records that run on every completed line: regexes whose pattern has
    /// no mandatory literal for the automaton to key on.
    every_line: Vec<usize>,
    /// In evaluation order: ascending priority, ties by registration
    /// order. Cell `i` of a session's state belongs to entry `i` here.
    stateful: Vec<StatefulRegistration>,
    /// In evaluation order: ascending priority, ties by registration
    /// order. The screen pass runs at evaluation points, not per line.
    screen: Vec<ScreenRegistration>,
    /// The per-evaluation safety ceiling — the runtime budget, unrelated
    /// to the benchmark lane's per-chain budget.
    guard: EvalGuard,
    /// What an unmatched *completed* line must end with to be worth an
    /// unrecognized event: a bracketed choice of two or more options, the
    /// strongest single signal a line is a prompt. Deliberately narrow —
    /// ordinary output ends with almost anything, and "never silent" is
    /// about prompt-shaped text, not about narrating the whole stream.
    prompt_shape: Regex,
    regex_evaluations: AtomicU64,
    /// This compilation's identity. Session state keys its cells to this
    /// engine's registration order *positionally*, so a state object paired
    /// with a different engine would misroute state silently — the pairing
    /// is asserted, in release builds too, on every evaluation.
    id: u64,
}

/// The best match so far during one evaluation pass.
struct Winner<'engine> {
    priority: u32,
    order: usize,
    id: &'engine MatcherId,
    emits: &'engine EmitSpec,
    captures: Captures,
}

// Shape only: the compiled set's sources are pack text, not secrets, but a
// dump of every expression is noise no log line wants — the counts say what
// a diagnostic needs.
impl std::fmt::Debug for MatcherEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MatcherEngine({} text records, {} prefiltered needles, {} per-line)",
            self.text.len(),
            self.needle_owner.len(),
            self.every_line.len()
        )
    }
}

impl MatcherEngine {
    pub fn builder() -> EngineBuilder {
        EngineBuilder::default()
    }

    /// Evaluates one completed line: automaton pass, candidate expressions
    /// in priority order, first match wins and becomes its record's event.
    ///
    /// This whole chain — the automaton scan, every expression it
    /// triggers, and every stateful matcher — is the unit the benchmark
    /// lane holds to its budget.
    ///
    /// Text records are tried in their pre-sorted order, so the first that
    /// matches is the best text match and the rest are skipped. Stateful
    /// matchers all run regardless — their view of the stream must have no
    /// gaps — but a stateful match only becomes the line's event by
    /// out-ranking the text winner.
    ///
    /// Trailing whitespace is not content: the engine evaluates the
    /// end-trimmed line, on this path and the pending path alike. Terminal
    /// lines end in cursor padding as often as not, and an end-anchored
    /// pattern that missed its prompt over an invisible trailing space
    /// would be a trap laid for every pack author.
    pub fn evaluate_line(&self, session: &mut SessionMatcherState, line: &str) -> Vec<EventBody> {
        self.assert_paired(session);
        let line = line.trim_end();
        // A completed line retires the unchanged-tail dedup: whatever
        // pending text preceded this line either became it or was
        // overwritten, so an identical tail later is a new prompt.
        session.last_pending = None;
        // The unrecognized dedup is scoped to one occurrence, and an
        // occurrence is consecutive: repaints of an unknown prompt and
        // its own tail-to-line completion repeat the same content
        // back-to-back. The moment a different line flows past, the
        // stream has moved on — a later identical prompt is a new
        // occurrence, and never-silent means it reports again.
        if session.last_unrecognized.as_deref() != Some(line) {
            session.last_unrecognized = None;
        }
        // And when this line *is* the tail that already emitted — a prompt
        // detected while it waited, whose newline finally arrived,
        // possibly after a repaint interleaved other lines — announcing it
        // again under a second id would leave two pending approvals for
        // one human question. The marker survives lines that are not the
        // announced text and is consumed by the one that is. The residual
        // is the identity question text cannot answer: an announced tail
        // wiped by a repaint and the same text later completing as a line
        // is usually that prompt finally completing (suppress, as here)
        // but could be a distinct new one (a report this suppresses,
        // once) — and either reading wrongs the other case. The session
        // layer arbitrates real prompt identity; it owns the approval
        // lifecycle and the one-active-approval rule.
        // The same occurrence check as the pending path, deliberately: a
        // repaint can shrink the tail after its fuller stage announced, and
        // the line then completes at the shorter stage — exact equality
        // here would re-emit the very occurrence the marker followed
        // through its repaints.
        let emitted_from_tail = session
            .pending_emitted
            .as_deref()
            .is_some_and(|announced| same_occurrence(announced, line));
        if emitted_from_tail {
            session.pending_emitted = None;
        }
        // The unknown sibling of the same handoff: a tail already reported
        // as unrecognized, now completing (possibly grown) as this line.
        // Only the degradation is spent — if the completed line matches a
        // record after all, the match must still emit.
        let reported_from_tail = session
            .pending_unrecognized
            .as_deref()
            .is_some_and(|reported| same_occurrence(reported, line));
        if reported_from_tail {
            session.pending_unrecognized = None;
        }

        let mut guard_events: Vec<EventBody> = Vec::new();
        let mut winner = if emitted_from_tail {
            None
        } else {
            self.text_pass(session, line, &mut guard_events)
        };

        let SessionMatcherState {
            cells,
            recent,
            disabled,
            ..
        } = session;
        let recent: &[String] = recent.make_contiguous();
        for (registration, cell) in self.stateful.iter().zip(cells) {
            if disabled.contains(&registration.id) {
                continue;
            }
            let window = TextWindow::new(line, recent);
            let started = Instant::now();
            let outcome = registration.matcher.evaluate(&window, cell);
            let elapsed = started.elapsed();
            if self.guard.breached(elapsed) {
                if disabled.insert(registration.id.clone()) {
                    guard_events.push(pattern_timeout_event(&registration.id, elapsed));
                }
                continue;
            }
            let Some(outcome) = outcome else {
                continue;
            };
            // A suppressed line is suppressed for every kind, not only the
            // text pass — otherwise a normally-outranked stateful matcher
            // would win the line by default, and whether that event exists
            // would depend on where the quiet period happened to fall.
            // Evaluation still ran: state advanced, the view has no gap.
            //
            // The handoff does privilege the text detection over a
            // higher-priority stateful one, and that is the stated trade:
            // a tail is only text-evaluable — stateful matchers' contract
            // is completed lines, or they would see the same text twice —
            // so a prompt announcing while it waits can only ever announce
            // its best *text* match. Holding the announcement until the
            // newline would leave real prompts, which never end their
            // line, unannounced; emitting the stateful winner here as well
            // would double-announce the line. Priority orders candidates
            // within an evaluation; it cannot order candidates that do not
            // exist yet.
            if emitted_from_tail {
                continue;
            }
            let outranks = winner.as_ref().is_none_or(|current| {
                (registration.priority, registration.order) < (current.priority, current.order)
            });
            if outranks {
                winner = Some(Winner {
                    priority: registration.priority,
                    order: registration.order,
                    id: &registration.id,
                    emits: &registration.emits,
                    captures: outcome.captures,
                });
            }
        }
        if !self.stateful.is_empty() {
            session.push_line(line);
        }

        let mut events = Vec::new();
        match winner {
            Some(winner) => {
                // The id, never the line: which pattern fired is diagnostic
                // gold, but the line it fired on is session output.
                tracing::debug!(matcher = %winner.id, "pattern matched");
                events.push(render_event(winner.emits, &winner.captures));
            }
            // Never silent: a completed line that looks like a prompt and
            // matched nothing degrades to "here is the text" rather than
            // vanishing — the resilience event for the day a CLI update
            // outruns its pack. Unless the line already spoke as a tail —
            // recognized or unrecognized — because suppressed is not
            // unmatched, and one occurrence is one report.
            None => {
                if reported_from_tail {
                    // Remember the content so consecutive repaints of the
                    // completed line stay reported-once too.
                    session.last_unrecognized = Some(line.to_string());
                } else if !emitted_from_tail
                    && self.prompt_shape.is_match(line)
                    && let Some(event) = unrecognized(session, line)
                {
                    events.push(event);
                }
            }
        }
        events.extend(guard_events);
        events
    }

    /// Evaluates the unterminated tail at an evaluation point — the quiet-
    /// period boundary or feed quiescence, the same cadence as the screen
    /// pass.
    ///
    /// A real prompt usually never ends its line; it is waiting. So the
    /// pending text gets the full text-record chain — an anchored approval
    /// pattern must fire on a prompt that will never see its newline — but
    /// *not* the stateful matchers, whose contract is completed lines: a
    /// tail evaluated now and re-evaluated when the line completes would
    /// hand them the same text twice and corrupt whatever they are
    /// assembling. A tail that matches nothing is reported unrecognized if
    /// it looks like it is asking — waiting alone is not enough, or every
    /// mid-line pause in a token stream would raise an event.
    pub fn evaluate_pending(
        &self,
        session: &mut SessionMatcherState,
        pending: &str,
    ) -> Vec<EventBody> {
        self.assert_paired(session);
        // End-trimmed like the line path, and doubly so here: a waiting
        // prompt's tail ends at the cursor, which sits after padding as
        // often as not, and an end-anchored approval pattern must fire on
        // the prompt the author actually sees.
        let pending = pending.trim_end();
        if pending.is_empty() || session.last_pending.as_deref() == Some(pending) {
            return Vec::new();
        }
        // One announcement per occurrence, even as the occurrence changes
        // shape. A tail that extends the announced text — or is a prefix
        // of it, mid-repaint — is the same waiting line with different
        // paint, not a new prompt; the marker follows the fuller text and
        // nothing re-fires. A tail that is neither is an overwrite: the
        // announced text no longer exists and can never complete as a
        // line, so the marker retires with it — kept, it would suppress
        // an unrelated future line that happens to share the text.
        if let Some(announced) = session.pending_emitted.as_deref() {
            if same_occurrence(pending, announced) {
                if pending.len() > announced.len() {
                    session.pending_emitted = Some(pending.to_string());
                }
                session.last_pending = Some(pending.to_string());
                return Vec::new();
            }
            session.pending_emitted = None;
        }
        session.last_pending = Some(pending.to_string());
        // As on the line path: a different tail means the previous
        // unrecognized occurrence is over, and a later distinct
        // appearance of the same unknown prompt must report again.
        if session.last_unrecognized.as_deref() != Some(pending) {
            session.last_unrecognized = None;
        }

        let mut events = Vec::new();
        match self.text_pass(session, pending, &mut events) {
            Some(winner) => {
                tracing::debug!(matcher = %winner.id, "pattern matched on pending tail");
                events.insert(0, render_event(winner.emits, &winner.captures));
                // Remember what just spoke: when this tail becomes a
                // completed line, that line has already been announced. A
                // match also supersedes any unknown-tail report — the
                // partial that degraded grew into a pattern the pack knows.
                session.pending_emitted = Some(pending.to_string());
                session.pending_unrecognized = None;
            }
            None => {
                let unknown_occurrence = session
                    .pending_unrecognized
                    .as_deref()
                    .is_some_and(|reported| same_occurrence(reported, pending));
                if unknown_occurrence {
                    // The already-reported unknown prompt, still painting:
                    // follow the fuller text, report nothing new.
                    if session
                        .pending_unrecognized
                        .as_deref()
                        .is_some_and(|reported| pending.len() > reported.len())
                    {
                        session.pending_unrecognized = Some(pending.to_string());
                    }
                } else {
                    let asking = self.prompt_shape.is_match(pending)
                        || pending.ends_with(['?', ':', '>', '❯']);
                    if asking && let Some(event) = unrecognized(session, pending) {
                        events.push(event);
                        session.pending_unrecognized = Some(pending.to_string());
                    } else {
                        // A tail unrelated to the reported one overwrote
                        // it: that occurrence is over.
                        session.pending_unrecognized = None;
                    }
                }
            }
        }
        events
    }

    /// The text-record chain over one piece of text: automaton pass, then
    /// candidate expressions in priority order, first match wins. Breaches
    /// of the safety ceiling land in `guard_events`.
    fn text_pass<'engine>(
        &'engine self,
        session: &mut SessionMatcherState,
        line: &str,
        guard_events: &mut Vec<EventBody>,
    ) -> Option<Winner<'engine>> {
        // The candidate flags live in the session as reusable scratch:
        // this runs per completed line, and an allocator round-trip per
        // line is exactly the kind of jitter the chain budget polices.
        let SessionMatcherState {
            disabled,
            candidate_scratch: candidate,
            ..
        } = session;
        candidate.clear();
        candidate.resize(self.text.len(), false);
        if let Some(automaton) = &self.automaton {
            // Overlapping search so one needle being a substring of another
            // never hides a record.
            for hit in automaton.find_overlapping_iter(line) {
                candidate[self.needle_owner[hit.pattern().as_usize()]] = true;
            }
        }
        for &index in &self.every_line {
            candidate[index] = true;
        }

        for (index, record) in self.text.iter().enumerate() {
            if !candidate[index] {
                continue;
            }
            if disabled.contains(&record.id) {
                continue;
            }
            // Substring evaluation is untimed: the automaton already did
            // the work, and a `starts_with` cannot be the slow one. The
            // ceiling guards the expression engine and the code kinds.
            let started = matches!(record.kind, TextKind::Regex { .. }).then(Instant::now);
            let matched = self.eval_text(record, line);
            if let Some(started) = started {
                let elapsed = started.elapsed();
                if self.guard.breached(elapsed) {
                    if disabled.insert(record.id.clone()) {
                        guard_events.push(pattern_timeout_event(&record.id, elapsed));
                    }
                    // The result is discarded with the matcher: a
                    // detection that took this long is not one to act on.
                    continue;
                }
            }
            if let Some(captures) = matched {
                return Some(Winner {
                    priority: record.priority,
                    order: record.order,
                    id: &record.id,
                    emits: &record.emits,
                    captures,
                });
            }
        }
        None
    }

    /// The lifetimes of the stateful registrations, in cell order — what a
    /// session's state object is built from.
    pub(crate) fn stateful_lifetimes(&self) -> Vec<StateLifetime> {
        self.stateful
            .iter()
            .map(|registration| registration.lifetime)
            .collect()
    }

    /// This compilation's identity, stamped into every session it creates.
    pub(crate) fn engine_id(&self) -> u64 {
        self.id
    }

    /// The pairing check, in release builds too: cells are positional, so
    /// a state object used with an engine that did not create it would
    /// hand the wrong state to the wrong matcher silently — a misuse that
    /// must fail loud, because reload-safety is a claim about exactly
    /// this. An adapter reload compiles a new engine and creates new
    /// sessions; migrating live state across compilations is that future
    /// wiring's problem, and until it exists no caller may improvise it.
    fn assert_paired(&self, session: &SessionMatcherState) {
        assert_eq!(
            session.engine_id(),
            self.id,
            "a session state object is only valid with the engine that created it"
        );
    }

    /// Whether any screen matcher is registered — the check that lets a
    /// session skip materializing a snapshot nobody would read.
    pub fn has_screen_matchers(&self) -> bool {
        !self.screen.is_empty()
    }

    /// The screen pass, run at evaluation points — never per byte, never
    /// per line. The caller brings the rendered snapshot and what changed
    /// since the last point; screen matchers see both, in priority order,
    /// first match wins the point. The safety ceiling applies here exactly
    /// as on the line path — screen matchers are code, which is what the
    /// ceiling exists for.
    pub fn evaluate_screen(
        &self,
        session: &mut SessionMatcherState,
        snapshot: &ScreenSnapshot,
        evaluation: &crate::screen::Evaluation,
    ) -> Vec<EventBody> {
        self.assert_paired(session);
        if self.screen.is_empty() {
            return Vec::new();
        }
        let novel: Vec<NovelRow<'_>> = evaluation
            .novel
            .iter()
            .map(|span| NovelRow {
                row: span.row,
                text: &span.text,
            })
            .collect();
        let diff = ScreenDiff {
            damaged: &evaluation.damaged,
            novel: &novel,
        };
        let mut events = Vec::new();
        for registration in &self.screen {
            if session.disabled.contains(&registration.id) {
                continue;
            }
            let started = Instant::now();
            let outcome = registration.matcher.evaluate(snapshot, &diff);
            let elapsed = started.elapsed();
            if self.guard.breached(elapsed) {
                if session.disabled.insert(registration.id.clone()) {
                    events.push(pattern_timeout_event(&registration.id, elapsed));
                }
                continue;
            }
            if let Some(outcome) = outcome {
                tracing::debug!(matcher = %registration.id, "screen pattern matched");
                events.insert(0, render_event(&registration.emits, &outcome.captures));
                break;
            }
        }
        events
    }

    /// Point-in-time counters, for the prefilter's tests and diagnostics.
    pub fn stats(&self) -> EngineStats {
        EngineStats {
            regex_evaluations: self.regex_evaluations.load(Ordering::Relaxed),
        }
    }

    fn eval_text(&self, record: &TextRecord, line: &str) -> Option<Captures> {
        match &record.kind {
            TextKind::Substring { needle } => {
                debug_assert!(
                    line.contains(needle.as_str()),
                    "candidate without its needle"
                );
                if record.anchored && !line.starts_with(needle.as_str()) {
                    return None;
                }
                Some(Captures::new())
            }
            TextKind::Regex {
                regex,
                reads_groups,
            } => {
                self.regex_evaluations.fetch_add(1, Ordering::Relaxed);
                if *reads_groups {
                    let found = regex.captures(line)?;
                    let mut captures = Captures::new();
                    for name in regex.capture_names().flatten() {
                        if let Some(value) = found.name(name) {
                            captures.insert(name, value.as_str());
                        }
                    }
                    Some(captures)
                } else {
                    regex.is_match(line).then(Captures::new)
                }
            }
        }
    }
}

/// Whether two sightings of a waiting line are the same occurrence: one
/// extends the other. A tail grows as it paints, and a mid-repaint tail is
/// a prefix of what it will become — neither is a new prompt.
fn same_occurrence(a: &str, b: &str) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// The unrecognized-output degradation, deduplicated per session: the same
/// content is reported once and then holds its peace until it changes,
/// because a prompt sitting unchanged across quiet periods is one prompt,
/// not a stream of them.
fn unrecognized(session: &mut SessionMatcherState, content: &str) -> Option<EventBody> {
    if session.last_unrecognized.as_deref() == Some(content) {
        return None;
    }
    session.last_unrecognized = Some(content.to_string());
    Some(EventBody::new(EventKind::StreamUnrecognizedOutput(
        StreamUnrecognizedOutput {
            content: content.to_string(),
        },
    )))
}

/// The literal a match cannot avoid, read off the pattern's parse tree.
///
/// Walks the parsed expression from the left: zero-width assertions are
/// skipped, groups are entered, literals accumulate, and the walk stops at
/// the first construct that makes continuation optional — a class, an
/// alternation, a zero-minimum repetition. An alternation reachable from
/// the first character means no literal is mandatory at all, and the record
/// runs on every line instead; that is a property of the pattern the author
/// wrote, not a tuning knob.
fn literal_prefix(source: &str) -> Option<String> {
    let parsed = regex_syntax::Parser::new().parse(source).ok()?;
    let mut prefix = String::new();
    collect_prefix(&parsed, &mut prefix);
    (!prefix.is_empty()).then_some(prefix)
}

/// Extends `out` with `hir`'s mandatory leading literal. Returns whether
/// the walk may continue past `hir` — true only when the whole node was
/// consumed as literal (or was zero-width).
fn collect_prefix(hir: &regex_syntax::hir::Hir, out: &mut String) -> bool {
    use regex_syntax::hir::HirKind;
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => true,
        HirKind::Literal(literal) => match std::str::from_utf8(&literal.0) {
            Ok(text) => {
                out.push_str(text);
                true
            }
            Err(_) => false,
        },
        HirKind::Concat(parts) => parts.iter().all(|part| collect_prefix(part, out)),
        HirKind::Capture(capture) => collect_prefix(&capture.sub, out),
        // One iteration is mandatory, so its literal is too — but nothing
        // after the repetition can be counted on to sit at a fixed offset,
        // so the walk ends here.
        HirKind::Repetition(repetition) if repetition.min >= 1 => {
            collect_prefix(&repetition.sub, out);
            false
        }
        HirKind::Class(_) | HirKind::Alternation(_) | HirKind::Repetition(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::parse_pack;
    use agent_bridge_events::EventKind;

    fn engine(yaml: &str) -> MatcherEngine {
        MatcherEngine::builder()
            .records(parse_pack("test-pack", yaml).expect("test pack parses"))
            .compile()
            .expect("test pack compiles")
    }

    /// One line through a throwaway session — for the tests whose matchers
    /// keep no state.
    fn eval(engine: &MatcherEngine, line: &str) -> Vec<EventBody> {
        engine.evaluate_line(&mut engine.new_session(), line)
    }

    fn tool_of(events: &[EventBody]) -> &str {
        let EventKind::ToolCallStarted(payload) = &events[0].kind else {
            panic!("expected tool.call_started, got {:?}", events[0].kind);
        };
        payload.tool.as_str()
    }

    const DETECTS: &str = r#"
- name: ready_marker
  matcher: { type: substring, source: 'session ready' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: ready }
- name: approval
  matcher:
    type: regex
    source: '^(?P<prompt>Allow .+\?) \[y/N\]$'
    anchor: line_start
  emits:
    event_type: prompt.approval_required
    fields:
      approval_id: '{{ uuid4() }}'
      prompt: '{{ matches.prompt }}'
      options: ['y', 'n']
"#;

    #[test]
    fn literal_and_regex_records_detect_their_fixture_lines() {
        let engine = engine(DETECTS);

        let literal = eval(&engine, "fake-cli: session ready");
        assert_eq!(literal.len(), 1);
        assert_eq!(tool_of(&literal), "ready");

        let regex = eval(&engine, "Allow filesystem write? [y/N]");
        assert_eq!(regex.len(), 1);
        let EventKind::PromptApprovalRequired(payload) = &regex[0].kind else {
            panic!("expected an approval, got {:?}", regex[0].kind);
        };
        assert_eq!(payload.prompt, "Allow filesystem write?");
        assert!(regex[0].approval_id.is_some());

        assert!(eval(&engine, "nothing to see").is_empty());
    }

    #[test]
    fn line_start_anchor_rejects_midline_spoof() {
        let engine = engine(DETECTS);
        // The spoof: approval-shaped text planted inside a token stream.
        // No approval fires — the anchor holds — and what fires instead is
        // the demotion: prompt-shaped text the matchers declined, reported
        // as unrecognized output rather than trusted or swallowed.
        let spoofed = eval(&engine, "token output Allow filesystem write? [y/N]");
        assert!(
            !spoofed
                .iter()
                .any(|event| matches!(event.kind, EventKind::PromptApprovalRequired(_))),
            "mid-line prompt text must never become an approval"
        );
        assert!(matches!(
            &spoofed[0].kind,
            EventKind::StreamUnrecognizedOutput(payload)
                if payload.content.contains("Allow filesystem write?")
        ));

        // The same defense holds for an anchored substring.
        let anchored_literal = super::super::parse_pack(
            "inline",
            r#"
- name: banner
  matcher: { type: substring, source: 'fake-cli:', anchor: line_start }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: banner }
"#,
        )
        .expect("parses");
        let engine = MatcherEngine::builder()
            .records(anchored_literal)
            .compile()
            .expect("compiles");
        assert_eq!(eval(&engine, "fake-cli: hello").len(), 1);
        assert!(eval(&engine, "mid fake-cli: hello").is_empty());
    }

    #[test]
    fn priority_resolution_is_deterministic_across_runs() {
        // Both records match the line. `urgent` has the larger record
        // index but the smaller priority number, so it wins; the two
        // `hundred`s tie on priority and resolve by record order.
        let contested = r#"
- name: hundred_first
  matcher: { type: substring, source: 'contested' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: first }
- name: hundred_second
  matcher: { type: substring, source: 'contested' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: second }
- name: urgent
  matcher: { type: substring, source: 'contested', anchor: line_start }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: urgent }
  priority: 10
"#;
        let engine = engine(contested);
        for _ in 0..100 {
            assert_eq!(tool_of(&eval(&engine, "contested line")), "urgent");
            // Mid-line, the anchored priority-10 record declines and the
            // tie falls to record order.
            assert_eq!(tool_of(&eval(&engine, "a contested line")), "first");
        }
    }

    #[test]
    fn ac_prefilter_runs_zero_regexes_on_a_nonmatching_line() {
        // Every record here has an extractable prefix, so a line the
        // automaton does not flag must trigger no expression at all.
        let prefixed = r#"
- name: approval
  matcher: { type: regex, source: '^Allow (?P<what>.+)\? \[y/N\]$', anchor: line_start }
  emits:
    event_type: prompt.approval_required
    fields: { approval_id: '{{ uuid4() }}', prompt: '{{ matches.what }}' }
- name: tool_marker
  matcher: { type: regex, source: '\{\{tool: (?P<tool>[a-z]+)\}\}' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: '{{ matches.tool }}' }
- name: done_marker
  matcher: { type: regex, source: '\{\{tool_done: (?P<code>[0-9]+)\}\}' }
  emits:
    event_type: tool.call_completed
    fields: { call_id: '{{ uuid4() }}', exit_code: '{{ matches.code }}' }
"#;
        let engine = engine(prefixed);
        for line in [
            "ordinary token output, quite unlike a prompt",
            "more of the same",
            "Allowance is not Allow-space",
        ] {
            assert!(eval(&engine, line).is_empty());
        }
        assert_eq!(engine.stats().regex_evaluations, 0);

        // A candidate line runs the one expression its needle owns —
        // including the near-miss where the needle hits but the regex
        // declines, which is exactly the two-stage split.
        assert_eq!(eval(&engine, "{{tool: bash}}").len(), 1);
        assert_eq!(engine.stats().regex_evaluations, 1);
        assert!(eval(&engine, "{{tool: BASH}}").is_empty());
        assert_eq!(engine.stats().regex_evaluations, 2);
    }

    #[test]
    fn a_prefixless_pattern_runs_on_every_line_by_design() {
        let alternation = r#"
- name: either
  matcher: { type: regex, source: '(^ready$|^done$)' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: either }
"#;
        let engine = engine(alternation);
        assert!(eval(&engine, "unrelated").is_empty());
        assert!(eval(&engine, "also unrelated").is_empty());
        assert_eq!(
            engine.stats().regex_evaluations,
            2,
            "no extractable prefix means the expression runs per line"
        );
        assert_eq!(eval(&engine, "done").len(), 1);
    }

    #[test]
    fn bad_regex_rejects_registration_naming_the_record() {
        let broken = parse_pack(
            "inline",
            r#"
- name: unclosed
  matcher: { type: regex, source: '(oops' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: x }
"#,
        )
        .expect("shape parses; compilation is the engine's job");
        let error = MatcherEngine::builder()
            .records(broken)
            .compile()
            .expect_err("an unclosed group cannot compile");
        assert!(error.to_string().contains("unclosed"), "got: {error}");

        let payload = error.to_adapter_error();
        assert_eq!(payload.code, AdapterErrorCode::PatternCompileFailed);
        assert!(payload.message.contains("unclosed"));
        assert_eq!(
            payload
                .detail
                .get("record")
                .and_then(|value| value.as_str()),
            Some("unclosed")
        );
    }

    #[test]
    fn an_emit_reading_an_undefined_group_rejects_registration() {
        let wrong_group = parse_pack(
            "inline",
            r#"
- name: mismatched
  matcher: { type: regex, source: '^ok (?P<yes>.+)$' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: '{{ matches.no }}' }
"#,
        )
        .expect("shape parses");
        let error = MatcherEngine::builder()
            .records(wrong_group)
            .compile()
            .expect_err("the group does not exist");
        assert!(
            matches!(error, CompileError::UnknownGroup { .. }),
            "got: {error}"
        );
        assert!(error.to_string().contains("matches.no"), "got: {error}");
    }

    #[test]
    fn duplicate_ids_across_sources_reject_registration() {
        let mut records = parse_pack("a", DETECTS).expect("parses");
        records.extend(parse_pack("b", DETECTS).expect("parses"));
        let error = MatcherEngine::builder()
            .records(records)
            .compile()
            .expect_err("two packs, same names");
        assert!(matches!(error, CompileError::DuplicateId { .. }));
    }

    #[test]
    fn literal_prefixes_come_off_the_parse_tree() {
        // The canonical approval shape: the literal lives inside a named
        // group behind an anchor, and the alternation after it does not
        // cancel it.
        assert_eq!(
            literal_prefix(r"^(?P<prompt>Do you want to (?P<verb>run|allow) .+?)\?"),
            Some("Do you want to ".to_string())
        );
        assert_eq!(
            literal_prefix(r"\{\{tool: (?P<t>[a-z]+)\}\}"),
            Some("{{tool: ".to_string())
        );
        // A quantifier keeps its mandatory first iteration.
        assert_eq!(literal_prefix(r"ab+c"), Some("ab".to_string()));
        // Top-level alternation: nothing is mandatory.
        assert_eq!(literal_prefix(r"ready|done"), None);
        assert_eq!(literal_prefix(r"(ready|done) now"), None);
        // Optional head: nothing is mandatory.
        assert_eq!(literal_prefix(r"x?y"), None);
        // A class from the first character.
        assert_eq!(literal_prefix(r"[a-z]+ ready"), None);
    }

    // -- the stateful kind ---------------------------------------------------

    use agent_bridge_adapter_api::{MatchOutcome, MatcherState, Template, TemplateValue};
    use std::collections::BTreeMap;

    fn emits(event_type: &str, fields: &[(&str, Template)]) -> EmitSpec {
        EmitSpec {
            event_type: event_type.to_string(),
            fields: fields
                .iter()
                .map(|(name, template)| (name.to_string(), TemplateValue::One(template.clone())))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    /// A two-line frame: remembers `BEGIN <name>`, fires on the `END` line
    /// with the remembered name as a capture.
    struct FrameMatcher {
        id: MatcherId,
        lifetime: StateLifetime,
        priority: u32,
    }

    impl FrameMatcher {
        fn boxed(id: &str, lifetime: StateLifetime, priority: u32) -> Box<Self> {
            Box::new(Self {
                id: MatcherId::new(id),
                lifetime,
                priority,
            })
        }

        fn result_emits() -> EmitSpec {
            emits(
                "tool.result",
                &[
                    ("call_id", Template::Uuid4),
                    ("content", Template::Group("frame".to_string())),
                ],
            )
        }
    }

    impl StatefulMatcher for FrameMatcher {
        fn id(&self) -> &MatcherId {
            &self.id
        }

        fn priority(&self) -> u32 {
            self.priority
        }

        fn state_lifetime(&self) -> StateLifetime {
            self.lifetime
        }

        fn evaluate(
            &self,
            window: &TextWindow<'_>,
            state: &mut MatcherState,
        ) -> Option<MatchOutcome> {
            let pending = state.get_or_insert_with(String::new);
            if let Some(name) = window.line().strip_prefix("BEGIN ") {
                *pending = name.to_string();
                return None;
            }
            if window.line() == "END" && !pending.is_empty() {
                let captures = Captures::new().with("frame", pending.clone());
                pending.clear();
                return Some(MatchOutcome::with_captures(captures));
            }
            None
        }
    }

    fn frame_content(events: &[EventBody]) -> &str {
        let EventKind::ToolResult(payload) = &events[0].kind else {
            panic!("expected tool.result, got {:?}", events[0].kind);
        };
        payload.content.as_str()
    }

    #[test]
    fn a_stateful_matcher_detects_across_lines() {
        let engine = MatcherEngine::builder()
            .stateful(
                FrameMatcher::boxed("frame", StateLifetime::PerSession, 100),
                FrameMatcher::result_emits(),
            )
            .compile()
            .expect("compiles");
        let mut session = engine.new_session();

        assert!(engine.evaluate_line(&mut session, "BEGIN alpha").is_empty());
        assert!(
            engine
                .evaluate_line(&mut session, "noise between")
                .is_empty()
        );
        let fired = engine.evaluate_line(&mut session, "END");
        assert_eq!(fired.len(), 1);
        assert_eq!(frame_content(&fired), "alpha");
        // The frame was consumed: a second END has nothing to close.
        assert!(engine.evaluate_line(&mut session, "END").is_empty());
    }

    #[test]
    fn per_session_state_clears_on_close_only() {
        let engine = MatcherEngine::builder()
            .stateful(
                FrameMatcher::boxed("frame", StateLifetime::PerSession, 100),
                FrameMatcher::result_emits(),
            )
            .compile()
            .expect("compiles");
        let mut session = engine.new_session();

        engine.evaluate_line(&mut session, "BEGIN alpha");
        // The awaiting-approval boundary is not this lifetime's boundary.
        session.on_awaiting_approval();
        assert_eq!(
            frame_content(&engine.evaluate_line(&mut session, "END")),
            "alpha"
        );

        engine.evaluate_line(&mut session, "BEGIN beta");
        assert_eq!(session.occupied_cells(), 1);
        session.on_session_close();
        assert_eq!(session.occupied_cells(), 0, "close empties the state map");
        assert!(engine.evaluate_line(&mut session, "END").is_empty());
    }

    #[test]
    fn per_prompt_state_clears_on_awaiting_approval() {
        // The per-prompt matcher carries the smaller priority number, so if
        // it still had its frame after the transition it would win the END
        // line — the per-session event is proof the clearing happened.
        let engine = MatcherEngine::builder()
            .stateful(
                FrameMatcher::boxed("per_prompt_frame", StateLifetime::PerPrompt, 10),
                emits(
                    "tool.result",
                    &[
                        ("call_id", Template::Uuid4),
                        ("content", Template::Literal("from per_prompt".to_string())),
                    ],
                ),
            )
            .stateful(
                FrameMatcher::boxed("per_session_frame", StateLifetime::PerSession, 100),
                FrameMatcher::result_emits(),
            )
            .compile()
            .expect("compiles");
        let mut session = engine.new_session();

        engine.evaluate_line(&mut session, "BEGIN alpha");
        let contested = engine.evaluate_line(&mut session, "END");
        assert_eq!(frame_content(&contested), "from per_prompt");

        engine.evaluate_line(&mut session, "BEGIN beta");
        session.on_awaiting_approval();
        let after = engine.evaluate_line(&mut session, "END");
        assert_eq!(
            frame_content(&after),
            "beta",
            "the per_prompt frame is gone; only the per_session one closes"
        );
    }

    /// Always fires on its needle — the cross-kind priority probe.
    struct NeedleMatcher {
        id: MatcherId,
        priority: u32,
    }

    impl StatefulMatcher for NeedleMatcher {
        fn id(&self) -> &MatcherId {
            &self.id
        }

        fn priority(&self) -> u32 {
            self.priority
        }

        fn state_lifetime(&self) -> StateLifetime {
            StateLifetime::PerSession
        }

        fn evaluate(
            &self,
            window: &TextWindow<'_>,
            _state: &mut MatcherState,
        ) -> Option<MatchOutcome> {
            window.line().contains("contested").then(MatchOutcome::new)
        }
    }

    #[test]
    fn priority_resolves_across_kinds() {
        let text_record = r#"
- name: text_side
  matcher: { type: substring, source: 'contested' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: text }
  priority: 50
"#;
        let stateful_emits = emits(
            "tool.call_started",
            &[
                ("call_id", Template::Uuid4),
                ("tool", Template::Literal("stateful".to_string())),
            ],
        );

        // The stateful matcher outranks the record...
        let engine_stateful_wins = MatcherEngine::builder()
            .records(parse_pack("inline", text_record).expect("parses"))
            .stateful(
                Box::new(NeedleMatcher {
                    id: MatcherId::new("stateful_side"),
                    priority: 5,
                }),
                stateful_emits.clone(),
            )
            .compile()
            .expect("compiles");
        let mut session = engine_stateful_wins.new_session();
        assert_eq!(
            tool_of(&engine_stateful_wins.evaluate_line(&mut session, "contested")),
            "stateful"
        );

        // ...and loses at the default priority, 100 against the record's 50.
        let engine_text_wins = MatcherEngine::builder()
            .records(parse_pack("inline", text_record).expect("parses"))
            .stateful(
                Box::new(NeedleMatcher {
                    id: MatcherId::new("stateful_side"),
                    priority: 100,
                }),
                stateful_emits,
            )
            .compile()
            .expect("compiles");
        let mut session = engine_text_wins.new_session();
        assert_eq!(
            tool_of(&engine_text_wins.evaluate_line(&mut session, "contested")),
            "text"
        );
    }

    /// Records when it was evaluated — the probe for evaluation order
    /// itself, which is observable (breach reports arrive in it) and so
    /// must follow the trait's promise, not registration order.
    struct OrderProbe {
        id: MatcherId,
        priority: u32,
        log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl StatefulMatcher for OrderProbe {
        fn id(&self) -> &MatcherId {
            &self.id
        }

        fn priority(&self) -> u32 {
            self.priority
        }

        fn state_lifetime(&self) -> StateLifetime {
            StateLifetime::PerSession
        }

        fn evaluate(
            &self,
            _window: &TextWindow<'_>,
            _state: &mut MatcherState,
        ) -> Option<MatchOutcome> {
            self.log
                .lock()
                .expect("no panics hold this lock")
                .push(self.id.as_str().to_string());
            None
        }
    }

    #[test]
    fn stateful_evaluation_runs_in_priority_order() {
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let noop_emits = || {
            emits(
                "tool.result",
                &[
                    ("call_id", Template::Uuid4),
                    ("content", Template::Literal("unused".to_string())),
                ],
            )
        };
        let engine = MatcherEngine::builder()
            .stateful(
                Box::new(OrderProbe {
                    id: MatcherId::new("registered_first"),
                    priority: 100,
                    log: std::sync::Arc::clone(&log),
                }),
                noop_emits(),
            )
            .stateful(
                Box::new(OrderProbe {
                    id: MatcherId::new("late_but_urgent"),
                    priority: 10,
                    log: std::sync::Arc::clone(&log),
                }),
                noop_emits(),
            )
            .compile()
            .expect("compiles");
        engine.evaluate_line(&mut engine.new_session(), "any line at all");
        assert_eq!(
            *log.lock().expect("no panics hold this lock"),
            vec![
                "late_but_urgent".to_string(),
                "registered_first".to_string()
            ],
            "ascending priority governs evaluation itself, not only who wins"
        );
    }

    /// Reads the window rather than its own state — what `recent` is for.
    struct WindowProbe {
        id: MatcherId,
    }

    impl StatefulMatcher for WindowProbe {
        fn id(&self) -> &MatcherId {
            &self.id
        }

        fn state_lifetime(&self) -> StateLifetime {
            StateLifetime::PerSession
        }

        fn evaluate(
            &self,
            window: &TextWindow<'_>,
            _state: &mut MatcherState,
        ) -> Option<MatchOutcome> {
            (window.line() == "three"
                && window.recent().len() == 2
                && window.recent()[0] == "one"
                && window.recent()[1] == "two")
                .then(MatchOutcome::new)
        }
    }

    #[test]
    fn the_text_window_carries_recent_lines_oldest_first() {
        let engine = MatcherEngine::builder()
            .stateful(
                Box::new(WindowProbe {
                    id: MatcherId::new("window_probe"),
                }),
                emits(
                    "tool.call_started",
                    &[
                        ("call_id", Template::Uuid4),
                        ("tool", Template::Literal("window".to_string())),
                    ],
                ),
            )
            .compile()
            .expect("compiles");
        let mut session = engine.new_session();
        assert!(engine.evaluate_line(&mut session, "one").is_empty());
        assert!(engine.evaluate_line(&mut session, "two").is_empty());
        assert_eq!(engine.evaluate_line(&mut session, "three").len(), 1);
    }

    #[test]
    fn duplicate_ids_between_records_and_stateful_reject() {
        let error = MatcherEngine::builder()
            .records(parse_pack("inline", DETECTS).expect("parses"))
            .stateful(
                FrameMatcher::boxed("approval", StateLifetime::PerSession, 100),
                FrameMatcher::result_emits(),
            )
            .compile()
            .expect_err("`approval` is already a record name");
        assert!(matches!(error, CompileError::DuplicateId { .. }));
    }

    // -- the safety ceiling --------------------------------------------------

    /// Sleeps past any test ceiling, and would match every line if the
    /// guard let its result stand.
    struct SleepyMatcher {
        id: MatcherId,
        sleep: Duration,
    }

    impl StatefulMatcher for SleepyMatcher {
        fn id(&self) -> &MatcherId {
            &self.id
        }

        fn state_lifetime(&self) -> StateLifetime {
            StateLifetime::PerSession
        }

        fn evaluate(
            &self,
            _window: &TextWindow<'_>,
            _state: &mut MatcherState,
        ) -> Option<MatchOutcome> {
            std::thread::sleep(self.sleep);
            Some(MatchOutcome::new())
        }
    }

    fn sleepy_engine(ceiling: Duration, sleep: Duration) -> MatcherEngine {
        MatcherEngine::builder()
            .stateful(
                Box::new(SleepyMatcher {
                    id: MatcherId::new("sleepy"),
                    sleep,
                }),
                emits(
                    "tool.call_started",
                    &[
                        ("call_id", Template::Uuid4),
                        ("tool", Template::Literal("sleepy".to_string())),
                    ],
                ),
            )
            .eval_timeout(ceiling)
            .compile()
            .expect("compiles")
    }

    fn timeout_count(events: &[EventBody]) -> usize {
        events
            .iter()
            .filter(|event| {
                matches!(
                    &event.kind,
                    EventKind::AdapterError(payload)
                        if payload.code == AdapterErrorCode::PatternTimeout
                )
            })
            .count()
    }

    /// The real-sleep variant: a matcher that blocks well past the ceiling
    /// is disabled for its session, its would-be match discarded — and
    /// another session keeps the matcher, because its evaluations were
    /// never the slow ones. Sleep and ceiling are far apart so a loaded CI
    /// host cannot blur the comparison.
    #[test]
    fn slow_matcher_disabled_per_session_only() {
        let engine = sleepy_engine(Duration::from_millis(5), Duration::from_millis(40));
        let mut first = engine.new_session();
        let mut second = engine.new_session();

        let breach = engine.evaluate_line(&mut first, "any line");
        assert_eq!(timeout_count(&breach), 1);
        assert_eq!(
            breach.len(),
            1,
            "the sleeper's own match was discarded, not emitted"
        );
        assert!(first.is_disabled(&MatcherId::new("sleepy")));

        assert!(
            engine.evaluate_line(&mut first, "next line").is_empty(),
            "disabled means skipped: no match, no repeat event"
        );

        let other = engine.evaluate_line(&mut second, "any line");
        assert_eq!(
            timeout_count(&other),
            1,
            "the second session still ran the matcher — the disable is per session"
        );
    }

    #[test]
    fn pattern_timeout_fires_once_not_per_line() {
        let engine = sleepy_engine(Duration::from_millis(5), Duration::from_millis(40));
        let mut session = engine.new_session();
        let mut total = 0;
        for line in ["one", "two", "three"] {
            total += timeout_count(&engine.evaluate_line(&mut session, line));
        }
        assert_eq!(total, 1, "insertion into the disabled set is the only edge");
    }

    /// The zero-ceiling seam: every guarded evaluation trips, which
    /// exercises the regex arm of the guard without a pathological
    /// pattern — and shows the substring arm is deliberately outside it,
    /// because the automaton pass cannot be the slow one.
    #[test]
    fn zero_ceiling_trips_regexes_but_never_substrings() {
        let engine = MatcherEngine::builder()
            .records(parse_pack("inline", DETECTS).expect("parses"))
            .eval_timeout(Duration::ZERO)
            .compile()
            .expect("compiles");
        let mut session = engine.new_session();

        let regex_line = engine.evaluate_line(&mut session, "Allow filesystem write? [y/N]");
        assert_eq!(timeout_count(&regex_line), 1);
        assert!(
            !regex_line
                .iter()
                .any(|event| matches!(event.kind, EventKind::PromptApprovalRequired(_))),
            "the regex match was discarded with the matcher"
        );
        assert!(
            regex_line
                .iter()
                .any(|event| matches!(event.kind, EventKind::StreamUnrecognizedOutput(_))),
            "a prompt-shaped line whose matcher was just disabled degrades, never silences"
        );
        assert!(session.is_disabled(&MatcherId::new("approval")));

        let substring_line = engine.evaluate_line(&mut session, "fake-cli: session ready");
        assert_eq!(timeout_count(&substring_line), 0);
        assert_eq!(
            tool_of(&substring_line),
            "ready",
            "substring evaluation is untimed and unaffected"
        );
    }

    // -- never silent --------------------------------------------------------

    fn unrecognized_content(events: &[EventBody]) -> Option<&str> {
        events.iter().find_map(|event| match &event.kind {
            EventKind::StreamUnrecognizedOutput(payload) => Some(payload.content.as_str()),
            _ => None,
        })
    }

    #[test]
    fn unmatched_prompt_shape_degrades_and_dedups() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();

        // A prompt wording the pack does not know: degraded, not silent.
        let first = engine.evaluate_line(&mut session, "Continue anyway? (y/n)");
        assert_eq!(unrecognized_content(&first), Some("Continue anyway? (y/n)"));

        // The same content again — a repaint, a repeated quiet period — is
        // one prompt, not a stream of them.
        assert!(
            engine
                .evaluate_line(&mut session, "Continue anyway? (y/n)")
                .is_empty()
        );

        // Different unknown prompt: reported again.
        let second = engine.evaluate_line(&mut session, "Overwrite existing? [Y/n]:");
        assert_eq!(
            unrecognized_content(&second),
            Some("Overwrite existing? [Y/n]:")
        );

        // Ordinary output is not prompt-shaped and raises nothing.
        assert!(
            engine
                .evaluate_line(&mut session, "compiling 3 crates, please hold")
                .is_empty()
        );
    }

    /// The dedup is scoped to one occurrence: an unknown prompt, ordinary
    /// output, and a later distinct appearance of the same unknown prompt
    /// must report again — never-silent covers the second occurrence too.
    #[test]
    fn a_distinct_reappearance_of_an_unknown_prompt_reports_again() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();

        let first = engine.evaluate_line(&mut session, "Continue anyway? (y/n)");
        assert_eq!(unrecognized_content(&first), Some("Continue anyway? (y/n)"));

        // A consecutive repaint of the same prompt stays reported-once…
        assert!(
            engine
                .evaluate_line(&mut session, "Continue anyway? (y/n)")
                .is_empty()
        );

        // …but once the stream moves on, the occurrence is over.
        assert!(
            engine
                .evaluate_line(&mut session, "ordinary output between the two")
                .is_empty()
        );
        let second = engine.evaluate_line(&mut session, "Continue anyway? (y/n)");
        assert_eq!(
            unrecognized_content(&second),
            Some("Continue anyway? (y/n)"),
            "a new occurrence of the same unknown prompt is a new report"
        );
    }

    /// Real prompts offer more than two choices as often as not — an
    /// overwrite dialog's `[y/n/a/q]`, an action menu's `[yes/no/all]` —
    /// and the never-silent net has to catch those shapes too.
    #[test]
    fn multi_choice_prompts_are_prompt_shaped() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();

        let three = engine.evaluate_line(&mut session, "Choose action [yes/no/all]");
        assert_eq!(
            unrecognized_content(&three),
            Some("Choose action [yes/no/all]")
        );
        let four = engine.evaluate_line(&mut session, "Overwrite existing? (y/n/a/q)");
        assert_eq!(
            unrecognized_content(&four),
            Some("Overwrite existing? (y/n/a/q)")
        );
        // A single bracketed word is not a choice.
        assert!(
            engine
                .evaluate_line(&mut session, "compiled the module [release]")
                .is_empty()
        );
    }

    #[test]
    fn the_pending_tail_matches_records_and_degrades_when_asking() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();

        // A prompt that will never see its newline still fires its record.
        let prompt = engine.evaluate_pending(&mut session, "Allow filesystem write? [y/N]");
        assert!(matches!(
            &prompt[0].kind,
            EventKind::PromptApprovalRequired(_)
        ));
        // The unchanged tail across further quiet periods is not
        // re-evaluated, so the approval does not repeat.
        assert!(
            engine
                .evaluate_pending(&mut session, "Allow filesystem write? [y/N]")
                .is_empty()
        );

        // An asking-shaped tail the pack does not know degrades…
        let asking = engine.evaluate_pending(&mut session, "continue> ");
        assert_eq!(unrecognized_content(&asking), Some("continue>"));
        // …but a mid-line pause in ordinary output raises nothing: waiting
        // alone is not asking.
        assert!(
            engine
                .evaluate_pending(&mut session, "The quick brown")
                .is_empty()
        );
        assert!(engine.evaluate_pending(&mut session, "").is_empty());
    }

    /// The one-prompt-one-id property: a prompt detected while it waited,
    /// whose newline finally arrives, must not be announced again under a
    /// second approval id.
    #[test]
    fn a_tail_matched_prompt_does_not_double_emit_when_its_line_completes() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();
        let tail = "Allow filesystem write? [y/N]";

        let from_tail = engine.evaluate_pending(&mut session, tail);
        assert!(matches!(
            &from_tail[0].kind,
            EventKind::PromptApprovalRequired(_)
        ));

        assert!(
            engine.evaluate_line(&mut session, tail).is_empty(),
            "the completed line is the same prompt, already announced from its tail"
        );

        // The suppression is one-shot: the same text appearing later is a
        // distinct prompt and fires normally.
        let later = engine.evaluate_line(&mut session, tail);
        assert!(matches!(
            &later[0].kind,
            EventKind::PromptApprovalRequired(_)
        ));
    }

    /// One announcement per occurrence, however the occurrence is
    /// interleaved: a TUI repaints a waiting prompt among status lines,
    /// and every reappearance of the announced tail is that same prompt —
    /// not a fresh one deserving a fresh id. The announcement ends with
    /// the occurrence: an overwrite retires it, and only then does the
    /// same text announce again.
    #[test]
    fn a_reappearing_announced_tail_stays_one_announcement() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();
        let tail = "Allow filesystem write? [y/N]";

        assert!(!engine.evaluate_pending(&mut session, tail).is_empty());
        // Interleaved repaint traffic completes other lines...
        engine.evaluate_line(&mut session, "ordinary output between repaints");
        // ...and the prompt's tail comes around again: same occurrence.
        assert!(
            engine.evaluate_pending(&mut session, tail).is_empty(),
            "the still-waiting prompt is already announced"
        );

        // The occurrence ends when the tail is overwritten; the same text
        // afterwards is a new prompt and announces again.
        assert!(
            engine
                .evaluate_pending(&mut session, "task finished cleanly")
                .is_empty()
        );
        let second = engine.evaluate_pending(&mut session, tail);
        assert!(matches!(
            &second[0].kind,
            EventKind::PromptApprovalRequired(_)
        ));
    }

    /// Terminal prompts end at a cursor that sits after padding as often
    /// as not: trailing whitespace must not defeat an end-anchored
    /// pattern, on the tail or on the completed line.
    #[test]
    fn trailing_whitespace_is_not_content() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();

        let padded_tail = "Allow filesystem write? [y/N] ";
        let from_tail = engine.evaluate_pending(&mut session, padded_tail);
        assert!(
            matches!(&from_tail[0].kind, EventKind::PromptApprovalRequired(_)),
            "an end-anchored pattern fires on a tail with cursor padding"
        );

        // The completed line arrives with its own padding: same prompt,
        // recognized as the one already announced — no second id, and no
        // unrecognized-output echo either.
        assert!(
            engine
                .evaluate_line(&mut session, "Allow filesystem write? [y/N]  ")
                .is_empty()
        );

        // A padded completed line also matches directly.
        let direct = engine.evaluate_line(&mut session, "Allow filesystem write? [y/N] ");
        assert!(matches!(
            &direct[0].kind,
            EventKind::PromptApprovalRequired(_)
        ));
    }

    /// A repaint can interleave other lines between the announcement and
    /// the prompt's own completed line; the suppression marker must
    /// survive the bystanders and be consumed by the line it names.
    #[test]
    fn an_intervening_line_does_not_rearm_the_double_announcement() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();
        let tail = "Allow filesystem write? [y/N]";

        assert!(!engine.evaluate_pending(&mut session, tail).is_empty());
        // The repaint pushes an unrelated line through first.
        assert!(
            engine
                .evaluate_line(&mut session, "processing your request")
                .is_empty()
        );
        assert!(
            engine.evaluate_line(&mut session, tail).is_empty(),
            "the repainted prompt line is the announced prompt, not a new one"
        );
    }

    /// Suppression covers every kind: a stateful matcher that would
    /// normally lose the line must not win it by default just because the
    /// text winner already spoke from the tail — whether that event exists
    /// must not depend on where a quiet period fell.
    #[test]
    fn tail_suppression_covers_the_stateful_pass() {
        let engine = MatcherEngine::builder()
            .records(parse_pack("inline", DETECTS).expect("parses"))
            .stateful(
                Box::new(NeedleMatcher {
                    id: MatcherId::new("also_fires"),
                    priority: 200,
                }),
                emits(
                    "tool.call_started",
                    &[
                        ("call_id", Template::Uuid4),
                        ("tool", Template::Literal("shadow".to_string())),
                    ],
                ),
            )
            .compile()
            .expect("compiles");
        let mut session = engine.new_session();

        // NeedleMatcher fires on lines containing "contested"; craft a
        // prompt line that both the approval record and the stateful
        // matcher match.
        let tail = "Allow contested write? [y/N]";
        let announced = engine.evaluate_pending(&mut session, tail);
        assert!(matches!(
            &announced[0].kind,
            EventKind::PromptApprovalRequired(_)
        ));

        assert!(
            engine.evaluate_line(&mut session, tail).is_empty(),
            "the suppressed line emits nothing from any kind"
        );
    }

    #[test]
    fn a_blank_id_rejects_compilation_on_every_registration_path() {
        // A code-registered stateful matcher with a whitespace name: the
        // loader never sees it, so the compiler must be the one to say no.
        let error = MatcherEngine::builder()
            .stateful(
                FrameMatcher::boxed("   ", StateLifetime::PerSession, 100),
                FrameMatcher::result_emits(),
            )
            .compile()
            .expect_err("a blank id can never be named in a diagnostic");
        assert!(matches!(error, CompileError::BlankId));
    }

    #[test]
    fn an_empty_source_rejects_compilation_even_from_code() {
        use agent_bridge_adapter_api::{MatcherSpec, PatternRecord, TextMatcherType};
        let record = PatternRecord {
            name: "hollow".to_string(),
            matcher: MatcherSpec {
                kind: TextMatcherType::Substring,
                source: String::new(),
                anchor: None,
            },
            emits: emits(
                "tool.call_started",
                &[
                    ("call_id", Template::Uuid4),
                    ("tool", Template::Literal("hollow".to_string())),
                ],
            ),
            priority: 100,
        };
        let error = MatcherEngine::builder()
            .records(vec![record])
            .compile()
            .expect_err("an empty needle matches everywhere, which is not a matcher");
        assert!(matches!(error, CompileError::EmptySource { .. }));
        assert!(error.to_string().contains("hollow"));
    }

    /// The announcement marker tracks the occurrence, not the exact
    /// bytes: a tail that grows while it waits is the same prompt with
    /// more paint, and must not fire once per paint stage.
    #[test]
    fn a_growing_announced_tail_does_not_reannounce() {
        let unanchored = r#"
- name: ready_marker
  matcher: { type: substring, source: 'ready' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: ready }
"#;
        let engine = engine(unanchored);
        let mut session = engine.new_session();

        assert_eq!(engine.evaluate_pending(&mut session, "ready").len(), 1);
        assert!(
            engine
                .evaluate_pending(&mut session, "ready now")
                .is_empty(),
            "the grown tail is the same waiting line, already announced"
        );
        assert!(
            engine.evaluate_line(&mut session, "ready now").is_empty(),
            "and its eventual completion is the same prompt too"
        );
        assert_eq!(
            engine.evaluate_line(&mut session, "ready now").len(),
            1,
            "the suppression was one-shot; a later identical line is new"
        );
    }

    /// An occurrence announced at its fuller paint stage and completing at
    /// a shorter one is still that occurrence: shrinkage is a repaint, and
    /// the one-prompt-one-id guarantee holds through it.
    #[test]
    fn a_shrunken_completion_of_an_announced_tail_stays_one_announcement() {
        let unanchored = r#"
- name: ready_marker
  matcher: { type: substring, source: 'ready' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: ready }
"#;
        let engine = engine(unanchored);
        let mut session = engine.new_session();

        assert_eq!(engine.evaluate_pending(&mut session, "ready now").len(), 1);
        // Mid-repaint, the tail shrinks: same occurrence, nothing new.
        assert!(engine.evaluate_pending(&mut session, "ready").is_empty());
        // And it completes at the shorter stage: still the same prompt.
        assert!(
            engine.evaluate_line(&mut session, "ready").is_empty(),
            "the shrunken completion is the announced occurrence"
        );
        // The suppression was one-shot; a later identical line is new.
        assert_eq!(engine.evaluate_line(&mut session, "ready").len(), 1);
    }

    /// An overwritten announcement retires its marker: the announced text
    /// can never complete as a line, and a kept marker would suppress an
    /// unrelated future line that happens to share the text.
    #[test]
    fn an_overwritten_announcement_does_not_suppress_a_later_line() {
        let unanchored = r#"
- name: ready_marker
  matcher: { type: substring, source: 'ready' }
  emits:
    event_type: tool.call_started
    fields: { call_id: '{{ uuid4() }}', tool: ready }
"#;
        let engine = engine(unanchored);
        let mut session = engine.new_session();

        assert_eq!(engine.evaluate_pending(&mut session, "ready").len(), 1);
        // A carriage-return repaint replaced the tail with something else.
        assert!(
            engine
                .evaluate_pending(&mut session, "working on it")
                .is_empty()
        );
        assert_eq!(
            engine.evaluate_line(&mut session, "ready").len(),
            1,
            "the stale marker must not silence a genuinely new line"
        );
    }

    /// The unknown handoff mirrors the recognized one: a tail reported as
    /// unrecognized, its occurrence interleaved with other lines, then
    /// completing — one prompt, one report.
    #[test]
    fn a_pending_unknown_prompt_reports_once_across_interleaved_lines() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();
        let tail = "Continue anyway? (y/n)";

        let reported = engine.evaluate_pending(&mut session, tail);
        assert_eq!(unrecognized_content(&reported), Some(tail));

        // An interleaved line retires the consecutive-content dedup…
        assert!(
            engine
                .evaluate_line(&mut session, "an interleaved log line")
                .is_empty()
        );
        // …but the occurrence marker carries the report across it.
        assert!(
            engine.evaluate_line(&mut session, tail).is_empty(),
            "the completing line is the reported occurrence, not a new prompt"
        );
        // And a consecutive repaint of the completed line stays quiet too.
        assert!(engine.evaluate_line(&mut session, tail).is_empty());

        // A genuinely new occurrence, after the stream moved on, reports.
        engine.evaluate_line(&mut session, "the stream moved on");
        assert_eq!(
            unrecognized_content(&engine.evaluate_line(&mut session, tail)),
            Some(tail)
        );
    }

    /// The reason the unknown marker is separate from the announced one:
    /// a partial unknown tail that grows into a pattern the pack knows
    /// must still emit its match.
    #[test]
    fn a_partial_unknown_tail_growing_into_a_known_pattern_still_matches() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();

        // Mid-paint, the prompt is unknown: it degrades.
        let partial = engine.evaluate_pending(&mut session, "Allow filesystem write?");
        assert_eq!(
            unrecognized_content(&partial),
            Some("Allow filesystem write?")
        );

        // Fully painted, it is the pack's approval — the match emits.
        let grown = engine.evaluate_pending(&mut session, "Allow filesystem write? [y/N]");
        assert!(matches!(
            &grown[0].kind,
            EventKind::PromptApprovalRequired(_)
        ));

        // And the completion of that occurrence is announced-once.
        assert!(
            engine
                .evaluate_line(&mut session, "Allow filesystem write? [y/N]")
                .is_empty()
        );
    }

    /// The unrecognized memory retires on the pending path exactly as it
    /// does on the line path: repaint cycles that never complete a line
    /// must not let one report silence a later distinct occurrence.
    #[test]
    fn a_changed_tail_retires_the_unrecognized_memory() {
        let engine = engine(DETECTS);
        let mut session = engine.new_session();

        let first = engine.evaluate_pending(&mut session, "continue>");
        assert_eq!(unrecognized_content(&first), Some("continue>"));

        // A repaint shows something else entirely — no completed line.
        assert!(engine.evaluate_pending(&mut session, "working").is_empty());

        let again = engine.evaluate_pending(&mut session, "continue>");
        assert_eq!(
            unrecognized_content(&again),
            Some("continue>"),
            "a distinct reappearance reports even with no line in between"
        );
    }

    #[test]
    fn stateful_matchers_never_see_the_pending_tail() {
        let engine = MatcherEngine::builder()
            .stateful(
                FrameMatcher::boxed("frame", StateLifetime::PerSession, 100),
                FrameMatcher::result_emits(),
            )
            .compile()
            .expect("compiles");
        let mut session = engine.new_session();

        // The tail carries a frame opener, but a tail is not a completed
        // line: the frame matcher must not have consumed it, or the same
        // text would reach it twice when the line completes.
        assert!(
            engine
                .evaluate_pending(&mut session, "BEGIN alpha")
                .is_empty()
        );
        assert!(
            engine.evaluate_line(&mut session, "END").is_empty(),
            "no frame is open: the pending BEGIN was never state-advanced"
        );
    }
}
