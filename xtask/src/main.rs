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
//!                            --install <text> [--model <name>] [--mask <text>]...
//!                            [--only <scenario>] [--dry-run]
//!                            # record every capture scenario for one CLI at one pinned
//!                            # version, both terminal sizes, into tests/corpus/ — then
//!                            # scrub and hold the corpus to its size budget. Maintainer-run,
//!                            # never on any CI tier: the claude campaign spends session quota.
//!                            # One sitting per CLI = one invocation per pinned version.
//!                            # --mask adds scrub needles beyond the username and the names
//!                            # auto-derived from git identity (those distinctive enough to
//!                            # mask safely — >=3 bytes with a letter). The claude TUI paints two
//!                            # account settings the campaign cannot derive: the account
//!                            # email (pass its local part) and the display name in the
//!                            # "Welcome back <name>!" splash — pass both, or the scrub aborts.

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
    // The detection spike's replay is deterministic and credential-free —
    // it reads only the committed corpus — but the binary's corpus walk and
    // report path are exercised end-to-end here, beyond what the library
    // integration lanes in `cargo test` cover.
    (
        "detection-spike (text-matching replay over the corpus)",
        &[
            "run",
            "--quiet",
            "--package",
            "agent-bridge-detection-spike",
            "--bin",
            "detection-spike",
            "--",
            "replay",
            "--config",
            "a",
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
///
/// Raised from the original 1 MiB once the first full claude sitting
/// measured reality: 3 pinned versions × 2 terminal sizes × 9 scenarios is
/// ~2.96 MiB, and the raw PTY byte streams alone (the irreducible replay
/// core, `design/17`) are already > 1 MiB across three versions — so 1 MiB
/// was infeasible for the corpus the plan scopes, not a trim target. Full
/// fidelity is kept deliberately: the transcripts' setup-noise records (the
/// largest trimmable chunk) are exactly what config (c)'s tailer must skip,
/// so dropping them would flatter the `unrecognized_output` metric this
/// spike exists to measure. Set to 3.5 MiB — comfortably above the measured
/// corpus so a re-record's ordinary size drift does not trip a false
/// over-budget — and the re-pricing is a Phase-2 sizing input in the report.
const CORPUS_BUDGET_BYTES: u64 = 3_670_016;

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
             --version-label <label> --install <text> [--model <name>] \
             [--mask <text>]... [--only <scenario>] [--dry-run]"
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
    // `--only <name>` records a single scenario into an existing version
    // directory — for adding a scenario to a corpus already captured, without
    // re-recording (and re-spending session quota on) the rest of the matrix.
    // The whole version directory is still scrubbed and sized afterwards, so
    // the addition lands held to the same guarantees as a full sitting.
    if let Some(only) = &campaign.only {
        scripts.retain(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_suffix(".record.json"))
                == Some(only.as_str())
        });
        if scripts.is_empty() {
            eprintln!(
                "xtask: capture-campaign: --only {only} matched no scenario in {} \
                 (expected {only}.record.json)",
                scenarios_dir.display()
            );
            return false;
        }
    }
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

    let mut masks = campaign.masks.clone();
    for needle in derived_identity_needles() {
        if !masks.contains(&needle) {
            // Report the source and length, never the needle bytes: the
            // needle *is* the identity being scrubbed, and campaign logs get
            // pasted into issues/PRs — printing it verbatim would re-expose
            // exactly what the corpus masks.
            eprintln!(
                "capture-campaign: auto-mask needle derived from the environment (git \
                 identity or temp-dir hash), {} bytes",
                needle.len()
            );
            masks.push(needle);
        }
    }
    if !scrub_fixtures(&version_dir, &masks) {
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
    /// Extra scrub needles beyond the username, masked the same way (a
    /// same-length `x`-run, raw bytes). The claude TUI paints two account
    /// settings the campaign cannot derive on its own, so the invoker names
    /// them: the logged-in account email (its local part) and the display
    /// name in the "Welcome back <name>!" splash. The post-scrub email and
    /// greeting sweeps abort — and remove the offending fixture — if either
    /// needle was forgotten.
    masks: Vec<Vec<u8>>,
    /// Record only this one scenario (by `<name>.record.json` stem) into the
    /// version directory, instead of the whole scenario set. For extending an
    /// already-captured corpus without re-recording the rest.
    only: Option<String>,
    dry_run: bool,
}

impl CampaignArgs {
    fn parse(args: &[String]) -> Option<Self> {
        let mut cli = None;
        let mut bin = None;
        let mut version_label = None;
        let mut install = None;
        let mut model = None;
        let mut masks: Vec<Vec<u8>> = Vec::new();
        let mut only = None;
        let mut dry_run = false;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--cli" => cli = Some(iter.next()?.clone()),
                "--bin" => bin = Some(iter.next()?.clone()),
                "--version-label" => version_label = Some(iter.next()?.clone()),
                "--install" => install = Some(iter.next()?.clone()),
                "--model" => model = Some(iter.next()?.clone()),
                "--only" => only = Some(iter.next()?.clone()),
                "--mask" => {
                    let needle = iter.next()?.clone().into_bytes();
                    // Same safety rule as the username needle: a mask is a
                    // raw-byte replacement across every artifact, and a
                    // short or digits-and-punctuation needle would also hit
                    // numeric NDJSON fields, version strings, and prose.
                    if !safe_mask_needle(&needle) {
                        eprintln!(
                            "xtask: capture-campaign: --mask needs at least 3 bytes and a \
                             letter (or non-ASCII); refusing an unsafe needle"
                        );
                        return None;
                    }
                    masks.push(needle);
                }
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
            masks,
            only,
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
/// (HOME's last component) and every `--mask` needle are masked with a
/// same-length run of `x`, so byte offsets recorded in the timing sidecars
/// stay valid; a fixture that contains the ANTHROPIC_API_KEY or
/// OPENAI_API_KEY value is a leak and aborts the campaign outright —
/// masking a credential and committing anyway would hide the evidence that
/// the capture setup is wrong. After masking, any email-shaped byte-run still present aborts
/// too: the claude TUI paints the logged-in account email into the byte
/// stream, so a surviving address means a `--mask` was forgotten (or the
/// scenario elicited an address), and only a human can tell which.
fn scrub_fixtures(dir: &Path, extra_masks: &[Vec<u8>]) -> bool {
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
    // One guarded credential per CLI the sittings record. Codex authenticates
    // through CODEX_HOME's auth.json, whose tokens never reach the PTY, but
    // an API key exported in the campaign's shell is the same leak class the
    // claude guard exists for — if either value lands in a fixture, the
    // setup is wrong, and masking it would hide exactly that.
    let api_keys: Vec<(&str, String)> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
        .into_iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|key| key.len() >= 8)
                .map(|key| (name, key))
        })
        .collect();

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
    // Pass 1 — credential guard, then mask and write each file. A credential
    // hit is a hard abort; the recorded file on disk holds the key, so it is
    // removed before returning, the same reason the sweeps in pass 2 remove
    // what they flag.
    let mut masked_files = 0usize;
    for path in &files {
        let Ok(mut bytes) = std::fs::read(path) else {
            eprintln!("capture-campaign: unreadable fixture {}", path.display());
            return false;
        };
        if let Some((name, _)) = api_keys
            .iter()
            .find(|(_, key)| find_subsequence(&bytes, key.as_bytes()).is_some())
        {
            remove_leaking_fixture(path);
            eprintln!(
                "capture-campaign: ABORT — {} contained the {name} value (removed). \
                 The capture setup leaked a credential into a fixture; unset the key, then \
                 re-record.",
                path.display()
            );
            return false;
        }
        let mut changed = false;
        for needle in username
            .as_deref()
            .into_iter()
            .chain(extra_masks.iter().map(Vec::as_slice))
        {
            let mut from = 0usize;
            while let Some(at) = find_subsequence(&bytes[from..], needle) {
                let at = from + at;
                bytes[at..at + needle.len()].fill(MASK_BYTE);
                from = at + needle.len();
                changed = true;
            }
        }
        // The account address is painted split across cursor-move escapes, so
        // once the `--mask` needle has masked the identifying local part, a
        // raw needle cannot reach the `@domain` that trails it — the escape
        // sits between the characters. Mask it here from the control-stripped
        // view instead, so no email-shaped string survives regardless of how
        // the repaint fragmented it.
        changed |= mask_email_domains(&mut bytes);
        if changed {
            if std::fs::write(path, &bytes).is_err() {
                eprintln!("capture-campaign: rewriting {} failed", path.display());
                return false;
            }
            masked_files += 1;
        }
    }
    // Pass 2 — sweep every masked file for identity a needle missed, and
    // remove each offender *before* returning, so an aborted run never leaves
    // a still-leaking fixture on disk for an accidental commit (a removed
    // file also makes its fixture directory trace_check-invalid, a second
    // guard). Every leaker is reported, not just the first, so one re-run
    // surfaces all the missing needles at once.
    let mut leaked = false;
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else {
            eprintln!("capture-campaign: unreadable fixture {}", path.display());
            return false;
        };
        // Report the shape and length of what survived, never its bytes: the
        // survivor is the identity itself, and this diagnostic can be pasted
        // into an issue/PR — the maintainer knows their own account email and
        // display name, so the category plus a length is enough to act on
        // without the log re-exposing what the corpus is scrubbing.
        if let Some(found) = surviving_email(&bytes) {
            remove_leaking_fixture(path);
            eprintln!(
                "capture-campaign: {} still held an email-shaped run ({} bytes) after masking \
                 (removed). Mask the account email's local part with --mask <local-part>; \
                 the @domain a name-mask leaves behind is cleared automatically.",
                path.display(),
                found.len(),
            );
            leaked = true;
        } else if let Some(found) = surviving_greeting(&bytes) {
            remove_leaking_fixture(path);
            eprintln!(
                "capture-campaign: {} still showed the account display name ({} bytes) in the \
                 splash greeting after masking (removed). Mask it with --mask <display-name>.",
                path.display(),
                found.len(),
            );
            leaked = true;
        }
    }
    if leaked {
        eprintln!(
            "capture-campaign: ABORT — removed the leaking fixture(s) above; add the missing \
             --mask needle(s) and re-run."
        );
        return false;
    }
    eprintln!(
        "capture-campaign: scrubbed {} files ({masked_files} carried a needle and were masked).",
        files.len()
    );
    true
}

/// Remove a fixture a scrub check flagged as still leaking, so an aborted
/// run cannot leave it on disk for an accidental commit. Best-effort: a
/// failure to remove is reported (so a human deletes it by hand) but does
/// not change the abort the caller is already returning.
fn remove_leaking_fixture(path: &Path) {
    if let Err(err) = std::fs::remove_file(path) {
        eprintln!(
            "capture-campaign: could not remove leaking {} ({err}) — delete it by hand \
             before committing",
            path.display()
        );
    }
}

/// The first email-shaped byte-run in `haystack`, or `None`. Hand-rolled
/// (this crate is deliberately dependency-free) and deliberately narrow: an
/// `@` with a `[A-Za-z0-9._%+-]` local part on its left and a dotted domain
/// whose final label is alphabetic (≥ 2 chars) on its right. The narrowness
/// is what keeps the sweep quiet on the bytes fixtures actually carry —
/// npm scopes (`@anthropic-ai/...`, no dotted domain), versioned installs
/// (`...@2.1.201`, numeric final label), and the `@` that terminates an
/// ANSI insert-character sequence (no local part or no dotted domain) all
/// pass — while anything a human would read as an address aborts.
///
/// The local-part walk may over-extend left into adjacent characters, so
/// the returned range *contains* an address, possibly with a run-on prefix.
/// The sweep only names what it aborts on; a wider range is more context,
/// not a wrong answer.
///
/// The `@` is sought at or after `from` (so a caller can resume past a match
/// it chose to ignore), but the local part still walks freely left of it.
fn find_email(haystack: &[u8], from: usize) -> Option<std::ops::Range<usize>> {
    for i in from..haystack.len() {
        if haystack[i] != b'@' {
            continue;
        }
        let is_local = |b: u8| b.is_ascii_alphanumeric() || b"._%+-".contains(&b);
        let mut start = i;
        while start > 0 && is_local(haystack[start - 1]) {
            start -= 1;
        }
        if start == i {
            continue;
        }
        let is_domain = |b: u8| b.is_ascii_alphanumeric() || b == b'.' || b == b'-';
        let mut end = i + 1;
        while end < haystack.len() && is_domain(haystack[end]) {
            end += 1;
        }
        // An address at the end of a sentence drags the full stop into the
        // domain walk; trimmed here so `user@mail.com.` still aborts the
        // campaign instead of slipping past on an empty final label.
        while end > i + 1 && matches!(haystack[end - 1], b'.' | b'-') {
            end -= 1;
        }
        let domain = &haystack[i + 1..end];
        let mut labels = domain.split(|&b| b == b'.');
        let Some(last) = labels.next_back() else {
            continue;
        };
        let tld_like = last.len() >= 2 && last.iter().all(u8::is_ascii_alphabetic);
        let rest_nonempty = {
            let mut any = false;
            let mut all_full = true;
            for label in labels {
                any = true;
                all_full &= !label.is_empty();
            }
            any && all_full
        };
        if tld_like && rest_nonempty {
            return Some(start..end);
        }
    }
    None
}

/// The byte a mask leaves in place of a needle. A local part carrying a run
/// of this byte is an already-scrubbed remnant, not identity, so the email
/// sweep skips it (see `surviving_email`).
const MASK_BYTE: u8 = b'x';

/// How many consecutive `MASK_BYTE`s at the end of a local part mark it as a
/// scrubbed remnant. It equals `safe_mask_needle`'s minimum length, so every
/// needle the campaign is allowed to mask leaves a recognizable run.
const MASK_RUN_LEN: usize = 3;

/// Is this address's local part already scrubbed — i.e. is what remains just
/// the `@domain` trailing a masked needle, rather than surviving identity?
///
/// The run must sit at the *end* of the local part, immediately before the
/// `@`. That is where masking a needle always leaves it: the needle covers
/// the identifying text right up to the `@`, so `<needle>@domain` becomes
/// `xxxx@domain`, and stripping terminal framing can only glue extra
/// characters onto the *front* (`fre` + `xxxx@…`). Testing for a run
/// *anywhere* in the local part would instead whitelist a genuine unmasked
/// address that merely happens to contain `xxx` (`xxxuser@example.com`) —
/// a false negative in a leak check, the one direction that must not fail.
fn local_part_is_scrubbed(local: &[u8]) -> bool {
    local
        .iter()
        .rev()
        .take_while(|&&byte| byte == MASK_BYTE)
        .count()
        >= MASK_RUN_LEN
}

/// An email-shaped run that survived masking, rendered as text for the
/// diagnostic, or `None` if the fixture is clean.
///
/// The scan runs on a *control-stripped* view of the bytes, because the
/// claude TUI paints the account footer through differential repaints: an
/// address can be interrupted mid-domain by a cursor-move escape
/// (`...@gma\x1b[28Gil.com`) that a raw contiguous scan reads straight past
/// — which is exactly how the account name leaked through the first sitting
/// while a raw sweep reported clean. Stripping the framing first makes the
/// split address contiguous again.
///
/// A match whose local part carries a `MASK_RUN_LEN` run of `MASK_BYTE` is
/// the `@domain` a name-mask leaves behind, not identity, so it is skipped
/// and the scan resumes past it — a real address elsewhere in the same file
/// still aborts. The run is tested rather than "all mask bytes" because
/// stripping the framing can glue a neighbouring footer word onto the front
/// of the masked local part (`fre` + `xxxxxxxxxxxxxxxx@…`), and that prefix
/// must not turn a scrubbed remnant back into an abort.
fn surviving_email(bytes: &[u8]) -> Option<String> {
    let rendered = strip_terminal_framing(bytes);
    let mut cursor = 0;
    while let Some(range) = find_email(&rendered, cursor) {
        let at = range.start
            + rendered[range.clone()]
                .iter()
                .position(|&b| b == b'@')
                .expect("find_email only returns ranges containing an @");
        if local_part_is_scrubbed(&rendered[range.start..at]) {
            cursor = range.end;
            continue;
        }
        return Some(String::from_utf8_lossy(&rendered[range]).into_owned());
    }
    None
}

/// The account display name still showing in the splash greeting after
/// masking, or `None`. The claude TUI paints `Welcome back <name>!` on the
/// startup splash, where `<name>` is the account's chosen display name — an
/// account setting the campaign cannot derive (unlike the username or the
/// git-owner), so it only leaves the corpus if the maintainer passes it as a
/// `--mask` needle. This sweep is the backstop that makes forgetting it a
/// loud abort rather than a silent leak, the same role the email sweep plays
/// for the account address; it was added after a display name slipped past a
/// purely token-based verification (nothing searched for a token nobody knew
/// to look for). The scan runs on the control-stripped view because the
/// greeting, like the footer, can be repaint-split; a name that is a run of
/// `MASK_BYTE`s is already scrubbed and ignored.
fn surviving_greeting(bytes: &[u8]) -> Option<String> {
    const MARKER: &[u8] = b"Welcome back ";
    let rendered = strip_terminal_framing(bytes);
    let mut from = 0;
    while let Some(rel) = find_subsequence(&rendered[from..], MARKER) {
        let name_start = from + rel + MARKER.len();
        // The greeting ends at `!`; the name is what precedes it. Bound the
        // scan so a missing `!` (truncated paint) cannot run to end of file.
        let tail = &rendered[name_start..];
        let name_end = tail
            .iter()
            .take(64)
            .position(|&b| b == b'!')
            .unwrap_or(tail.len().min(64));
        let name = &tail[..name_end];
        let all_masked = !name.is_empty() && name.iter().all(|&b| b == MASK_BYTE);
        if !name.is_empty() && !all_masked {
            return Some(String::from_utf8_lossy(name).into_owned());
        }
        from = name_start;
    }
    None
}

/// A copy of `bytes` with terminal control framing removed: an ESC-led
/// sequence (CSI `\x1b[`…final, OSC `\x1b]`…BEL/ST, or any other two-byte
/// `\x1b X`) and every C0 control or DEL is dropped; printable bytes,
/// including UTF-8 continuation bytes, pass through. This is a detection aid
/// only — masking still edits the original bytes in place so the recorded
/// timing offsets stay valid. Dropping newlines can concatenate unrelated
/// text, which can only ever make the sweep *more* eager to abort; erring
/// toward a spurious abort (a maintainer looks) beats erring toward a silent
/// leak (identity ships).
fn strip_terminal_framing(bytes: &[u8]) -> Vec<u8> {
    render_indexed(bytes).0
}

/// `strip_terminal_framing`, but also returning, for each kept byte, its
/// index in the original `bytes`. The map is what lets a masker edit the
/// original bytes behind an on-screen run it found in the stripped view (the
/// account address, split across cursor-move escapes), masking the printable
/// characters while leaving the escapes — and the total length — untouched.
fn render_indexed(bytes: &[u8]) -> (Vec<u8>, Vec<usize>) {
    let mut out = Vec::with_capacity(bytes.len());
    let mut src = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            i += 1;
            let Some(&intro) = bytes.get(i) else { break };
            match intro {
                b'[' => {
                    // CSI: parameter/intermediate bytes, then a final in @-~.
                    i += 1;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += usize::from(i < bytes.len()); // consume the final byte
                }
                b']' => {
                    // OSC: runs until BEL, or ST (`\x1b\\`).
                    i += 1;
                    while i < bytes.len() && bytes[i] != 0x07 {
                        if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    i += usize::from(bytes.get(i) == Some(&0x07)); // consume BEL
                }
                // Any other `\x1b X` (a two-byte escape) drops both bytes.
                _ => i += 1,
            }
            continue;
        }
        if b >= 0x20 && b != 0x7f {
            out.push(b);
            src.push(i);
        }
        i += 1;
    }
    (out, src)
}

/// Mask the `@domain` of every already-scrubbed email address in `bytes`, in
/// place, returning whether anything changed. Finds each address in the
/// control-stripped view and masks the on-screen `@` and domain characters
/// through the source-index map, so the escapes a repaint left between them
/// (and the total byte length, which the timing sidecars index) are
/// preserved. Only addresses whose local part is already a `MASK_BYTE` run
/// are touched: the identity is the local part, masked by a `--mask` needle
/// first, and an address whose local part is *not* masked is left intact for
/// the email sweep to abort on — so a forgotten needle still fails loudly.
fn mask_email_domains(bytes: &mut [u8]) -> bool {
    let (rendered, src) = render_indexed(bytes);
    let mut changed = false;
    let mut cursor = 0;
    while let Some(range) = find_email(&rendered, cursor) {
        let at = range.start
            + rendered[range.clone()]
                .iter()
                .position(|&b| b == b'@')
                .expect("find_email only returns ranges containing an @");
        if local_part_is_scrubbed(&rendered[range.start..at]) {
            for j in at..range.end {
                if bytes[src[j]] != MASK_BYTE {
                    bytes[src[j]] = MASK_BYTE;
                    changed = true;
                }
            }
        }
        cursor = range.end;
    }
    changed
}

/// Is this needle safe to mask by raw-byte replacement? Shared rule for the
/// username and every `--mask`: long enough to be distinctive, and carrying
/// a letter (or any non-ASCII byte — a name in a non-Latin script), so it
/// cannot collide with the numeric NDJSON fields and version strings that
/// digits-and-punctuation needles would corrupt.
fn safe_mask_needle(needle: &[u8]) -> bool {
    let name_like = needle
        .iter()
        .any(|byte| byte.is_ascii_alphabetic() || !byte.is_ascii());
    needle.len() >= 3 && name_like
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
    safe_mask_needle(&bytes).then_some(bytes)
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

/// The owner segment of a git remote URL — the `owner` in `owner/repo` — for
/// both `scheme://host/owner/repo` and scp-style `user@host:owner/repo`
/// forms, or `None` when the URL has no host-then-path shape. The `.git`
/// suffix and any port ride along on the repo/host parts this ignores.
fn remote_owner_from_url(url: &str) -> Option<&str> {
    let after_host = match url.trim().split_once("://") {
        Some((_scheme, rest)) => {
            let (host, path) = rest.split_once('/')?;
            // A hostless URL (`file:///path`) has no owner to speak of; its
            // first path segment is just a directory, not a repo owner.
            if host.is_empty() {
                return None;
            }
            path
        }
        // scp-like `user@host:owner/repo` — everything after the first colon.
        // `git remote get-url` can also return a Windows local-clone path
        // (`C:/Users/…` or `C:\repos\…`); its single-letter drive "host"
        // must not be read as an scp host, or the scrub would derive a
        // bogus, over-generic owner needle (`Users`, `repos`) and mask it
        // everywhere. A drive letter is one alphabetic char, and a real
        // scp path carries an `owner/repo` slash a bare drive path may not.
        None => {
            let (host, path) = url.trim().split_once(':')?;
            let is_drive_letter = matches!(host.as_bytes(), [c] if c.is_ascii_alphabetic());
            if is_drive_letter || !path.contains('/') {
                return None;
            }
            path
        }
    };
    let owner = after_host.trim_start_matches('/').split('/').next()?;
    (!owner.is_empty()).then_some(owner)
}

/// Scrub needles the campaign derives from its own environment, so a
/// machine-local token that only ever reaches a fixture through a path is
/// masked without the maintainer having to remember it. Two sources: the
/// repository's git identity — the hook command is the probe's absolute
/// `current_exe`, which sits under `~/…/<remote-owner>/<repo>/…`, so the
/// remote owner and committer name are needles (a name has no shape the
/// email sweep can catch, so an un-derived one would leak silently, exactly
/// how the repo-owner name slipped through the first sitting) — and the
/// temp directory, whose opaque per-user hash component (macOS
/// `/var/folders/<bucket>/<hash>/T`) the CLI paints into every `cwd` and
/// `transcript_path`. That hash is not personal identity, but it is
/// machine-local host-specific noise that makes the corpus needlessly
/// host-bound; masking it keeps the fixtures portable. Additive to the
/// explicit `--mask` list; each needle still passes `safe_mask_needle`.
fn derived_identity_needles() -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut consider = |text: &str| {
        let needle = text.trim().as_bytes().to_vec();
        if safe_mask_needle(&needle) && !out.contains(&needle) {
            out.push(needle);
        }
    };
    if let Some(name) = git(&["config", "user.name"]) {
        consider(&name);
    }
    if let Some(url) = git(&["remote", "get-url", "origin"])
        && let Some(owner) = remote_owner_from_url(&url)
    {
        consider(owner);
    }
    for hash in hash_like_path_components(&std::env::temp_dir()) {
        consider(&hash);
    }
    out
}

/// The shortest temp-directory path component treated as a per-user hash to
/// mask. The macOS `_CS_DARWIN_USER_TEMP_DIR` hash is ~28–30 chars; ordinary
/// components (`var`, `folders`, `tmp`, `T`, `AppData`, `Local`, `Temp`) are
/// all shorter, so this bar masks the hash without touching structure.
const TEMP_HASH_MIN_LEN: usize = 16;

/// The long, purely-alphanumeric path components of `path` — the per-user
/// temp-dir hash on macOS, and nothing on Linux (`/tmp`) or the short,
/// dictionary-like segments of any temp path. Pulled out of the derivation
/// so the length/charset rule is unit-tested rather than exercised only on
/// whatever machine happens to run the campaign.
fn hash_like_path_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => part.to_str(),
            _ => None,
        })
        .filter(|text| {
            text.len() >= TEMP_HASH_MIN_LEN && text.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_hash_component_is_masked_but_structure_is_not() {
        // The macOS temp path yields exactly the per-user hash; the
        // structural segments (var, folders, the `_3` bucket which is not
        // all-alphanumeric, T) stay. Linux `/tmp` yields nothing. The hash
        // here is a placeholder of the right shape, not any real machine's.
        assert_eq!(
            hash_like_path_components(Path::new("/var/folders/_9/aaaabbbbccccdddd1111222233/T")),
            vec!["aaaabbbbccccdddd1111222233".to_string()],
        );
        assert!(hash_like_path_components(Path::new("/tmp")).is_empty());
    }

    #[test]
    fn remote_owner_is_parsed_from_every_url_shape() {
        // The owner is what a machine-local path carries (`~/…/<owner>/<repo>`)
        // and what the campaign auto-masks. Both git URL grammars must yield
        // it, including the SSH host-alias form where the alias itself
        // repeats the owner (`github.com-owner`) — the parse must return the
        // path owner, not the host.
        for (url, owner) in [
            ("git@github.com:acme/widget.git", "acme"),
            ("git@github.com-acme:acme/widget.git", "acme"),
            ("https://github.com/acme/widget.git", "acme"),
            ("https://github.com:443/acme/widget", "acme"),
            ("ssh://git@github.com/acme/widget.git", "acme"),
        ] {
            assert_eq!(remote_owner_from_url(url), Some(owner), "{url}");
        }
    }

    #[test]
    fn remote_owner_declines_urls_with_no_owner() {
        // No host-then-path shape, so nothing safe to treat as an owner; the
        // deriver must add nothing rather than mask a stray directory name.
        for url in ["file:///srv/repos/widget.git", "not-a-url", "https://", ""] {
            assert_eq!(remote_owner_from_url(url), None, "{url}");
        }
    }

    #[test]
    fn remote_owner_declines_windows_local_paths() {
        // `git remote get-url` returns a bare filesystem path for a local
        // clone. A Windows drive-letter path must not be parsed as scp-style
        // (`C:owner/…`) — that derived `Users`/`repos` needles that the scrub
        // would then mask across every fixture. Both slash directions, and a
        // bare `host:repo` with no owner slash, decline.
        for url in [
            "C:/Users/dev/widget.git",
            r"C:\Users\dev\widget.git",
            "D:/repos/widget",
            "server:widget.git", // no owner/repo slash
        ] {
            assert_eq!(remote_owner_from_url(url), None, "{url}");
        }
    }

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
    fn email_sweep_finds_addresses_wherever_they_hide() {
        // The first real claude sitting found the TUI paints the logged-in
        // account email into the raw byte stream, wrapped in ANSI color
        // sequences. The sweep must see through that framing, and through
        // JSON, or a forgotten --mask commits identity to the public corpus.
        for (haystack, expect) in [
            (
                &b"plain someone@example.com text"[..],
                "someone@example.com",
            ),
            (
                &b"\x1b[38;2;153;153;153msomeone@example.com's Organization\x1b[54G"[..],
                "someone@example.com",
            ),
            (&br#"{"email":"a.b+tag@mail.co"}"#[..], "a.b+tag@mail.co"),
            (
                &b"reach me at user@mail.com. Next sentence."[..],
                "user@mail.com",
            ),
        ] {
            let range = find_email(haystack, 0).expect("address must be found");
            // Suffix, not equality: the local-part walk may drag framing
            // bytes (an SGR sequence's `153m`) into the front of the range.
            assert!(
                haystack[range.clone()].ends_with(expect.as_bytes()),
                "found {:?} in {:?}",
                String::from_utf8_lossy(&haystack[range]),
                String::from_utf8_lossy(haystack),
            );
        }
    }

    #[test]
    fn email_sweep_stays_quiet_on_fixture_bytes_that_merely_carry_an_at() {
        // Every fixture legitimately carries `@`s that are not addresses:
        // the manifest's npm install line (scope has no dotted domain; the
        // version pin's final label is numeric), ANSI insert-character
        // sequences ending in `@`, and the x-runs the mask itself leaves
        // behind. Aborting on those would make the sweep impossible to
        // live with, and it would get disabled instead of heeded.
        for haystack in [
            &b"npm @anthropic-ai/claude-code@2.1.201"[..],
            &b"install: \"npm @anthropic-ai/claude-code@2.1.201\""[..],
            &b"\x1b[4@after"[..],
            &b"xxxxxxxxxxxxxxxxxxxxxxxxxx's Organization"[..],
            &b"bare @ sign"[..],
            &b"trailing@"[..],
        ] {
            assert_eq!(
                find_email(haystack, 0),
                None,
                "{}",
                String::from_utf8_lossy(haystack)
            );
        }
    }

    #[test]
    fn the_sweep_sees_an_address_split_across_a_repaint_escape() {
        // The exact shape that leaked in the first sitting: the account
        // footer's domain interrupted by a differential-repaint cursor move,
        // so the address is not contiguous in the raw bytes. A raw scan
        // reads past it; the framing-stripped sweep must catch it.
        let raw = b"\x1b[7G\x1b[38;2;153;153;153msomeone@gma\x1b[28Gil.com's Organization";
        assert_eq!(find_email(raw, 0), None, "raw scan cannot see the split");
        assert_eq!(
            surviving_email(raw).as_deref(),
            Some("someone@gmail.com"),
            "the framing-stripped sweep reassembles it"
        );
    }

    #[test]
    fn the_sweep_ignores_the_domain_a_name_mask_leaves_behind() {
        // After masking the identifying local part, the residual `@domain`
        // still forms a syntactic address, but the local part carries a run
        // of mask bytes and no identity — the sweep must not abort on it, or
        // a correctly-scrubbed fixture could never be committed. This holds
        // when the domain is itself split across a repaint escape, and when
        // stripping the framing glues a neighbouring footer word (`fre`, off
        // the end of "free") onto the front of the masked run — the real
        // false positive that a live capture hit.
        for clean in [
            &b"\x1b[7Gxxxxxxxxxxxxxxxx@gmail.com's Organization"[..],
            &b"\x1b[7Gxxxxxxxxxxxxxxxx@gma\x1b[28Gil.com's Organization"[..],
            &b"terminal fre\x1b[2Cxxxxxxxxxxxxxxxx@gmail.com's Organization"[..],
        ] {
            assert_eq!(
                surviving_email(clean),
                None,
                "{}",
                String::from_utf8_lossy(clean)
            );
        }
    }

    #[test]
    fn a_mask_run_only_counts_where_masking_leaves_it_before_the_at() {
        // The scrubbed-remnant test looks for mask bytes at the END of the
        // local part, because that is the only place masking a needle can
        // leave them. Accepting a run anywhere would whitelist a genuine
        // address that merely contains "xxx" — a missed leak, the failure
        // direction that matters.
        assert!(
            local_part_is_scrubbed(b"xxxxxxxxxxxxxxxx"),
            "a fully masked local part is a remnant"
        );
        assert!(
            local_part_is_scrubbed(b"frexxxxxxxxxxxxxxxx"),
            "framing glued onto the front still leaves the run before the @"
        );
        assert!(
            !local_part_is_scrubbed(b"xxxuser"),
            "a real address that happens to start with xxx is NOT scrubbed"
        );
        assert!(!local_part_is_scrubbed(b"user"), "no mask bytes at all");
        // End to end: the address must be reported, not silently skipped.
        assert_eq!(
            surviving_email(b"contact xxxuser@example.com now").as_deref(),
            Some("xxxuser@example.com"),
        );
    }

    #[test]
    fn the_sweep_finds_a_real_address_past_a_masked_remnant() {
        // A masked remnant earlier in the file must not shadow a genuine
        // address the maintainer forgot to mask further along.
        let bytes = b"xxxx@gmail.com ... later real@example.org";
        assert_eq!(surviving_email(bytes).as_deref(), Some("real@example.org"));
    }

    #[test]
    fn the_greeting_sweep_catches_an_unmasked_display_name() {
        // The exact shape that slipped past token-based verification: the
        // account display name in the startup splash greeting, wrapped in
        // SGR sequences and box-drawing. A placeholder name (not any real
        // account's) carrying a non-ASCII byte, as a real one can.
        let raw = "\u{2502}\x1b[1mWelcome back Zoë!\x1b[0m\u{2502}".as_bytes();
        assert_eq!(surviving_greeting(raw).as_deref(), Some("Zoë"));
    }

    #[test]
    fn the_greeting_sweep_ignores_a_masked_name_and_absent_greetings() {
        // A masked name (the scrubbed state) must not abort, and a fixture
        // with no greeting at all is clean.
        assert_eq!(
            surviving_greeting(b"\x1b[1mWelcome back xxxxx!\x1b[0m"),
            None
        );
        assert_eq!(surviving_greeting(b"no greeting here, just prose"), None);
        // A greeting with an empty name (nothing between marker and `!`) is
        // not a leak either.
        assert_eq!(surviving_greeting(b"Welcome back !"), None);
    }

    #[test]
    fn domain_masker_clears_a_repaint_split_domain_in_place() {
        // The exact shape a raw needle cannot reach: the account address with
        // the local part already masked and the domain split across a
        // cursor-move escape. The masker must clear the on-screen @domain,
        // leave the escape and total length intact, and leave no email-shaped
        // run behind.
        let mut bytes = b"footer xxxxxxxxxxxxxxxx@gma\x1b[28Gil.com's Org".to_vec();
        let before = bytes.len();
        assert!(mask_email_domains(&mut bytes));
        assert_eq!(bytes.len(), before, "masking must preserve byte length");
        assert!(bytes.windows(5).any(|w| w == b"\x1b[28G"), "escape kept");
        assert_eq!(surviving_email(&bytes), None, "no email-shaped run remains");
    }

    #[test]
    fn domain_masker_leaves_an_unmasked_address_for_the_sweep() {
        // If the local part was never masked (a forgotten --mask), the domain
        // masker must not touch it — the email sweep has to still see a real
        // address and abort, rather than the domain being quietly masked into
        // a shape the sweep no longer recognises.
        let mut bytes = b"contact realname@example.com now".to_vec();
        assert!(!mask_email_domains(&mut bytes));
        assert_eq!(
            surviving_email(&bytes).as_deref(),
            Some("realname@example.com")
        );
    }

    #[test]
    fn framing_stripper_drops_escapes_and_keeps_utf8_text() {
        // CSI, OSC (both BEL- and ST-terminated), a lone two-byte escape,
        // and a C0 control all vanish; printable ASCII and multi-byte UTF-8
        // survive. Reading the survivors off the input: a b c d e f é.
        let framed = b"a\x1b[1;2mb\x1b]0;x\x07c\x1b]8;;u\x1b\\d\x1bXe\nf\xc3\xa9";
        assert_eq!(strip_terminal_framing(framed), "abcdef\u{e9}".as_bytes());
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
