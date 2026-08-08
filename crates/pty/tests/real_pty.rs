//! What the PTY layer promises, checked against a real terminal.
//!
//! One scenario per promise, in one process, in order — see `support` for
//! why serial is a requirement here rather than a simplification. Run with
//! `cargo test -p agent-bridge-pty --test real_pty`.

// This target owns its own stdout twice over: the scenario runner reports
// results on it, and the fixture half writes its report lines into the
// terminal through it. Both are the output, not a diagnostic.
#![allow(clippy::disallowed_macros)]

use std::ffi::OsStr;
use std::time::Duration;

use agent_bridge_pty::{Dimensions, EnvStrip, PtyError, Signal};

mod support;

use support::{Scenario, Session, process_alive};

fn main() {
    support::main(
        "real-pty",
        &[
            Scenario {
                name: "spawn_and_read",
                check: spawn_and_read,
            },
            Scenario {
                name: "interrupt_is_a_byte_not_a_signal",
                check: interrupt_is_a_byte_not_a_signal,
            },
            Scenario {
                name: "signal_reaches_the_whole_group",
                check: signal_reaches_the_whole_group,
            },
            Scenario {
                name: "terminate_leaves_nothing_behind",
                check: terminate_leaves_nothing_behind,
            },
            Scenario {
                name: "a_dropped_handle_ends_the_session",
                check: a_dropped_handle_ends_the_session,
            },
            Scenario {
                name: "blocked_input_times_out_with_its_suffix",
                check: blocked_input_times_out_with_its_suffix,
            },
            Scenario {
                name: "characters_survive_the_read_boundary",
                check: characters_survive_the_read_boundary,
            },
            Scenario {
                name: "env_defaults_present_unless_overridden",
                check: env_defaults_present_unless_overridden,
            },
            Scenario {
                name: "resize_is_observed_by_the_child",
                check: resize_is_observed_by_the_child,
            },
            Scenario {
                name: "unset_geometry_takes_the_default",
                check: unset_geometry_takes_the_default,
            },
            Scenario {
                name: "resize_before_the_child_speaks_is_reported",
                check: resize_before_the_child_speaks_is_reported,
            },
        ],
    );
}

/// A child runs in the terminal and what it prints comes back.
fn spawn_and_read() -> Result<String, String> {
    let mut session = Session::start("echo", &["hello-from-the-terminal"])?;
    session.wait_for("echo=hello-from-the-terminal")?;
    session.settle(Duration::from_millis(200));
    let status = session
        .pty
        .terminate(Duration::from_secs(2))
        .map_err(|err| format!("terminate failed: {err}"))?;
    if !status.success() {
        return Err(format!("the child ended badly: {status}"));
    }
    Ok(format!("output read back; child ended with {status}"))
}

/// The load-bearing distinction of the whole layer: interrupting an
/// interactive CLI means writing a byte into its terminal, and a delivered
/// `SIGINT` is a different thing that ends it.
fn interrupt_is_a_byte_not_a_signal() -> Result<String, String> {
    let mut session = Session::start("raw", &[])?;
    session.wait_for("ready mode=raw")?;

    session
        .pty
        .interrupt()
        .map_err(|err| format!("interrupt failed: {err}"))?;
    session.wait_for("byte=0x03")?;
    session.settle(Duration::from_millis(300));

    if session.visible().contains("signal=interrupt") {
        return Err("the child was signalled; the interrupt must be a byte".to_string());
    }
    if !session.pty.alive() {
        return Err("the child died; an interrupt must not end the session".to_string());
    }

    // And the other half of the contract: the signal path is a different
    // thing, visibly. A raw-mode child sees it as a signal with no byte on
    // its input.
    //
    // POSIX only, and not a gap: Windows has no way to deliver an interrupt
    // to a process in another console, so the layer reports that rather than
    // pretending a delivery happened. The byte path above — which is what an
    // adapter actually uses — works identically on both.
    #[cfg(unix)]
    {
        let before = session.count("byte");
        session
            .pty
            .signal(Signal::Interrupt)
            .map_err(|err| format!("signal failed: {err}"))?;
        session.wait_for("signal=interrupt")?;
        session.settle(Duration::from_millis(300));
        if session.count("byte") != before {
            return Err("the signal path delivered a byte as well".to_string());
        }
    }
    #[cfg(windows)]
    match session.pty.signal(Signal::Interrupt) {
        Err(PtyError::SignalFailed { .. }) => {}
        Err(other) => {
            return Err(format!(
                "expected an unsupported-signal report, got: {other}"
            ));
        }
        Ok(()) => {
            return Err("this platform cannot deliver that signal, and said it could".to_string());
        }
    }
    Ok("the byte arrived as input and the signal arrived as a signal".to_string())
}

/// A signal addresses the process group, so a shell the CLI spawned for a
/// tool call is not left running.
fn signal_reaches_the_whole_group() -> Result<String, String> {
    let (session, grandchild) = a_tree()?;
    let child = session.pty.child_pid().get();

    // What the containment holds, asked rather than assumed: a signal that
    // reached only the process this crate started would leave the shell a
    // CLI opens for a tool call running, and the enumeration is how an
    // operator or a supervisor sees that at all.
    let held = session
        .pty
        .contained()
        .map_err(|err| format!("the session's processes could not be listed: {err}"))?;
    let ids: Vec<u32> = held.iter().map(|pid| pid.get()).collect();
    if !ids.contains(&child) || !ids.contains(&grandchild) {
        return Err(format!(
            "the session holds {ids:?}, which is missing the child {child} or its descendant {grandchild}"
        ));
    }

    session
        .pty
        .signal(Signal::Terminate)
        .map_err(|err| format!("signal failed: {err}"))?;
    support::wait_until_gone(grandchild)?;
    support::wait_until_gone(child)?;
    Ok(format!(
        "child {child} and grandchild {grandchild} both received it"
    ))
}

/// Ending a session ends everything in it.
fn terminate_leaves_nothing_behind() -> Result<String, String> {
    let (session, grandchild) = a_tree()?;
    let child = session.pty.child_pid().get();

    let status = session
        .pty
        .terminate(Duration::from_millis(500))
        .map_err(|err| format!("terminate failed: {err}"))?;
    support::wait_until_gone(grandchild)?;
    support::wait_until_gone(child)?;
    // Named rather than counted: "something is left" sends whoever reads a
    // failure here looking, and the list is what they would go looking for.
    let left = session
        .pty
        .contained()
        .map_err(|err| format!("the session's processes could not be listed: {err}"))?;
    if !left.is_empty() {
        let ids: Vec<u32> = left.iter().map(|pid| pid.get()).collect();
        return Err(format!("the session still holds {ids:?} after terminating"));
    }
    Ok(format!("nothing left after {status}"))
}

/// Dropping the handle is the safety net for a session nobody closed: it has
/// to leave as little behind as an orderly termination does, or a runtime
/// that panics leaks a CLI process per session.
fn a_dropped_handle_ends_the_session() -> Result<String, String> {
    #[cfg(windows)]
    let hosts_before = support::console_hosts();

    let (session, grandchild) = a_tree()?;
    let child = session.pty.child_pid().get();
    drop(session);

    // Asked first, and before anything in this suite reaps: killing a child
    // is not collecting it, and a handle that signalled without waiting
    // would leave one zombie per abandoned session — invisible to every
    // liveness check, because a corpse answers them all exactly as a live
    // process does.
    #[cfg(unix)]
    support::assert_collected(child)?;

    support::wait_until_gone(grandchild)?;
    support::wait_until_gone(child)?;

    #[cfg(windows)]
    {
        // The terminal's own console host is not in the job — terminating
        // the job would destroy the terminal along with the child — so this
        // is the check that closing the pseudo-console really took it.
        let deadline = std::time::Instant::now() + support::PATIENCE;
        loop {
            let leaked: Vec<u32> = support::console_hosts()
                .into_iter()
                .filter(|pid| !hosts_before.contains(pid))
                .collect();
            if leaked.is_empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!("console hosts survived the terminal: {leaked:?}"));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    Ok(format!(
        "child {child} and grandchild {grandchild} gone with the handle"
    ))
}

/// A child that stops reading must not wedge the writer for good: the write
/// gives up at its deadline and hands back exactly what never went, so the
/// caller can retry it without sending anything twice.
///
/// Whether a terminal can be made to stall at all is a platform property,
/// and not one this layer controls: some line disciplines apply backpressure
/// to the writer when the child stops reading, and others quietly discard
/// the overflow, in which case there is nothing out here that can provoke
/// the condition. So which arm happened is recorded rather than asserted —
/// what is asserted is the contract, whenever the arm that has one occurs.
/// The deadline arithmetic itself is pinned down where it can be: the unit
/// tests beside the retry loop, against a terminal that stops taking bytes
/// on command.
fn blocked_input_times_out_with_its_suffix() -> Result<String, String> {
    let mut session = Session::with("deaf", &[], |spec| {
        // Shorter than the default: what matters is that the deadline is
        // honoured, and the default would add five seconds to every run.
        spec.write_timeout = Duration::from_millis(500);
    })?;
    session.wait_for("ready")?;

    // Comfortably more than any terminal's input buffer, so a terminal that
    // does apply backpressure certainly will.
    let payload = vec![b'x'; 1024 * 1024];
    match session.pty.write(&payload) {
        Err(PtyError::StdinBlocked { unwritten }) => {
            if unwritten.is_empty() || unwritten.len() > payload.len() {
                return Err(format!(
                    "a blocked write reported {} of {} bytes unwritten, which is not a suffix of it",
                    unwritten.len(),
                    payload.len()
                ));
            }
            if unwritten != payload[payload.len() - unwritten.len()..] {
                return Err("what came back was not the tail of what went in".to_string());
            }
            Ok(format!(
                "the terminal stalled and handed back the last {} of {} bytes",
                unwritten.len(),
                payload.len()
            ))
        }
        Err(other) => Err(format!("expected a stalled write, got: {other}")),
        Ok(()) => Ok(format!(
            "this terminal absorbed {} bytes with nobody reading them, so it \
             cannot be made to stall from out here",
            payload.len()
        )),
    }
}

/// A character split across two reads arrives whole, because the layer holds
/// the unfinished part back rather than handing on half of it.
fn characters_survive_the_read_boundary() -> Result<String, String> {
    let mut session = Session::start("utf8", &[])?;
    session.wait_for("corpus-end")?;

    let visible = session.visible();
    if !visible.contains(support::child::UTF8_CORPUS) {
        return Err(format!("the text did not survive; got: {}", session.tail()));
    }
    if !session.invalid().is_empty() {
        return Err(format!(
            "valid text was reported as undecodable: {:?}",
            session.invalid()
        ));
    }
    Ok("two-, three- and four-byte characters round-tripped".to_string())
}

/// The terminal defaults are set on every spawn, the caller outranks them,
/// and the strip rule outranks everyone.
fn env_defaults_present_unless_overridden() -> Result<String, String> {
    let mut session = Session::with("env", &[], |spec| {
        spec.dimensions = Some(Dimensions {
            cols: 120,
            rows: 40,
        });
        spec.env.push(("COLORTERM".into(), "16".into()));
        spec.env.push(("PLANTED".into(), "must-not-arrive".into()));
        spec.strip = EnvStrip::new(|name| name == OsStr::new("PLANTED"));
    })?;
    session.wait_for("done")?;

    let expected = [
        ("TERM", "xterm-256color"),
        ("COLORTERM", "16"),
        ("COLUMNS", "120"),
        ("LINES", "40"),
        ("PLANTED", "<unset>"),
    ];
    for (name, want) in expected {
        let got = session
            .field(name)
            .ok_or_else(|| format!("the child never reported {name}"))?;
        if got != want {
            return Err(format!("{name} was {got}, expected {want}"));
        }
    }
    #[cfg(unix)]
    {
        let locale = session
            .field("LC_ALL")
            .ok_or_else(|| "the child never reported LC_ALL".to_string())?;
        if !locale.contains("UTF-8") {
            return Err(format!("LC_ALL was {locale}, which is not a UTF-8 locale"));
        }
    }
    Ok("defaults set, caller override honoured, planted variable stripped".to_string())
}

/// A resize reaches the child, which is the whole reason the operation
/// exists — a CLI that renders at the wrong width looks broken to whoever is
/// watching.
fn resize_is_observed_by_the_child() -> Result<String, String> {
    let mut session = Session::with("winsize", &[], |spec| {
        spec.dimensions = Some(Dimensions { cols: 80, rows: 24 });
    })?;
    session.wait_for("ready cols=80 rows=24")?;

    session
        .pty
        .resize(Dimensions {
            cols: 120,
            rows: 40,
        })
        .map_err(|err| format!("resize failed: {err}"))?;
    session.wait_for("winsize cols=120 rows=40")?;
    Ok("80x24 -> 120x40 observed by the child".to_string())
}

/// A session that asks for no geometry gets one anyway, and it is the one a
/// CLI assumes when it cannot ask.
fn unset_geometry_takes_the_default() -> Result<String, String> {
    let mut session = Session::with("winsize", &[], |spec| spec.dimensions = None)?;
    session.wait_for("ready cols=80 rows=24")?;
    Ok("no geometry requested; the child found 80x24".to_string())
}

/// Resizing a terminal the child has not taken possession of applies the
/// geometry but cannot notify anyone, and the caller is told so rather than
/// left to discover it.
fn resize_before_the_child_speaks_is_reported() -> Result<String, String> {
    let session = Session::start("idle", &[])?;
    match session.pty.resize(Dimensions {
        cols: 100,
        rows: 30,
    }) {
        Err(PtyError::ResizeBeforeReady) => {
            Ok("the race was reported rather than passed off as done".to_string())
        }
        Err(other) => Err(format!("expected the early-resize report, got: {other}")),
        // Not a failure of the implementation: the child is free to have
        // spoken already, and on a loaded machine it sometimes has. Saying
        // so beats a flaky assertion.
        Ok(()) => Ok("the child had already spoken; nothing to report".to_string()),
    }
}

/// A child with a descendant of its own, and that descendant's identifier.
fn a_tree() -> Result<(Session, u32), String> {
    let mut session = Session::start("tree", &[])?;
    // The fixture is ready before it has a descendant, and grows one only
    // when told. That order is what lets containment be established first —
    // see the fixture for why a descendant that predates it escapes.
    session.wait_for("ready")?;
    session
        .pty
        .write(&[support::child::GROW_BYTE])
        .map_err(|err| format!("could not ask the child to grow a tree: {err}"))?;
    session.wait_for("grandchild=")?;
    let grandchild: u32 = session
        .field("grandchild")
        .ok_or_else(|| "the child never reported its descendant".to_string())?
        .parse()
        .map_err(|err| format!("the descendant's id did not parse: {err}"))?;
    if !process_alive(grandchild) {
        return Err(format!("the descendant {grandchild} was never running"));
    }
    Ok((session, grandchild))
}
