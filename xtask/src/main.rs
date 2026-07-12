//! Dev-task runner — the single source of truth for the check sequence that
//! both local development and CI run. `cargo xtask ci` runs exactly what the
//! CI workflow runs, so "green locally" and "green in CI" cannot diverge.
//!
//! Deliberately dependency-free (std only): a contributor needs nothing beyond
//! the pinned toolchain and `git`, both of which every dev machine and CI
//! runner already have. Cross-platform (no shell scripts) so Windows, macOS,
//! and Linux run the identical logic.
//!
//! Usage:
//!   cargo xtask ci           # format check + clippy + build + test + probes + drift-gate
//!   cargo xtask probe        # the deterministic probes only — what the container CI lane runs
//!   cargo xtask live-probe   # probes that spawn a real CLI; needs credentials, never on the PR tier
//!   cargo xtask drift-gate   # the reserved-pattern gate only
//!   cargo xtask capture-campaign --cli <name> --bin <path> --version-label <label>
//!                            --install <text> [--model <name>] [--dry-run]
//!                            # record every capture scenario for one CLI at one pinned
//!                            # version, both terminal sizes, into tests/corpus/ — then
//!                            # scrub and hold the corpus to its size budget. Maintainer-run,
//!                            # never on any CI tier: the claude campaign spends session quota.
//!                            # One sitting per CLI = one invocation per pinned version.

use std::path::{Path, PathBuf};
use std::process::{Command, exit};

/// The check sequence, in order. Every entry is a `cargo` subcommand; the
/// gate ties them to what CI enforces so the two stay identical.
const STEPS: &[(&str, &[&str])] = &[
    ("format", &["fmt", "--all", "--", "--check"]),
    (
        "clippy",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    ),
    ("build", &["build", "--workspace"]),
    ("test", &["test", "--workspace"]),
];

/// The probe binaries *run* (not just build) — a PTY that cannot be
/// allocated on a platform is exactly what they exist to catch, and that
/// only shows at runtime. Split out of STEPS so the container CI lane can
/// run just this slice as `cargo xtask probe`.
///
/// Every probe here is deterministic and credential-free: the interactive
/// lane drives a stand-in fixture, not a real CLI. Probes that need a real
/// CLI and credentials live in `LIVE_PROBE_STEPS`.
const PROBE_STEPS: &[(&str, &[&str])] = &[
    (
        "pty-probe",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-pty-probe",
            "--bin",
            "pty-probe",
        ],
    ),
    (
        "pty-probe (env defaults)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-pty-probe",
            "--bin",
            "pty-probe",
            "--",
            "--check-env",
        ],
    ),
    // `cargo run --bin X` builds only X, but the interactive probe spawns
    // its stand-in fixture as a sibling binary. Build both first: the
    // `cargo xtask probe` lane runs only these PROBE_STEPS, not the
    // workspace-wide `build` from STEPS, so without this step the fixture
    // would be missing and the probe would fail to spawn it.
    (
        "interactive-probe (build both bins)",
        &[
            "build",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bins",
        ],
    ),
    (
        "interactive-probe (stand-in fixture)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bin",
            "interactive-probe",
            "--",
            "standin",
        ],
    ),
    // The signal, resize, UTF-8, and cleanup probes spawn their fixtures
    // (probe-child, resize-child, utf8-child, tree-child) from a sibling
    // package, which `cargo run --bin <probe>` alone would not build.
    (
        "build the probe fixtures (probe-child, resize-child, utf8-child, tree-child)",
        &["build", "--quiet", "--package", "agent-bridge-probe-child"],
    ),
    // Both interrupt-delivery scenarios: to a raw-mode child (the mode
    // interactive CLIs run in) 0x03 is data and a process-group SIGINT is a
    // separate, distinct path; to a cooked-mode child the terminal itself
    // turns the same byte into the interrupt.
    (
        "signal-probe (raw-mode child)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-signal-probe",
            "--bin",
            "signal-probe",
            "--",
            "raw",
        ],
    ),
    (
        "signal-probe (cooked-mode child)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-signal-probe",
            "--bin",
            "signal-probe",
            "--",
            "cooked",
        ],
    ),
    // Both resize scenarios: the steady grow-and-shrink pair proves resize
    // propagation is observed and repeatable with the dimension env pinned
    // at spawn-time values; the early scenario characterizes the
    // resize-before-ready launch race as a typed, recorded outcome.
    (
        "resize-probe (steady grow/shrink pair)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-resize-probe",
            "--bin",
            "resize-probe",
            "--",
            "steady",
        ],
    ),
    (
        "resize-probe (early resize before ready)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-resize-probe",
            "--bin",
            "resize-probe",
            "--",
            "early",
        ],
    ),
    // Both UTF-8 scenarios: the sweep respawns the fixture once per
    // read-buffer size (down to a single byte) and holds the reassembled
    // multi-byte corpus to the fixture's checksum trailer; the invalid lane
    // proves bytes that can never be UTF-8 surface as exactly-located spans
    // — or, on Windows, as a recorded ConPTY substitution — never as
    // silence.
    (
        "utf8-probe (read-buffer sweep over the multi-byte corpus)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-utf8-probe",
            "--bin",
            "utf8-probe",
            "--",
            "sweep",
        ],
    ),
    (
        "utf8-probe (invalid bytes between valid sequences)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-utf8-probe",
            "--bin",
            "utf8-probe",
            "--",
            "invalid",
        ],
    ),
    // Both cleanup scenarios: the clean exit proves an ended session leaves
    // nothing behind — process group / job object empty, PTY released,
    // fd/handle counts back to baseline, no ConPTY console host — with the
    // setsid escapee detected as outside the group rather than pretended
    // away; the terminate scenario proves the polite-then-forced escalation
    // empties the tree even when the fixture ignores the polite signal.
    (
        "cleanup-probe (clean exit of a process tree)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-cleanup-probe",
            "--bin",
            "cleanup-probe",
            "--",
            "clean",
        ],
    ),
    (
        "cleanup-probe (terminate escalation past a stubborn tree)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-cleanup-probe",
            "--bin",
            "cleanup-probe",
            "--",
            "terminate",
        ],
    ),
];

/// Probes that spawn a **real** interactive CLI. They need the CLI on PATH
/// and working credentials, so they never run on the PR tier: `cargo xtask
/// live-probe` is invoked only by the opt-in live CI lane and by a
/// maintainer locally.
///
/// Both lanes pin the cheapest model: they assert event shapes and
/// sequences, never model output, so a larger model would buy nothing and
/// cost quota. The four-point lane runs here — on POSIX it is the baseline
/// the Windows-client results are compared against, and running it every
/// time the label goes on keeps that baseline from going stale.
const LIVE_PROBE_STEPS: &[(&str, &[&str])] = &[
    (
        "interactive-probe (real CLI)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bin",
            "interactive-probe",
            "--",
            "probe",
            "--model",
            "haiku",
        ],
    ),
    (
        "interactive-probe (four-point hook-channel verification)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bin",
            "interactive-probe",
            "--",
            "fourpoint",
            "--model",
            "haiku",
        ],
    ),
    // The clean-shutdown observation the deterministic cleanup lanes cannot
    // make: a typed /exit into the real CLI, asserting SessionEnd fires AND
    // the child process actually terminates AND the PTY tears down, with
    // the SessionEnd-to-exit interval reported.
    (
        "interactive-probe (real-CLI /exit cleanup)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-interactive-probe",
            "--bin",
            "interactive-probe",
            "--",
            "cleanup",
            "--model",
            "haiku",
        ],
    ),
];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let task = args.first().map(String::as_str).unwrap_or_default();
    let passed = match task {
        "ci" => run_ci(),
        "probe" => run_steps(PROBE_STEPS),
        "live-probe" => run_live_probes(),
        "drift-gate" => drift_gate(),
        "capture-campaign" => run_capture_campaign(&args[1..]),
        other => {
            eprintln!(
                "unknown xtask '{other}'. usage: cargo xtask <ci|probe|live-probe|drift-gate|capture-campaign>"
            );
            exit(2);
        }
    };
    if !passed {
        exit(1);
    }
}

/// Run every step and the drift gate, reporting all failures rather than
/// stopping at the first, so one run surfaces every problem.
fn run_ci() -> bool {
    let checks = run_steps(STEPS);
    let probes = run_steps(PROBE_STEPS);
    // Run the gate regardless of earlier failures so one run reports everything.
    let gate = drift_gate();
    checks && probes && gate
}

/// The live-CLI probes. Credential presence is logged — never the value —
/// and its absence fails loudly here rather than surfacing as a confusing
/// authentication error inside the spawned CLI.
fn run_live_probes() -> bool {
    let credential = ["ANTHROPIC_API_KEY", "CLAUDE_CONFIG_DIR"]
        .into_iter()
        .find(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()));
    match credential {
        Some(name) => eprintln!("── xtask: live-probe: credential present ({name} is set) ──"),
        None => {
            eprintln!(
                "xtask: live-probe: credential absent — set ANTHROPIC_API_KEY (CI) or \
                 CLAUDE_CONFIG_DIR (local). The live lane cannot run without one."
            );
            return false;
        }
    }
    // The stand-in fixture's build step is not needed here, but the real-CLI
    // probe binary is: `cargo run` builds it.
    let steps = run_steps(LIVE_PROBE_STEPS);
    // Run the capture smoke even if a probe failed: one live run should
    // report everything it can, and the smoke's one extra session is the
    // cheapest part of the lane.
    let smoke = live_capture_smoke();
    steps && smoke
}

/// One scripted capture session against the real CLI — the path the capture
/// campaign depends on, exercised whenever the live tier runs so it cannot
/// rot between capture sittings. Cheapest cell of the campaign matrix
/// (token streaming, default size, haiku-class model), recorded into
/// `target/` where nothing will mistake it for a committed fixture.
fn live_capture_smoke() -> bool {
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        eprintln!("xtask: live-probe: `git rev-parse --show-toplevel` failed");
        return false;
    };
    let root = PathBuf::from(top.trim_end());
    let script = root.join("tests/capture-scenarios/claude/token-streaming.record.json");
    let out = root.join("target/live-capture-smoke");
    let args: Vec<String> = [
        "run",
        "--quiet",
        "--package",
        "agent-bridge-interactive-probe",
        "--bin",
        "interactive-probe",
        "--",
        "record",
        "--model",
        "haiku",
        "--expect-cli",
        "claude",
    ]
    .map(str::to_string)
    .into_iter()
    .chain([
        "--script".to_string(),
        script.to_string_lossy().into_owned(),
        "--out".to_string(),
        out.to_string_lossy().into_owned(),
    ])
    .collect();
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    cargo("interactive-probe (scripted capture smoke)", &refs)
}

/// The two terminal sizes every capture scenario is recorded at: the
/// runtime's default, and a larger size that makes a TUI paint (wrap,
/// reflow, redraw) differently. Classification must hold at both.
const CAPTURE_DIMS: [(u16, u16); 2] = [(80, 24), (120, 40)];

/// The per-adapter corpus budget, in bytes. The campaign reports against it
/// after every run so an over-budget corpus is discovered at capture time —
/// when trimming is a re-record away — not at review time.
const CORPUS_BUDGET_BYTES: u64 = 1_048_576;

/// One capture sitting: every `tests/capture-scenarios/<cli>/*.record.json`
/// scenario, at both capture sizes, recorded through the interactive
/// probe's `record` lane into `tests/corpus/<cli>/<version-label>/`. Stops
/// at the first failed scenario — for the claude CLI every session costs
/// quota, and a systematically broken setup must not burn the rest of the
/// matrix discovering itself. After recording, the fixtures are scrubbed
/// (the local username is masked, same-length so the timing offsets stay
/// valid; a credential hit aborts) and the whole adapter corpus is sized
/// against its budget.
fn run_capture_campaign(args: &[String]) -> bool {
    let Some(campaign) = CampaignArgs::parse(args) else {
        eprintln!(
            "usage: cargo xtask capture-campaign --cli <name> --bin <path> \
             --version-label <label> --install <text> [--model <name>] [--dry-run]"
        );
        return false;
    };
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        eprintln!("xtask: capture-campaign: `git rev-parse --show-toplevel` failed");
        return false;
    };
    let root = PathBuf::from(top.trim_end());

    let scenarios_dir = root.join("tests/capture-scenarios").join(&campaign.cli);
    let mut scripts: Vec<PathBuf> = match std::fs::read_dir(&scenarios_dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                // Real regular files only — `is_file` would follow a
                // symlink, and the campaign's inputs stay confined to the
                // scenario tree the same way its scrub walk does. A
                // directory or any symlink named *.record.json would
                // otherwise be handed to the record lane as a script and
                // fail there, confusingly.
                std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".record.json"))
            })
            .collect(),
        Err(err) => {
            eprintln!(
                "xtask: capture-campaign: no scenario directory at {} ({err}) — write the \
                 <name>.record.json scripts before running the campaign",
                scenarios_dir.display()
            );
            return false;
        }
    };
    scripts.sort();
    if scripts.is_empty() {
        eprintln!(
            "xtask: capture-campaign: {} holds no *.record.json scenarios",
            scenarios_dir.display()
        );
        return false;
    }

    let version_dir = root
        .join("tests/corpus")
        .join(&campaign.cli)
        .join(&campaign.version_label);
    eprintln!(
        "── xtask: capture-campaign: {} scenarios × {} sizes for cli={} version={} into {} ──",
        scripts.len(),
        CAPTURE_DIMS.len(),
        campaign.cli,
        campaign.version_label,
        version_dir.display(),
    );
    for script in &scripts {
        for (cols, rows) in CAPTURE_DIMS {
            eprintln!(
                "  {} @ {cols}x{rows} -> {}",
                script.file_name().unwrap_or_default().to_string_lossy(),
                version_dir
                    .join(fixture_dir_name(script, cols, rows))
                    .display(),
            );
        }
    }
    if campaign.dry_run {
        eprintln!("capture-campaign: dry run — nothing recorded.");
        return true;
    }

    for script in &scripts {
        for (cols, rows) in CAPTURE_DIMS {
            let out = version_dir.join(fixture_dir_name(script, cols, rows));
            let mut record_args: Vec<String> = [
                "run",
                "--quiet",
                "--package",
                "agent-bridge-interactive-probe",
                "--bin",
                "interactive-probe",
                "--",
                "record",
            ]
            .map(str::to_string)
            .to_vec();
            record_args.extend([
                "--script".to_string(),
                script.to_string_lossy().into_owned(),
                "--out".to_string(),
                out.to_string_lossy().into_owned(),
                "--cols".to_string(),
                cols.to_string(),
                "--rows".to_string(),
                rows.to_string(),
                "--cli-bin".to_string(),
                campaign.bin.clone(),
            ]);
            record_args.extend(["--install".to_string(), campaign.install.clone()]);
            // A script misfiled under this CLI's scenario directory fails
            // inside the record lane with both names stated.
            record_args.extend(["--expect-cli".to_string(), campaign.cli.clone()]);
            // The claude profile reports its own --version; every other CLI
            // gets the campaign's label stamped into its manifests.
            if campaign.cli != "claude" {
                record_args.extend(["--cli-version".to_string(), campaign.version_label.clone()]);
            }
            if let Some(model) = &campaign.model {
                record_args.extend(["--model".to_string(), model.clone()]);
            }
            let name = format!(
                "record {} @ {cols}x{rows}",
                script.file_name().unwrap_or_default().to_string_lossy()
            );
            let refs: Vec<&str> = record_args.iter().map(String::as_str).collect();
            if !cargo(&name, &refs) {
                eprintln!(
                    "capture-campaign: stopping at the first failure — the remaining matrix \
                     would spend sessions against the same broken setup"
                );
                return false;
            }
        }
    }

    if !scrub_fixtures(&version_dir) {
        return false;
    }
    report_corpus_budget(&root.join("tests/corpus").join(&campaign.cli))
}

struct CampaignArgs {
    cli: String,
    bin: String,
    version_label: String,
    /// How the pinned binary was obtained (e.g. `npm
    /// @anthropic-ai/claude-code@2.1.201`). Required: the version-drift
    /// measurement needs "which release, obtained how" in every manifest,
    /// and the campaign is the one place that knows it.
    install: String,
    model: Option<String>,
    dry_run: bool,
}

impl CampaignArgs {
    fn parse(args: &[String]) -> Option<Self> {
        let mut cli = None;
        let mut bin = None;
        let mut version_label = None;
        let mut install = None;
        let mut model = None;
        let mut dry_run = false;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--cli" => cli = Some(iter.next()?.clone()),
                "--bin" => bin = Some(iter.next()?.clone()),
                "--version-label" => version_label = Some(iter.next()?.clone()),
                "--install" => install = Some(iter.next()?.clone()),
                "--model" => model = Some(iter.next()?.clone()),
                "--dry-run" => dry_run = true,
                other => {
                    eprintln!("xtask: capture-campaign: unknown option {other}");
                    return None;
                }
            }
        }
        let (cli, bin, version_label, install) = (cli?, bin?, version_label?, install?);
        // Both become path components under tests/corpus/ — hold them to
        // characters that stay put in directory names on every OS, and
        // anchor the first and last character to an alphanumeric so `.`,
        // `..`, and leading/trailing separators cannot slip through as a
        // traversal or an awkward dot-directory. `--cli` is stricter: it
        // must match a record script's kebab-case `cli` field, where a dot
        // is a parse error — accepting one here would only defer the
        // mismatch to every scenario in the sitting.
        for (name, value, charset, dot_ok) in [
            ("--cli", &cli, "[a-z0-9-]", false),
            ("--version-label", &version_label, "[a-z0-9.-]", true),
        ] {
            let body_clean = value.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || (dot_ok && c == '.')
            });
            let anchored = value
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
                && value
                    .chars()
                    .last()
                    .is_some_and(|c| c.is_ascii_alphanumeric());
            if !(body_clean && anchored) {
                eprintln!(
                    "xtask: capture-campaign: {name} must be {charset} and start/end alphanumeric, got \"{value}\""
                );
                return None;
            }
        }
        Some(Self {
            cli,
            bin,
            version_label,
            install,
            model,
            dry_run,
        })
    }
}

/// `token-streaming.record.json` at 80×24 → `token-streaming-80x24`.
fn fixture_dir_name(script: &Path, cols: u16, rows: u16) -> String {
    let stem = script
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".record.json"))
        .unwrap_or("scenario");
    format!("{stem}-{cols}x{rows}")
}

/// Scrub committed fixtures of machine-local identity. The local username
/// (HOME's last component) is masked with a same-length run of `x`, so
/// byte offsets recorded in the timing sidecars stay valid; a fixture that
/// contains the ANTHROPIC_API_KEY value is a leak and aborts the campaign
/// outright — masking a credential and committing anyway would hide the
/// evidence that the capture setup is wrong.
fn scrub_fixtures(dir: &Path) -> bool {
    eprintln!("── xtask: capture-campaign: scrub {} ──", dir.display());
    // A maskable username needs some length and at least one letter: the
    // mask is a raw byte replacement across every artifact, and an
    // all-digit name (say "123") would also match inside the numeric
    // fields of the NDJSON sidecars and corrupt them.
    let username =
        maskable_username(std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")));
    if username.is_none() {
        eprintln!(
            "capture-campaign: no safely maskable username (HOME unset, too short, or only \
             digits and punctuation — such a needle would also hit numeric NDJSON fields and \
             ordinary prose); skipping the mask, review the fixtures by hand"
        );
    }
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .ok()
        .filter(|key| key.len() >= 8);

    let mut files = Vec::new();
    if let Err(err) = collect_files(dir, &mut files) {
        eprintln!(
            "capture-campaign: ABORT — the scrub could not walk the fixture tree, so it cannot \
             claim the fixtures are clean: {err}"
        );
        return false;
    }
    // Deterministic order, so which file aborts a run (a credential hit)
    // and the order of diagnostics do not vary by platform or filesystem.
    files.sort();
    let mut masked_files = 0usize;
    for path in &files {
        let Ok(mut bytes) = std::fs::read(path) else {
            eprintln!("capture-campaign: unreadable fixture {}", path.display());
            return false;
        };
        if let Some(key) = &api_key
            && find_subsequence(&bytes, key.as_bytes()).is_some()
        {
            eprintln!(
                "capture-campaign: ABORT — {} contains the ANTHROPIC_API_KEY value. The capture \
                 setup leaked a credential into a fixture; fix that and re-record.",
                path.display()
            );
            return false;
        }
        if let Some(needle) = username.as_deref() {
            let mut changed = false;
            let mut from = 0usize;
            while let Some(at) = find_subsequence(&bytes[from..], needle) {
                let at = from + at;
                bytes[at..at + needle.len()].fill(b'x');
                from = at + needle.len();
                changed = true;
            }
            if changed {
                if std::fs::write(path, &bytes).is_err() {
                    eprintln!("capture-campaign: rewriting {} failed", path.display());
                    return false;
                }
                masked_files += 1;
            }
        }
    }
    eprintln!(
        "capture-campaign: scrubbed {} files ({masked_files} carried the username and were masked).",
        files.len()
    );
    true
}

/// Any single fixture file above this is flagged for a truncation review —
/// the per-file complement to the per-adapter total.
const FIXTURE_REVIEW_BYTES: u64 = 100 * 1024;

/// Size the whole per-adapter corpus against its budget, and name every
/// file large enough to need a truncation review. Over budget is a loud
/// warning, not a hard failure: the maintainer trimming scenarios needs the
/// fixtures on disk to decide what to cut.
fn report_corpus_budget(adapter_dir: &Path) -> bool {
    let mut files = Vec::new();
    if let Err(err) = collect_files(adapter_dir, &mut files) {
        eprintln!(
            "capture-campaign: the corpus could not be walked, so the budget report would \
             under-count: {err}"
        );
        return false;
    }
    // Deterministic order, so the large-file warnings read the same on
    // every platform and diff cleanly between sittings.
    files.sort();
    let mut total = 0u64;
    for path in &files {
        // `collect_files` only returns real regular files, so this cannot
        // follow a link — symlink_metadata keeps the walk's stance uniform.
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            continue;
        };
        total += meta.len();
        if meta.len() > FIXTURE_REVIEW_BYTES {
            eprintln!(
                "capture-campaign: {} is {} bytes (> {FIXTURE_REVIEW_BYTES}) — review for truncation",
                path.display(),
                meta.len(),
            );
        }
    }
    let verdict = if total <= CORPUS_BUDGET_BYTES {
        "within budget"
    } else {
        "OVER BUDGET — trim before committing"
    };
    eprintln!(
        "── xtask: capture-campaign: {} holds {total} bytes across {} files ({verdict}, budget {CORPUS_BUDGET_BYTES}) ──",
        adapter_dir.display(),
        files.len(),
    );
    true
}

/// Regular files under `dir`, recursively — never through a symlink. The
/// scrub rewrites what this returns in place and the budget sums its
/// sizes; following a symlink would let either walk out of the fixture
/// tree it is supposed to be confined to. Anything the walk cannot soundly
/// cover is an error, not a skip: an unreadable entry, a symlink (whose
/// target text can itself carry a machine-local path), or a special file
/// silently left out would let the scrub claim fixtures are clean without
/// having covered them, and the budget under-count what is committed.
fn collect_files(dir: &Path, into: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|err| format!("listing {} failed: {err}", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("reading an entry of {} failed: {err}", dir.display()))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|err| format!("inspecting {} failed: {err}", path.display()))?;
        if meta.is_dir() {
            collect_files(&path, into)?;
        } else if meta.is_file() {
            into.push(path);
        } else {
            return Err(format!(
                "{} is neither a real directory nor a real regular file (a symlink or special \
                 entry) — the walk refuses entries it cannot soundly cover",
                path.display()
            ));
        }
    }
    Ok(())
}

/// The username worth masking out of committed fixtures: the home path's
/// final component, when long and lettered enough to be a safe raw-byte
/// needle. Trailing separators on the home path are irrelevant — path
/// parsing skips empty components on every platform — and a home that is
/// just a root has no component at all, which the caller turns into a loud
/// warning rather than a silent skip.
///
/// The needle is the component's *bytes*, not a String: on Unix a home path
/// need not be UTF-8, and a needle lossily converted through replacement
/// characters could never match the raw bytes a fixture actually carries —
/// it would claim a mask it did not perform. Windows environment values are
/// Unicode in practice; the lossy fallback there is exact for real inputs.
///
/// "Safe" means the needle contains something name-like: an ASCII letter,
/// or any non-ASCII byte — which is what a username in any non-Latin
/// script is made of, so those mask too. Needles of only ASCII digits and
/// punctuation are refused: they collide with numeric NDJSON fields,
/// version strings, and ordinary prose.
fn maskable_username(home: Option<std::ffi::OsString>) -> Option<Vec<u8>> {
    let name = PathBuf::from(home?).file_name()?.to_os_string();
    #[cfg(unix)]
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(name.as_os_str()).to_vec();
    #[cfg(not(unix))]
    let bytes = name.to_string_lossy().into_owned().into_bytes();
    let name_like = bytes
        .iter()
        .any(|byte| byte.is_ascii_alphabetic() || !byte.is_ascii());
    (bytes.len() >= 3 && name_like).then_some(bytes)
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn run_steps(steps: &[(&str, &[&str])]) -> bool {
    let mut passed = true;
    for &(name, args) in steps {
        if !cargo(name, args) {
            passed = false;
        }
    }
    passed
}

/// Run one `cargo` subcommand, streaming its output; returns whether it passed.
fn cargo(name: &str, args: &[&str]) -> bool {
    eprintln!("── xtask: {name} ──");
    match Command::new("cargo").args(args).status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            eprintln!("xtask: {name} failed ({status})");
            false
        }
        Err(err) => {
            eprintln!("xtask: {name} could not start: {err}");
            false
        }
    }
}

/// Reserved-pattern drift gate. Some contradictions in this project's contracts
/// were re-introduced repeatedly after being fixed; this gate fails the build
/// when a tracked file re-pairs one of them, unless the head commit message
/// carries a `WAIVE-DRIFT: <reason>` line (the deliberate, auditable escape).
fn drift_gate() -> bool {
    eprintln!("── xtask: drift-gate ──");
    // `git ls-files` lists only files under the current directory and returns
    // paths relative to it, so a run from a subdirectory would silently scan
    // a subset of the repo. Anchor the listing and every read to the
    // repository root instead.
    let Some(top) = git(&["rev-parse", "--show-toplevel"]) else {
        eprintln!("xtask: drift-gate: `git rev-parse --show-toplevel` failed");
        return false;
    };
    let root = std::path::PathBuf::from(top.trim_end());
    let Some(listing) = git(&["-C", top.trim_end(), "ls-files"]) else {
        eprintln!("xtask: drift-gate: `git ls-files` failed");
        return false;
    };

    let mut violations = Vec::new();
    for path in listing.lines() {
        // Only this file is exempt: it names the patterns to define them.
        if path == "xtask/src/main.rs" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(path)) else {
            continue; // binary or unreadable file — nothing to scan
        };
        if let Some(reason) = reserved_pattern_hit(&text) {
            violations.push(format!("{path}: {reason}"));
        }
    }

    if violations.is_empty() {
        eprintln!("drift-gate: clean.");
        return true;
    }
    for v in &violations {
        eprintln!("drift-gate: {v}");
    }
    if head_commit_waives() {
        eprintln!(
            "drift-gate: reserved pattern present but waived by the head commit (WAIVE-DRIFT)."
        );
        return true;
    }
    eprintln!(
        "drift-gate: FAILED. If this pairing is intentional, add a \
         'WAIVE-DRIFT: <reason>' line to the head commit message."
    );
    false
}

/// Returns a description if `text` re-pairs a reserved contradiction, else `None`.
fn reserved_pattern_hit(text: &str) -> Option<String> {
    // The protocol this runtime will expose reconnects a client to a live
    // session via a `session.attach` request; output missed while detached is
    // reported inside the replay payload (a gap marker), never as a dedicated
    // JSON-RPC error. A recurring design error re-introduced an error code
    // (-32004) for that gap — a file pairing the two is re-importing the
    // contradiction.
    let attach_error = format!("-{}", "32004");
    if text.contains(&attach_error) && text.contains("session.attach") {
        return Some(format!("{attach_error} paired with session.attach"));
    }
    // The runtime will reconstruct a terminal screen (a "virtual terminal")
    // from the output stream so clients get render-state, not raw bytes. That
    // reconstruction belongs to the stream/event layer; a recurring design
    // error assigned it to the PTY layer (the process-hosting layer), which
    // must stay a plain byte pipe.
    let lower = text.to_lowercase();
    if (lower.contains("virtual terminal") || lower.contains("screen state"))
        && lower.contains("pty layer")
    {
        return Some("virtual-terminal / screen-state described as PTY-layer-owned".to_string());
    }
    None
}

fn head_commit_waives() -> bool {
    git(&["log", "-1", "--format=%B"])
        .is_some_and(|msg| msg.lines().any(|line| line.starts_with("WAIVE-DRIFT:")))
}

/// Run a `git` command, returning stdout on success.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_separators_do_not_defeat_the_username_mask() {
        // A reviewed claim held that a trailing separator on HOME makes
        // file_name return None and silently skips the mask. Path parsing
        // skips empty trailing components on every platform; this test runs
        // on all three CI OSes so that stays a checked fact, not a belief.
        for home in ["/Users/alice", "/Users/alice/", "/Users/alice//"] {
            assert_eq!(
                maskable_username(Some(home.into())).as_deref(),
                Some(b"alice".as_slice()),
                "{home}"
            );
        }
        #[cfg(windows)]
        for home in [r"C:\Users\alice", r"C:\Users\alice\", "C:/Users/alice/"] {
            assert_eq!(
                maskable_username(Some(home.into())).as_deref(),
                Some(b"alice".as_slice()),
                "{home}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_username_masks_by_its_exact_bytes() {
        // A Unix home path need not be UTF-8. The needle must be the
        // component's exact bytes — a lossily-converted needle could never
        // match what a fixture actually carries, claiming a mask that never
        // happened.
        use std::os::unix::ffi::OsStringExt;
        let raw = b"/Users/al\x80ce".to_vec(); // invalid UTF-8 in the name
        let needle = maskable_username(Some(std::ffi::OsString::from_vec(raw)))
            .expect("a lettered non-UTF-8 name must still mask");
        assert_eq!(needle, b"al\x80ce");
    }

    #[test]
    fn non_latin_usernames_mask_by_their_utf8_bytes() {
        // Requiring an ASCII letter would skip every CJK or Cyrillic
        // username and leak exactly the identity the scrub exists to
        // remove. Any non-ASCII byte is name-like enough to be a safe
        // needle — it cannot collide with a numeric field.
        for name in ["\u{7530}\u{4e2d}", "\u{0438}\u{0432}\u{0430}\u{043d}"] {
            let home = format!("/Users/{name}");
            assert_eq!(
                maskable_username(Some(home.clone().into())).as_deref(),
                Some(name.as_bytes()),
                "{home}"
            );
        }
    }

    #[test]
    fn unmaskable_homes_are_none_so_the_caller_warns_loudly() {
        // No home, a bare root, an empty value, a too-short name, and
        // needles of only ASCII digits and punctuation all decline the
        // mask — digits collide with numeric NDJSON fields, and something
        // like "1-2" or "..." with version strings and prose. Each reaches
        // the caller's explicit warning path instead of masking unsafely.
        for home in [
            None,
            Some("/"),
            Some(""),
            Some("/Users/123"),
            Some("/Users/ab"),
            Some("/Users/1-2"),
            Some("/Users/..."),
        ] {
            assert_eq!(
                maskable_username(home.map(std::ffi::OsString::from)),
                None,
                "{home:?}"
            );
        }
    }
}
