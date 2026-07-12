//! The live `/exit` cleanup lane — closes the one gap the interactive
//! feasibility work left open. The hook half of a clean shutdown was
//! verified there (`SessionEnd` fires when `/exit` is typed), but the
//! sandbox denied the rig any view of what happens *next*, so "the process
//! actually terminates and the PTY tears down" remained asserted-by-design
//! rather than observed. This lane observes it, against the real CLI:
//!
//! 1. launch and establish a session through the shared rig, run one
//!    trivial prompt turn (shape-asserted, never content-asserted);
//! 2. type `/exit` and wait for `SessionEnd` — the structured proof the
//!    CLI accepted the command;
//! 3. poll the child PID to actual termination, with a deliberately
//!    generous ceiling, and report the `SessionEnd`-to-exit interval. A CLI
//!    that lingers past the ceiling is a *finding* about how long a
//!    shutdown drain needs — the interval is the datum either way;
//! 4. close the PTY through the deadlock-guarded teardown (the reader
//!    reaching end-of-stream is the "PTY released" proof — a leaked child
//!    holding the slave would stall exactly here);
//! 5. assert nothing survived: the child's process group is empty on
//!    POSIX (a session's subshells die with it), the job object the lane
//!    bound at launch is empty on Windows, and no ConPTY console host this
//!    process spawned outlives the run.
//!
//! Needs the real CLI and credentials, so it runs in the opt-in live CI
//! tier next to the `probe` and `fourpoint` lanes.

use std::time::Duration;

use crate::rig::{self, LiveSession, ProbeConfig, TURN_TIMEOUT, TYPE_SETTLE};
use crate::{Failure, inspect, print_step};

/// How long `/exit` gets to produce `SessionEnd` — the same budget the
/// rig's own graceful path allows.
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the process gets to actually leave after `SessionEnd`.
/// Deliberately generous: the point is to *measure* the interval, and a
/// failure here should mean "genuinely lingering", not "slow CI minute".
const TERMINATION_CEILING: Duration = Duration::from_secs(60);

/// How long the post-exit emptiness checks may poll — they ride out
/// orphan reaping, not real work.
const EMPTY_TIMEOUT: Duration = Duration::from_secs(10);

/// What the capture of this lane is labelled with.
const SCENARIO: &str =
    "clean shutdown: launch, one prompt turn, /exit, termination and teardown observed";

/// What contains the session's process tree on this platform — recorded
/// while the child is alive, checked after it is gone.
struct Containment {
    #[cfg(unix)]
    pgid: i32,
    #[cfg(windows)]
    job: inspect::Job,
}

/// The lane. Once the child exists it is torn down on every path out,
/// mirroring the sibling lanes' discipline.
pub fn run(config: &ProbeConfig) -> Result<(), Failure> {
    // The census must predate the PTY allocation inside launch: the ConPTY
    // console host this run is accountable for appears during it.
    #[cfg(windows)]
    let hosts_before = inspect::console_hosts_parented_here()
        .map_err(|detail| Failure::new("contain", 70, detail))?;

    let mut session = rig::launch(config)?;

    let containment = match record_containment(&session) {
        Ok(containment) => containment,
        Err(failure) => {
            session.abandon(SCENARIO, &failure);
            return Err(failure);
        }
    };

    if let Err(failure) = drive_to_termination(&mut session) {
        session.abandon(SCENARIO, &failure);
        return Err(failure);
    }

    // The PTY-released proof: the guarded master close, with the reader
    // reaching end-of-stream. A leaked slave holder would stall this step,
    // which is exactly the failure it exists to surface.
    session.conclude(SCENARIO)?;

    let empty_detail =
        await_tree_empty(&containment).map_err(|detail| Failure::new("tree_empty", 74, detail))?;
    print_step("tree_empty", "pass", &empty_detail);

    #[cfg(windows)]
    console_host_check(&hosts_before)?;
    #[cfg(not(windows))]
    console_host_check()?;
    Ok(())
}

/// Record what will be held accountable after exit: the child's process
/// group as the OS reports it (POSIX), or a freshly created-and-bound job
/// object (Windows). Bound immediately after spawn on purpose — children
/// the CLI spawns *after* the binding inherit membership, and the CLI does
/// its spawning later in the session.
fn record_containment(session: &LiveSession) -> Result<Containment, Failure> {
    let pid = session
        .child_pid()
        .ok_or_else(|| Failure::new("contain", 70, "the spawned child reports no pid"))?;
    #[cfg(unix)]
    {
        let pid = i32::try_from(pid)
            .map_err(|_| Failure::new("contain", 70, format!("pid {pid} does not fit i32")))?;
        let pgid = inspect::pgid_of(pid).map_err(|detail| Failure::new("contain", 70, detail))?;
        // Orphans of the CLI's tree must land somewhere that reaps, or an
        // environment without a reaping init (a container) would show them
        // as zombie survivors in the emptiness check.
        let orphans = inspect::adopt_orphans();
        print_step(
            "contain",
            "pass",
            &format!(
                "child pid {pid} leads process group {pgid} (the PTY spawn's setsid); group-scoped emptiness is checked after exit; {orphans}"
            ),
        );
        Ok(Containment { pgid })
    }
    #[cfg(windows)]
    {
        let job = inspect::Job::create_kill_on_close()
            .map_err(|detail| Failure::new("contain", 70, detail))?;
        job.assign(pid)
            .map_err(|detail| Failure::new("contain", 70, detail))?;
        print_step(
            "contain",
            "pass",
            &format!(
                "child pid {pid} bound to a KILL_ON_JOB_CLOSE job object moments after spawn; descendants spawned from here on inherit membership"
            ),
        );
        Ok(Containment { job })
    }
}

/// Establish → one trivial turn → `/exit` → `SessionEnd` → observed
/// termination, with the `SessionEnd`-to-exit interval reported.
fn drive_to_termination(session: &mut LiveSession) -> Result<(), Failure> {
    session.establish()?;

    let turn = session
        .run_turn("Reply with exactly: ok", TURN_TIMEOUT)
        .map_err(|detail| Failure::new("prompt_turn", 71, detail))?;
    print_step(
        "prompt_turn",
        "pass",
        &format!(
            "turn completed in {}ms (Stop observed; hooks: [{}])",
            turn.duration.as_millis(),
            turn.hook_names().join(", ")
        ),
    );

    let mark = session.hook_mark();
    session
        .writer
        .type_line("/exit", TYPE_SETTLE)
        .map_err(|err| Failure::new("exit_command", 72, format!("typing /exit failed: {err}")))?;
    // The arrival instant, not the wait's return: the hook-wait loop pumps
    // in ~100ms slices, and against a sub-second interval that slack would
    // be a visible measurement error.
    let (_, session_end_at) = session
        .wait_for_hook_arrival("SessionEnd", |_| true, mark, SESSION_END_TIMEOUT)
        .map_err(|detail| Failure::new("exit_command", 72, detail))?;
    print_step(
        "exit_command",
        "pass",
        "/exit typed; SessionEnd over the hook channel — the CLI accepted the shutdown",
    );

    // The other half of the claim, observed rather than assumed: the PID
    // leaves. The interval is the datum — it sizes how long a shutdown
    // drain must be willing to wait after the hooks say goodbye.
    let exit_detail = session
        .await_child_exit(TERMINATION_CEILING)
        .map_err(|detail| {
            Failure::new(
                "termination",
                73,
                format!(
                    "{detail} — the CLI outlived SessionEnd by more than {}s; that interval is a finding about shutdown drain sizing, and it still fails this lane",
                    TERMINATION_CEILING.as_secs()
                ),
            )
        })?;
    print_step(
        "termination",
        "pass",
        &format!(
            "{exit_detail}; exit_after_session_end_ms={}",
            session_end_at.elapsed().as_millis()
        ),
    );
    Ok(())
}

/// Nothing of the session's tree survives its end: group-empty on POSIX,
/// job-empty on Windows. Runs after teardown, so orphan reaping has had
/// its moment and anything still here is a real survivor.
fn await_tree_empty(containment: &Containment) -> Result<String, String> {
    #[cfg(unix)]
    {
        let ms = inspect::await_group_empty(containment.pgid, EMPTY_TIMEOUT)?;
        Ok(format!(
            "group_survivors=0 — kill(-{}, 0) reports an empty process group {ms}ms after teardown",
            containment.pgid
        ))
    }
    #[cfg(windows)]
    {
        let ms = containment.job.await_empty(EMPTY_TIMEOUT)?;
        Ok(format!(
            "group_survivors=0 — the job object reports zero members {ms}ms after teardown"
        ))
    }
}

#[cfg(windows)]
fn console_host_check(hosts_before: &[u32]) -> Result<(), Failure> {
    let leaked = inspect::new_console_hosts(hosts_before)
        .map_err(|detail| Failure::new("console_host", 75, detail))?;
    if !leaked.is_empty() {
        return Err(Failure::new(
            "console_host",
            75,
            format!(
                "console_host_gone=false — ConPTY console host(s) survived teardown: {leaked:?}"
            ),
        ));
    }
    print_step(
        "console_host",
        "pass",
        "console_host_gone=true — no conhost.exe/OpenConsole.exe spawned by this run survives",
    );
    Ok(())
}

#[cfg(not(windows))]
fn console_host_check() -> Result<(), Failure> {
    print_step(
        "console_host",
        "skip",
        "the ConPTY console host is a Windows artifact; nothing exists to leak here",
    );
    Ok(())
}
