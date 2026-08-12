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

use agent_bridge_adapter_api::{
    Anchor, Captures, EmitSpec, MatcherId, NovelRow, PatternRecord, ScreenDiff, ScreenMatcher,
    StateLifetime, StatefulMatcher, TextMatcherType, TextWindow,
};
use agent_bridge_events::{AdapterErrorCode, AdapterErrorPayload, EventBody, ScreenSnapshot};
use aho_corasick::AhoCorasick;
use regex::Regex;

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
        | Self::BadEmit { record, .. } = self
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

    /// Compiles the set: expressions, prefilter automaton, evaluation
    /// order. Failure rejects the whole registration.
    pub fn compile(self) -> Result<MatcherEngine, CompileError> {
        let mut ids = std::collections::BTreeSet::new();
        let mut text = Vec::with_capacity(self.records.len());
        for (record, order) in self.records {
            validate_emit_spec(&record.emits).map_err(|message| CompileError::BadEmit {
                record: record.name.clone(),
                message,
            })?;
            if !ids.insert(record.name.clone()) {
                return Err(CompileError::DuplicateId { id: record.name });
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

        let mut screen_sortable = Vec::with_capacity(self.screen.len());
        for (matcher, emits, order) in self.screen {
            let id = matcher.id().clone();
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
            regex_evaluations: AtomicU64::new(0),
        })
    }
}

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
    /// In registration order; cell `i` of a session's state belongs to
    /// entry `i` here.
    stateful: Vec<StatefulRegistration>,
    /// In evaluation order: ascending priority, ties by registration
    /// order. The screen pass runs at evaluation points, not per line.
    screen: Vec<ScreenRegistration>,
    regex_evaluations: AtomicU64,
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
    pub fn evaluate_line(&self, session: &mut SessionMatcherState, line: &str) -> Vec<EventBody> {
        debug_assert_eq!(
            session.cells.len(),
            self.stateful.len(),
            "a session state object is only valid with the engine that created it"
        );
        let mut candidate = vec![false; self.text.len()];
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

        struct Winner<'engine> {
            priority: u32,
            order: usize,
            id: &'engine MatcherId,
            emits: &'engine EmitSpec,
            captures: Captures,
        }
        let mut winner: Option<Winner<'_>> = None;
        for (record, _) in self
            .text
            .iter()
            .zip(candidate)
            .filter(|(_, is_candidate)| *is_candidate)
        {
            if let Some(captures) = self.eval_text(record, line) {
                winner = Some(Winner {
                    priority: record.priority,
                    order: record.order,
                    id: &record.id,
                    emits: &record.emits,
                    captures,
                });
                break;
            }
        }

        let SessionMatcherState { cells, recent, .. } = session;
        for (registration, cell) in self.stateful.iter().zip(cells) {
            let window = TextWindow::new(line, recent);
            let Some(outcome) = registration.matcher.evaluate(&window, cell) else {
                continue;
            };
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

        match winner {
            Some(winner) => {
                // The id, never the line: which pattern fired is diagnostic
                // gold, but the line it fired on is session output.
                tracing::debug!(matcher = %winner.id, "pattern matched");
                vec![render_event(winner.emits, &winner.captures)]
            }
            None => Vec::new(),
        }
    }

    /// The lifetimes of the stateful registrations, in cell order — what a
    /// session's state object is built from.
    pub(crate) fn stateful_lifetimes(&self) -> Vec<StateLifetime> {
        self.stateful
            .iter()
            .map(|registration| registration.lifetime)
            .collect()
    }

    /// Whether any screen matcher is registered — the check that lets a
    /// session skip materializing a snapshot nobody would read.
    pub fn has_screen_matchers(&self) -> bool {
        !self.screen.is_empty()
    }

    /// The screen pass, run at evaluation points — never per byte, never
    /// per line. The caller brings the rendered snapshot and what changed
    /// since the last point; screen matchers see both, in priority order,
    /// first match wins the point.
    pub fn evaluate_screen(
        &self,
        snapshot: &ScreenSnapshot,
        evaluation: &crate::screen::Evaluation,
    ) -> Vec<EventBody> {
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
        for registration in &self.screen {
            if let Some(outcome) = registration.matcher.evaluate(snapshot, &diff) {
                tracing::debug!(matcher = %registration.id, "screen pattern matched");
                return vec![render_event(&registration.emits, &outcome.captures)];
            }
        }
        Vec::new()
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
        assert!(eval(&engine, "token output Allow filesystem write? [y/N]").is_empty());

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
}
