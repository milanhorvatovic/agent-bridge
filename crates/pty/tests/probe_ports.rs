//! The Phase-0 validation findings, re-asserted against this crate's API.
//!
//! Before any of this existed, a set of throwaway probes established on all
//! three platforms that a terminal can be allocated and torn down without
//! leaking, that the interrupt byte and the interrupt signal are genuinely
//! different things, and that a resize reaches the child — including the
//! awkward case where it is issued before the child has taken the terminal.
//! Those findings are the reason the layer is shaped the way it is, so they
//! are re-established here against the shape rather than left as evidence
//! about a program nobody runs any more.
//!
//! Run with `cargo test -p agent-bridge-pty --test probe_ports`.

// Owns its own stdout: the scenario report on one side, the fixture's report
// lines into the terminal on the other.
#![allow(clippy::disallowed_macros)]

use std::time::Duration;

use agent_bridge_pty::Dimensions;

mod support;

use support::{Scenario, Session};

/// How many sessions the leak check runs. Enough that a per-session leak
/// shows as a count well outside the noise of a build machine, few enough
/// that the scenario stays under a second.
const LEAK_SESSIONS: usize = 20;

fn main() {
    support::main(
        "probe-ports",
        &[
            Scenario {
                name: "allocate_spawn_read_exit",
                check: allocate_spawn_read_exit,
            },
            Scenario {
                name: "sessions_leave_no_descriptors_behind",
                check: sessions_leave_no_descriptors_behind,
            },
            Scenario {
                name: "raw_mode_delivers_the_byte",
                check: raw_mode_delivers_the_byte,
            },
            Scenario {
                name: "cooked_mode_turns_the_byte_into_a_signal",
                check: cooked_mode_turns_the_byte_into_a_signal,
            },
            Scenario {
                name: "a_settled_resize_is_observed",
                check: a_settled_resize_is_observed,
            },
            Scenario {
                name: "an_early_resize_still_recovers",
                check: an_early_resize_still_recovers,
            },
        ],
    );
}

/// The original allocation probe, end to end: a terminal is allocated, a
/// child runs in it, its output comes back, and the stream ends when the
/// child does. A platform that cannot do this is a platform this project
/// does not support, which is what the probe existed to find out.
fn allocate_spawn_read_exit() -> Result<String, String> {
    let mut session = Session::start("echo", &["probe"])?;
    session.wait_for("echo=probe")?;
    let status = session
        .pty
        .terminate(Duration::from_secs(2))
        .map_err(|err| format!("terminate failed: {err}"))?;
    if !status.success() {
        return Err(format!("the child ended with {status}"));
    }
    // End-of-stream is the half a spawn-and-read check usually forgets: a
    // reader that never finishes is how a session leaks a thread apiece.
    // Reached by closing the terminal, which is the only thing that ends the
    // stream on every platform: a terminated child is enough on POSIX and
    // not on Windows, where the pseudo-console holds its output open until
    // the console itself closes.
    let reason = session.close_and_drain()?;
    Ok(format!(
        "read back, exited cleanly, stream ended ({reason})"
    ))
}

/// Sessions come and go without accumulating descriptors.
///
/// The resource half of the original probes, and the one that catches the
/// mistake that matters most in a long-running runtime: a terminal closed
/// but a duplicate of it left open, which no single-session test can see.
fn sessions_leave_no_descriptors_behind() -> Result<String, String> {
    // One session first, so anything allocated once and cached for the
    // process's lifetime is already accounted for in the baseline.
    run_one_session()?;
    let baseline = support::open_channels()?;
    for _ in 0..LEAK_SESSIONS {
        run_one_session()?;
    }
    let settled = support::await_channel_baseline(baseline)?;
    Ok(format!(
        "{LEAK_SESSIONS} sessions, {} count back to {baseline} ({settled})",
        support::CHANNEL_KIND
    ))
}

/// One whole session, drained to the end so its reader has finished before
/// the count is taken.
fn run_one_session() -> Result<(), String> {
    let mut session = Session::start("echo", &["leak-check"])?;
    session.wait_for("echo=leak-check")?;
    session
        .pty
        .terminate(Duration::from_secs(2))
        .map_err(|err| format!("terminate failed: {err}"))?;
    // Closed and drained rather than merely dropped: the reader releases its
    // descriptor when it finishes, so a count taken before that measures a
    // race rather than a leak.
    session.close_and_drain()?;
    Ok(())
}

/// With the terminal in the mode an interactive CLI uses, the interrupt
/// character is input: the child reads the byte and no signal is raised.
///
/// This is why `interrupt` writes a byte. The probe measured it against a
/// real CLI, which took a `SIGINT` as a request to shut down.
fn raw_mode_delivers_the_byte() -> Result<String, String> {
    let mut session = Session::start("raw", &[])?;
    session.wait_for("ready mode=raw")?;
    session
        .pty
        .write(&[0x03])
        .map_err(|err| format!("write failed: {err}"))?;
    session.wait_for("byte=0x03")?;
    session.settle(Duration::from_millis(300));
    if session.visible().contains("signal=interrupt") {
        return Err("a raw-mode terminal must not synthesise a signal".to_string());
    }
    if !session.pty.alive() {
        return Err("the child did not survive its own interrupt character".to_string());
    }
    Ok("the byte reached the child's input and nothing was signalled".to_string())
}

/// With the terminal left in its default mode, the same byte never reaches
/// the child: the terminal itself consumes it and raises a signal.
///
/// The other arm of the same finding, and the reason the two operations are
/// separate — which one works is a property of the CLI, not of this layer.
fn cooked_mode_turns_the_byte_into_a_signal() -> Result<String, String> {
    let mut session = Session::start("cooked", &[])?;
    session.wait_for("ready mode=cooked")?;
    session
        .pty
        .write(&[0x03])
        .map_err(|err| format!("write failed: {err}"))?;
    session.wait_for("signal=interrupt")?;
    session.settle(Duration::from_millis(300));
    if session.visible().contains("byte=0x03") {
        return Err("the byte reached the child as well as raising a signal".to_string());
    }
    Ok("the terminal consumed the byte and raised a signal instead".to_string())
}

/// A resize issued to a child that is up and listening is delivered, in both
/// directions.
fn a_settled_resize_is_observed() -> Result<String, String> {
    let mut session = Session::with("winsize", &[], |spec| {
        spec.dimensions = Some(Dimensions { cols: 80, rows: 24 });
    })?;
    session.wait_for("ready cols=80 rows=24")?;

    for target in [
        Dimensions {
            cols: 120,
            rows: 40,
        },
        Dimensions { cols: 80, rows: 24 },
    ] {
        session
            .pty
            .resize(target)
            .map_err(|err| format!("resize to {target} failed: {err}"))?;
        session.wait_for(&format!(
            "winsize cols={} rows={}",
            target.cols, target.rows
        ))?;
    }
    Ok("grew and shrank, both observed by the child".to_string())
}

/// A resize issued in the moment before the child takes the terminal either
/// lands or is lost, and the probe found both arms on Windows within a run.
///
/// So the assertion is not which arm happened — that is a platform race, not
/// a contract — but that the channel still works afterwards: a follow-up
/// resize away from wherever the geometry settled must be observed.
fn an_early_resize_still_recovers() -> Result<String, String> {
    let mut session = Session::with("winsize", &[], |spec| {
        spec.dimensions = Some(Dimensions { cols: 80, rows: 24 });
    })?;
    let early = Dimensions {
        cols: 132,
        rows: 43,
    };
    let raced = session.pty.resize(early).is_err();
    session.wait_for("ready cols=")?;

    let settled_cols: u16 = session
        .field("cols")
        .ok_or_else(|| "the child never reported its geometry".to_string())?
        .parse()
        .map_err(|err| format!("the reported width did not parse: {err}"))?;
    if settled_cols != early.cols && settled_cols != 80 {
        return Err(format!(
            "the terminal settled at {settled_cols} columns, which is neither geometry"
        ));
    }

    // Away from wherever it settled: resizing to the size it already has is
    // a change no platform notifies about.
    let recover = if settled_cols == early.cols {
        Dimensions { cols: 80, rows: 24 }
    } else {
        early
    };
    session
        .pty
        .resize(recover)
        .map_err(|err| format!("the follow-up resize failed: {err}"))?;
    session.wait_for(&format!(
        "winsize cols={} rows={}",
        recover.cols, recover.rows
    ))?;
    Ok(format!(
        "settled at {settled_cols} columns ({}), and the follow-up resize was still observed",
        if raced {
            "reported as racing the child"
        } else {
            "the child was already up"
        }
    ))
}
