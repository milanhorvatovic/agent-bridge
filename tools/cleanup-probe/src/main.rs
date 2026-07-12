//! Cleanup probe — proves, against the tree-child fixture under a real PTY,
//! that ending a session leaves nothing behind, on the two paths a runtime
//! actually takes:
//!
//! - **`clean`**: the fixture grows its tree (an in-group sleeper plus a
//!   `setsid`/job-breakaway escape attempt), then exits on command. After
//!   the exit: the process group (POSIX) / job object (Windows) is empty,
//!   the PTY closes with the reader reaching end-of-stream, the open-fd /
//!   handle count returns to its pre-allocation baseline, and on Windows no
//!   ConPTY console host this probe spawned survives.
//! - **`terminate`**: the same tree in stubborn mode ignores the polite
//!   signal, and the probe runs the PTY-owned escalation — SIGTERM to the
//!   group, a grace window, then SIGKILL to the group on POSIX;
//!   `TerminateJobObject` on Windows, where no polite OS phase exists to
//!   escalate from — and asserts the tree is gone within grace + timeout.
//!
//! Measurement before assertion: fd/handle counts are compared as deltas
//! against a baseline taken before the measured PTY exists — after a
//! throwaway warm-up session has absorbed the process's one-time costs —
//! and group emptiness is enumerated per recorded PID (a `getpgid` scan —
//! the same on-demand traversal the runtime's PTY layer will use) as well
//! as group-wide, so pre-existing runner noise cannot flake a lane. On
//! Linux the probe makes itself a subreaper: an orphan's zombie would
//! otherwise wait on a containerized PID 1 that never reaps, reading as an
//! unkillable survivor.
//!
//! The escapee is the honest limitation on display, not a bug to fix: a
//! `setsid` escapee survives group-scoped delivery *by design* (it is what
//! a daemonizing grandchild does), so the probe asserts it is **detected**
//! as outside the group and then reaps it from its recorded PID. On
//! Windows the same attempt is **denied** at spawn by the job object —
//! which is the guarantee, and is asserted as such.
//!
//! One scenario per invocation: `cleanup-probe clean` /
//! `cleanup-probe terminate`. Same step contract as the sibling probes —
//! one machine-readable `step=… status=… detail="…"` line per step, exit
//! non-zero with a step-identifying code on the first failure, so CI
//! asserts the exit status while a human reads the log.

// This crate legitimately owns stdout — the step-result lines *are* its
// output — so it is exempt from the workspace-wide stdout-macro ban in
// clippy.toml.
#![allow(clippy::disallowed_macros)]

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::{Duration, Instant};

use agent_bridge_interactive_probe::firsttoken::FirstTokenClock;
use agent_bridge_interactive_probe::pty::{
    OutputTracker, SharedWriter, alloc_pty, force_kill, spawn_reader, teardown, wait_child,
};
use agent_bridge_interactive_probe::reports::wait_for_report;
use agent_bridge_interactive_probe::{inspect, platform_report};
use agent_bridge_probe_child::{
    ESCAPE_DENIED, EVENT_QUIT, EVENT_READY, EVENT_TREE, QUIT_BYTE, Report, TREE_BYTE,
};
use portable_pty::{Child, CommandBuilder, PtyPair};

/// Deliberately wide: ConPTY reflows output to the PTY width, and a report
/// line hard-wrapped mid-`key=value` would not parse. The fixture's longest
/// line is well under half of this.
const COLS: u16 = 200;
const ROWS: u16 = 50;

const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// The grace the polite signal gets before escalation. The design pins the
/// sequence (polite → grace → force), not this value; it is logged and
/// tunable so a finding about real drain times can move it.
const DEFAULT_GRACE_MS: u64 = 2_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Clean,
    Terminate,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Scenario::Clean => "clean",
            Scenario::Terminate => "terminate",
        }
    }

    /// The fixture mode this scenario runs against: the terminate lane
    /// needs a tree that ignores the polite signal.
    fn fixture_mode(self) -> &'static str {
        match self {
            Scenario::Clean => "clean",
            Scenario::Terminate => "stubborn",
        }
    }
}

struct Failure {
    step: &'static str,
    code: i32,
    detail: String,
}

impl Failure {
    fn new(step: &'static str, code: i32, detail: impl Into<String>) -> Self {
        Self {
            step,
            code,
            detail: detail.into(),
        }
    }
}

fn main() {
    let (scenario, timeout, grace) = match parse_args(std::env::args().skip(1)) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("cleanup-probe: {message}");
            std::process::exit(2);
        }
    };
    println!("cleanup-probe {}", platform_report());
    match run(scenario, timeout, grace) {
        Ok(()) => println!("cleanup-probe mode={} result=pass", scenario.name()),
        Err(failure) => {
            print_step(failure.step, "fail", &failure.detail);
            eprintln!(
                "cleanup-probe: step {} failed: {}",
                failure.step, failure.detail
            );
            std::process::exit(failure.code);
        }
    }
}

fn parse_args<I: Iterator<Item = String>>(
    mut args: I,
) -> Result<(Scenario, Duration, Duration), String> {
    const USAGE: &str = "usage: cleanup-probe <clean|terminate> [--timeout-secs N] [--grace-ms N]";
    let mut scenario: Option<Scenario> = None;
    let mut timeout = Duration::from_secs(DEFAULT_TIMEOUT_SECS);
    let mut grace = Duration::from_millis(DEFAULT_GRACE_MS);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "clean" if scenario.is_none() => scenario = Some(Scenario::Clean),
            "terminate" if scenario.is_none() => scenario = Some(Scenario::Terminate),
            "--timeout-secs" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--timeout-secs needs a value. {USAGE}"))?;
                let secs: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid --timeout-secs value: {value}"))?;
                timeout = Duration::from_secs(secs);
            }
            "--grace-ms" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("--grace-ms needs a value. {USAGE}"))?;
                let ms: u64 = value
                    .parse()
                    .map_err(|_| format!("invalid --grace-ms value: {value}"))?;
                grace = Duration::from_millis(ms);
            }
            other => return Err(format!("unexpected argument: {other}. {USAGE}")),
        }
    }
    scenario
        .map(|scenario| (scenario, timeout, grace))
        .ok_or_else(|| format!("a scenario is required. {USAGE}"))
}

fn print_step(step: &str, status: &str, detail: &str) {
    // Keep every step line single-line and parseable: the detail field is
    // quoted, so newlines and double quotes inside it are normalized away.
    let clean = detail.replace(['\r', '\n'], " ").replace('"', "'");
    println!("cleanup-probe step={step} status={status} detail=\"{clean}\"");
}

/// What the fixture's escape attempt produced, as parsed from its tree
/// report.
#[derive(Debug, PartialEq, Eq)]
enum Escape {
    /// The escapee runs, outside the group, at this PID (the POSIX
    /// outcome).
    Escaped(u32),
    /// The OS refused the spawn — the expected Windows outcome under a job
    /// object without breakaway permission.
    Denied,
}

/// The tree as the fixture reported it.
#[derive(Debug, PartialEq, Eq)]
struct TreePids {
    ingroup: u32,
    escape: Escape,
}

fn parse_tree(report: &Report) -> Result<TreePids, String> {
    let ingroup = report
        .field("ingroup")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("the tree report carries no parseable ingroup pid: {report}"))?;
    let escape = match report.field("escape") {
        Some(value) if value == ESCAPE_DENIED => Escape::Denied,
        Some(value) => Escape::Escaped(value.parse().map_err(|_| {
            format!("the tree report's escape field is neither a pid nor a denial: {report}")
        })?),
        None => return Err(format!("the tree report carries no escape field: {report}")),
    };
    Ok(TreePids { ingroup, escape })
}

/// What contains the fixture's tree on this platform — the process group
/// the PTY spawn created (POSIX) or the job object this probe creates and
/// binds (Windows, the runtime's future job-per-child pattern).
struct Containment {
    #[cfg(unix)]
    pgid: i32,
    #[cfg(windows)]
    job: inspect::Job,
}

/// The scenario, in lifecycle order. Exit codes are step-stable across both
/// scenarios: 10 alloc, 11 spawn, 12 ready, 13 contain, 14 tree, 15 detect,
/// 16 quit, 17 polite, 18 escalate, 19 child_exit, 20 empty, 21 survivor,
/// 22 teardown, 23 console_host, 24 resources, 25 warmup.
fn run(scenario: Scenario, timeout: Duration, grace: Duration) -> Result<(), Failure> {
    // A throwaway session runs before the baseline: a process's first PTY
    // cycle pays one-time costs the counters see — on windows-2022 the
    // first ConPTY allocation leaves a stable +9 handles behind (console
    // connection machinery, lazily started internals) that no amount of
    // teardown returns. Those are per-process, not per-session; the
    // baseline must charge the measured session only with what a session
    // costs, or the delta assertion tests the runtime's warm-up instead of
    // the cleanup contract.
    warm_up(timeout).map_err(|detail| Failure::new("warmup", 25, detail))?;
    print_step(
        "warmup",
        "pass",
        "one throwaway pty session absorbed the process's one-time costs; the baseline is taken after it",
    );

    // Baselines come next, before the measured PTY exists: everything the
    // run opens after this line is something the run must also release.
    let baseline =
        inspect::open_channels().map_err(|detail| Failure::new("resources", 24, detail))?;
    #[cfg(windows)]
    let hosts_before = inspect::console_hosts_parented_here()
        .map_err(|detail| Failure::new("console_host", 23, detail))?;

    let (pair, alloc_ms) =
        alloc_pty(COLS, ROWS, timeout).map_err(|detail| Failure::new("alloc", 10, detail))?;
    print_step(
        "alloc",
        "pass",
        &format!("pty allocated at {COLS}x{ROWS} in {alloc_ms}ms"),
    );
    let PtyPair { master, slave } = pair;

    let fixture = sibling_fixture().map_err(|detail| Failure::new("spawn", 11, detail))?;
    let mut command = CommandBuilder::new(&fixture);
    command.arg(scenario.fixture_mode());
    let mut child = slave
        .spawn_command(command)
        .map_err(|err| Failure::new("spawn", 11, format!("child spawn failed: {err:#}")))?;
    // Release our copy of the child end: holding it open would keep the
    // master from ever seeing end-of-stream after the child exits.
    drop(slave);
    let root_pid = child
        .process_id()
        .ok_or_else(|| Failure::new("spawn", 11, "the spawned child reports no pid"))?;
    print_step(
        "spawn",
        "pass",
        &format!(
            "spawned `{} {}` pid={root_pid}",
            fixture.display(),
            scenario.fixture_mode(),
        ),
    );

    let reader = master
        .try_clone_reader()
        .map_err(|err| Failure::new("ready", 12, format!("cloning the reader failed: {err:#}")))?;
    let writer =
        SharedWriter::new(master.take_writer().map_err(|err| {
            Failure::new("ready", 12, format!("taking the writer failed: {err:#}"))
        })?);
    let events = spawn_reader(reader, writer.clone(), Arc::new(AtomicU32::new(0)));
    let mut tracker = OutputTracker::new(events, FirstTokenClock::new(Instant::now()), None);

    let ready = wait_for_report(
        &mut tracker,
        "the fixture's ready report",
        |report| report.event == EVENT_READY,
        timeout,
    )
    .map_err(|detail| Failure::new("ready", 12, detail))?;
    if ready.field("mode") != Some(scenario.fixture_mode()) {
        return Err(Failure::new(
            "ready",
            12,
            format!("the fixture came up in the wrong mode: {ready}"),
        ));
    }
    print_step("ready", "pass", &format!("fixture reports: {ready}"));

    // Containment is established before the tree exists — on Windows a
    // descendant spawned before the job binding would sit outside the job
    // and invalidate every membership assertion below.
    let (containment, contain_detail) =
        contain(root_pid, &ready).map_err(|detail| Failure::new("contain", 13, detail))?;
    print_step("contain", "pass", &contain_detail);

    writer
        .send(&[TREE_BYTE])
        .map_err(|err| Failure::new("tree", 14, format!("writing the tree byte failed: {err}")))?;
    let tree_report = wait_for_report(
        &mut tracker,
        "the fixture's tree report",
        |report| report.event == EVENT_TREE,
        timeout,
    )
    .map_err(|detail| Failure::new("tree", 14, detail))?;
    let tree = parse_tree(&tree_report).map_err(|detail| Failure::new("tree", 14, detail))?;
    print_step("tree", "pass", &format!("fixture reports: {tree_report}"));

    let detect_detail = detect(&containment, root_pid, &tree)
        .map_err(|detail| Failure::new("detect", 15, detail))?;
    print_step("detect", "pass", &detect_detail);

    let (exit_detail, escalated) = match scenario {
        Scenario::Clean => {
            let quit_detail = clean_quit(&writer, &mut tracker, &tree, timeout)
                .map_err(|detail| Failure::new("quit", 16, detail))?;
            print_step("quit", "pass", &quit_detail);
            let exit_detail = wait_child(child.as_mut(), timeout)
                .map_err(|detail| Failure::new("child_exit", 19, detail))?;
            (exit_detail, None)
        }
        Scenario::Terminate => {
            let polite_detail = polite(&containment, root_pid, &tree, &mut tracker, grace, timeout)
                .map_err(|detail| Failure::new("polite", 17, detail))?;
            print_step("polite", "pass", &polite_detail);

            let escalated_at = Instant::now();
            let escalate_detail =
                escalate(&containment).map_err(|detail| Failure::new("escalate", 18, detail))?;
            print_step("escalate", "pass", &escalate_detail);

            let exit_detail = reap_any(child.as_mut(), timeout)
                .map_err(|detail| Failure::new("child_exit", 19, detail))?;
            (exit_detail, Some(escalated_at))
        }
    };
    print_step("child_exit", "pass", &exit_detail);

    let empty_detail = await_empty(&containment, root_pid, &tree, timeout)
        .map_err(|detail| Failure::new("empty", 20, detail))?;
    match escalated {
        Some(at) => print_step(
            "empty",
            "pass",
            &format!(
                "{empty_detail}; escalated_after_ms={}",
                at.elapsed().as_millis()
            ),
        ),
        None => print_step("empty", "pass", &empty_detail),
    }

    let survivor_detail = survivor(scenario, &tree, timeout)
        .map_err(|detail| Failure::new("survivor", 21, detail))?;
    print_step("survivor", "pass", &survivor_detail);

    // The child handle is dropped before the resource baseline is compared:
    // on Windows it holds a process handle that would otherwise count as a
    // leak of the probe's own making.
    drop(child);

    let (events, _, end) = tracker.into_teardown_parts();
    let teardown_detail = teardown(master, &events, end, timeout)
        .map_err(|detail| Failure::new("teardown", 22, detail))?;
    print_step("teardown", "pass", &teardown_detail);
    // The writer is the last PTY fd this probe holds (the reader thread's
    // clones die with it at end-of-stream); the baseline cannot settle
    // while it lives.
    drop(writer);

    #[cfg(windows)]
    {
        let leaked = inspect::new_console_hosts(&hosts_before)
            .map_err(|detail| Failure::new("console_host", 23, detail))?;
        if !leaked.is_empty() {
            return Err(Failure::new(
                "console_host",
                23,
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
    }
    #[cfg(not(windows))]
    print_step(
        "console_host",
        "skip",
        "the ConPTY console host is a Windows artifact; nothing exists to leak here",
    );

    let resources_detail = inspect::await_baseline(baseline, timeout)
        .map_err(|detail| Failure::new("resources", 24, detail))?;
    print_step("resources", "pass", &resources_detail);
    Ok(())
}

/// Establish the containment boundary and cross-check it: on POSIX the
/// fixture's self-reported process group must match what `getpgid` says
/// from outside (the enumeration mechanism validating itself); on Windows
/// the probe creates the job and binds the root into it.
#[cfg(unix)]
fn contain(root_pid: u32, ready: &Report) -> Result<(Containment, String), String> {
    let reported: i32 = ready
        .field("pgid")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("the ready report carries no parseable pgid: {ready}"))?;
    let pid = i32::try_from(root_pid).map_err(|_| format!("pid {root_pid} does not fit i32"))?;
    let observed = inspect::pgid_of(pid)?;
    if observed != reported {
        return Err(format!(
            "the fixture reports pgid {reported} but getpgid({pid}) says {observed} — the two views of the group must agree"
        ));
    }
    // Arranged before any descendant exists: once the root dies, its
    // orphans must land somewhere that reaps — in containerized CI the
    // default PID 1 does not, and an unreaped zombie reads as an unkillable
    // survivor in every existence check below.
    let orphans = inspect::adopt_orphans();
    Ok((
        Containment { pgid: reported },
        format!(
            "process group {reported} (fixture-reported and getpgid-observed agree); group-scoped assertions target it; {orphans}"
        ),
    ))
}

#[cfg(windows)]
fn contain(root_pid: u32, _ready: &Report) -> Result<(Containment, String), String> {
    let job = inspect::Job::create_kill_on_close()?;
    job.assign(root_pid)?;
    Ok((
        Containment { job },
        format!(
            "root pid {root_pid} bound to a KILL_ON_JOB_CLOSE job object (no breakaway permission) before the tree grows"
        ),
    ))
}

/// The detection assertion — the mechanism the runtime will rely on to
/// enumerate a session's tree, exercised from outside the fixture. POSIX:
/// the in-group descendant reads as a member and the escapee as outside
/// the group. Windows: the job's own PID list carries root and in-group
/// descendant, and the breakaway was denied.
#[cfg(unix)]
fn detect(containment: &Containment, root_pid: u32, tree: &TreePids) -> Result<String, String> {
    use inspect::GroupStanding;

    let pgid = containment.pgid;
    let ingroup = to_pid(tree.ingroup)?;
    match inspect::standing_in(pgid, ingroup) {
        GroupStanding::Member => {}
        other => {
            return Err(format!(
                "the in-group descendant {ingroup} should be a member of group {pgid}, but reads as {other:?}"
            ));
        }
    }
    let Escape::Escaped(escapee) = tree.escape else {
        return Err("POSIX cannot deny a setsid escape, yet the fixture reports one".to_string());
    };
    let escapee = to_pid(escapee)?;
    let outside = match inspect::standing_in(pgid, escapee) {
        GroupStanding::Outside { pgid: Some(other) } => format!("in its own group {other}"),
        GroupStanding::Outside { pgid: None } => {
            "in another session (getpgid refuses across the boundary)".to_string()
        }
        GroupStanding::Member => {
            return Err(format!(
                "escapee_detected=false — the escapee {escapee} still reads as a member of group {pgid}; setsid did not detach it"
            ));
        }
        GroupStanding::Gone => {
            return Err(format!(
                "the escapee {escapee} is already gone before any cleanup ran"
            ));
        }
    };
    let members = inspect::surviving_members(pgid, &[to_pid(root_pid)?, ingroup]);
    Ok(format!(
        "escapee_detected=true — getpgid scan: group {pgid} holds {members:?}; escapee {escapee} is {outside}, where group-scoped delivery cannot reach it"
    ))
}

#[cfg(windows)]
fn detect(containment: &Containment, root_pid: u32, tree: &TreePids) -> Result<String, String> {
    let members = containment.job.pids()?;
    for (name, pid) in [("root", root_pid), ("in-group descendant", tree.ingroup)] {
        if !members.contains(&pid) {
            return Err(format!(
                "the {name} (pid {pid}) is missing from the job's pid list {members:?}"
            ));
        }
    }
    match tree.escape {
        Escape::Denied => Ok(format!(
            "escapee_detected=true — the job refused the breakaway at spawn (escape=denied), and its pid list {members:?} holds exactly the tree"
        )),
        Escape::Escaped(pid) => Err(format!(
            "the breakaway unexpectedly succeeded (escapee pid {pid}) — the job was created without breakaway permission, so the OS should have denied it"
        )),
    }
}

/// Clean scenario: ask the fixture to end its session and hold its quit
/// report to the contract — the in-group descendant reaped by the fixture
/// itself, the escapee deliberately left (POSIX) for the probe to reap.
fn clean_quit(
    writer: &SharedWriter,
    tracker: &mut OutputTracker,
    tree: &TreePids,
    timeout: Duration,
) -> Result<String, String> {
    writer
        .send(&[QUIT_BYTE])
        .map_err(|err| format!("writing the quit byte failed: {err}"))?;
    let quit = wait_for_report(
        tracker,
        "the fixture's quit report",
        |report| report.event == EVENT_QUIT,
        timeout,
    )?;
    if quit.field("ingroup") != Some("reaped") {
        return Err(format!(
            "the fixture did not reap its in-group descendant on the way out: {quit}"
        ));
    }
    let expected_escape = match tree.escape {
        Escape::Escaped(_) => "left",
        Escape::Denied => "none",
    };
    if quit.field("escape") != Some(expected_escape) {
        return Err(format!(
            "the quit report's escape outcome should be '{expected_escape}': {quit}"
        ));
    }
    Ok(format!("fixture reports: {quit}"))
}

/// Terminate scenario, polite phase. POSIX: SIGTERM to the process group
/// must be delivered (the stubborn fixture reports surviving it) and must
/// leave the group intact through the grace window — otherwise there is
/// nothing to escalate past and the lane proves nothing. Windows: no OS
/// polite phase exists that this probe can deliver across consoles; the
/// design routes terminate to job-object termination outright.
#[cfg(unix)]
fn polite(
    containment: &Containment,
    root_pid: u32,
    tree: &TreePids,
    tracker: &mut OutputTracker,
    grace: Duration,
    timeout: Duration,
) -> Result<String, String> {
    use agent_bridge_probe_child::EVENT_TERM;

    let pgid = containment.pgid;
    inspect::signal_group(pgid, libc::SIGTERM)?;
    wait_for_report(
        tracker,
        "the fixture's survived-SIGTERM report",
        |report| report.event == EVENT_TERM,
        timeout,
    )?;
    // The grace window the real sequence would grant: drain output while it
    // elapses, then require the group to have shrugged the signal off.
    tracker
        .pump(grace)
        .map_err(|detail| format!("draining output through the grace window failed: {detail}"))?;
    let candidates = [to_pid(root_pid)?, to_pid(tree.ingroup)?];
    let members = inspect::surviving_members(pgid, &candidates);
    if members.len() != candidates.len() {
        return Err(format!(
            "the polite SIGTERM already thinned group {pgid} to {members:?} — a stubborn tree must survive it, or the escalation is untested"
        ));
    }
    Ok(format!(
        "kill(-{pgid}, SIGTERM) delivered; the fixture reported surviving it and the group is intact after the {}ms grace",
        grace.as_millis()
    ))
}

#[cfg(windows)]
fn polite(
    _containment: &Containment,
    _root_pid: u32,
    _tree: &TreePids,
    _tracker: &mut OutputTracker,
    _grace: Duration,
    _timeout: Duration,
) -> Result<String, String> {
    Ok(
        "skipped by design: Windows offers no cross-console polite signal for this probe to deliver — the terminate sequence here *is* job-object termination, asserted next"
            .to_string(),
    )
}

/// Terminate scenario, force phase: SIGKILL to the group (POSIX) — which
/// no handler can ignore — or `TerminateJobObject` (Windows).
#[cfg(unix)]
fn escalate(containment: &Containment) -> Result<String, String> {
    inspect::signal_group(containment.pgid, libc::SIGKILL)?;
    Ok(format!(
        "kill(-{}, SIGKILL) issued — the disposition-proof half of the sequence",
        containment.pgid
    ))
}

#[cfg(windows)]
fn escalate(containment: &Containment) -> Result<String, String> {
    /// Arbitrary but recognizable in logs as "killed by the probe".
    const TERMINATED_EXIT_CODE: u32 = 101;
    containment.job.terminate(TERMINATED_EXIT_CODE)?;
    Ok("TerminateJobObject issued against the bound job".to_string())
}

/// Reap a child whatever its exit status — the terminate lane kills it, so
/// unlike `wait_child` a non-success status is the expected outcome, not a
/// failure.
fn reap_any(child: &mut dyn Child, timeout: Duration) -> Result<String, String> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(format!(
                    "child reaped in {}ms ({status})",
                    started.elapsed().as_millis()
                ));
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    return Err(format!(
                        "child still running {}s after the escalation; {}",
                        timeout.as_secs(),
                        force_kill(child)
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(err) => return Err(format!("child wait failed: {err}")),
        }
    }
}

/// The emptiness assertion, by both available mechanisms on POSIX: the
/// per-PID getpgid scan over the recorded tree *and* the group-wide
/// existence check that would catch a member the run never recorded.
#[cfg(unix)]
fn await_empty(
    containment: &Containment,
    root_pid: u32,
    tree: &TreePids,
    timeout: Duration,
) -> Result<String, String> {
    let pgid = containment.pgid;
    let candidates = [to_pid(root_pid)?, to_pid(tree.ingroup)?];
    let deadline = Instant::now() + timeout;
    let started = Instant::now();
    loop {
        // A group-killed orphan (the in-group sleeper whose parent died
        // with it) reparents to this probe and stays a zombie — and a
        // zombie still answers getpgid — until reaped here.
        inspect::reap_adopted();
        let members = inspect::surviving_members(pgid, &candidates);
        if members.is_empty() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "group_survivors={} — the getpgid scan still finds {members:?} in group {pgid} after {}ms",
                members.len(),
                timeout.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let remaining = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1));
    inspect::await_group_empty(pgid, remaining)?;
    Ok(format!(
        "group_survivors=0 — getpgid scan of the recorded tree is empty and kill(-{pgid}, 0) reports no members, {}ms after the exit",
        started.elapsed().as_millis()
    ))
}

#[cfg(windows)]
fn await_empty(
    containment: &Containment,
    _root_pid: u32,
    _tree: &TreePids,
    timeout: Duration,
) -> Result<String, String> {
    let ms = containment.job.await_empty(timeout)?;
    Ok(format!(
        "group_survivors=0 — the job object reports zero members {ms}ms after the exit"
    ))
}

/// The escapee's fate, asserted honestly. POSIX: it must still be alive —
/// having survived either the fixture's clean exit or the group-wide
/// SIGKILL, because group-scoped delivery cannot reach it — and is then
/// reaped from its recorded PID, the exact move a runtime's
/// orphan-handling would make. Windows: the job denied the breakaway, so
/// no survivor can exist; the denial is re-asserted as the guarantee.
fn survivor(scenario: Scenario, tree: &TreePids, timeout: Duration) -> Result<String, String> {
    match tree.escape {
        Escape::Denied => Ok(
            "no survivor is possible: the job denied the breakaway at spawn — the Windows containment guarantee this probe exists to record"
                .to_string(),
        ),
        Escape::Escaped(pid) => reap_escapee(scenario, pid, timeout),
    }
}

#[cfg(unix)]
fn reap_escapee(scenario: Scenario, pid: u32, timeout: Duration) -> Result<String, String> {
    let survived_what = match scenario {
        Scenario::Clean => "the tree's clean exit",
        Scenario::Terminate => "the group-wide SIGKILL",
    };
    let pid = to_pid(pid)?;
    if !inspect::process_alive(pid) {
        return Err(format!(
            "the escapee {pid} should have survived {survived_what} — outside the group, nothing aimed at the group can have killed it — but it is gone"
        ));
    }
    inspect::kill_pid(pid)?;
    let deadline = Instant::now() + timeout;
    loop {
        // The killed escapee is an orphan this probe adopted; it stays a
        // kill(pid, 0)-visible zombie until reaped here.
        inspect::reap_adopted();
        if !inspect::process_alive(pid) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the escapee {pid} was killed by pid but still exists after {}ms",
                timeout.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(format!(
        "escapee {pid} survived {survived_what} exactly as a setsid escapee must — group-scoped cleanup is necessary but not sufficient — and was then reaped from its recorded pid"
    ))
}

/// Unreachable off-POSIX: a Windows escape is denied at spawn, so
/// [`survivor`] never reaches this call. Present so the match stays
/// exhaustive per platform.
#[cfg(windows)]
fn reap_escapee(_scenario: Scenario, pid: u32, _timeout: Duration) -> Result<String, String> {
    Err(format!(
        "an escapee (pid {pid}) exists on Windows — the breakaway should have been denied"
    ))
}

#[cfg(unix)]
fn to_pid(pid: u32) -> Result<i32, String> {
    i32::try_from(pid).map_err(|_| format!("pid {pid} does not fit i32"))
}

/// One complete throwaway session — alloc, spawn the fixture, quit it,
/// reap, guarded close — so every lazily initialized, process-lifetime
/// resource (ConPTY connection internals, spawn machinery, reader-thread
/// plumbing) exists before the baseline is taken. Nothing here is
/// asserted; the measured session that follows is the assertion.
fn warm_up(timeout: Duration) -> Result<(), String> {
    let (pair, _) = alloc_pty(COLS, ROWS, timeout)
        .map_err(|detail| format!("warm-up pty allocation failed: {detail}"))?;
    let PtyPair { master, slave } = pair;
    let fixture = sibling_fixture()?;
    let mut command = CommandBuilder::new(&fixture);
    command.arg(Scenario::Clean.fixture_mode());
    let mut child = slave
        .spawn_command(command)
        .map_err(|err| format!("warm-up spawn failed: {err:#}"))?;
    drop(slave);

    let reader = master
        .try_clone_reader()
        .map_err(|err| format!("warm-up reader clone failed: {err:#}"))?;
    let writer = SharedWriter::new(
        master
            .take_writer()
            .map_err(|err| format!("warm-up writer failed: {err:#}"))?,
    );
    let events = spawn_reader(reader, writer.clone(), Arc::new(AtomicU32::new(0)));
    let mut tracker = OutputTracker::new(events, FirstTokenClock::new(Instant::now()), None);
    wait_for_report(
        &mut tracker,
        "the warm-up fixture's ready report",
        |report| report.event == EVENT_READY,
        timeout,
    )?;
    writer
        .send(&[QUIT_BYTE])
        .map_err(|err| format!("warm-up quit failed: {err}"))?;
    wait_child(child.as_mut(), timeout)?;
    drop(child);
    let (events, _, end) = tracker.into_teardown_parts();
    teardown(master, &events, end, timeout)?;
    Ok(())
}

/// The fixture binary sits next to this one — cargo builds every workspace
/// binary into the same directory.
fn sibling_fixture() -> Result<std::path::PathBuf, String> {
    let me = std::env::current_exe().map_err(|err| format!("current_exe failed: {err}"))?;
    let dir = me
        .parent()
        .ok_or_else(|| "current_exe has no parent directory".to_string())?;
    let fixture = dir.join(format!("tree-child{}", std::env::consts::EXE_SUFFIX));
    if fixture.exists() {
        Ok(fixture)
    } else {
        Err(format!(
            "fixture binary not found at {} — build it first: \
             cargo build --package agent-bridge-probe-child",
            fixture.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_select_scenario_timeout_and_grace() {
        let args = ["terminate", "--timeout-secs", "3", "--grace-ms", "500"].map(String::from);
        let (scenario, timeout, grace) = parse_args(args.into_iter()).unwrap();
        assert_eq!(scenario.name(), "terminate");
        assert_eq!(timeout, Duration::from_secs(3));
        assert_eq!(grace, Duration::from_millis(500));
    }

    #[test]
    fn a_scenario_is_required_and_unknown_arguments_are_rejected() {
        assert!(parse_args(std::iter::empty()).is_err());
        assert!(parse_args(["--bogus".to_string()].into_iter()).is_err());
        assert!(parse_args(["clean".to_string(), "terminate".to_string()].into_iter()).is_err());
    }

    #[test]
    fn the_terminate_scenario_runs_the_stubborn_fixture() {
        // The escalation lane is only meaningful against a tree that
        // ignores the polite signal; a clean fixture would die to SIGTERM
        // and the SIGKILL half would be asserted against a corpse.
        assert_eq!(Scenario::Terminate.fixture_mode(), "stubborn");
        assert_eq!(Scenario::Clean.fixture_mode(), "clean");
    }

    #[test]
    fn tree_reports_parse_into_pids_and_outcomes() {
        let escaped = Report::parse("probe-child event=tree ingroup=123 escape=456").unwrap();
        assert_eq!(
            parse_tree(&escaped).unwrap(),
            TreePids {
                ingroup: 123,
                escape: Escape::Escaped(456),
            }
        );
        let denied = Report::parse("probe-child event=tree ingroup=123 escape=denied").unwrap();
        assert_eq!(
            parse_tree(&denied).unwrap(),
            TreePids {
                ingroup: 123,
                escape: Escape::Denied,
            }
        );
    }

    #[test]
    fn a_malformed_tree_report_is_an_error_not_a_guess() {
        for line in [
            "probe-child event=tree escape=456",
            "probe-child event=tree ingroup=123",
            "probe-child event=tree ingroup=123 escape=maybe",
            "probe-child event=tree ingroup=nope escape=456",
        ] {
            let report = Report::parse(line).unwrap();
            assert!(parse_tree(&report).is_err(), "should not parse: {line}");
        }
    }
}
