//! The parent half: spawning real sessions and reading what they emit.
//!
//! Nothing is mocked. Every scenario spawns an actual terminal with an
//! actual child, and the only test-owned piece is the [`Recorder`] —
//! the crate's own sink seam, which is a contract surface rather than a
//! mock of one: the core implements it over the bus, scenarios implement
//! it over a vector, and both see exactly the events the actor published.
//!
//! Scenarios run one after another, each on a runtime of its own, so a
//! task leaked by one cannot read a neighbour's state as its own.

#![allow(dead_code)]

pub mod fixture;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_bridge_events::{EventBody, EventKind};
use agent_bridge_session::{
    EventSink, InputStep, LaunchSpec, SessionConfig, SessionHandle, SessionId, SessionSpec,
    SessionState, ShutdownHint, SinkSealed, SubscriberId,
};

/// How long a scenario waits for something it expects. Generous, because a
/// loaded build machine is not a failing implementation.
pub const PATIENCE: Duration = Duration::from_secs(15);

/// One named check.
pub struct Scenario {
    pub name: &'static str,
    pub check: fn() -> Result<String, String>,
}

/// The explicit marker that turns this binary into a fixture: only the
/// scenarios' own spawns pass it, so every other argument — a filter, or
/// harness flags such as `--nocapture` — falls through to the suite
/// instead of being misread as a role.
pub const FIXTURE_ROLE_FLAG: &str = "--fixture-role";

/// Run the suite, or become a fixture when handed the role marker.
pub fn main(suite: &str, scenarios: &[Scenario]) {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(FIXTURE_ROLE_FLAG) {
        let Some(role) = args.get(1) else {
            eprintln!("{FIXTURE_ROLE_FLAG} requires a role name");
            std::process::exit(2);
        };
        fixture::run(role, &args[2..]);
    }
    #[cfg(target_os = "linux")]
    // Become the reaper for orphaned descendants: a killed fixture's
    // grandchild reparents to process one, and the container lane's
    // process one never collects it — after which every liveness question
    // answers "still here" for the rest of the run.
    // SAFETY: takes plain integers and touches no memory.
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }

    let mut failed = 0;
    for scenario in scenarios {
        let (status, detail) = match (scenario.check)() {
            Ok(detail) => ("pass", detail),
            Err(detail) => {
                failed += 1;
                ("fail", detail)
            }
        };
        println!(
            "{suite} step={} status={status} detail=\"{}\"",
            scenario.name,
            detail.replace(['"', '\n', '\r'], " ")
        );
    }
    println!("{suite} scenarios={} failed={failed}", scenarios.len());
    if failed > 0 {
        std::process::exit(1);
    }
}

/// Run one scenario body on a runtime of its own.
pub fn on_runtime<F>(body: F) -> Result<String, String>
where
    F: std::future::Future<Output = Result<String, String>>,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|err| format!("runtime: {err}"))?;
    let outcome = runtime.block_on(body);
    // Dropping the runtime aborts anything a session left behind, which is
    // the isolation the one-runtime-per-scenario rule promises.
    drop(runtime);
    outcome
}

/// One recorded publish: the stamped seq and the body's readable parts.
#[derive(Clone)]
pub struct Recorded {
    pub seq: u64,
    pub event_type: String,
    pub approval_id: Option<String>,
    pub kind: EventKind,
}

/// The sink seam over a vector: what the bus would receive, readable by
/// the scenario.
#[derive(Clone, Default)]
pub struct Recorder {
    inner: Arc<RecorderInner>,
}

#[derive(Default)]
struct RecorderInner {
    events: Mutex<Vec<Recorded>>,
    next_seq: AtomicU64,
    sealed: AtomicU64,
}

impl Recorder {
    pub fn events(&self) -> Vec<Recorded> {
        self.inner
            .events
            .lock()
            .expect("the recorder lock is never poisoned")
            .clone()
    }

    pub fn event_types(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .map(|recorded| recorded.event_type)
            .collect()
    }

    pub fn sealed_count(&self) -> u64 {
        self.inner.sealed.load(Ordering::Relaxed)
    }

    /// The `lifecycle.session.closed` payload, once it was published.
    pub fn closed_payload(&self) -> Option<agent_bridge_events::LifecycleSessionClosed> {
        self.events().into_iter().find_map(|recorded| {
            if let EventKind::LifecycleSessionClosed(payload) = recorded.kind {
                Some(payload)
            } else {
                None
            }
        })
    }
}

impl EventSink for Recorder {
    fn publish(&self, body: EventBody) -> Result<u64, SinkSealed> {
        if self.inner.sealed.load(Ordering::Relaxed) > 0 {
            return Err(SinkSealed);
        }
        let seq = self.inner.next_seq.fetch_add(1, Ordering::Relaxed);
        self.inner
            .events
            .lock()
            .expect("the recorder lock is never poisoned")
            .push(Recorded {
                seq,
                event_type: body.kind.event_type().to_owned(),
                approval_id: body.approval_id.clone(),
                kind: body.kind,
            });
        Ok(seq)
    }

    fn seal(&self) {
        self.inner.sealed.fetch_add(1, Ordering::Relaxed);
    }
}

/// A per-scenario workspace directory for session logs and pid files.
pub fn scratch_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("agent-bridge-session-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch dir must be creatable");
    dir
}

/// The hint the cooperative fixture honors: the exit command, a settle,
/// then Enter — the ordered-steps shape a raw-mode TUI needs.
pub fn cooperative_hint() -> ShutdownHint {
    ShutdownHint::Input(vec![
        InputStep::Write("exit".into()),
        InputStep::Settle(Duration::from_millis(100)),
        InputStep::Write("\r".into()),
    ])
}

/// A spec that launches this binary in the given fixture role.
pub fn fixture_spec(
    role: &str,
    extra: &[&str],
    hint: ShutdownHint,
    log_dir: PathBuf,
    tweak: impl FnOnce(&mut SessionConfig),
) -> SessionSpec {
    let own = std::env::current_exe().expect("this binary must be findable");
    let mut launch = LaunchSpec::new(own);
    launch.args = [FIXTURE_ROLE_FLAG, role]
        .into_iter()
        .map(ToString::to_string)
        .chain(extra.iter().map(ToString::to_string))
        .collect();
    // Wide, so a console cannot reflow a fixture's report line mid-field.
    launch.dimensions = Some((200, 50));
    let mut config = SessionConfig::new(log_dir);
    tweak(&mut config);
    SessionSpec {
        session_id: SessionId::new(),
        adapter: "fixture".to_string(),
        launch,
        shutdown_hint: hint,
        creator: Some(SubscriberId("peer-0".to_string())),
        config,
    }
}

/// Wait until the session reports `want`, or say where it stalled.
pub async fn wait_state(handle: &SessionHandle, want: SessionState) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        let state = handle.state();
        if state == want {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("state stalled at {state}, wanted {want}"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether a process still exists (a zombie counts — the reaping question
/// is asked separately by whoever owns the corpse).
#[cfg(unix)]
pub fn process_alive(pid: u32) -> bool {
    reap_orphans();
    // SAFETY: signal zero validates without delivering.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Collect any adopted corpses, so liveness questions see the living only.
#[cfg(unix)]
fn reap_orphans() {
    loop {
        // SAFETY: a non-blocking wait with a null status pointer, which is
        // permitted; this process's own children are reaped by the session
        // layer before any scenario asks.
        let collected = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
        if collected <= 0 {
            return;
        }
    }
}

#[cfg(windows)]
pub fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
    };
    // Opening is not the question — a terminated process can be opened for
    // as long as anything holds its object. Whether the handle is
    // signalled is.
    // SAFETY: plain arguments; the handle is checked before use and closed
    // on every path.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let exited = WaitForSingleObject(handle, 0) == WAIT_OBJECT_0;
        CloseHandle(handle);
        !exited
    }
}

/// Wait for a process to be gone for good.
pub async fn wait_until_gone(pid: u32) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if !process_alive(pid) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "process {pid} still running after the patience limit"
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
