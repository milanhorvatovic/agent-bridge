//! stub-adapter — every committed fake-CLI conformance scenario, run
//! end-to-end through the launch path.
//!
//! Walks `tests/corpus/fake/` for scenario directories (the ones carrying a
//! `scenario.json`), runs each through [`agent_bridge_stub_adapter::run_scenario`],
//! and reports one probe-style line per scenario. New scenarios join the
//! lane by being committed; nothing here enumerates them by name.
//!
//! Binary contract:
//!   stub-adapter
//!
//! Prints one `step=<scenario> status=<ok|fail> detail="…"` line per
//! scenario and exits non-zero if any scenario failed to exit cleanly — or
//! if no scenario was found at all, because an empty lane that reports
//! green would be indistinguishable from coverage.

// Probe-style report lines are this binary's output contract, and stdout is
// where CI reads them; the workspace-wide println ban targets the future
// JSON-RPC wire, which this tool will never own.
#![allow(clippy::disallowed_macros)]

use std::path::{Path, PathBuf};
use std::process::exit;

use agent_bridge_stub_adapter::run_scenario;

fn main() {
    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/corpus/fake");
    let scenarios = match scenario_files(&corpus) {
        Ok(scenarios) if scenarios.is_empty() => {
            eprintln!(
                "stub-adapter: no scenario directories under {} — an empty lane must not pass",
                corpus.display()
            );
            exit(2);
        }
        Ok(scenarios) => scenarios,
        Err(err) => {
            eprintln!("stub-adapter: {err}");
            exit(2);
        }
    };

    let mut failed = false;
    for scenario in &scenarios {
        let name = scenario
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| scenario.display().to_string());
        match run_scenario(scenario) {
            Ok(report) if report.clean() => {
                println!(
                    "step={name} status=ok detail=\"exit 0, {} stdout bytes, {} stderr bytes\"",
                    report.stdout_bytes, report.stderr_bytes
                );
            }
            Ok(report) => {
                println!(
                    "step={name} status=fail detail=\"exit {:?}, {} stdout bytes, {} stderr bytes\"",
                    report.exit_code, report.stdout_bytes, report.stderr_bytes
                );
                failed = true;
            }
            Err(err) => {
                println!("step={name} status=fail detail=\"{err}\"");
                failed = true;
            }
        }
    }
    if failed {
        exit(1);
    }
}

/// The `scenario.json` files under the corpus, sorted for a stable report
/// order. A scenario directory is defined by carrying one; capture-fixture
/// version directories do not, and are skipped by the same rule.
fn scenario_files(corpus: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(corpus)
        .map_err(|err| format!("listing {} failed: {err}", corpus.display()))?;
    let mut scenarios = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|err| format!("reading {} failed: {err}", corpus.display()))?;
        let scenario = entry.path().join("scenario.json");
        if scenario.is_file() {
            scenarios.push(scenario);
        }
    }
    scenarios.sort();
    Ok(scenarios)
}
