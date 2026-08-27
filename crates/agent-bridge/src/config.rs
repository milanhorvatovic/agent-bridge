//! The startup configuration: a small TOML file, validated without launching
//! a session.
//!
//! Only the keys this PR's runtime actually consumes are read; the rest of the
//! documented schema is tolerated so a fuller config does not fail an early
//! runtime. Two rules are enforced: a `config_version` the runtime does not
//! understand is rejected outright (a newer file against an older binary is a
//! mistake worth stopping for), and an unrecognized top-level key is warned
//! about but tolerated, so the schema can grow without breaking a pinned
//! runtime.
//!
//! Warnings are returned rather than logged here: config load runs before
//! logging is initialized (the log level it carries is one of its outputs), so
//! the caller emits them once the log is up.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};

use crate::paths;

/// The `config_version` this runtime understands. A file declaring a higher
/// one is rejected.
const SUPPORTED_CONFIG_VERSION: i64 = 1;

/// The largest `transport.max_frame_bytes` accepted — 1 GiB, far above the
/// 16 MiB default and any real need, and comfortably under the bounded
/// writer's own overflow ceiling. Bounds the value into the range the runtime
/// can stand up without a downstream assertion firing.
const MAX_FRAME_CEILING: usize = 1 << 30;

/// The smallest `transport.max_frame_bytes` accepted — 4 KiB. This value is
/// also the bounded writer's capacity, and the writer seals once an outbound
/// frame passes four times that. A cap of a few bytes would therefore let the
/// very first framed response outgrow the ceiling and report a blocked wire to
/// a parent that is reading fine; the floor keeps room for any control frame
/// the runtime emits, well within that headroom.
const MIN_FRAME_BYTES: usize = 4 * 1024;

/// The largest `transport.stdin_drain_seconds` accepted — one day, the session
/// layer's own deadline ceiling. Past it the drain would panic the actor's
/// deadline arithmetic.
const MAX_DRAIN_SECONDS: u64 = 86_400;

/// The top-level keys the documented schema defines. An unknown one is warned
/// about, not fatal.
const KNOWN_TOP_LEVEL_KEYS: &[&str] = &[
    "config_version",
    "runtime",
    "transport",
    "stream",
    "pty",
    "logs",
    "security",
    "adapters",
];

/// The consumed configuration. Fields not yet wired into the runtime are
/// deliberately absent rather than parsed-and-ignored.
#[derive(Debug, Clone)]
pub struct Config {
    /// The runtime log level (`runtime.log_level`), overridable by
    /// `--log-level`.
    pub log_level: String,
    /// The maximum wire frame body (`transport.max_frame_bytes`).
    pub max_frame_bytes: usize,
    /// The stdin-drain grace (`transport.stdin_drain_seconds`).
    pub stdin_drain: Duration,
    /// The fixture adapter's scenario file (`adapters.fixture.scenario`); a
    /// create against the fixture adapter fails at launch without one.
    pub fixture_scenario: Option<PathBuf>,
    /// An explicit path to the fixture CLI (`adapters.fixture.cli_path`);
    /// resolved beside the runtime binary when absent.
    pub fixture_cli_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: "info".to_string(),
            max_frame_bytes: agent_bridge_transport::defaults::MAX_FRAME_BYTES,
            stdin_drain: agent_bridge_transport::defaults::DRAIN_GRACE,
            fixture_scenario: None,
            fixture_cli_path: None,
        }
    }
}

/// A loaded config together with any non-fatal warnings, for the caller to log
/// once logging is up.
pub struct Loaded {
    /// The configuration.
    pub config: Config,
    /// Non-fatal notes — an unknown key, a missing version — surfaced after
    /// logging init.
    pub warnings: Vec<String>,
}

/// Load configuration.
///
/// `explicit` is the `--config`/`AGENT_BRIDGE_CONFIG` path, which must exist if
/// given; without it, the default OS location is tried and a missing file
/// there is not an error — the defaults stand. Malformed TOML, a
/// `config_version` outside the supported range, and a present section or
/// value of the wrong type are fatal; a merely unknown key or an absent
/// version degrades to a warning, and the default for it stands.
pub fn load(explicit: Option<&Path>) -> anyhow::Result<Loaded> {
    let (path, required) = match explicit {
        Some(path) => (path.to_path_buf(), true),
        None => (paths::default_config_path(), false),
    };

    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => {
            return Ok(Loaded {
                config: Config::default(),
                warnings: Vec::new(),
            });
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading config at {}", path.display()));
        }
    };

    let table: toml::Table = text
        .parse()
        .with_context(|| format!("parsing config at {}", path.display()))?;
    from_table(&table)
}

/// Build a config from a parsed table, enforcing the version rule and
/// collecting warnings. Split out so the validation is testable without a
/// file.
fn from_table(table: &toml::Table) -> anyhow::Result<Loaded> {
    let mut warnings = Vec::new();

    match table.get("config_version") {
        // Present and integer: it must fall in the supported range. Version 1
        // is the first and only schema, so 0 and any negative are as invalid as
        // a version past the ceiling — none of them run under v1 semantics.
        Some(value) if value.as_integer().is_some() => {
            let version = value.as_integer().unwrap_or_default();
            if !(1..=SUPPORTED_CONFIG_VERSION).contains(&version) {
                bail!(
                    "config_version {version} is not supported; this runtime supports \
                     1..={SUPPORTED_CONFIG_VERSION} (upgrade the runtime or pin the config)"
                );
            }
        }
        // Present but not an integer: a string or float here would otherwise
        // read as "absent" and silently bypass the ceiling check — a config
        // authored for a future runtime (`config_version = "2"` / `2.0`) must
        // be refused, not run against an older binary.
        Some(value) => bail!("config_version must be an integer, found {value}"),
        None => warnings.push(
            "config has no config_version; assuming 1 — future runtimes may require it".to_string(),
        ),
    }

    for key in table.keys() {
        if !KNOWN_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            warnings.push(format!(
                "config has an unknown top-level key `{key}`; ignoring it"
            ));
        }
    }

    let mut config = Config::default();
    if let Some(runtime) = table.get("runtime") {
        // A present section of the wrong type, or a consumed value of the wrong
        // type, is refused rather than silently ignored and defaulted — the
        // same contract the transport section keeps.
        let runtime = runtime
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("runtime must be a table"))?;
        if let Some(value) = runtime.get("log_level") {
            config.log_level = value
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("runtime.log_level must be a string"))?
                .to_string();
        }
    }
    if let Some(transport) = table.get("transport") {
        // A present `transport` must be a table; a scalar or array there is
        // refused rather than silently treated as absent and run with the
        // default frame and drain limits — the same contract its values keep.
        let transport = transport
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("transport must be a table"))?;
        // A present-but-invalid value is refused with a clear message rather
        // than silently defaulted or — worse — passed through to panic a
        // downstream assertion or seal the wire. Below the floor, the value
        // doubles as the writer capacity and the first response would outgrow
        // its 4x ceiling; an absurd value would trip the overflow-ceiling
        // assert. Both ends are held to the runnable range.
        if let Some(value) = transport.get("max_frame_bytes") {
            let bytes = value
                .as_integer()
                .and_then(|integer| usize::try_from(integer).ok())
                .filter(|&bytes| (MIN_FRAME_BYTES..=MAX_FRAME_CEILING).contains(&bytes))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "transport.max_frame_bytes must be an integer between \
                         {MIN_FRAME_BYTES} and {MAX_FRAME_CEILING}"
                    )
                })?;
            config.max_frame_bytes = bytes;
        }
        // Bounded by the session layer's own deadline ceiling of one day; past
        // it, the drain would panic the session actor's deadline arithmetic.
        if let Some(value) = transport.get("stdin_drain_seconds") {
            let seconds = value
                .as_integer()
                .and_then(|integer| u64::try_from(integer).ok())
                .filter(|&seconds| seconds <= MAX_DRAIN_SECONDS)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "transport.stdin_drain_seconds must be an integer between 0 and \
                         {MAX_DRAIN_SECONDS}"
                    )
                })?;
            config.stdin_drain = Duration::from_secs(seconds);
        }
    }
    if let Some(adapters) = table.get("adapters") {
        let adapters = adapters
            .as_table()
            .ok_or_else(|| anyhow::anyhow!("adapters must be a table"))?;
        if let Some(fixture) = adapters.get("fixture") {
            let fixture = fixture
                .as_table()
                .ok_or_else(|| anyhow::anyhow!("adapters.fixture must be a table"))?;
            if let Some(value) = fixture.get("scenario") {
                let scenario = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("adapters.fixture.scenario must be a string"))?;
                config.fixture_scenario = Some(PathBuf::from(scenario));
            }
            // An empty `cli_path` is treated as unset — resolved beside the
            // runtime binary — but a non-string is a malformed value, refused.
            if let Some(value) = fixture.get("cli_path") {
                let cli_path = value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("adapters.fixture.cli_path must be a string"))?;
                if !cli_path.is_empty() {
                    config.fixture_cli_path = Some(PathBuf::from(cli_path));
                }
            }
        }
    }

    Ok(Loaded { config, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_version_outside_the_supported_range_is_rejected() {
        // Version 1 is the first and only schema, so a newer version, zero, and
        // any negative are all fatal rather than silently run under v1.
        for version in ["2", "0", "-1"] {
            let table: toml::Table = format!("config_version = {version}").parse().unwrap();
            assert!(
                from_table(&table).is_err(),
                "config_version {version} must be refused"
            );
        }
    }

    #[test]
    fn a_non_integer_config_version_is_refused_not_ignored() {
        // A string or float version would otherwise read as "absent" and slip
        // past the ceiling check.
        for text in ["config_version = \"2\"", "config_version = 2.0"] {
            let table: toml::Table = text.parse().unwrap();
            assert!(from_table(&table).is_err(), "{text} must be refused");
        }
    }

    #[test]
    fn an_invalid_max_frame_bytes_is_refused_rather_than_crashing_later() {
        for text in [
            "config_version = 1\n[transport]\nmax_frame_bytes = 0",
            "config_version = 1\n[transport]\nmax_frame_bytes = -1",
            // Below the floor: a viable value must leave the writer's 4x
            // ceiling room for a response rather than sealing on the first one.
            "config_version = 1\n[transport]\nmax_frame_bytes = 100",
        ] {
            let table: toml::Table = text.parse().unwrap();
            assert!(from_table(&table).is_err(), "{text} must be refused");
        }
    }

    #[test]
    fn a_malformed_consumed_section_or_value_is_refused() {
        // Every consumed section and value keeps the same contract: a present
        // one of the wrong type is an error, not silently ignored and defaulted.
        for text in [
            "config_version = 1\nruntime = \"bad\"",
            "config_version = 1\n[runtime]\nlog_level = 3",
            "config_version = 1\nadapters = \"bad\"",
            "config_version = 1\n[adapters]\nfixture = \"bad\"",
            "config_version = 1\n[adapters.fixture]\nscenario = 3",
            "config_version = 1\n[adapters.fixture]\ncli_path = 3",
        ] {
            let table: toml::Table = text.parse().unwrap();
            assert!(from_table(&table).is_err(), "{text} must be refused");
        }
    }

    #[test]
    fn a_transport_section_that_is_not_a_table_is_refused() {
        // A present `transport` of the wrong type must be an error, not treated
        // as absent and run with defaults — the "present invalid values are
        // refused" contract covers the section's type, not only its values.
        let table: toml::Table = "config_version = 1\ntransport = \"nope\"".parse().unwrap();
        assert!(
            from_table(&table).is_err(),
            "a non-table transport must be refused"
        );
    }

    #[test]
    fn an_out_of_range_drain_is_refused() {
        let table: toml::Table = "config_version = 1\n[transport]\nstdin_drain_seconds = 100000"
            .parse()
            .unwrap();
        assert!(
            from_table(&table).is_err(),
            "a drain past one day is refused"
        );
    }

    #[test]
    fn an_unknown_top_level_key_warns_but_loads() {
        let table: toml::Table = "config_version = 1\n[nonsense]\nx = 1".parse().unwrap();
        let loaded = from_table(&table).unwrap();
        assert!(loaded.warnings.iter().any(|w| w.contains("nonsense")));
    }

    #[test]
    fn consumed_keys_are_read_and_the_rest_defaulted() {
        let table: toml::Table = "
            config_version = 1
            [runtime]
            log_level = \"debug\"
            [transport]
            stdin_drain_seconds = 5
            [adapters.fixture]
            scenario = \"/tmp/s.json\"
        "
        .parse()
        .unwrap();
        let config = from_table(&table).unwrap().config;
        assert_eq!(config.log_level, "debug");
        assert_eq!(config.stdin_drain, Duration::from_secs(5));
        assert_eq!(config.fixture_scenario, Some(PathBuf::from("/tmp/s.json")));
        assert_eq!(
            config.max_frame_bytes,
            agent_bridge_transport::defaults::MAX_FRAME_BYTES
        );
    }
}
