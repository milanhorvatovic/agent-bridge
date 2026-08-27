//! The runtime binary — the one artifact this project distributes.
//!
//! It brings up the layers below it and serves the JSON-RPC surface on stdio.
//! Startup follows a fixed sequence: parse argv, load configuration, capture
//! stdout for the wire and repoint descriptor 1 at the null sink, initialize
//! logging, register adapters, acquire the single-instance lock, and serve.
//! The lifecycle contract it keeps is the operator's: on stdin EOF, a
//! `runtime.shutdown`, or a termination signal, the runtime records operator
//! intent in the lockfile *before* it drains, so a supervisor can always tell
//! an intended stop from a crash — and it removes the lock only on a clean
//! exit.
//!
//! stdout belongs to the protocol. This binary never writes to it directly;
//! the transport's framer owns the captured copy, structured logs go to
//! stderr, and a stray print from anywhere in the process reaches the null sink
//! descriptor 1 is repointed at — never the wire.

// Not `forbid(unsafe_code)`, unlike most of the workspace: the single-instance
// lock takes an operating-system lock on its file (`flock` on POSIX), which is
// below what the standard library exposes. The unsafety is confined to the unix
// branch of `lockfile`, one call behind a safe function; the Windows branch
// needs none.

mod config;
mod fixture;
mod lockfile;
mod paths;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use agent_bridge_core::{BusConfig, EventBus, RegistryConfig, SessionConfig, SessionRegistry};
use agent_bridge_transport::{
    RuntimeContext, RuntimeInfoRef, ServeControl, ServeOutcome, StdoutRedirect, capture_stdout,
    defaults, rfc3339_now, serve,
};
use anyhow::Context;
use lockfile::{LockError, Lockfile, SECOND_INSTANCE_EXIT_CODE};

/// Exit code for a clean, drained shutdown.
const EXIT_CLEAN: u8 = 0;
/// Exit code for a startup failure the runtime could not recover from.
const EXIT_FAILURE: u8 = 1;
/// Exit code for a usage error in the arguments.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            // Before logging is up, and for a failure that prevents it, stderr
            // is the channel — never stdout, which is the wire.
            eprintln!("agent-bridge: {error:#}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// The startup sequence and serve, returning the process exit code.
fn run() -> anyhow::Result<u8> {
    // Step 1 — argv.
    let args = match Args::parse() {
        Ok(args) => args,
        Err(usage) => {
            eprintln!("agent-bridge: {usage}");
            return Ok(EXIT_USAGE);
        }
    };

    // Step 2 — configuration (before logging: it carries the log level). The
    // config path comes from `--config`, else the `AGENT_BRIDGE_CONFIG`
    // environment variable, else the OS default location; the flag wins over
    // the variable, and either being set makes the file required to exist.
    let config_path = args.config.clone().or_else(|| {
        std::env::var_os("AGENT_BRIDGE_CONFIG")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    });
    let loaded = config::load(config_path.as_deref())?;
    let config = loaded.config;
    let log_level = args
        .log_level
        .clone()
        .unwrap_or_else(|| config.log_level.clone());

    // Step 3 — capture stdout for the wire and repoint descriptor 1 at the
    // null sink, so a stray write from anywhere in the process is discarded
    // rather than corrupting the wire. Structured logs go to stderr (below), so
    // descriptor 1 carries nothing worth keeping; discarding it is the wire's
    // guarantee. Done before logging and everything after it.
    let wire = capture_stdout(StdoutRedirect::Discard)
        .context("capturing stdout for the JSON-RPC wire")?;

    // Step 4 — logging, to stderr as structured records (never stdout, which is
    // the wire). Readiness is the first line written; a supervisor that needs
    // more polls `runtime.health` later.
    init_logging(&log_level);

    // A test-only hook that proves the capture holds: when asked, attempt a
    // direct library-level stdout write. It must go to the null sink descriptor
    // 1 was repointed at, never the wire the framer owns — a client's framed
    // exchange staying valid is the proof the write did not reach the wire.
    // Gated behind an environment variable no normal run sets.
    if std::env::var_os("AGENT_BRIDGE_SELFTEST_STRAY_STDOUT").is_some() {
        use std::io::Write;
        let mut stdout = std::io::stdout();
        let _ = stdout.write_all(b"STRAY-STDOUT-MUST-NOT-REACH-THE-WIRE\n");
        let _ = stdout.flush();
    }

    // Step 6 partway — acquire the single-instance lock before serving. A live
    // second instance is refused with the reserved exit code; a lock left by a
    // crashed instance does not block us, because the operating system released
    // it when that process died.
    let lock = match Lockfile::acquire(&paths::lockfile_path(&args.instance), rfc3339_now()) {
        Ok(lock) => Arc::new(lock),
        Err(LockError::AlreadyRunning { path }) => {
            tracing::error!(path = %path.display(), "another instance is already running");
            eprintln!(
                "agent-bridge: another instance is already running (holds {})",
                path.display()
            );
            return Ok(u8::try_from(SECOND_INSTANCE_EXIT_CODE).unwrap_or(EXIT_FAILURE));
        }
        Err(error) => return Err(error.into()),
    };

    // The async runtime hosts the registry's tasks, the session actors, the
    // serve loop, and the signal handlers.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the async runtime")?;

    let outcome = runtime.block_on(serve_runtime(
        &args,
        &config,
        wire,
        Arc::clone(&lock),
        loaded.warnings,
    ));

    // The serve loop has ended; the exit contract below is all that remains.
    // Detach the runtime rather than dropping it inline: `tokio::io::stdin`
    // reads on a blocking thread that stays parked in a `read` syscall until
    // its input closes, and a runtime drop waits for that thread — so a caller
    // that shut the runtime down over `runtime.shutdown` while holding its end
    // of the pipe open would wedge the exit. Backgrounding the runtime lets the
    // process exit; the parked thread goes with it.
    runtime.shutdown_background();

    // Step 7 aftermath — the exit contract. A clean drain removes the lock and
    // exits zero; a die-loudly or a protocol close leaves the lock (no operator
    // intent was recorded) and exits non-zero, so a supervisor restarts.
    match outcome {
        ServeOutcome::Drained => {
            if let Err(error) = lock.remove() {
                tracing::error!(%error, "failed to remove the lockfile on clean exit");
            }
            Ok(EXIT_CLEAN)
        }
        ServeOutcome::StdoutBlocked => {
            tracing::error!("stdout blocked: the caller stopped reading; exiting");
            Ok(EXIT_FAILURE)
        }
        ServeOutcome::ProtocolClosed => {
            tracing::error!("the transport closed on a protocol violation; exiting");
            Ok(EXIT_FAILURE)
        }
    }
}

/// Build the runtime context, wire up signals, and serve until an end
/// condition. Runs inside the async runtime because the registry spawns tasks.
async fn serve_runtime(
    args: &Args,
    config: &config::Config,
    wire: tokio::fs::File,
    lock: Arc<Lockfile>,
    config_warnings: Vec<String>,
) -> ServeOutcome {
    // Step 5 — the registry and its one adapter. The bus carries session
    // events; the registry mints and hosts sessions.
    let bus = EventBus::new(BusConfig::default());
    let mut session_config = SessionConfig::new(paths::state_dir(&args.instance));
    session_config.stdin_drain = config.stdin_drain;
    let registry = SessionRegistry::new(bus.clone(), RegistryConfig::new(session_config));
    registry.register_adapter(
        "fixture",
        Arc::new(fixture::FixtureAdapter::new(
            config.fixture_cli_path.clone(),
            config.fixture_scenario.clone(),
        )),
    );

    let ctx = RuntimeContext {
        registry,
        bus,
        info: RuntimeInfoRef {
            version: env!("CARGO_PKG_VERSION").to_string(),
            adapters: vec!["fixture".to_string()],
            capabilities: vec!["session.attach".to_string()],
            schema_version: agent_bridge_events::SCHEMA_VERSION,
        },
    };

    // The shutdown channel: the dispatcher flips it for `runtime.shutdown`, and
    // the signal handlers below flip it for SIGTERM / SIGINT (or Ctrl-C on
    // Windows). The serve loop watches a receiver derived from it.
    let (shutdown, _rx) = tokio::sync::watch::channel(false);
    spawn_signal_handlers(shutdown.clone());

    // Step 7 — the readiness line, then serve. It is the first log line a
    // supervisor waits on, so it comes before the deferred config warnings
    // rather than after them.
    tracing::info!(
        version = ctx.info.version,
        adapters = ?ctx.info.adapters,
        schema_version = ctx.info.schema_version,
        "agent-bridge runtime ready",
    );
    for warning in config_warnings {
        tracing::warn!("{warning}");
    }

    let control = ServeControl {
        shutdown,
        drain_grace: config.stdin_drain,
        stdout_deadline: defaults::STDOUT_DEADLINE,
        max_frame_bytes: config.max_frame_bytes,
    };
    let intent_lock = Arc::clone(&lock);
    serve(ctx, tokio::io::stdin(), wire, control, move || {
        // Recorded before the drain begins, on every operator path. The failure
        // is returned, not swallowed: `serve` logs it on the shutdown path.
        intent_lock
            .write_operator_intent()
            .map_err(std::io::Error::other)
    })
    .await
}

/// Flip the shutdown flag when a termination signal arrives. POSIX forwards
/// SIGTERM and SIGINT; Windows has no catchable terminate, so Ctrl-C is the
/// operator path and `runtime.shutdown` covers the rest.
fn spawn_signal_handlers(shutdown: tokio::sync::watch::Sender<bool>) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
            let shutdown = shutdown.clone();
            match signal(kind) {
                Ok(mut stream) => {
                    tokio::spawn(async move {
                        if stream.recv().await.is_some() {
                            let _ = shutdown.send(true);
                        }
                    });
                }
                // A handler that cannot register means that signal reverts to
                // its default disposition — an abrupt, crash-class exit with no
                // drain and no operator intent. Say so loudly rather than let it
                // fail silently.
                Err(error) => {
                    tracing::error!(%error, "could not install a shutdown signal handler");
                }
            }
        }
    }
    #[cfg(windows)]
    {
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = shutdown.send(true);
            }
        });
    }
}

/// Bring up structured logging to stderr. Never stdout — that is the wire.
/// `RUST_LOG` wins when set, else the resolved level.
fn init_logging(level: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .json()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .try_init();
}

/// The parsed command line: the three flags the design's startup names.
#[derive(Debug, Default)]
struct Args {
    config: Option<PathBuf>,
    log_level: Option<String>,
    instance: String,
}

impl Args {
    /// Parse argv, or return a usage message. Accepts `--flag value` and
    /// `--flag=value`; an unknown flag or a missing value is a usage error.
    fn parse() -> Result<Self, String> {
        let mut args = Args {
            instance: paths::DEFAULT_INSTANCE.to_string(),
            ..Args::default()
        };
        let mut argv = std::env::args().skip(1);
        while let Some(arg) = argv.next() {
            let (flag, inline) = match arg.split_once('=') {
                Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
                None => (arg.clone(), None),
            };
            let mut value = || {
                inline
                    .clone()
                    .or_else(|| argv.next())
                    .ok_or_else(|| format!("{flag} requires a value"))
            };
            match flag.as_str() {
                "--config" => args.config = Some(PathBuf::from(value()?)),
                "--log-level" => args.log_level = Some(value()?),
                "--instance" => args.instance = value()?,
                "--help" | "-h" => {
                    return Err(
                        "usage: agent-bridge [--config <path>] [--log-level <level>] \
                         [--instance <name>]"
                            .to_string(),
                    );
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        // The instance name is a path segment scoping the state, log, and lock
        // locations, so it is held to a safe charset: a name with a separator
        // or `..` would escape or collide the intended `agent-bridge/<name>/`
        // scope rather than isolate it.
        if args.instance.is_empty() {
            return Err("--instance must not be empty".to_string());
        }
        if !args
            .instance
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err("--instance may contain only letters, digits, '-', and '_'".to_string());
        }
        Ok(args)
    }
}
