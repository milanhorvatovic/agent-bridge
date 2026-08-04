//! The prototype pattern set and its compiled matcher engine.
//!
//! Every needle below was read out of the committed captures, not imagined:
//! the set was tuned against **claude 2.1.201** and **codex 0.145.0** (the
//! middle pinned version of each CLI) and is deliberately left untouched for
//! the neighbouring versions, so a vendor-side wording or paint change
//! surfaces as a measured miss instead of being quietly papered over.
//!
//! Three roles keep the accounting honest:
//!
//! - **Anchored** patterns detect an event the driver step log records
//!   (a permission dialog, a tool result, an interrupt notice). Each has an
//!   expected firing count derived from the log, and a shortfall is a false
//!   negative.
//! - **Control** patterns are measurement instruments: the phrasing a
//!   pattern author would naturally write, kept even though the capture
//!   shows the surface defeats it. `Do you want to proceed?` never survives
//!   the claude TUI's cursor-positioned paint (it arrives as
//!   `Doyouwanttoproceed?`), and the idle notification paints nothing at
//!   all — their false-negative rates *are* findings, so they stay.
//! - **Ambient** patterns classify recurring chrome (status hints, borders,
//!   banners) that has no per-event ground truth. They contribute to the
//!   recognized share of emissions but never to false negatives.
//!
//! Two pattern sets share the engine and the roles. The **stream set**
//! ([`PATTERNS`]) is configuration (a)'s: tuned to the stripped byte stream,
//! cursor-mash artefacts included. The **screen set** ([`SCREEN_PATTERNS`])
//! is configuration (b)'s: tuned to the rendered screen, where cursor
//! positioning lands text in cells and the paint reads the way a human sees
//! it. The two sets mirror each other's controls deliberately — the spaced
//! dialog title is a control in the stream set because the stream never
//! carries it, and the mashed title is a control in the screen set because
//! a screen never shows it. Those mirrored rates *are* the configuration
//! delta under measurement.
//!
//! The engine mirrors the planned runtime's execution model: literal needles
//! and regex prefilters are compiled into one Aho-Corasick automaton per
//! CLI, and a regex runs only on lines the automaton flags (a regex with no
//! usable prefilter runs on every line). Each regex evaluation is timed
//! against a wall-clock safety ceiling — measured after the evaluation
//! returns, not preempting it — and an over-ceiling matcher is disabled for
//! the rest of the session with the trip reported. A single evaluation
//! cannot wedge in the first place: the regex engine guarantees linear-time
//! matching, which is what makes post-hoc detection sufficient here. The
//! guard exists so anomalously slow patterns surface in the accounting the
//! way the planned runtime would disable them, not to enforce a timeout.

use std::time::{Duration, Instant};

use aho_corasick::AhoCorasick;
use regex::Regex;

/// The CLIs the corpus captures. The fake CLI's corpus is conformance
/// fixtures for other tooling, not a detection target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cli {
    Claude,
    Codex,
}

impl Cli {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        }
    }

    /// The pinned version every matcher set of this CLI was read out of.
    /// The other pinned versions replay against the sets untouched, so
    /// their shortfalls are the version-drift measurement.
    pub fn tuned_version(self) -> &'static str {
        match self {
            Self::Claude => "2.1.201",
            Self::Codex => "0.145.0",
        }
    }
}

/// Accounting role of a pattern; see the module header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Anchored,
    Control,
    Ambient,
}

impl Role {
    pub fn name(self) -> &'static str {
        match self {
            Self::Anchored => "anchored",
            Self::Control => "control",
            Self::Ambient => "ambient",
        }
    }
}

/// How a pattern matches a stripped line.
#[derive(Debug)]
pub enum MatcherKind {
    /// Fires when the needle occurs anywhere in the line. The Aho-Corasick
    /// pass *is* the evaluation.
    Literal(&'static str),
    /// Fires when the regex matches. With a prefilter, the regex runs only
    /// on lines where the automaton found the prefilter needle.
    Regex {
        pattern: &'static str,
        prefilter: Option<&'static str>,
    },
}

pub struct PatternSpec {
    /// Stable identifier, `<cli>/<name>` — the key in every report row.
    pub id: &'static str,
    pub cli: Cli,
    /// Pipeline-local classification the pattern votes for. These are spike
    /// classifications, not runtime wire events.
    pub class: &'static str,
    pub role: Role,
    pub kind: MatcherKind,
}

/// The prototype set. Per-pattern comments record the observed anchor the
/// needle came from.
pub const PATTERNS: &[PatternSpec] = &[
    // ----- claude: anchored -------------------------------------------------
    // The permission dialog title as the TUI actually delivers it: cursor
    // positioning between words strips the spaces out of the byte stream.
    PatternSpec {
        id: "claude/permission-title-mashed",
        cli: Cli::Claude,
        class: "dialog.permission",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Doyouwanttoproceed?"),
    },
    // First menu option of the permission dialog, painted with its
    // selection caret.
    PatternSpec {
        id: "claude/permission-option-yes",
        cli: Cli::Claude,
        class: "dialog.permission",
        role: Role::Anchored,
        kind: MatcherKind::Literal("❯ 1. Yes"),
    },
    PatternSpec {
        id: "claude/permission-option-no",
        cli: Cli::Claude,
        class: "dialog.permission",
        role: Role::Anchored,
        kind: MatcherKind::Literal("2. No"),
    },
    // Shell tool result block: `⎿  $ <command>`. Covers the Bash echo the
    // captures contain; the Read-tool result is the separate record below.
    PatternSpec {
        id: "claude/tool-command-echo",
        cli: Cli::Claude,
        class: "tool.result",
        role: Role::Anchored,
        kind: MatcherKind::Regex {
            pattern: "⎿\\s+\\$\\s",
            prefilter: Some("⎿"),
        },
    },
    // Read tool result, added as the timed add-a-pattern trial of the
    // metrics step (it was deliberately uncovered until then). The durable
    // mark in the stream is the same folded summary the settled screen
    // shows — `Read 2 files` — so batched same-type calls leave one line
    // for two events, and the pattern still under-fires per event wherever
    // repaints don't duplicate the line. That shortfall is a property of
    // the surface, measured rather than papered over.
    PatternSpec {
        id: "claude/tool-read-result",
        cli: Cli::Claude,
        class: "tool.result",
        role: Role::Anchored,
        kind: MatcherKind::Regex {
            pattern: "Read \\d+ files",
            prefilter: Some("Read "),
        },
    },
    // Response block bullet — the durable mark a completed assistant turn
    // leaves in the scrollback.
    PatternSpec {
        id: "claude/response-bullet",
        cli: Cli::Claude,
        class: "content.response",
        role: Role::Anchored,
        kind: MatcherKind::Literal("⏺"),
    },
    // Painted after Esc interrupts generation.
    PatternSpec {
        id: "claude/interrupted-notice",
        cli: Cli::Claude,
        class: "session.interrupted",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Interrupted · What should Claude do instead"),
    },
    // Outcome line of the /compact scenario. Known red on 2.1.202 at 80x24,
    // where the capture holds only cursor-mashed variants of the sentence —
    // kept as the measured drift datapoint, not fixed by widening.
    PatternSpec {
        id: "claude/compact-result",
        cli: Cli::Claude,
        class: "compact.result",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Not enough messages to compact"),
    },
    // ----- claude: controls -------------------------------------------------
    // The dialog title as a pattern author would write it. The TUI never
    // paints it contiguously, so this measures the naive-phrasing miss.
    PatternSpec {
        id: "claude/permission-title-spaced",
        cli: Cli::Claude,
        class: "dialog.permission",
        role: Role::Control,
        kind: MatcherKind::Literal("Do you want to proceed?"),
    },
    // The idle notification's hook message. The TUI paints nothing when it
    // fires, so this measures a surface that structurally does not exist in
    // the byte stream.
    PatternSpec {
        id: "claude/idle-notice",
        cli: Cli::Claude,
        class: "notice.idle",
        role: Role::Control,
        kind: MatcherKind::Literal("Claude is waiting for your input"),
    },
    // ----- claude: ambient --------------------------------------------------
    PatternSpec {
        id: "claude/trust-option",
        cli: Cli::Claude,
        class: "dialog.trust",
        role: Role::Ambient,
        kind: MatcherKind::Literal("trust this folder"),
    },
    PatternSpec {
        id: "claude/status-esc-hint",
        cli: Cli::Claude,
        class: "status.hint",
        role: Role::Ambient,
        kind: MatcherKind::Literal("esc to interrupt"),
    },
    PatternSpec {
        id: "claude/splash-welcome",
        cli: Cli::Claude,
        class: "chrome.splash",
        role: Role::Ambient,
        kind: MatcherKind::Literal("Welcome back"),
    },
    PatternSpec {
        id: "claude/shortcut-hint",
        cli: Cli::Claude,
        class: "chrome.hint",
        role: Role::Ambient,
        kind: MatcherKind::Literal("? for shortcuts"),
    },
    PatternSpec {
        id: "claude/prompt-echo",
        cli: Cli::Claude,
        class: "chrome.prompt",
        role: Role::Ambient,
        kind: MatcherKind::Literal("❯ "),
    },
    // Version banner; the paint is sometimes mashed (`Claude Codev2.1.201`),
    // so the space is optional.
    PatternSpec {
        id: "claude/version-banner",
        cli: Cli::Claude,
        class: "chrome.banner",
        role: Role::Ambient,
        kind: MatcherKind::Regex {
            pattern: "Claude Code ?v\\d",
            prefilter: Some("Claude Code"),
        },
    },
    PatternSpec {
        id: "claude/divider",
        cli: Cli::Claude,
        class: "chrome.divider",
        role: Role::Ambient,
        kind: MatcherKind::Regex {
            pattern: "^─{10,}",
            prefilter: Some("──────────"),
        },
    },
    PatternSpec {
        id: "claude/box-border",
        cli: Cli::Claude,
        class: "chrome.box",
        role: Role::Ambient,
        kind: MatcherKind::Literal("│"),
    },
    // ----- codex: anchored --------------------------------------------------
    // The workspace-trust prompt, cursor-mashed by the startup paint in
    // every capture; the spaced form below is the control twin.
    PatternSpec {
        id: "codex/trust-title-mashed",
        cli: Cli::Codex,
        class: "dialog.trust",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Doyoutrust"),
    },
    PatternSpec {
        id: "codex/approval-title",
        cli: Cli::Codex,
        class: "dialog.approval",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Would you like to run the following command?"),
    },
    PatternSpec {
        id: "codex/approval-option-proceed",
        cli: Cli::Codex,
        class: "dialog.approval",
        role: Role::Anchored,
        kind: MatcherKind::Literal("1. Yes, proceed"),
    },
    PatternSpec {
        id: "codex/approval-confirm-hint",
        cli: Cli::Codex,
        class: "dialog.approval",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Press enter to confirm or esc to cancel"),
    },
    // Confirmation painted once an approval is granted, in both the
    // arrow-key and number-key variants.
    PatternSpec {
        id: "codex/approved-notice",
        cli: Cli::Codex,
        class: "tool.approved",
        role: Role::Anchored,
        kind: MatcherKind::Literal("You approved codex to run"),
    },
    PatternSpec {
        id: "codex/tool-explored",
        cli: Cli::Codex,
        class: "tool.result",
        role: Role::Anchored,
        kind: MatcherKind::Literal("• Explored"),
    },
    PatternSpec {
        id: "codex/interrupted-notice",
        cli: Cli::Codex,
        class: "session.interrupted",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Conversation interrupted"),
    },
    PatternSpec {
        id: "codex/compacted-notice",
        cli: Cli::Codex,
        class: "compact.result",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Context compacted"),
    },
    // ----- codex: controls --------------------------------------------------
    PatternSpec {
        id: "codex/trust-title-spaced",
        cli: Cli::Codex,
        class: "dialog.trust",
        role: Role::Control,
        kind: MatcherKind::Literal("Do you trust the contents"),
    },
    // ----- codex: ambient ---------------------------------------------------
    PatternSpec {
        id: "codex/banner",
        cli: Cli::Codex,
        class: "chrome.banner",
        role: Role::Ambient,
        kind: MatcherKind::Literal("OpenAI Codex (v"),
    },
    PatternSpec {
        id: "codex/status-esc-hint",
        cli: Cli::Codex,
        class: "status.hint",
        role: Role::Ambient,
        kind: MatcherKind::Literal("esc to interrupt"),
    },
    PatternSpec {
        id: "codex/resume-hint",
        cli: Cli::Codex,
        class: "session.resume",
        role: Role::Ambient,
        kind: MatcherKind::Literal("To continue this session, run codex resume"),
    },
    PatternSpec {
        id: "codex/tool-ran-notice",
        cli: Cli::Codex,
        class: "tool.result",
        role: Role::Ambient,
        kind: MatcherKind::Literal("• Ran"),
    },
    PatternSpec {
        id: "codex/divider",
        cli: Cli::Codex,
        class: "chrome.divider",
        role: Role::Ambient,
        kind: MatcherKind::Regex {
            pattern: "^─{10,}",
            prefilter: Some("──────────"),
        },
    },
    PatternSpec {
        id: "codex/box-border",
        cli: Cli::Codex,
        class: "chrome.box",
        role: Role::Ambient,
        kind: MatcherKind::Literal("│"),
    },
];

/// The screen-set counterpart of [`PATTERNS`]: needles read out of the
/// rendered screens of the same tuned versions (claude 2.1.201, codex
/// 0.145.0), evaluated over deduplicated viewport rows at evaluation
/// points. Where the surface changes what a needle can be, the anchored /
/// control roles swap relative to the stream set; needles the rendering
/// does not affect carry over unchanged under a `screen-` id.
pub const SCREEN_PATTERNS: &[PatternSpec] = &[
    // ----- claude: anchored -------------------------------------------------
    // The permission dialog title as the screen shows it: the virtual
    // terminal places the cursor-addressed words in their cells, so the
    // spaced phrasing is the one that exists here.
    PatternSpec {
        id: "claude/screen-permission-title",
        cli: Cli::Claude,
        class: "dialog.permission",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Do you want to proceed?"),
    },
    // The dialog options. The selection caret moves between rows as the
    // driver arrows through the menu, so the needles anchor on the stable
    // numbered labels and leave the caret to the dialog detector.
    PatternSpec {
        id: "claude/screen-permission-option-yes",
        cli: Cli::Claude,
        class: "dialog.permission",
        role: Role::Anchored,
        kind: MatcherKind::Literal("1. Yes"),
    },
    PatternSpec {
        id: "claude/screen-permission-option-no",
        cli: Cli::Claude,
        class: "dialog.permission",
        role: Role::Anchored,
        kind: MatcherKind::Literal("2. No"),
    },
    // The tool result as the settled screen shows it: the TUI folds a
    // completed shell call into `Ran N shell command(s)` — the stream's
    // `⎿  $ …` expansion is a transient paint that is gone by the next
    // quiet period.
    PatternSpec {
        id: "claude/screen-tool-result-ran",
        cli: Cli::Claude,
        class: "tool.result",
        role: Role::Anchored,
        kind: MatcherKind::Regex {
            pattern: "Ran \\d+ shell command",
            prefilter: Some("Ran "),
        },
    },
    // The Read-tool collapse, added as the timed add-a-pattern trial of
    // the metrics step (deliberately uncovered until then). The screen
    // folds batched same-type calls into one `Read 2 files` line, so the
    // covering pattern fires once for two events — the by-construction
    // per-event shortfall the trial was designated to demonstrate.
    PatternSpec {
        id: "claude/screen-tool-result-read",
        cli: Cli::Claude,
        class: "tool.result",
        role: Role::Anchored,
        kind: MatcherKind::Regex {
            pattern: "Read \\d+ files",
            prefilter: Some("Read "),
        },
    },
    PatternSpec {
        id: "claude/screen-response-bullet",
        cli: Cli::Claude,
        class: "content.response",
        role: Role::Anchored,
        kind: MatcherKind::Literal("⏺"),
    },
    PatternSpec {
        id: "claude/screen-interrupted-notice",
        cli: Cli::Claude,
        class: "session.interrupted",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Interrupted · What should Claude do instead"),
    },
    PatternSpec {
        id: "claude/screen-compact-result",
        cli: Cli::Claude,
        class: "compact.result",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Not enough messages to compact"),
    },
    // ----- claude: controls -------------------------------------------------
    // The stream set's mashed needle, kept as this set's control: cursor
    // artefacts do not exist on a rendered screen, so a firing here would
    // mean the virtual terminal failed at its one job.
    PatternSpec {
        id: "claude/screen-permission-title-mashed",
        cli: Cli::Claude,
        class: "dialog.permission",
        role: Role::Control,
        kind: MatcherKind::Literal("Doyouwanttoproceed?"),
    },
    // The idle notification paints nothing on any surface; the structural
    // miss carries over from the stream set unchanged.
    PatternSpec {
        id: "claude/screen-idle-notice",
        cli: Cli::Claude,
        class: "notice.idle",
        role: Role::Control,
        kind: MatcherKind::Literal("Claude is waiting for your input"),
    },
    // The busy-status hint exists only while the CLI is painting; by every
    // quiet-period boundary it is gone. Its zero hit count against the
    // stream set's steady firing is the direct measure of what
    // evaluation-point sampling cannot see — kept as an instrument, like
    // the mashed titles.
    PatternSpec {
        id: "claude/screen-status-esc-hint",
        cli: Cli::Claude,
        class: "status.hint",
        role: Role::Control,
        kind: MatcherKind::Literal("esc to interrupt"),
    },
    // ----- claude: ambient --------------------------------------------------
    PatternSpec {
        id: "claude/screen-trust-option",
        cli: Cli::Claude,
        class: "dialog.trust",
        role: Role::Ambient,
        kind: MatcherKind::Literal("trust this folder"),
    },
    PatternSpec {
        id: "claude/screen-splash-welcome",
        cli: Cli::Claude,
        class: "chrome.splash",
        role: Role::Ambient,
        kind: MatcherKind::Literal("Welcome back"),
    },
    PatternSpec {
        id: "claude/screen-shortcut-hint",
        cli: Cli::Claude,
        class: "chrome.hint",
        role: Role::Ambient,
        kind: MatcherKind::Literal("? for shortcuts"),
    },
    PatternSpec {
        id: "claude/screen-prompt-echo",
        cli: Cli::Claude,
        class: "chrome.prompt",
        role: Role::Ambient,
        kind: MatcherKind::Literal("❯ "),
    },
    // On the screen the banner is always spaced, but the tolerant stream
    // regex costs nothing to keep identical across the sets.
    PatternSpec {
        id: "claude/screen-version-banner",
        cli: Cli::Claude,
        class: "chrome.banner",
        role: Role::Ambient,
        kind: MatcherKind::Regex {
            pattern: "Claude Code ?v\\d",
            prefilter: Some("Claude Code"),
        },
    },
    PatternSpec {
        id: "claude/screen-divider",
        cli: Cli::Claude,
        class: "chrome.divider",
        role: Role::Ambient,
        kind: MatcherKind::Regex {
            pattern: "^─{10,}",
            prefilter: Some("──────────"),
        },
    },
    PatternSpec {
        id: "claude/screen-box-border",
        cli: Cli::Claude,
        class: "chrome.box",
        role: Role::Ambient,
        kind: MatcherKind::Literal("│"),
    },
    // ----- codex: anchored --------------------------------------------------
    // The workspace-trust prompt, spaced on the screen; its mashed stream
    // twin is this set's control below.
    PatternSpec {
        id: "codex/screen-trust-title",
        cli: Cli::Codex,
        class: "dialog.trust",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Do you trust the contents"),
    },
    PatternSpec {
        id: "codex/screen-approval-title",
        cli: Cli::Codex,
        class: "dialog.approval",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Would you like to run the following command?"),
    },
    PatternSpec {
        id: "codex/screen-approval-option-proceed",
        cli: Cli::Codex,
        class: "dialog.approval",
        role: Role::Anchored,
        kind: MatcherKind::Literal("1. Yes, proceed"),
    },
    PatternSpec {
        id: "codex/screen-approval-confirm-hint",
        cli: Cli::Codex,
        class: "dialog.approval",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Press enter to confirm or esc to cancel"),
    },
    PatternSpec {
        id: "codex/screen-approved-notice",
        cli: Cli::Codex,
        class: "tool.approved",
        role: Role::Anchored,
        kind: MatcherKind::Literal("You approved codex to run"),
    },
    PatternSpec {
        id: "codex/screen-tool-explored",
        cli: Cli::Codex,
        class: "tool.result",
        role: Role::Anchored,
        kind: MatcherKind::Literal("• Explored"),
    },
    PatternSpec {
        id: "codex/screen-interrupted-notice",
        cli: Cli::Codex,
        class: "session.interrupted",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Conversation interrupted"),
    },
    PatternSpec {
        id: "codex/screen-compacted-notice",
        cli: Cli::Codex,
        class: "compact.result",
        role: Role::Anchored,
        kind: MatcherKind::Literal("Context compacted"),
    },
    // ----- codex: controls --------------------------------------------------
    PatternSpec {
        id: "codex/screen-trust-title-mashed",
        cli: Cli::Codex,
        class: "dialog.trust",
        role: Role::Control,
        kind: MatcherKind::Literal("Doyoutrust"),
    },
    // Transient-surface control, same reasoning as the claude twin: the
    // working indicator never survives to a settled screen.
    PatternSpec {
        id: "codex/screen-status-esc-hint",
        cli: Cli::Codex,
        class: "status.hint",
        role: Role::Control,
        kind: MatcherKind::Literal("esc to interrupt"),
    },
    // ----- codex: ambient ---------------------------------------------------
    PatternSpec {
        id: "codex/screen-banner",
        cli: Cli::Codex,
        class: "chrome.banner",
        role: Role::Ambient,
        kind: MatcherKind::Literal("OpenAI Codex (v"),
    },
    PatternSpec {
        id: "codex/screen-resume-hint",
        cli: Cli::Codex,
        class: "session.resume",
        role: Role::Ambient,
        kind: MatcherKind::Literal("To continue this session, run codex resume"),
    },
    PatternSpec {
        id: "codex/screen-tool-ran-notice",
        cli: Cli::Codex,
        class: "tool.result",
        role: Role::Ambient,
        kind: MatcherKind::Literal("• Ran"),
    },
    PatternSpec {
        id: "codex/screen-divider",
        cli: Cli::Codex,
        class: "chrome.divider",
        role: Role::Ambient,
        kind: MatcherKind::Regex {
            pattern: "^─{10,}",
            prefilter: Some("──────────"),
        },
    },
    PatternSpec {
        id: "codex/screen-box-border",
        cli: Cli::Codex,
        class: "chrome.box",
        role: Role::Ambient,
        kind: MatcherKind::Literal("│"),
    },
];

/// Wall-clock ceiling per regex evaluation. Mirrors the planned runtime's
/// safety threshold in value and in disable-for-session semantics, but is
/// detection, not enforcement: elapsed time is checked after an evaluation
/// returns, never preempting one in flight. The regex engine's linear-time
/// guarantee is what rules out a wedged evaluation; this ceiling makes an
/// anomalously slow matcher a reported, disabled finding.
pub const SAFETY_CEILING: Duration = Duration::from_millis(50);

/// A safety-ceiling violation, reported in the replay output.
#[derive(Debug, serde::Serialize)]
pub struct GuardTrip {
    pub pattern_id: &'static str,
    pub elapsed_us: u128,
    pub line_number: u64,
}

enum Compiled {
    Literal,
    Regex(Regex),
}

/// The per-session matcher engine for one CLI: the automaton, the compiled
/// regexes, and the disabled set the safety guard grows.
pub struct CompiledPatterns {
    specs: Vec<&'static PatternSpec>,
    compiled: Vec<Compiled>,
    automaton: AhoCorasick,
    /// Automaton pattern index → spec index.
    needle_owner: Vec<usize>,
    /// Spec indices of regexes with no prefilter — evaluated on every line.
    unfiltered: Vec<usize>,
    disabled: Vec<bool>,
    safety_ceiling: Duration,
}

impl CompiledPatterns {
    /// The stream set (configuration a) for one CLI.
    pub fn for_cli(cli: Cli) -> Result<Self, String> {
        Self::with_safety_ceiling(cli, SAFETY_CEILING)
    }

    /// The screen set (configuration b) for one CLI.
    pub fn for_screen(cli: Cli) -> Result<Self, String> {
        Self::compile(
            SCREEN_PATTERNS
                .iter()
                .filter(|spec| spec.cli == cli)
                .collect(),
            cli,
            SAFETY_CEILING,
        )
    }

    /// Test seam: a zero ceiling makes every regex evaluation trip, which is
    /// how the disable path is exercised without a pathological pattern.
    pub fn with_safety_ceiling(cli: Cli, safety_ceiling: Duration) -> Result<Self, String> {
        Self::compile(
            PATTERNS.iter().filter(|spec| spec.cli == cli).collect(),
            cli,
            safety_ceiling,
        )
    }

    fn compile(
        specs: Vec<&'static PatternSpec>,
        cli: Cli,
        safety_ceiling: Duration,
    ) -> Result<Self, String> {
        let mut needles: Vec<&'static str> = Vec::new();
        let mut needle_owner = Vec::new();
        let mut compiled = Vec::with_capacity(specs.len());
        let mut unfiltered = Vec::new();
        for (index, spec) in specs.iter().enumerate() {
            match &spec.kind {
                MatcherKind::Literal(needle) => {
                    needles.push(needle);
                    needle_owner.push(index);
                    compiled.push(Compiled::Literal);
                }
                MatcherKind::Regex { pattern, prefilter } => {
                    match prefilter {
                        Some(needle) => {
                            needles.push(needle);
                            needle_owner.push(index);
                        }
                        None => unfiltered.push(index),
                    }
                    let regex = Regex::new(pattern).map_err(|err| format!("{}: {err}", spec.id))?;
                    compiled.push(Compiled::Regex(regex));
                }
            }
        }
        let automaton =
            AhoCorasick::new(&needles).map_err(|err| format!("{} automaton: {err}", cli.name()))?;

        let disabled = vec![false; specs.len()];
        Ok(Self {
            specs,
            compiled,
            automaton,
            needle_owner,
            unfiltered,
            disabled,
            safety_ceiling,
        })
    }

    pub fn specs(&self) -> &[&'static PatternSpec] {
        &self.specs
    }

    /// Evaluate one stripped line. Returns the spec indices that fired (each
    /// at most once per line — firing is line-level) and appends any safety
    /// trips to `trips`.
    pub fn evaluate(
        &mut self,
        line: &str,
        line_number: u64,
        trips: &mut Vec<GuardTrip>,
    ) -> Vec<usize> {
        let mut fired = vec![false; self.specs.len()];
        let mut candidates = vec![false; self.specs.len()];

        // Overlapping search so one needle being a substring of another
        // never hides a pattern.
        for hit in self.automaton.find_overlapping_iter(line) {
            candidates[self.needle_owner[hit.pattern().as_usize()]] = true;
        }
        for &index in &self.unfiltered {
            candidates[index] = true;
        }

        for (index, is_candidate) in candidates.iter().enumerate() {
            if !is_candidate || self.disabled[index] {
                continue;
            }
            match &self.compiled[index] {
                // The automaton hit is the literal match.
                Compiled::Literal => fired[index] = true,
                Compiled::Regex(regex) => {
                    let started = Instant::now();
                    let matched = regex.is_match(line);
                    let elapsed = started.elapsed();
                    // `>=` so the test seam's zero ceiling trips even on a
                    // coarse clock that reports a zero elapsed time.
                    if elapsed >= self.safety_ceiling {
                        self.disabled[index] = true;
                        trips.push(GuardTrip {
                            pattern_id: self.specs[index].id,
                            elapsed_us: elapsed.as_micros(),
                            line_number,
                        });
                        continue;
                    }
                    if matched {
                        fired[index] = true;
                    }
                }
            }
        }

        fired
            .iter()
            .enumerate()
            .filter_map(|(index, &hit)| hit.then_some(index))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine(cli: Cli) -> CompiledPatterns {
        CompiledPatterns::for_cli(cli).expect("pattern set compiles")
    }

    fn fired_ids(engine: &mut CompiledPatterns, line: &str) -> Vec<&'static str> {
        let mut trips = Vec::new();
        let fired = engine.evaluate(line, 1, &mut trips);
        assert!(trips.is_empty(), "unexpected guard trips: {trips:?}");
        fired
            .into_iter()
            .map(|index| engine.specs()[index].id)
            .collect()
    }

    /// One observed corpus line per pattern, so the suite exercises every
    /// record in the set: the 100%-coverage discipline in prototype form.
    /// Lines come verbatim from the captures the needles were tuned on.
    const FIXTURE_LINES: &[(&str, &str)] = &[
        ("claude/permission-title-mashed", "Doyouwanttoproceed?"),
        ("claude/permission-option-yes", "❯ 1. Yes"),
        ("claude/permission-option-no", "  2. No"),
        ("claude/tool-command-echo", "⎿  $ echo lifecycle-test"),
        ("claude/tool-read-result", "Read 2 files"),
        ("claude/response-bullet", "⏺Thecommandexecutedsuccessfully."),
        (
            "claude/interrupted-notice",
            "⎿ \u{a0}Interrupted · What should Claude do instead?",
        ),
        (
            "claude/compact-result",
            "⎿  Not enough messages to compact.",
        ),
        ("claude/permission-title-spaced", "Do you want to proceed?"),
        ("claude/idle-notice", "Claude is waiting for your input"),
        ("claude/trust-option", "Yes, I trust this folder✔"),
        (
            "claude/status-esc-hint",
            "esc to interrupt · ← for agents│/…/project│/release-notes for more│",
        ),
        ("claude/splash-welcome", "Welcome back xxxxxxxxxxx!"),
        ("claude/shortcut-hint", "? for shortcuts"),
        ("claude/prompt-echo", "❯ /compact"),
        (
            "claude/version-banner",
            "╭───Claude Codev2.1.201─────────────────────────────╮",
        ),
        (
            "claude/divider",
            "────────────────────────────────────────────────────────────",
        ),
        (
            "claude/box-border",
            "│/…/agent-bridge-interactive-probe-fb89e17b/project│",
        ),
        (
            "codex/trust-title-mashed",
            ">You are in /private/var/T/agent-bridDoyoutrustthecontentsofthisdi",
        ),
        (
            "codex/approval-title",
            "•RunningtouchWould you like to run the following command?Environment:local",
        ),
        ("codex/approval-option-proceed", "› 1. Yes, proceed (y)"),
        (
            "codex/approval-confirm-hint",
            "Press enter to confirm or esc to cancel",
        ),
        (
            "codex/approved-notice",
            "✔ You approved codex to run touch marker.txt this time",
        ),
        ("codex/tool-explored", "• Explored"),
        (
            "codex/interrupted-notice",
            "■ Conversation interrupted - tell the model what to do differently.",
        ),
        ("codex/compacted-notice", "• Context compacted"),
        (
            "codex/trust-title-spaced",
            "Do you trust the contents of this directory?",
        ),
        ("codex/banner", "│ >_ OpenAI Codex (v0.145.0)  │"),
        ("codex/status-esc-hint", "•Working(0s • esc to interrupt)"),
        (
            "codex/resume-hint",
            "To continue this session, run codex resume 019fc92c-2fe1-72d1.",
        ),
        ("codex/tool-ran-notice", "• Ran touch marker.txt"),
        (
            "codex/divider",
            "────────────────────────────────────────────────────────────",
        ),
        (
            "codex/box-border",
            "│ model:     gpt-5.5   /model to change │",
        ),
    ];

    #[test]
    fn every_pattern_has_a_fixture_line_and_fires_on_it() {
        for cli in [Cli::Claude, Cli::Codex] {
            let mut engine = engine(cli);
            for spec in PATTERNS.iter().filter(|spec| spec.cli == cli) {
                let (_, line) = FIXTURE_LINES
                    .iter()
                    .find(|(id, _)| *id == spec.id)
                    .unwrap_or_else(|| panic!("{}: no fixture line in the suite", spec.id));
                let fired = fired_ids(&mut engine, line);
                assert!(
                    fired.contains(&spec.id),
                    "{} did not fire on its fixture line {line:?} (fired: {fired:?})",
                    spec.id
                );
            }
        }
    }

    #[test]
    fn fixture_line_table_carries_no_stale_ids() {
        for (id, _) in FIXTURE_LINES {
            assert!(
                PATTERNS.iter().any(|spec| spec.id == *id),
                "{id}: fixture line for a pattern that no longer exists"
            );
        }
    }

    /// One observed row per screen pattern. Anchored and ambient lines come
    /// verbatim from rendered screens of the tuned versions; the controls,
    /// which by design never fire on a screen, use the stream lines they
    /// were tuned against so the suite still proves each needle compiles
    /// and matches its own surface.
    const SCREEN_FIXTURE_LINES: &[(&str, &str)] = &[
        ("claude/screen-permission-title", " Do you want to proceed?"),
        ("claude/screen-permission-option-yes", "❯ 1. Yes"),
        ("claude/screen-permission-option-no", "  2. No"),
        ("claude/screen-tool-result-ran", "  Ran 1 shell command"),
        ("claude/screen-tool-result-read", " Read 2 files"),
        (
            "claude/screen-response-bullet",
            "⏺ The command executed successfully. The output is:",
        ),
        (
            "claude/screen-interrupted-notice",
            "  ⎿  Interrupted · What should Claude do instead?",
        ),
        (
            "claude/screen-compact-result",
            "  ⎿  Not enough messages to compact.",
        ),
        (
            "claude/screen-permission-title-mashed",
            "Doyouwanttoproceed?",
        ),
        (
            "claude/screen-idle-notice",
            "Claude is waiting for your input",
        ),
        (
            "claude/screen-status-esc-hint",
            "esc to interrupt · ← for agents│/…/project│/release-notes for more│",
        ),
        (
            "claude/screen-trust-option",
            " ❯ 1. Yes, I trust this folder",
        ),
        (
            "claude/screen-splash-welcome",
            "│                 Welcome back xxxxx!                │ started                 │",
        ),
        (
            "claude/screen-shortcut-hint",
            "  ? for shortcuts · ← for agents",
        ),
        (
            "claude/screen-prompt-echo",
            "❯ Run the shell command `echo lifecycle-test` and show me its output.",
        ),
        (
            "claude/screen-version-banner",
            "╭─── Claude Code v2.1.201 ─────────────────────────────────────────────────────╮",
        ),
        (
            "claude/screen-divider",
            "────────────────────────────────────────────────────────────────────────────────",
        ),
        (
            "claude/screen-box-border",
            "│     Haiku 4.5 · Claude Max ·                       │ Fixed the terminal fre… │",
        ),
        (
            "codex/screen-trust-title",
            "  Do you trust the contents of this directory? Working with untrusted contents",
        ),
        (
            "codex/screen-approval-title",
            "  Would you like to run the following command?",
        ),
        (
            "codex/screen-approval-option-proceed",
            "› 1. Yes, proceed (y)",
        ),
        (
            "codex/screen-approval-confirm-hint",
            "  Press enter to confirm or esc to cancel",
        ),
        (
            "codex/screen-approved-notice",
            "✔ You approved codex to run touch marker.txt this time",
        ),
        ("codex/screen-tool-explored", "• Explored"),
        (
            "codex/screen-interrupted-notice",
            "■ Conversation interrupted - tell the model what to do differently. Something",
        ),
        ("codex/screen-compacted-notice", "• Context compacted"),
        (
            "codex/screen-trust-title-mashed",
            ">You are in /private/var/T/agent-bridDoyoutrustthecontentsofthisdi",
        ),
        (
            "codex/screen-status-esc-hint",
            "•Working(0s • esc to interrupt)",
        ),
        (
            "codex/screen-banner",
            "│ >_ OpenAI Codex (v0.145.0)                               │",
        ),
        (
            "codex/screen-resume-hint",
            "To continue this session, run codex resume 019fc929-f3c9-7ee2-b13c-41488851e0be",
        ),
        ("codex/screen-tool-ran-notice", "• Ran touch marker.txt"),
        (
            "codex/screen-divider",
            "────────────────────────────────────────────────────────────────────────────────",
        ),
        (
            "codex/screen-box-border",
            "│ model:     gpt-5.5   /model to change                    │",
        ),
    ];

    #[test]
    fn every_screen_pattern_has_a_fixture_line_and_fires_on_it() {
        for cli in [Cli::Claude, Cli::Codex] {
            let mut engine = CompiledPatterns::for_screen(cli).expect("screen set compiles");
            for spec in SCREEN_PATTERNS.iter().filter(|spec| spec.cli == cli) {
                let (_, line) = SCREEN_FIXTURE_LINES
                    .iter()
                    .find(|(id, _)| *id == spec.id)
                    .unwrap_or_else(|| panic!("{}: no fixture line in the suite", spec.id));
                let fired = fired_ids(&mut engine, line);
                assert!(
                    fired.contains(&spec.id),
                    "{} did not fire on its fixture line {line:?} (fired: {fired:?})",
                    spec.id
                );
            }
        }
    }

    #[test]
    fn screen_fixture_line_table_carries_no_stale_ids() {
        for (id, _) in SCREEN_FIXTURE_LINES {
            assert!(
                SCREEN_PATTERNS.iter().any(|spec| spec.id == *id),
                "{id}: fixture line for a pattern that no longer exists"
            );
        }
    }

    #[test]
    fn screen_near_misses_do_not_fire() {
        let cases: &[(&str, &str)] = &[
            // The mashed stream artefact must not satisfy the spaced needle.
            ("claude/screen-permission-title", "Doyouwanttoproceed?"),
            // The collapsed result line always carries a count.
            ("claude/screen-tool-result-ran", "  Ran shell commands"),
            // The stream's transient expansion is not the settled result.
            ("claude/screen-tool-result-ran", "⎿  $ echo lifecycle-test"),
            // The Read fold always carries a spaced count.
            ("claude/screen-tool-result-read", " Reading1file…"),
        ];
        for (id, line) in cases {
            let mut engine =
                CompiledPatterns::for_screen(Cli::Claude).expect("screen set compiles");
            let fired = fired_ids(&mut engine, line);
            assert!(!fired.contains(id), "{id} fired on near-miss line {line:?}");
        }
    }

    #[test]
    fn matcher_ids_are_unique_across_every_set() {
        // Every report keys rows by id, so a collision would silently merge
        // two matchers' accounting.
        let mut seen = std::collections::BTreeSet::new();
        let ids = PATTERNS
            .iter()
            .map(|spec| spec.id)
            .chain(SCREEN_PATTERNS.iter().map(|spec| spec.id))
            .chain(crate::dialog::DIALOGS.iter().map(|spec| spec.id))
            .chain(
                crate::channel::CHANNEL_CLASSIFIERS
                    .iter()
                    .map(|spec| spec.id),
            );
        for id in ids {
            assert!(seen.insert(id), "{id}: duplicate matcher id");
        }
    }

    #[test]
    fn near_misses_do_not_fire() {
        let cases: &[(&str, &str)] = &[
            // Mashed differently than the recorded paint.
            ("claude/permission-title-mashed", "Doyouwantto proceed?"),
            // Caret on a different option.
            ("claude/permission-option-yes", "❯ 2. No"),
            // Result connector without a shell prompt.
            ("claude/tool-command-echo", "⎿  Read 5 lines"),
            // The transient progress paint is not the settled fold.
            ("claude/tool-read-result", " Reading1file…"),
            // The mashed prompt echo carries no spaced count.
            (
                "claude/tool-read-result",
                "Readthesetwofilesandshowmethefirstline",
            ),
            // The interrupt hint is not the interrupt notice.
            ("claude/interrupted-notice", "esc to interrupt"),
            // Another CLI's approval wording.
            ("codex/approval-title", "Do you want to proceed?"),
            // Resume advice with different phrasing.
            ("codex/resume-hint", "To continue, run codex resume"),
        ];
        for (id, line) in cases {
            let cli = if id.starts_with("claude/") {
                Cli::Claude
            } else {
                Cli::Codex
            };
            let mut engine = engine(cli);
            let fired = fired_ids(&mut engine, line);
            assert!(!fired.contains(id), "{id} fired on near-miss line {line:?}");
        }
    }

    #[test]
    fn prefiltered_regex_does_not_run_without_its_needle() {
        // `claude/version-banner` requires the automaton to see the literal
        // prefilter "Claude Code". A line the regex alone would match — with
        // the needle broken by fragmentation — must not fire: the prefilter
        // gate is part of the execution model under measurement.
        let mut engine = engine(Cli::Claude);
        let fired = fired_ids(&mut engine, "Claude Codev2.1.201");
        assert!(fired.contains(&"claude/version-banner"));
        let fired = fired_ids(&mut engine, "Claude~Code v2.1.201");
        assert!(!fired.contains(&"claude/version-banner"));
    }

    #[test]
    fn patterns_only_fire_for_their_own_cli() {
        let mut engine = engine(Cli::Codex);
        let fired = fired_ids(&mut engine, "Doyouwanttoproceed? ⏺");
        assert!(
            fired.is_empty(),
            "claude needles fired through the codex engine: {fired:?}"
        );
    }

    #[test]
    fn safety_trip_disables_the_matcher_for_the_session() {
        let mut engine = CompiledPatterns::with_safety_ceiling(Cli::Claude, Duration::ZERO)
            .expect("pattern set compiles");
        let mut trips = Vec::new();

        let fired = engine.evaluate("⎿  $ echo tripwire", 1, &mut trips);
        let echo_index = engine
            .specs()
            .iter()
            .position(|spec| spec.id == "claude/tool-command-echo")
            .expect("spec present");
        assert!(
            !fired.contains(&echo_index),
            "tripped matcher must not fire"
        );
        assert!(
            trips
                .iter()
                .any(|trip| trip.pattern_id == "claude/tool-command-echo"),
            "trip recorded: {trips:?}"
        );

        // Second evaluation: disabled, so no further trip is recorded.
        let before = trips.len();
        engine.evaluate("⎿  $ echo again", 2, &mut trips);
        assert_eq!(trips.len(), before, "disabled matcher tripped again");
    }
}
