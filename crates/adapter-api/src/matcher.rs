//! The pattern-matcher protocol: what an adapter declares, so the stream
//! layer can run the search.
//!
//! An adapter never parses output itself. It hands the runtime a set of
//! *matchers* — declarative YAML records for the common cases, code for the
//! two kinds that need it — and the stream crate's engine evaluates them
//! against the post-strip feed. The split is deliberate and load-bearing:
//! records and protocol live here, where the adapter contract will be frozen,
//! while compilation and evaluation live in the engine, so a new prompt
//! wording in a CLI release is a data edit plus a fixture, never an engine
//! change.
//!
//! Four kinds share one protocol. A **literal** fires when its needle occurs
//! in a line; a **regex** fires when its pre-compiled expression matches; a
//! **stateful** matcher is a small function over a sliding window of lines
//! with explicit per-session state; a **screen** matcher is a function over
//! the reconstructed screen, evaluated at evaluation points rather than per
//! byte, and only for sessions that keep a screen. The first two ship as
//! data records; the last two are code, registered alongside the records.
//!
//! Two rules keep the code kinds honest. State never lives in the adapter:
//! the engine owns every stateful matcher's state, keyed by session and
//! matcher and scoped to one compilation — an adapter reload compiles a
//! new engine and starts fresh sessions rather than migrating live state —
//! and state is cleared on the session boundaries the lifetime names. And
//! no matcher is trusted with the clock: every evaluation that returns is
//! measured against the engine's wall-clock ceiling, and one that breaches
//! it is disabled for that session and reported. An evaluation that never
//! returns is beyond any post-return check; it is bounded at the dispatch
//! side's deadline, so a matcher must not rely on being preempted — it
//! will not be.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer};

/// The evaluation-order default: records and code matchers that do not say
/// otherwise evaluate at this rank.
///
/// Priorities order evaluation when several matchers could fire on the same
/// input: **smaller numbers evaluate first and win**, ties fall to
/// registration order. Evaluation runs per kind — the text pass first, so
/// the automaton's early exit can do its work, then the code matchers —
/// with priority governing the order within each pass and deciding the
/// winner across them. Starting everything at 100 leaves room to promote
/// one record ahead of a pack without renumbering the rest.
pub const DEFAULT_PRIORITY: u32 = 100;

/// A matcher's stable identity — the name events, metrics, and the
/// per-session disabled set refer to it by.
///
/// For a data record this is the record's `name`; a code matcher states its
/// own. Identity is per adapter registration: two adapters may both have an
/// `approval_bash`, but one adapter registering the same name twice is a
/// compile-time rejection, because an id that names two matchers can name
/// neither in an error.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MatcherId(String);

impl MatcherId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MatcherId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The four ways a matcher can look at output. See the module header for
/// what each is for; the names here are the wire spellings events and
/// diagnostics use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatcherKind {
    Literal,
    Regex,
    Stateful,
    Screen,
}

impl MatcherKind {
    /// The kind's wire spelling.
    pub fn name(self) -> &'static str {
        match self {
            Self::Literal => "literal",
            Self::Regex => "regex",
            Self::Stateful => "stateful",
            Self::Screen => "screen",
        }
    }
}

/// When a stateful matcher's state is thrown away.
///
/// The engine owns the state either way; the lifetime only names the
/// boundary that clears it. `PerSession` state survives until the session
/// closes. `PerPrompt` state additionally resets every time the session
/// moves from running to awaiting an approval — the boundary a multi-line
/// prompt assembler cares about, because whatever it was assembling is
/// either the prompt that was just detected or noise that predates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateLifetime {
    PerSession,
    PerPrompt,
}

// ---------------------------------------------------------------------------
// The data-record shape: what a pattern pack's YAML deserializes into.
// ---------------------------------------------------------------------------

/// One pattern record from a versioned pack file.
///
/// A pack is a YAML list of these. The shape is deliberately a simple
/// primitive — a matcher, an event to emit, field templates, a priority —
/// with no conditionals, no loops, and no expressions beyond the two
/// template helpers. Anything that needs more than that is not a record; it
/// is a stateful matcher, written as code.
///
/// ```yaml
/// - name: approval_bash
///   matcher:
///     type: regex
///     source: '^(?P<prompt>Do you want to (?P<verb>run|allow) .+?)\?'
///     anchor: line_start
///   emits:
///     event_type: prompt.approval_required
///     fields:
///       approval_id: '{{ uuid4() }}'
///       prompt: '{{ matches.prompt }}'
///       tool: bash
/// ```
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatternRecord {
    /// The record's identity — becomes its [`MatcherId`].
    pub name: String,
    /// What to look for.
    pub matcher: MatcherSpec,
    /// The event a match becomes.
    pub emits: EmitSpec,
    /// Evaluation rank; smaller evaluates first and wins. See
    /// [`DEFAULT_PRIORITY`].
    #[serde(default = "default_priority")]
    pub priority: u32,
}

fn default_priority() -> u32 {
    DEFAULT_PRIORITY
}

/// A record's matcher: the text-stream kinds a pack file can carry.
///
/// Screen records have a richer shape — a viewport-row anchor and an
/// optional menu grammar — and their loading lands with the first pack that
/// carries one; until then a pack file declaring `type: screen` is rejected
/// at load with an error naming the record, and screen matchers register
/// through the code path ([`ScreenMatcher`]).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatcherSpec {
    /// `regex` or `substring`. A substring is the faster path: the literal
    /// scan is the whole evaluation, no expression engine involved.
    #[serde(rename = "type")]
    pub kind: TextMatcherType,
    /// The needle (`substring`) or the expression (`regex`), with named
    /// groups feeding the `matches.<group>` field templates.
    pub source: String,
    /// Where in a line the matcher is allowed to fire. Absent means
    /// anywhere.
    #[serde(default)]
    pub anchor: Option<Anchor>,
}

/// The text-stream matcher types a record can declare.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextMatcherType {
    Regex,
    Substring,
}

impl TextMatcherType {
    /// The [`MatcherKind`] this record type evaluates as.
    pub fn kind(self) -> MatcherKind {
        match self {
            Self::Regex => MatcherKind::Regex,
            Self::Substring => MatcherKind::Literal,
        }
    }
}

/// A positional constraint on where a text matcher may fire.
///
/// `line_start` is the approval-prompt spoofing defense: a matcher so
/// anchored fires only when its match begins at the first column, so prompt
/// text planted mid-line — inside a token stream an attacker controls —
/// does not match. Approval patterns should carry it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Anchor {
    LineStart,
}

/// The event a matched record emits: a type from the published taxonomy and
/// the templates that fill its fields.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmitSpec {
    /// The namespaced event type, e.g. `prompt.approval_required`. Must be a
    /// type the engine knows how to construct; an unknown type fails the
    /// pack at load rather than at match time.
    pub event_type: String,
    /// Field name → template. Which names are meaningful — and which are
    /// required — depends on the event type, and the loader validates both.
    #[serde(default)]
    pub fields: BTreeMap<String, TemplateValue>,
}

/// A field's value in an `emits` block: one template, or a list of them for
/// fields that are lists (an approval prompt's `options`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum TemplateValue {
    One(Template),
    Many(Vec<Template>),
}

/// One field template. The whole helper vocabulary is two entries:
///
/// - `{{ uuid4() }}` — a fresh v4 UUID per match, for identifiers.
/// - `{{ matches.<group> }}` — the text a named regex group captured.
///
/// Anything else inside `{{ }}` fails the pack at load, and a template must
/// be the field's whole value — there is no interpolation into surrounding
/// text, because interpolation is the first step toward the expression
/// language this format exists to not become. A value with no braces is
/// itself, verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Template {
    /// `{{ uuid4() }}`
    Uuid4,
    /// `{{ matches.<group> }}` — the named capture to read.
    Group(String),
    /// A verbatim value.
    Literal(String),
}

impl Template {
    /// Parses one authored field value. Errors are the loader's to attach to
    /// a record name; the message describes only the value.
    pub fn parse(value: &str) -> Result<Self, String> {
        let trimmed = value.trim();
        let inner = match trimmed
            .strip_prefix("{{")
            .and_then(|rest| rest.strip_suffix("}}"))
        {
            Some(inner) if !inner.contains("{{") => inner.trim(),
            // Braces that are not a whole-value template: either a helper
            // embedded in surrounding text, or nested braces. Both are
            // interpolation, which the format does not have.
            _ if trimmed.contains("{{") || trimmed.contains("}}") => {
                return Err(format!(
                    "template must be the whole value, `{{{{ uuid4() }}}}` or \
                     `{{{{ matches.<group> }}}}`: `{trimmed}`"
                ));
            }
            _ => return Ok(Self::Literal(value.to_string())),
        };
        if inner == "uuid4()" {
            return Ok(Self::Uuid4);
        }
        if let Some(group) = inner.strip_prefix("matches.") {
            let valid_start = group
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_alphabetic() || first == '_');
            if valid_start && group.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Ok(Self::Group(group.to_string()));
            }
            return Err(format!("`matches.` needs a capture-group name: `{inner}`"));
        }
        Err(format!(
            "unknown template helper `{inner}` — the vocabulary is `uuid4()` and \
             `matches.<group>`"
        ))
    }
}

impl<'de> Deserialize<'de> for Template {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// The code-path protocol: what stateful and screen matchers implement.
// ---------------------------------------------------------------------------

/// What a match hands back: the named captures the emit templates read.
///
/// For a regex record the engine fills this from the expression's named
/// groups; a code matcher fills it by hand with whatever it extracted. The
/// values are fragments of session output, which is why this type's `Debug`
/// shows counts and never content — output reaches logs through events,
/// deliberately, or not at all.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Captures(BTreeMap<String, String>);

impl Captures {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a named capture, returning `self` so a code matcher can build
    /// an outcome in one expression.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(name.into(), value.into());
        self
    }

    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.0.insert(name.into(), value.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Captures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Captures({} groups)", self.0.len())
    }
}

/// A successful evaluation: the captures the winning matcher's emit mapping
/// will read. Which matcher won, and at what priority, is the engine's
/// bookkeeping — a matcher cannot claim someone else's identity by writing
/// it into its result.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct MatchOutcome {
    pub captures: Captures,
}

impl MatchOutcome {
    /// A match with nothing to extract — the common case for detections
    /// whose event fields are all fixed or generated.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_captures(captures: Captures) -> Self {
        Self { captures }
    }
}

impl fmt::Debug for MatchOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MatchOutcome({:?})", self.captures)
    }
}

/// What a stateful matcher sees: the line under evaluation and a bounded
/// window of the completed lines before it, oldest first.
///
/// The window is the engine's, and its depth is the engine's choice; a
/// matcher that needs more history than it is given keeps that history in
/// its [`MatcherState`], which is what the state cell is for. Holds session
/// output, so `Debug` shows shape only.
pub struct TextWindow<'a> {
    line: &'a str,
    recent: &'a [String],
}

impl<'a> TextWindow<'a> {
    pub fn new(line: &'a str, recent: &'a [String]) -> Self {
        Self { line, recent }
    }

    /// The line under evaluation.
    pub fn line(&self) -> &'a str {
        self.line
    }

    /// Completed lines before this one, oldest first, bounded by the
    /// engine's window depth.
    pub fn recent(&self) -> &'a [String] {
        self.recent
    }
}

impl fmt::Debug for TextWindow<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TextWindow({} bytes, {} recent)",
            self.line.len(),
            self.recent.len()
        )
    }
}

/// One stateful matcher's state cell, owned by the engine and keyed by
/// `(session, matcher)`.
///
/// The cell is typed by its own matcher: store whatever state the detection
/// needs, get it back on the next line. Because exactly one matcher reads a
/// given cell, a type mismatch can only mean that matcher changed its own
/// state type — so the cell resets rather than erroring. The cell's life is
/// bounded by its compilation: an adapter reload compiles a new engine and
/// creates fresh sessions, so state never has to survive code it was not
/// written by — never *in* the adapter, never carried across a reload.
#[derive(Default)]
pub struct MatcherState {
    slot: Option<Box<dyn Any + Send>>,
}

impl MatcherState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The typed state, created by `init` if the cell is empty (or holds a
    /// previous incarnation's type).
    pub fn get_or_insert_with<T: Send + 'static>(&mut self, init: impl FnOnce() -> T) -> &mut T {
        let holds = self.slot.as_ref().is_some_and(|current| current.is::<T>());
        if !holds {
            self.slot = Some(Box::new(init()));
        }
        self.slot
            .as_mut()
            .and_then(|slot| slot.downcast_mut::<T>())
            .expect("slot was just checked or replaced to hold T")
    }

    /// The typed state, if this cell holds one.
    pub fn get<T: Send + 'static>(&self) -> Option<&T> {
        self.slot.as_ref().and_then(|slot| slot.downcast_ref::<T>())
    }

    /// Empties the cell — what the engine does at the boundary the
    /// matcher's [`StateLifetime`] names.
    pub fn clear(&mut self) {
        self.slot = None;
    }

    pub fn is_empty(&self) -> bool {
        self.slot.is_none()
    }
}

impl fmt::Debug for MatcherState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.slot.is_some() {
            "MatcherState(occupied)"
        } else {
            "MatcherState(empty)"
        })
    }
}

/// A matcher that needs memory across lines: multi-line prompts, framed
/// output, anything one line cannot decide.
///
/// `evaluate` is called once per completed line, in priority order, with the
/// engine-owned state cell for this session — it runs even when a
/// higher-priority matcher already matched the line, so its view of the
/// stream never has gaps, but its match only becomes an event when it wins.
/// Every call that returns is measured against the engine's per-evaluation
/// wall-clock ceiling; a breach disables the matcher for that session and
/// is reported once. The measurement happens after the return — there is
/// no preemption — so an implementation that can block must bound its own
/// waiting: a call that never returns is bounded only by the dispatch
/// side's deadline, and the session pays for it until then.
pub trait StatefulMatcher: Send + Sync {
    fn id(&self) -> &MatcherId;

    /// Evaluation rank within its kind; across kinds the rank decides who
    /// wins, with the text pass evaluating first by design.
    fn priority(&self) -> u32 {
        DEFAULT_PRIORITY
    }

    /// Which boundary clears this matcher's state.
    fn state_lifetime(&self) -> StateLifetime;

    /// One completed line, with this session's state cell. Pure but for the
    /// cell: everything remembered lives in `state`, nowhere else.
    fn evaluate(&self, window: &TextWindow<'_>, state: &mut MatcherState) -> Option<MatchOutcome>;
}

/// What changed on the screen between two evaluation points.
///
/// `damaged` lists the rows written to since the last evaluation point — the
/// rows worth examining, repaints included. `novel` is the subset of that
/// text not reported recently: the rows worth emitting about. A row that
/// went blank is damage with no novel entry, because emptiness is not
/// content — a matcher watching for a dialog's *disappearance* reads
/// `damaged` plus the snapshot, not `novel`.
pub struct ScreenDiff<'a> {
    pub damaged: &'a [u16],
    pub novel: &'a [NovelRow<'a>],
}

impl fmt::Debug for ScreenDiff<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ScreenDiff({} damaged, {} novel)",
            self.damaged.len(),
            self.novel.len()
        )
    }
}

/// One row whose text is newly on screen: its position and what it says.
pub struct NovelRow<'a> {
    pub row: u16,
    pub text: &'a str,
}

impl fmt::Debug for NovelRow<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NovelRow(row {}, {} bytes)", self.row, self.text.len())
    }
}

/// A matcher over the reconstructed screen, for CLIs that draw their
/// prompts instead of printing them.
///
/// Evaluated at evaluation points — the quiet-period boundary or feed
/// quiescence, whichever a burst reaches first — never per byte, and only
/// for sessions whose effective `tui_aware` keeps a screen. Measured
/// against the same per-evaluation ceiling as every other kind, with the
/// same limit: the check runs after the call returns, so an implementation
/// that can block must bound its own waiting.
pub trait ScreenMatcher: Send + Sync {
    fn id(&self) -> &MatcherId;

    /// Evaluation rank within its kind — the screen pass has a cadence of
    /// its own and never contests the per-line passes.
    fn priority(&self) -> u32 {
        DEFAULT_PRIORITY
    }

    /// The rendered screen and what changed since the last evaluation
    /// point. To read a snapshot row as text, skip its width-0 cells and
    /// concatenate the rest.
    fn evaluate(
        &self,
        snapshot: &agent_bridge_events::ScreenSnapshot,
        diff: &ScreenDiff<'_>,
    ) -> Option<MatchOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record example from the adapter-system design contract, verbatim:
    /// the shape this file exists to deserialize.
    #[test]
    fn design_contract_example_deserializes() {
        let yaml = r#"
- name: approval_bash
  matcher:
    type: regex
    source: '^(?P<prompt>Do you want to (?P<verb>run|allow) .+?)\?'
    anchor: line_start
  emits:
    event_type: prompt.approval_required
    fields:
      approval_id: '{{ uuid4() }}'
      prompt: '{{ matches.prompt }}'
      tool: bash
"#;
        let records: Vec<PatternRecord> = serde_norway::from_str(yaml).expect("the design example");
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.name, "approval_bash");
        assert_eq!(record.matcher.kind, TextMatcherType::Regex);
        assert_eq!(record.matcher.anchor, Some(Anchor::LineStart));
        assert_eq!(record.priority, DEFAULT_PRIORITY);
        assert_eq!(record.emits.event_type, "prompt.approval_required");
        assert_eq!(
            record.emits.fields.get("approval_id"),
            Some(&TemplateValue::One(Template::Uuid4))
        );
        assert_eq!(
            record.emits.fields.get("prompt"),
            Some(&TemplateValue::One(Template::Group("prompt".into())))
        );
        assert_eq!(
            record.emits.fields.get("tool"),
            Some(&TemplateValue::One(Template::Literal("bash".into())))
        );
    }

    #[test]
    fn priority_and_anchor_are_optional_with_documented_defaults() {
        let yaml = r#"
- name: minimal
  matcher:
    type: substring
    source: 'esc to interrupt'
  emits:
    event_type: prompt.approval_required
    fields:
      approval_id: '{{ uuid4() }}'
      prompt: literal prompt
"#;
        let records: Vec<PatternRecord> = serde_norway::from_str(yaml).expect("minimal record");
        assert_eq!(records[0].priority, DEFAULT_PRIORITY);
        assert_eq!(records[0].matcher.anchor, None);
        assert_eq!(records[0].matcher.kind.kind(), MatcherKind::Literal);
    }

    #[test]
    fn a_list_valued_field_deserializes_per_template() {
        let yaml = r#"
- name: listy
  matcher:
    type: substring
    source: '[y/N]'
  emits:
    event_type: prompt.approval_required
    fields:
      approval_id: '{{ uuid4() }}'
      prompt: '{{ matches.prompt }}'
      options: ['y', 'n']
"#;
        let records: Vec<PatternRecord> = serde_norway::from_str(yaml).expect("list field");
        assert_eq!(
            records[0].emits.fields.get("options"),
            Some(&TemplateValue::Many(vec![
                Template::Literal("y".into()),
                Template::Literal("n".into()),
            ]))
        );
    }

    /// The helper vocabulary is closed: anything else inside braces is a
    /// load-time error, not a literal that happens to look like a mistake.
    #[test]
    fn unknown_template_helpers_fail_to_deserialize() {
        for bad in [
            "{{ now() }}",
            "{{ matches. }}",
            "{{ matches.1prompt }}",
            "{{ matches.prompt.upper }}",
            "prefix {{ uuid4() }}",
            "{{ uuid4() }} suffix",
            "{{ {{ uuid4() }} }}",
        ] {
            assert!(Template::parse(bad).is_err(), "`{bad}` should be rejected");
        }
    }

    #[test]
    fn braceless_values_are_verbatim_literals() {
        assert_eq!(
            Template::parse("bash").expect("plain literal"),
            Template::Literal("bash".into())
        );
        assert_eq!(
            Template::parse("a } stray brace").expect("single braces are text"),
            Template::Literal("a } stray brace".into())
        );
        assert_eq!(
            Template::parse("  spaced  ").expect("whitespace is content"),
            Template::Literal("  spaced  ".into())
        );
    }

    #[test]
    fn template_whitespace_inside_braces_is_forgiven() {
        assert_eq!(
            Template::parse("{{uuid4()}}").expect("tight"),
            Template::Uuid4
        );
        assert_eq!(
            Template::parse("{{   matches.verb   }}").expect("loose"),
            Template::Group("verb".into())
        );
    }

    /// A typo'd field name must fail the pack, not ride along ignored — the
    /// record format validates without a runtime, and unknown keys are the
    /// most common authoring mistake there is.
    #[test]
    fn unknown_record_fields_are_rejected() {
        let yaml = r#"
- name: typo
  matcher:
    type: regex
    source: 'x'
    ancor: line_start
  emits:
    event_type: prompt.approval_required
"#;
        let result: Result<Vec<PatternRecord>, _> = serde_norway::from_str(yaml);
        assert!(result.is_err());
    }

    #[test]
    fn state_cell_is_typed_by_its_matcher() {
        let mut state = MatcherState::new();
        assert!(state.is_empty());
        *state.get_or_insert_with(|| 0u64) += 41;
        *state.get_or_insert_with(|| 0u64) += 1;
        assert_eq!(state.get::<u64>(), Some(&42));

        // A different type means the matcher changed its own state shape:
        // the cell resets rather than misreading old bytes as the new type.
        assert_eq!(state.get::<String>(), None);
        state.get_or_insert_with(String::new).push('x');
        assert_eq!(state.get::<String>().map(String::as_str), Some("x"));

        state.clear();
        assert!(state.is_empty());
    }

    #[test]
    fn content_holding_types_debug_without_content() {
        let secret = "Do you want to run rm -rf /? [y/N]";
        let window = TextWindow::new(secret, &[]);
        let captures = Captures::new().with("prompt", secret);
        let novel = NovelRow {
            row: 3,
            text: secret,
        };
        let diff = ScreenDiff {
            damaged: &[3],
            novel: std::slice::from_ref(&novel),
        };
        for rendered in [
            format!("{window:?}"),
            format!("{captures:?}"),
            format!("{:?}", MatchOutcome::with_captures(captures.clone())),
            format!("{novel:?}"),
            format!("{diff:?}"),
        ] {
            assert!(
                !rendered.contains("rm -rf"),
                "content leaked into Debug: {rendered}"
            );
        }
    }
}
