//! The runtime binary over a real process boundary.
//!
//! Each check spawns the actual `agent-bridge` binary, its stdio a real pipe,
//! and speaks framed JSON-RPC to it with the transport's own client — the same
//! bytes an external consumer would. The binary's on-disk state is redirected
//! into a per-check temporary directory (by pointing the platform's state and
//! config variables at it), so the lockfile, the log, and the second-instance
//! refusal can be observed without touching the developer's real config.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agent_bridge_transport::{Client, defaults};
use serde_json::json;
use tokio::process::{Child, Command};

/// How long a check waits for the runtime to start, answer, or exit.
const PATIENCE: Duration = Duration::from_secs(20);

/// The framed client over a spawned runtime's stdio.
type RuntimeClient = Client<tokio::process::ChildStdout, tokio::process::ChildStdin>;

/// A spawned runtime with its state directory isolated under `root`.
struct Runtime {
    child: Child,
    root: PathBuf,
    instance: String,
}

impl Runtime {
    /// Spawn the runtime for `instance`, with its state and config rooted at a
    /// fresh temporary directory, and `extra_env` applied on top.
    fn spawn(tag: &str, instance: &str, extra_env: &[(&str, &str)]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "agent-bridge-runtime-{tag}-{}-{instance}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("the state root");

        let mut command = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
        command
            .arg("--instance")
            .arg(instance)
            // Point every platform's state and config root at the temp dir, so
            // the check owns the lockfile, the log, and config discovery.
            .env("HOME", &root)
            .env("USERPROFILE", &root)
            .env("XDG_STATE_HOME", &root)
            .env("XDG_CONFIG_HOME", &root)
            .env("LOCALAPPDATA", &root)
            .env("APPDATA", &root)
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let child = command.spawn().expect("the runtime binary must spawn");
        Self {
            child,
            root,
            instance: instance.to_string(),
        }
    }

    /// A framed client over the runtime's stdio. Takes the pipe handles, so it
    /// is called once.
    fn client(&mut self) -> RuntimeClient {
        let stdout = self.child.stdout.take().expect("stdout piped");
        let stdin = self.child.stdin.take().expect("stdin piped");
        Client::new(stdout, stdin, defaults::MAX_FRAME_BYTES)
    }

    /// Wait for the lockfile to appear — startup has reached the point of
    /// holding the single-instance lock.
    async fn await_lockfile(&self) -> PathBuf {
        let deadline = tokio::time::Instant::now() + PATIENCE;
        loop {
            if let Some(path) = find_file(&self.root, "runtime.lock") {
                return path;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the runtime never wrote its lockfile"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Wait for the process to exit and return its code.
    async fn wait_code(&mut self) -> Option<i32> {
        tokio::time::timeout(PATIENCE, self.child.wait())
            .await
            .expect("the runtime must exit within patience")
            .expect("waiting on the runtime")
            .code()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Never leave a child or its state behind, even on a failed assertion.
        let _ = self.child.start_kill();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Recursively find the first file named `name` under `root`.
fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|file| file == name) {
            return Some(path);
        }
    }
    None
}

/// `runtime.info` reports the runtime, and `runtime.shutdown` drains it to a
/// clean exit that removes the lockfile.
#[tokio::test]
async fn info_then_shutdown_exits_clean_and_removes_the_lock() {
    let mut runtime = Runtime::spawn("info", "alpha", &[]);
    let lock = runtime.await_lockfile().await;
    let mut client = runtime.client();

    let info = client
        .call(json!(1), "runtime.info", json!({}))
        .await
        .expect("framing")
        .expect("runtime.info result");
    assert_eq!(
        info["schema_version"],
        json!(agent_bridge_events::SCHEMA_VERSION)
    );
    assert_eq!(info["adapters"], json!(["fixture"]));

    let ack = client
        .call(json!(2), "runtime.shutdown", json!({}))
        .await
        .expect("framing")
        .expect("shutdown result");
    assert_eq!(ack["ok"], json!(true));

    assert_eq!(runtime.wait_code().await, Some(0), "a clean drain exits 0");
    assert!(!lock.exists(), "a clean exit removes the lockfile");
}

/// Closing stdin drains the runtime to a clean exit — the operator path a
/// caller takes by dropping its end of the pipe.
#[tokio::test]
async fn stdin_eof_drains_and_exits_clean() {
    let mut runtime = Runtime::spawn("eof", "beta", &[]);
    let lock = runtime.await_lockfile().await;
    // Dropping stdin closes the runtime's inbound stream — its stdin EOF.
    drop(runtime.child.stdin.take());
    assert_eq!(runtime.wait_code().await, Some(0));
    assert!(!lock.exists(), "the clean exit removed the lockfile");
}

/// The runtime reads its config from `AGENT_BRIDGE_CONFIG`, and a
/// `config_version` it does not understand fails startup before it locks or
/// serves — so no lockfile is left behind.
#[tokio::test]
async fn a_config_from_the_env_var_with_a_future_version_fails_startup() {
    let root = std::env::temp_dir().join(format!("agent-bridge-cfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the config root");
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, "config_version = 999\n").expect("write the config");

    let code = tokio::time::timeout(
        PATIENCE,
        Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
            .arg("--instance")
            .arg("zeta")
            .env("HOME", &root)
            .env("XDG_STATE_HOME", &root)
            .env("LOCALAPPDATA", &root)
            .env("AGENT_BRIDGE_CONFIG", &config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn")
            .wait(),
    )
    .await
    .expect("startup must fail promptly")
    .expect("waiting on the runtime")
    .code();
    assert_eq!(code, Some(1), "a future config_version fails startup");
    assert!(
        find_file(&root, "runtime.lock").is_none(),
        "a config rejected before the lock leaves none behind"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// `--help` is a request, not a usage error: it prints usage and exits zero,
/// where a malformed command line exits with the usage code. It also returns
/// before the lock or the wire is touched, so it needs no state root.
#[tokio::test]
async fn help_is_a_clean_exit_not_a_usage_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("--help")
        .output()
        .await
        .expect("the runtime binary must spawn");
    assert!(
        output.status.success(),
        "--help must exit zero, got {:?}",
        output.status.code()
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("usage: agent-bridge"),
        "--help prints the usage line"
    );
}

/// A second instance under the same name refuses to start with exit code 4,
/// while the first keeps running.
#[tokio::test]
async fn a_second_instance_refuses_with_exit_code_4() {
    let mut first = Runtime::spawn("second", "gamma", &[]);
    first.await_lockfile().await;

    // Same instance name, same state root — the isolation is by root, so the
    // second must share it to contend for the lock.
    let mut second = Command::new(env!("CARGO_BIN_EXE_agent-bridge"));
    second
        .arg("--instance")
        .arg(&first.instance)
        .env("HOME", &first.root)
        .env("USERPROFILE", &first.root)
        .env("XDG_STATE_HOME", &first.root)
        .env("XDG_CONFIG_HOME", &first.root)
        .env("LOCALAPPDATA", &first.root)
        .env("APPDATA", &first.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut second = second.spawn().expect("the second instance spawns");
    let code = tokio::time::timeout(PATIENCE, second.wait())
        .await
        .expect("the second instance exits promptly")
        .expect("waiting on the second instance")
        .code();
    assert_eq!(
        code,
        Some(4),
        "a live second instance is refused with exit 4"
    );

    // The first is unaffected and shuts down cleanly.
    let mut client = first.client();
    let _ = client.call(json!(1), "runtime.shutdown", json!({})).await;
    assert_eq!(first.wait_code().await, Some(0));
}

/// A stray library-level stdout write never reaches the wire. The runtime is
/// launched with a hook that attempts a direct stdout write at startup; the
/// framed exchange still succeeds, which is the proof — a stray byte on the
/// wire would have corrupted framing and failed the call.
#[tokio::test]
async fn a_stray_stdout_write_never_reaches_the_wire() {
    let mut runtime = Runtime::spawn(
        "stdio",
        "delta",
        &[("AGENT_BRIDGE_SELFTEST_STRAY_STDOUT", "1")],
    );
    runtime.await_lockfile().await;
    let mut client = runtime.client();
    // The exchange succeeding at all proves the wire carried only valid frames:
    // a stray write on it would have corrupted framing and failed this call.
    let info = client
        .call(json!(1), "runtime.info", json!({}))
        .await
        .expect("the wire must stay valid frames despite the stray write")
        .expect("runtime.info result");
    assert!(info.get("version").is_some());

    let _ = client.call(json!(2), "runtime.shutdown", json!({})).await;
    assert_eq!(runtime.wait_code().await, Some(0));
}

/// A SIGKILL leaves the lockfile without operator intent, so a supervisor
/// reads it as a crash and restarts — the contract that distinguishes a kill
/// from an intended stop. POSIX only: Windows has no SIGKILL equivalent to
/// deliver here.
#[cfg(unix)]
#[tokio::test]
async fn a_sigkilled_runtime_leaves_no_operator_intent() {
    let mut runtime = Runtime::spawn("sigkill", "epsilon", &[]);
    let lock = runtime.await_lockfile().await;

    runtime.child.start_kill().expect("SIGKILL the runtime");
    let _ = runtime.child.wait().await;

    // The lock survives a kill, and it never gained operator intent — nothing
    // on the kill path records one.
    let body: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&lock).expect("the lock survives a kill"))
            .expect("the lock is valid JSON");
    assert!(
        body["shutdown_intent"].is_null(),
        "a SIGKILL must not record operator intent: {body}"
    );
}
