//! Where the runtime keeps its on-disk state, per OS convention.
//!
//! Two roots: config (read at startup) and state (logs and the lockfile).
//! Both follow the platform's own conventions rather than a single
//! `~/.agent-bridge` — the shape a prior draft used and which is wrong on
//! macOS and Windows. An `--instance <name>` scopes the state root so a second
//! runtime can run isolated for CI, and the default instance is `default`.

use std::path::PathBuf;

/// The default instance name, when `--instance` is not given.
pub const DEFAULT_INSTANCE: &str = "default";

/// The directory holding one instance's logs and lockfile.
///
/// Linux: `${XDG_STATE_HOME:-~/.local/state}/agent-bridge/<instance>/logs/`.
/// macOS: `~/Library/Logs/agent-bridge/<instance>/`. Windows:
/// `%LOCALAPPDATA%\agent-bridge\<instance>\logs\`. The instance segment is
/// what keeps two runtimes' logs and locks from colliding.
#[must_use]
pub fn state_dir(instance: &str) -> PathBuf {
    let mut dir = state_root();
    dir.push("agent-bridge");
    dir.push(instance);
    #[cfg(not(target_os = "macos"))]
    dir.push("logs");
    dir
}

/// The lockfile path for one instance — `runtime.lock`, beside the logs.
#[must_use]
pub fn lockfile_path(instance: &str) -> PathBuf {
    state_dir(instance).join("runtime.lock")
}

/// The default config path, `<config-root>/agent-bridge/config.toml`. Startup
/// overrides it with `--config` or `AGENT_BRIDGE_CONFIG`, so this is only the
/// fallback discovery location; a missing file there is not an error.
#[must_use]
pub fn default_config_path() -> PathBuf {
    let mut dir = config_root();
    dir.push("agent-bridge");
    dir.push("config.toml");
    dir
}

/// The platform state root, before the `agent-bridge/<instance>` segments.
fn state_root() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        env_path("XDG_STATE_HOME").unwrap_or_else(|| home().join(".local").join("state"))
    }
    #[cfg(target_os = "macos")]
    {
        home().join("Library").join("Logs")
    }
    #[cfg(windows)]
    {
        env_path("LOCALAPPDATA").unwrap_or_else(|| home().join("AppData").join("Local"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        env_path("XDG_STATE_HOME").unwrap_or_else(|| home().join(".local").join("state"))
    }
}

/// The platform config root, before the `agent-bridge` segment.
fn config_root() -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config"))
    }
    #[cfg(target_os = "macos")]
    {
        home().join("Library").join("Application Support")
    }
    #[cfg(windows)]
    {
        env_path("APPDATA").unwrap_or_else(|| home().join("AppData").join("Roaming"))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config"))
    }
}

/// A non-empty path from an environment variable, or `None`. Empty is treated
/// as unset, the shell convention.
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The user's home directory, from the platform's conventional variable.
/// Falls back to the current directory so a runtime in an environment with no
/// home still starts, keeping its state local rather than failing.
fn home() -> PathBuf {
    #[cfg(windows)]
    let key = "USERPROFILE";
    #[cfg(not(windows))]
    let key = "HOME";
    env_path(key).unwrap_or_else(|| PathBuf::from("."))
}
