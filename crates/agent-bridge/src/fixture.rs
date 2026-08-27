//! The fixture adapter: the runtime's one registered adapter in Phase 1.
//!
//! The real adapters (`claude`, `codex`) are a later phase; until they exist,
//! the runtime still needs *an* adapter so the wire is a runnable, demonstrable
//! thing — spawn it, create a session, stream events, close it. That adapter
//! launches the deterministic scripted stand-in CLI, `fake-cli`, on a scenario
//! the config names. It is a development and conformance adapter, not a
//! product one: `fake-cli` is not a distributed binary, so the adapter resolves
//! it beside the runtime binary the way the conformance tooling does.

use std::path::{Path, PathBuf};

use agent_bridge_core::{AdapterSeam, CreateOptions, LaunchSpec, ShutdownHint};

/// Launches `fake-cli` on a configured scenario as the session's child.
pub struct FixtureAdapter {
    cli: PathBuf,
    scenario: Option<PathBuf>,
}

impl FixtureAdapter {
    /// Build the adapter, resolving the CLI path: the explicit
    /// `adapters.fixture.cli_path` when set, else `fake-cli` beside the running
    /// binary. `scenario` is the scenario file a create will run; without one,
    /// a create fails at launch with a readable error rather than hanging.
    pub fn new(cli_path: Option<PathBuf>, scenario: Option<PathBuf>) -> Self {
        Self {
            cli: cli_path.unwrap_or_else(fake_cli_beside_runtime),
            scenario,
        }
    }
}

impl AdapterSeam for FixtureAdapter {
    fn launch_spec(&self, _options: &CreateOptions) -> LaunchSpec {
        let mut launch = LaunchSpec::new(&self.cli);
        if let Some(scenario) = &self.scenario {
            launch.args = vec![scenario.display().to_string()];
        }
        launch
    }

    fn shutdown_hint(&self) -> ShutdownHint {
        // `fake-cli` ends on its scripted `exit` step or when its input closes;
        // the terminal layer has no per-direction close yet, so this hint is
        // recorded but undeliverable and the close proceeds through the drain
        // window and termination. The fixture's scenarios are short-lived, so
        // the escalation is immediate in practice.
        ShutdownHint::CloseStdin
    }
}

/// `fake-cli` next to the runtime binary — the same current-exe-relative
/// resolution the conformance tooling uses, tolerant of the `deps/`
/// subdirectory a test binary runs from.
fn fake_cli_beside_runtime() -> PathBuf {
    let mut dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    if dir.file_name().is_some_and(|name| name == "deps") {
        dir.pop();
    }
    dir.join(format!("fake-cli{}", std::env::consts::EXE_SUFFIX))
}
