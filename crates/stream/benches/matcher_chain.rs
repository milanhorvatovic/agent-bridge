//! The evaluation chain held to its budget: P99 of one full
//! `evaluate_line` — automaton pass, every expression it triggers, every
//! stateful matcher — per line, gated.
//!
//! Gated, unlike this crate's other benches, because this number owns a
//! stated budget: fifty microseconds at the ninety-ninth percentile. That
//! is the *performance* budget, a CI property whose breach is a review
//! problem — deliberately three orders of magnitude away from the runtime
//! safety ceiling, with which it shares no constant and no code path. Two
//! verdicts are enforced: the absolute budget, and no more than a twenty
//! percent P99 regression against the committed per-OS baseline when one
//! exists (shared runners are too noisy for a tighter band; the absolute
//! budget still backstops a missing baseline).
//!
//! The measured pack is representative, not minimal: the committed
//! fake-CLI records plus a synthetic set shaped like a real adapter's —
//! mostly prefiltered, a couple of deliberately prefix-less expressions
//! paying the documented every-line cost, and a stateful matcher in the
//! chain. The line corpus is mostly ordinary output, salted with
//! near-misses that hit needles without matching expressions, because
//! near-misses are exactly what the prefilter's cost model has to survive.
//!
//! `--verify-gate` is the gate's own test: a planted pathological pack —
//! dozens of prefix-less expressions over long lines — must *fail* the
//! budget, and this mode exits zero only when it does. A gate that cannot
//! fail is a green light wired to nothing.

#![allow(
    clippy::disallowed_macros,
    reason = "a benchmark's report is its output, and it is run by hand or by the bench lane \
              rather than by the runtime — nothing is reading a protocol on this stdout"
)]

use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agent_bridge_adapter_api::{
    EmitSpec, MatchOutcome, MatcherId, MatcherState, StateLifetime, StatefulMatcher, Template,
    TemplateValue, TextWindow,
};
use agent_bridge_stream::{MatcherEngine, load_dir, parse_pack};

/// The chain budget: P99, nanoseconds, per full evaluation chain per line.
const CHAIN_BUDGET_NS: u64 = 50_000;

/// The regression band over a committed baseline, in percent.
const REGRESSION_LIMIT_PERCENT: u64 = 20;

/// Timed rounds over the corpus; one extra untimed round warms up.
const ROUNDS: u32 = 30;

fn main() {
    let mut out: Option<PathBuf> = None;
    let mut baseline: Option<PathBuf> = None;
    let mut verify_gate = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = args.next().map(|path| workspace_relative(&path)),
            "--baseline" => baseline = args.next().map(|path| workspace_relative(&path)),
            "--verify-gate" => verify_gate = true,
            // `cargo bench` forwards its own harness flags; ignore them.
            _ => {}
        }
    }

    if verify_gate {
        return verify_the_gate_can_fail();
    }

    let engine = representative_engine();
    let corpus = corpus();
    let report = measure(&engine, &corpus);
    println!(
        "matcher_chain: {} lines x {ROUNDS} rounds, p50 {} ns, p99 {} ns, max {} ns, \
         {} regex evaluations",
        corpus.len(),
        report.p50_ns,
        report.p99_ns,
        report.max_ns,
        report.regex_evaluations,
    );

    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create report directory");
        }
        std::fs::write(&path, report.to_json()).expect("write report");
        println!("matcher_chain: report written to {}", path.display());
    }

    let mut failed = false;
    if report.p99_ns > CHAIN_BUDGET_NS {
        println!(
            "matcher_chain: FAIL — p99 {} ns exceeds the {CHAIN_BUDGET_NS} ns chain budget",
            report.p99_ns
        );
        failed = true;
    }
    match baseline {
        Some(path) if path.exists() => {
            let recorded = baseline_p99(&path);
            let ceiling = recorded + recorded * REGRESSION_LIMIT_PERCENT / 100;
            if report.p99_ns > ceiling {
                println!(
                    "matcher_chain: FAIL — p99 {} ns is more than {REGRESSION_LIMIT_PERCENT}% \
                     over the committed baseline ({recorded} ns) from {}",
                    report.p99_ns,
                    path.display()
                );
                failed = true;
            } else {
                println!(
                    "matcher_chain: within {REGRESSION_LIMIT_PERCENT}% of the committed \
                     baseline ({recorded} ns)"
                );
            }
        }
        Some(path) => println!(
            "matcher_chain: no baseline recorded for this OS yet ({}) — absolute budget \
             only; commit a trusted run's report there to arm the regression gate",
            path.display()
        ),
        None => {}
    }
    if failed {
        std::process::exit(1);
    }
}

struct Report {
    p50_ns: u64,
    p99_ns: u64,
    max_ns: u64,
    regex_evaluations: u64,
    lines: usize,
    rounds: u32,
}

impl Report {
    fn to_json(&self) -> String {
        // Hand-assembled so the report shape is visible right here; the
        // reader on the other side is `baseline_p99`, six lines down.
        format!(
            "{{\n  \"bench\": \"matcher_chain\",\n  \"os\": \"{}\",\n  \"arch\": \"{}\",\n  \
             \"lines\": {},\n  \"rounds\": {},\n  \"p50_ns\": {},\n  \"p99_ns\": {},\n  \
             \"max_ns\": {},\n  \"regex_evaluations\": {},\n  \"budget_ns\": {}\n}}\n",
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.lines,
            self.rounds,
            self.p50_ns,
            self.p99_ns,
            self.max_ns,
            self.regex_evaluations,
            CHAIN_BUDGET_NS,
        )
    }
}

fn baseline_p99(path: &Path) -> u64 {
    let text = std::fs::read_to_string(path).expect("read baseline");
    let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse baseline");
    parsed
        .get("p99_ns")
        .and_then(serde_json::Value::as_u64)
        .expect("baseline carries p99_ns")
}

fn measure(engine: &MatcherEngine, corpus: &[String]) -> Report {
    let mut session = engine.new_session();
    // Warm-up: fault in the automaton, the expressions, the allocator.
    for line in corpus {
        black_box(engine.evaluate_line(&mut session, line));
    }

    let before_regexes = engine.stats().regex_evaluations;
    let mut samples: Vec<u64> = Vec::with_capacity(corpus.len() * ROUNDS as usize);
    for _ in 0..ROUNDS {
        for line in corpus {
            let started = Instant::now();
            let events = engine.evaluate_line(&mut session, line);
            let elapsed = started.elapsed();
            samples.push(u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
            black_box(events);
        }
    }
    samples.sort_unstable();
    let percentile = |p: usize| samples[(samples.len() * p / 100).min(samples.len() - 1)];
    Report {
        p50_ns: percentile(50),
        p99_ns: percentile(99),
        max_ns: *samples.last().expect("samples exist"),
        regex_evaluations: engine.stats().regex_evaluations - before_regexes,
        lines: corpus.len(),
        rounds: ROUNDS,
    }
}

/// The committed fake-CLI pack plus a synthetic set at a real adapter's
/// scale: mostly literals and prefixed expressions, two deliberately
/// prefix-less, one stateful matcher.
fn representative_engine() -> MatcherEngine {
    let pack_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../patterns/fake-cli/1.0");
    let committed = load_dir(&pack_dir).expect("the committed pack loads");

    let mut synthetic = String::new();
    // Chrome and status literals, the bulk of a real stream set.
    for (index, needle) in [
        "esc to interrupt",
        "Thinking",
        "ctrl+c to stop",
        "tokens remaining",
        "auto-accept edits",
        "context left until",
        "ide disconnected",
        "MCP server",
        "plan mode on",
        "bypassing permissions",
        "waiting for approval",
        "session resumed",
    ]
    .iter()
    .enumerate()
    {
        synthetic.push_str(&format!(
            "- name: literal_{index}\n  matcher: {{ type: substring, source: '{needle}' }}\n  \
             emits:\n    event_type: tool.call_started\n    fields: \
             {{ call_id: '{{{{ uuid4() }}}}', tool: chrome }}\n",
        ));
    }
    // Prefixed expressions: the shape the prefilter exists for.
    for (index, source) in [
        r"^\{\{tool_done: (?P<code>[0-9]+)\}\}$",
        r"^\{\{tool_err: (?P<reason>.+)\}\}$",
        r"^error\[E(?P<code>[0-9]{4})\]",
        r"^warning: (?P<message>.+)$",
        r"^Compiling (?P<krate>[a-z0-9_-]+) v",
        r"^\$ (?P<command>.+)$",
        r"^Do you want to (?P<verb>run|allow) (?P<what>.+)\?",
        r"^Wrote (?P<count>[0-9]+) lines to (?P<path>.+)$",
        r"^Reading (?P<path>[^ ]+)…",
        r"^● (?P<tool>[A-Z][a-z]+)\((?P<argument>.+)\)$",
    ]
    .iter()
    .enumerate()
    {
        synthetic.push_str(&format!(
            "- name: prefixed_{index}\n  matcher:\n    type: regex\n    source: '{source}'\n    \
             anchor: line_start\n  emits:\n    event_type: tool.call_started\n    fields: \
             {{ call_id: '{{{{ uuid4() }}}}', tool: status }}\n",
        ));
    }
    // Prefix-less: the documented every-line cost, present so the budget
    // is measured over the pack shape adapters are allowed to write.
    for (index, source) in [r"(ready|done|complete)\s*$", r"(?i)(y/n|yes/no)[\]\)]\s*$"]
        .iter()
        .enumerate()
    {
        synthetic.push_str(&format!(
            "- name: unfiltered_{index}\n  matcher: {{ type: regex, source: '{source}' }}\n  \
             emits:\n    event_type: tool.call_started\n    fields: \
             {{ call_id: '{{{{ uuid4() }}}}', tool: everyline }}\n",
        ));
    }

    MatcherEngine::builder()
        // The runtime safety ceiling is the other budget, and it stays out
        // of this one: armed here, a single scheduler stall on a shared
        // runner would disable a matcher mid-run and every later sample
        // would measure a smaller chain — a deflated P99 that could wave
        // a real regression through. Disarmed, every sample measures the
        // whole chain.
        .eval_timeout(Duration::from_secs(3600))
        .records(committed)
        .records(parse_pack("synthetic", &synthetic).expect("synthetic pack parses"))
        .stateful(
            Box::new(FrameTracker {
                id: MatcherId::new("bench_frame"),
            }),
            EmitSpec {
                event_type: "tool.result".to_string(),
                fields: [
                    ("call_id".to_string(), TemplateValue::One(Template::Uuid4)),
                    (
                        "content".to_string(),
                        TemplateValue::One(Template::Group("frame".to_string())),
                    ),
                ]
                .into(),
            },
        )
        .compile()
        .expect("the representative set compiles")
}

/// A realistic small stateful matcher: remembers an opener, fires on the
/// closer.
struct FrameTracker {
    id: MatcherId,
}

impl StatefulMatcher for FrameTracker {
    fn id(&self) -> &MatcherId {
        &self.id
    }

    fn state_lifetime(&self) -> StateLifetime {
        StateLifetime::PerSession
    }

    fn evaluate(&self, window: &TextWindow<'_>, state: &mut MatcherState) -> Option<MatchOutcome> {
        let pending = state.get_or_insert_with(String::new);
        if let Some(name) = window.line().strip_prefix("::begin ") {
            *pending = name.to_string();
            return None;
        }
        if window.line() == "::end" && !pending.is_empty() {
            let outcome = MatchOutcome::with_captures(
                agent_bridge_adapter_api::Captures::new().with("frame", pending.clone()),
            );
            pending.clear();
            return Some(outcome);
        }
        None
    }
}

/// Deterministic corpus: mostly ordinary output, salted with near-misses
/// and a few true matches — no wall clock, no randomness source, so every
/// run measures the same work.
fn corpus() -> Vec<String> {
    let words = [
        "the", "stream", "layer", "adds", "meaning", "to", "bytes", "while", "keeping", "dispatch",
        "cheap", "and", "every", "session", "isolated", "under", "load", "reading", "terminal",
        "output", "without", "guessing",
    ];
    let mut lines = Vec::with_capacity(2000);
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        // xorshift*: deterministic, seedable, and not a dependency.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        state
    };
    for index in 0..2000u64 {
        let roll = next() % 100;
        let mut line = String::new();
        match roll {
            // Ordinary output, 12 to 20 words.
            0..=69 => {
                let count = 12 + next() % 9;
                for _ in 0..count {
                    line.push_str(words[(next() % words.len() as u64) as usize]);
                    line.push(' ');
                }
            }
            // Near-misses: a needle occurs, its expression declines — the
            // case the prefilter's cost model has to survive.
            70..=84 => {
                line.push_str("narration mentioning ");
                line.push_str(match next() % 4 {
                    0 => "{{tool: [not the shape]",
                    1 => "Allow me to elaborate on this at length",
                    2 => "warning: shaped text mid-line",
                    _ => "esc to interrupt among other keys",
                });
                line.push_str(" and continuing on");
            }
            // Long-ish token lines.
            85..=94 => {
                let count = 30 + next() % 20;
                for _ in 0..count {
                    line.push_str(words[(next() % words.len() as u64) as usize]);
                    line.push(' ');
                }
            }
            // True matches, rare, as they are live.
            95..=97 => line.push_str("{{tool: bash, cmd: git status}}"),
            98 => line.push_str("Allow filesystem write? [y/N]"),
            _ => line.push_str(if index % 2 == 0 {
                "::begin frame"
            } else {
                "::end"
            }),
        }
        lines.push(line);
    }
    lines
}

/// The gate's own test: a planted pathological pack must fail the budget.
fn verify_the_gate_can_fail() {
    // Dozens of prefix-less, literal-free expressions over long lines.
    // Class-only patterns deny the expression engine every literal
    // shortcut it has — no prefilter needle for this engine, no internal
    // substring search for that one — so each of the sixty-four scans the
    // whole line, every line. That product is the cost model the budget
    // exists to reject.
    let mut pathological = String::new();
    for index in 0..64 {
        let spread = 1 + index % 7;
        pathological.push_str(&format!(
            "- name: pathological_{index}\n  matcher:\n    type: regex\n    source: \
             '[a-z]+[0-9][a-z]{{{spread},}}[0-9][a-z]+'\n  \
             emits:\n    event_type: tool.call_started\n    fields: \
             {{ call_id: '{{{{ uuid4() }}}}', tool: pathological }}\n",
        ));
    }
    let engine = MatcherEngine::builder()
        // Disarmed for the same reason as the representative engine — and
        // doubly here, where every evaluation is deliberately slow enough
        // that the runtime ceiling would otherwise disable the whole pack
        // after one line each.
        .eval_timeout(Duration::from_secs(3600))
        .records(parse_pack("pathological", &pathological).expect("parses"))
        .compile()
        .expect("compiles");

    let long_line = "lorem noise filler salad ".repeat(320);
    let corpus: Vec<String> = (0..200).map(|_| long_line.clone()).collect();
    let report = measure(&engine, &corpus);
    println!(
        "matcher_chain: --verify-gate measured p99 {} ns against the {CHAIN_BUDGET_NS} ns budget",
        report.p99_ns
    );
    if report.p99_ns > CHAIN_BUDGET_NS {
        println!("matcher_chain: the gate fires on a pathological pack — gate verified");
    } else {
        println!(
            "matcher_chain: FAIL — the planted pathological pack passed the budget; \
             the gate would not catch a real regression"
        );
        std::process::exit(1);
    }
}

/// Paths from the bench lane arrive workspace-relative; the bench itself
/// runs from the package directory. Anchor on the manifest so both
/// invocations mean the same file.
fn workspace_relative(path: &str) -> PathBuf {
    let candidate = PathBuf::from(path);
    if candidate.is_absolute() {
        candidate
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(candidate)
    }
}
