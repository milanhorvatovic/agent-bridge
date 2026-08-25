//! The session contract against a real terminal and a real child, end to
//! end: create, run, approve, interrupt, close — every path that needs a
//! live process rather than the transition table.
//!
//! One scenario per promise, in one process, in order (the terminal
//! crate's pattern, for the same reasons). Run with
//! `cargo test -p agent-bridge-session --test lifecycle`.

// This target owns its own stdout twice over: the scenario runner reports
// results on it, and the fixture half writes its report lines into the
// terminal through it.
#![allow(clippy::disallowed_macros)]

use std::time::Duration;

use agent_bridge_pty::PtyError;
use agent_bridge_session::{
    ApprovalDecision, ApprovalId, ApprovalIdentity, ApprovalResolution, InputStep, SessionError,
    SessionState, ShutdownHint, spawn_session,
};
use bytes::Bytes;

mod support;

use support::{
    PATIENCE, Recorder, Scenario, cooperative_hint, fixture_spec, on_runtime, process_alive,
    scratch_dir, wait_state, wait_until_gone,
};

use agent_bridge_events::ApprovalPrompt;

fn main() {
    support::main(
        "session-lifecycle",
        &[
            Scenario {
                name: "cold_start_full_lifecycle_and_log_shape",
                check: cold_start_full_lifecycle_and_log_shape,
            },
            Scenario {
                name: "launch_failure_to_closed_with_paired_error",
                check: launch_failure_to_closed_with_paired_error,
            },
            Scenario {
                name: "child_exit_before_output_to_closed",
                check: child_exit_before_output_to_closed,
            },
            Scenario {
                name: "post_running_failure_routes_through_closing",
                check: post_running_failure_routes_through_closing,
            },
            Scenario {
                name: "output_then_instant_exit_still_counts_as_running",
                check: output_then_instant_exit_still_counts_as_running,
            },
            Scenario {
                name: "approvals_pend_and_resolve_independently",
                check: approvals_pend_and_resolve_independently,
            },
            Scenario {
                name: "interrupt_cancels_pending_set_then_resumes",
                check: interrupt_cancels_pending_set_then_resumes,
            },
            Scenario {
                name: "noncooperating_cli_escalates",
                check: noncooperating_cli_escalates,
            },
            Scenario {
                name: "force_close_during_drain_escalates_now",
                check: force_close_during_drain_escalates_now,
            },
            Scenario {
                name: "force_close_before_hint_dispatch_escalates_now",
                check: force_close_before_hint_dispatch_escalates_now,
            },
            Scenario {
                name: "cleanup_invariants_cover_the_grandchild",
                check: cleanup_invariants_cover_the_grandchild,
            },
            Scenario {
                name: "resize_bounds_and_writer_clearing",
                check: resize_bounds_and_writer_clearing,
            },
            Scenario {
                name: "input_saturation_refuses_and_close_stays_prompt",
                check: input_saturation_refuses_and_close_stays_prompt,
            },
        ],
    );
}

/// The cold-start contract in one pass: create to `Running`, a
/// cooperative close through the hint, the full event ladder in `seq`
/// order, and a session log in the runtime's record shape.
fn cold_start_full_lifecycle_and_log_shape() -> Result<String, String> {
    let dir = scratch_dir("cold-start");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec(
            "cooperative",
            &[],
            cooperative_hint(),
            log_dir.clone(),
            |_| {},
        );
        let session_id = spec.session_id;
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        let handle = spawned.handle;

        if handle.state() == SessionState::Created || handle.state() == SessionState::Launching {
            return Err(format!("launch resolved in {}", handle.state()));
        }
        wait_state(&handle, SessionState::Running).await?;

        // Input is accepted and ignored by the fixture; it exists here to
        // put bytes on the write-side counter.
        handle
            .send(Bytes::from_static(b"noop\r"))
            .await
            .map_err(|err| format!("send failed: {err}"))?;

        handle
            .close(false)
            .await
            .map_err(|err| format!("close failed: {err}"))?;
        if handle.state() != SessionState::Closed {
            return Err("close returned before Closed".to_string());
        }

        // The ladder, in order, with gap-free recorder seqs.
        let events = recorder.events();
        for (position, recorded) in events.iter().enumerate() {
            if recorded.seq != position as u64 {
                return Err(format!("seq {} at position {position}", recorded.seq));
            }
        }
        let types = recorder.event_types();
        let expected_prefix = [
            "lifecycle.session.created",
            "lifecycle.session.launching",
            "lifecycle.session.connecting",
            "lifecycle.session.running",
        ];
        if types.len() < 6 || types[..4] != expected_prefix {
            return Err(format!("wrong ladder start: {types:?}"));
        }
        if types[types.len() - 2..] != ["lifecycle.session.closing", "lifecycle.session.closed"] {
            return Err(format!("wrong ladder end: {types:?}"));
        }
        if recorder.sealed_count() != 1 {
            return Err(format!("sealed {} times", recorder.sealed_count()));
        }

        let closed = recorder.closed_payload().ok_or("no closed payload")?;
        if closed.drained != Some(true) {
            return Err(format!("drained = {:?}, wanted Some(true)", closed.drained));
        }
        if closed.exit_code != Some(0) {
            return Err(format!(
                "exit_code = {:?}, wanted Some(0)",
                closed.exit_code
            ));
        }
        if closed.bytes_read.unwrap_or(0) == 0 {
            return Err("bytes_read empty despite fixture output".to_string());
        }
        if closed.bytes_written.unwrap_or(0) < 5 {
            return Err(format!("bytes_written = {:?}", closed.bytes_written));
        }
        if closed.duration_ms.is_none() {
            return Err("duration_ms missing".to_string());
        }
        if closed.cleanup_verified != Some(true) || closed.remaining_processes.is_some() {
            return Err(format!(
                "cleanup not verified clean: {:?} / {:?}",
                closed.cleanup_verified, closed.remaining_processes
            ));
        }

        let metadata = handle.metadata();
        if metadata.started_at.is_none() || metadata.closed_at.is_none() {
            return Err("lifecycle timestamps missing from metadata".to_string());
        }

        // The session log: NDJSON in the runtime's record shape,
        // lifecycle at info, the event-metadata mirror at debug, and no
        // payload mirroring by default.
        let log_path = handle_log_path(&log_dir, &session_id.to_string());
        let text = std::fs::read_to_string(&log_path)
            .map_err(|err| format!("log unreadable at {}: {err}", log_path.display()))?;
        let mut debug_mirrors = 0;
        let mut info_lifecycle = 0;
        for line in text.lines() {
            let record: serde_json::Value =
                serde_json::from_str(line).map_err(|err| format!("bad NDJSON line: {err}"))?;
            for key in [
                "ts",
                "level",
                "component",
                "session_id",
                "event",
                "schema_version",
            ] {
                if record.get(key).is_none() {
                    return Err(format!("record missing {key}: {line}"));
                }
            }
            if record["component"] != "session" || record["session_id"] != session_id.to_string() {
                return Err(format!("mis-stamped record: {line}"));
            }
            if record["fields"].get("payload").is_some() {
                return Err("payload mirrored with mirror_payloads off".to_string());
            }
            if record["level"] == "debug" && record["fields"].get("seq").is_some() {
                debug_mirrors += 1;
            }
            if record["level"] == "info"
                && record["event"]
                    .as_str()
                    .is_some_and(|event| event.starts_with("lifecycle.session."))
            {
                info_lifecycle += 1;
            }
        }
        if debug_mirrors < 6 {
            return Err(format!("only {debug_mirrors} debug mirror records"));
        }
        if info_lifecycle < 6 {
            return Err(format!("only {info_lifecycle} info lifecycle records"));
        }

        Ok(format!(
            "{} events, {debug_mirrors} mirrors, drained cleanly",
            events.len()
        ))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// A binary that cannot exec: `Launching → Closed` with the paired
/// `pty.error`, and `-32005` back to the creator.
fn launch_failure_to_closed_with_paired_error() -> Result<String, String> {
    let dir = scratch_dir("launch-failure");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let mut spec = fixture_spec("unused", &[], ShutdownHint::CloseStdin, log_dir, |_| {});
        spec.launch.program = std::path::PathBuf::from("agent-bridge-no-such-binary");
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        let refusal = spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?;
        let Err(error) = refusal else {
            return Err("a nonexistent binary launched".to_string());
        };
        if error.jsonrpc_code() != -32005 {
            return Err(format!("wrong code {} for {error}", error.jsonrpc_code()));
        }
        wait_state(&spawned.handle, SessionState::Closed).await?;

        let types = recorder.event_types();
        if !types.contains(&"pty.error".to_string()) {
            return Err(format!("no paired pty.error in {types:?}"));
        }
        if types.contains(&"lifecycle.session.connecting".to_string()) {
            return Err("a failed launch reached Connecting".to_string());
        }
        if types.last().map(String::as_str) != Some("lifecycle.session.closed") {
            return Err(format!("ladder did not end closed: {types:?}"));
        }
        if recorder.sealed_count() != 1 {
            return Err(format!("sealed {} times", recorder.sealed_count()));
        }
        // No terminal stack ever stood, so the payload carries no byte
        // counts — absence, not a measured zero.
        let closed = recorder.closed_payload().ok_or("no closed payload")?;
        if closed.bytes_read.is_some() || closed.bytes_written.is_some() {
            return Err(format!(
                "byte counts present on a failed launch: read {:?}, written {:?}",
                closed.bytes_read, closed.bytes_written
            ));
        }
        // No terminal ever stood, so no census ever ran: the cleanup
        // verdict is absent, not a claimed pass.
        if closed.cleanup_verified.is_some() {
            return Err(format!(
                "cleanup verdict on a failed launch: {:?}",
                closed.cleanup_verified
            ));
        }
        Ok(format!("refused with -32005; ladder {types:?}"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// A child that exits without ever speaking: `Connecting → Closed`, no
/// `Closing` on the way.
fn child_exit_before_output_to_closed() -> Result<String, String> {
    let dir = scratch_dir("instant-exit");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec(
            "instant-exit",
            &[],
            ShutdownHint::CloseStdin,
            log_dir,
            |_| {},
        );
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        wait_state(&spawned.handle, SessionState::Closed).await?;

        let types = recorder.event_types();
        if types.contains(&"lifecycle.session.running".to_string()) {
            return Err(format!("a silent child reached Running: {types:?}"));
        }
        if types.contains(&"lifecycle.session.closing".to_string()) {
            return Err(format!(
                "Connecting → Closed must not pass Closing: {types:?}"
            ));
        }
        if !types.contains(&"pty.error".to_string()) {
            return Err(format!("no paired pty.error: {types:?}"));
        }
        let closed = recorder.closed_payload().ok_or("no closed payload")?;
        if closed.exit_code != Some(0) {
            return Err(format!("exit_code = {:?}", closed.exit_code));
        }
        Ok(format!("ladder {types:?}"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// A child that dies after speaking routes through `Closing`, never
/// straight to `Closed`.
fn post_running_failure_routes_through_closing() -> Result<String, String> {
    let dir = scratch_dir("crash");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec("crash", &[], ShutdownHint::CloseStdin, log_dir, |_| {});
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        wait_state(&spawned.handle, SessionState::Running).await?;
        wait_state(&spawned.handle, SessionState::Closed).await?;

        let types = recorder.event_types();
        let closing = types
            .iter()
            .position(|event| event == "lifecycle.session.closing")
            .ok_or_else(|| format!("no Closing on a post-Running failure: {types:?}"))?;
        let closed = types
            .iter()
            .position(|event| event == "lifecycle.session.closed")
            .ok_or("no closed event")?;
        if closing >= closed {
            return Err(format!("closing after closed: {types:?}"));
        }
        let payload = recorder.closed_payload().ok_or("no closed payload")?;
        if payload.exit_code != Some(3) {
            return Err(format!(
                "exit_code = {:?}, wanted Some(3)",
                payload.exit_code
            ));
        }
        Ok(format!("ladder {types:?}"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// A child that writes and exits in one breath: the exit signal can
/// outrun the first-output signal across their separate tasks, and the
/// session must still report that it ran — `running` and `closing` in the
/// ladder, never the exited-before-output shortcut.
fn output_then_instant_exit_still_counts_as_running() -> Result<String, String> {
    let dir = scratch_dir("flash");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec("flash", &[], ShutdownHint::CloseStdin, log_dir, |_| {});
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        wait_state(&spawned.handle, SessionState::Closed).await?;

        let types = recorder.event_types();
        if !types.contains(&"lifecycle.session.running".to_string()) {
            return Err(format!(
                "output was produced but Running is missing: {types:?}"
            ));
        }
        if !types.contains(&"lifecycle.session.closing".to_string()) {
            return Err(format!(
                "a session that ran must close via Closing: {types:?}"
            ));
        }
        let closed = recorder.closed_payload().ok_or("no closed payload")?;
        if closed.exit_code != Some(0) {
            return Err(format!(
                "exit_code = {:?}, wanted Some(0)",
                closed.exit_code
            ));
        }
        Ok(format!("ladder {types:?}"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// The multi-pending contract live: two pending hook approvals coexist,
/// resolve independently by id, a stale id changes nothing, and the
/// screen path keeps its one-dialog rule.
fn approvals_pend_and_resolve_independently() -> Result<String, String> {
    let dir = scratch_dir("approvals");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec("cooperative", &[], cooperative_hint(), log_dir, |_| {});
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        let handle = spawned.handle;
        wait_state(&handle, SessionState::Running).await?;

        let (_, mut first) = handle
            .announce_approval(
                ApprovalIdentity::Hook(ApprovalId("tool-a".into())),
                ApprovalPrompt::new("Allow bash?").tool("bash"),
            )
            .await
            .map_err(|err| format!("first announce: {err}"))?;
        let (_, mut second) = handle
            .announce_approval(
                ApprovalIdentity::Hook(ApprovalId("tool-b".into())),
                ApprovalPrompt::new("Allow write?").tool("write"),
            )
            .await
            .map_err(|err| format!("second announce: {err}"))?;
        if handle.state() != SessionState::AwaitingApproval {
            return Err(format!("state {} with two pending", handle.state()));
        }

        // One state event for the whole set; each prompt carries its id.
        let types = recorder.event_types();
        let awaiting = types
            .iter()
            .filter(|event| *event == "lifecycle.session.awaiting_approval")
            .count();
        if awaiting != 1 {
            return Err(format!("{awaiting} awaiting_approval events for one set"));
        }
        let prompt_ids: Vec<Option<String>> = recorder
            .events()
            .into_iter()
            .filter(|recorded| recorded.event_type == "prompt.approval_required")
            .map(|recorded| recorded.approval_id)
            .collect();
        if prompt_ids != [Some("tool-a".to_string()), Some("tool-b".to_string())] {
            return Err(format!("prompt correlation ids: {prompt_ids:?}"));
        }

        // Resolving one leaves the other pending, and the parked source
        // hears exactly its own verdict.
        handle
            .resolve_approval(ApprovalId("tool-b".into()), ApprovalDecision::Allow)
            .await
            .map_err(|err| format!("resolve b: {err}"))?;
        if !matches!(second.try_recv(), Ok(ApprovalResolution::Allow)) {
            return Err("source b did not hear Allow".to_string());
        }
        if handle.state() != SessionState::AwaitingApproval {
            return Err("the set emptied early".to_string());
        }

        // A stale id is rejected with the set untouched.
        let stale = handle
            .resolve_approval(ApprovalId("tool-b".into()), ApprovalDecision::Allow)
            .await;
        match stale {
            Err(error) if error.jsonrpc_code() == -32007 => {}
            other => return Err(format!("stale id: {other:?}")),
        }
        if first.try_recv().is_ok() {
            return Err("the stale id disturbed the pending prompt".to_string());
        }

        handle
            .resolve_approval(
                ApprovalId("tool-a".into()),
                ApprovalDecision::Deny {
                    reason: Some("not in this repo".into()),
                },
            )
            .await
            .map_err(|err| format!("resolve a: {err}"))?;
        match first.try_recv() {
            Ok(ApprovalResolution::Deny { reason })
                if reason.as_deref() == Some("not in this repo") => {}
            other => return Err(format!("source a heard {other:?}")),
        }
        wait_state(&handle, SessionState::Running).await?;

        // The screen path keeps one dialog at a time; hooks still pend
        // beside it.
        let (screen_id, _screen) = handle
            .announce_approval(
                ApprovalIdentity::Screen,
                ApprovalPrompt::new("Trust this folder?"),
            )
            .await
            .map_err(|err| format!("screen announce: {err}"))?;
        // The id is the runtime's mint, not anything a source supplied —
        // the announcing surface cannot even pass one for a screen prompt.
        if uuid::Uuid::parse_str(&screen_id.0).is_err() {
            return Err(format!("screen id {:?} is not a minted UUID", screen_id.0));
        }
        let violation = handle
            .announce_approval(
                ApprovalIdentity::Screen,
                ApprovalPrompt::new("Second dialog?"),
            )
            .await;
        if !matches!(
            violation,
            Err(SessionError::ScreenApprovalContractViolation)
        ) {
            return Err(format!("second screen prompt: {violation:?}"));
        }
        handle
            .announce_approval(
                ApprovalIdentity::Hook(ApprovalId("tool-c".into())),
                ApprovalPrompt::new("Allow read?"),
            )
            .await
            .map_err(|err| format!("hook beside screen: {err}"))?;

        handle
            .close(true)
            .await
            .map_err(|err| format!("close: {err}"))?;
        Ok("multi-pending, stale-id, and screen rules held".to_string())
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// Interrupt during `AwaitingApproval` cancels every pending approval,
/// the ack lands the `Interrupted` state instead of a wedge, and resume
/// returns to `Running`.
fn interrupt_cancels_pending_set_then_resumes() -> Result<String, String> {
    let dir = scratch_dir("interrupt");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec("raw", &[], ShutdownHint::CloseStdin, log_dir, |_| {});
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        let handle = spawned.handle;
        wait_state(&handle, SessionState::Running).await?;

        let (_, mut first) = handle
            .announce_approval(
                ApprovalIdentity::Hook(ApprovalId("tool-a".into())),
                ApprovalPrompt::new("Allow bash?"),
            )
            .await
            .map_err(|err| format!("announce: {err}"))?;
        let (_, mut second) = handle
            .announce_approval(
                ApprovalIdentity::Hook(ApprovalId("tool-b".into())),
                ApprovalPrompt::new("Allow write?"),
            )
            .await
            .map_err(|err| format!("announce: {err}"))?;

        handle
            .interrupt()
            .await
            .map_err(|err| format!("interrupt: {err}"))?;
        // Both parked sources hear Cancelled at delivery, before any
        // acknowledgement.
        if !matches!(first.try_recv(), Ok(ApprovalResolution::Cancelled))
            || !matches!(second.try_recv(), Ok(ApprovalResolution::Cancelled))
        {
            return Err("pending approvals were not cancelled at interrupt".to_string());
        }

        // The sweep empties the set AwaitingApproval stands for, so the
        // state returns to Running at once — never claiming approvals
        // that no longer exist — and the acknowledgement then lands
        // Interrupted with its published meaning intact.
        wait_state(&handle, SessionState::Running).await?;
        handle.interrupt_acknowledged().await;
        wait_state(&handle, SessionState::Interrupted).await?;
        let resolve_now = handle
            .resolve_approval(ApprovalId("tool-a".into()), ApprovalDecision::Allow)
            .await;
        match resolve_now {
            Err(error) if error.jsonrpc_code() == -32006 => {}
            other => return Err(format!("resolve while Interrupted: {other:?}")),
        }

        handle.resumed().await;
        wait_state(&handle, SessionState::Running).await?;

        // And the plain Running-interrupt for completeness of the edge
        // pair.
        handle
            .interrupt()
            .await
            .map_err(|err| format!("second interrupt: {err}"))?;
        handle.interrupt_acknowledged().await;
        wait_state(&handle, SessionState::Interrupted).await?;

        // No failure routing fired anywhere in this scenario: the fixture
        // survived both interrupts.
        let types = recorder.event_types();
        if types.contains(&"lifecycle.session.closing".to_string()) {
            return Err(format!("something closed early: {types:?}"));
        }

        handle
            .close(true)
            .await
            .map_err(|err| format!("close from Interrupted: {err}"))?;
        Ok("cancel-at-interrupt, ack edge, and resume all held".to_string())
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// A CLI that ignores its hint: the drain window expires, the terminate
/// sequence ends the session, and `drained: false` says so.
fn noncooperating_cli_escalates() -> Result<String, String> {
    let dir = scratch_dir("deaf");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec("deaf", &[], cooperative_hint(), log_dir, |config| {
            config.stdin_drain = Duration::from_millis(500);
        });
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        let handle = spawned.handle;
        wait_state(&handle, SessionState::Running).await?;

        let started = tokio::time::Instant::now();
        handle
            .close(false)
            .await
            .map_err(|err| format!("close: {err}"))?;
        if started.elapsed() > PATIENCE {
            return Err("escalation took longer than patience".to_string());
        }
        let closed = recorder.closed_payload().ok_or("no closed payload")?;
        if closed.drained != Some(false) {
            return Err(format!(
                "drained = {:?}, wanted Some(false)",
                closed.drained
            ));
        }
        Ok(format!("escalated in {:?}", started.elapsed()))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// `close(force = true)` while a graceful drain is waiting escalates
/// immediately instead of serving out the window.
fn force_close_during_drain_escalates_now() -> Result<String, String> {
    let dir = scratch_dir("force-during-drain");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec("deaf", &[], cooperative_hint(), log_dir, |config| {
            // Long enough that only the forced escalation can explain a
            // prompt close.
            config.stdin_drain = Duration::from_secs(60);
        });
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        let handle = spawned.handle;
        wait_state(&handle, SessionState::Running).await?;

        let graceful = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.close(false).await })
        };
        wait_state(&handle, SessionState::Closing).await?;
        // Synchronize on delivery, not on elapsed time: the hint's five
        // keystrokes land on the write counter, and only after the last
        // one can `HintDispatched` arm the window. The short grace after
        // covers command-queue processing alone. Should a pathologically
        // loaded runner still beat the arming, the force takes the
        // pre-dispatch path — whose observable behavior is identical and
        // separately covered — so the assertion below holds either way.
        let hint_bytes = 5;
        let delivered = tokio::time::Instant::now() + PATIENCE;
        while handle.metadata().bytes_written < hint_bytes {
            if tokio::time::Instant::now() >= delivered {
                return Err("the hint's keystrokes were never delivered".to_string());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        let started = tokio::time::Instant::now();
        handle
            .close(true)
            .await
            .map_err(|err| format!("force close: {err}"))?;
        if started.elapsed() > Duration::from_secs(10) {
            return Err(format!("forced escalation took {:?}", started.elapsed()));
        }
        graceful
            .await
            .map_err(|_| "the graceful closer panicked".to_string())?
            .map_err(|err| format!("graceful close: {err}"))?;
        let closed = recorder.closed_payload().ok_or("no closed payload")?;
        if closed.drained != Some(false) {
            return Err(format!(
                "drained = {:?}, wanted Some(false)",
                closed.drained
            ));
        }
        Ok(format!("both closers resolved in {:?}", started.elapsed()))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// A force-close that lands while the input hint is still dispatching —
/// `Closing`, but before `HintDispatched` has armed any drain window.
/// The unarmed span concedes nothing: escalation is immediate and the
/// payload reads `drained: false`, exactly as a force during the armed
/// window does.
fn force_close_before_hint_dispatch_escalates_now() -> Result<String, String> {
    let dir = scratch_dir("force-before-dispatch");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        // A settle far longer than the moment the force arrives, so the
        // hint task is deterministically still mid-dispatch — the window
        // unarmed — when the force lands. Well under the dispatch budget,
        // so nothing here is the budget path.
        let hint = ShutdownHint::Input(vec![
            InputStep::Write("ignored".into()),
            InputStep::Settle(Duration::from_secs(30)),
            InputStep::Write("\r".into()),
        ]);
        let spec = fixture_spec("deaf", &[], hint, log_dir, |config| {
            config.stdin_drain = Duration::from_secs(60);
        });
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        let handle = spawned.handle;
        wait_state(&handle, SessionState::Running).await?;

        let graceful = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.close(false).await })
        };
        // The state watch is the synchronization; the 30-second settle
        // still guarantees `HintDispatched` has not fired when the force
        // lands below.
        wait_state(&handle, SessionState::Closing).await?;
        let started = tokio::time::Instant::now();
        handle
            .close(true)
            .await
            .map_err(|err| format!("force close: {err}"))?;
        if started.elapsed() > Duration::from_secs(10) {
            return Err(format!("forced escalation took {:?}", started.elapsed()));
        }
        graceful
            .await
            .map_err(|_| "the graceful closer panicked".to_string())?
            .map_err(|err| format!("graceful close: {err}"))?;
        let closed = recorder.closed_payload().ok_or("no closed payload")?;
        if closed.drained != Some(false) {
            return Err(format!(
                "drained = {:?}, wanted Some(false)",
                closed.drained
            ));
        }
        Ok(format!(
            "forced past the mid-dispatch hint in {:?}",
            started.elapsed()
        ))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// The cleanup invariants at session level, grandchild included: on
/// `Closed`, nothing the session spawned is left running.
fn cleanup_invariants_cover_the_grandchild() -> Result<String, String> {
    let dir = scratch_dir("tree");
    let log_dir = dir.clone();
    let pid_file = dir.join("grandchild.pid");
    let pid_file_arg = pid_file.to_string_lossy().to_string();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec(
            "tree",
            &[pid_file_arg.as_str()],
            ShutdownHint::CloseStdin,
            log_dir,
            |_| {},
        );
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        let handle = spawned.handle;
        wait_state(&handle, SessionState::Running).await?;

        handle
            .send(Bytes::from_static(b"t\r"))
            .await
            .map_err(|err| format!("send: {err}"))?;
        let grandchild = wait_for_pid_file(&pid_file).await?;
        if !process_alive(grandchild) {
            return Err("the grandchild never lived".to_string());
        }

        handle
            .close(true)
            .await
            .map_err(|err| format!("close: {err}"))?;
        // The invariant: the whole tree is gone, not just the child the
        // terminal spawned.
        wait_until_gone(grandchild).await?;
        if recorder.sealed_count() != 1 {
            return Err(format!("sealed {} times", recorder.sealed_count()));
        }
        Ok(format!("grandchild {grandchild} contained and gone"))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// Geometry: in-bounds resizes land in metadata, out-of-bounds requests
/// are refused before the terminal hears of them — and the writer field
/// clears on transport drop.
fn resize_bounds_and_writer_clearing() -> Result<String, String> {
    let dir = scratch_dir("resize");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec(
            "cooperative",
            &[],
            cooperative_hint(),
            log_dir.clone(),
            |config| {
                config.mirror_payloads = true;
            },
        );
        let session_id = spec.session_id;
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        let handle = spawned.handle;
        wait_state(&handle, SessionState::Running).await?;

        handle
            .resize(120, 40)
            .await
            .map_err(|err| format!("resize: {err}"))?;
        let dims = handle.metadata().dimensions;
        if (dims.cols, dims.rows) != (120, 40) {
            return Err(format!("metadata says {dims}"));
        }
        for (cols, rows) in [(0, 24), (80, 0), (500, 50), (200, 101)] {
            match handle.resize(cols, rows).await {
                Err(error) if error.jsonrpc_code() == -32602 => {}
                other => return Err(format!("resize {cols}x{rows}: {other:?}")),
            }
        }

        // An approval passes through while payload mirroring is opted in,
        // so the log assertions below can prove the prompt's text is the
        // one payload that still never reaches disk.
        // The receiver is held for the resolve below: a source that
        // vanishes forfeits its entry, and resolving a forfeited prompt
        // reports stale.
        let (_, _resolution) = handle
            .announce_approval(
                ApprovalIdentity::Hook(ApprovalId("tool-log".into())),
                ApprovalPrompt::new("Allow POST with header Bearer hunter2?"),
            )
            .await
            .map_err(|err| format!("announce: {err}"))?;
        handle
            .resolve_approval(ApprovalId("tool-log".into()), ApprovalDecision::Allow)
            .await
            .map_err(|err| format!("resolve: {err}"))?;

        if handle.writer().map(|writer| writer.0) != Some("peer-0".to_string()) {
            return Err("the creator is not the writer".to_string());
        }
        handle.transport_dropped().await;
        let deadline = tokio::time::Instant::now() + PATIENCE;
        while handle.writer().is_some() {
            if tokio::time::Instant::now() >= deadline {
                return Err("writer never cleared after transport drop".to_string());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        handle
            .close(false)
            .await
            .map_err(|err| format!("close: {err}"))?;

        // Live operations against a closed session answer -32003; a second
        // close is a race resolved, not an error.
        match handle.send(Bytes::from_static(b"late\r")).await {
            Err(error) if error.jsonrpc_code() == -32003 => {}
            other => return Err(format!("send after close: {other:?}")),
        }
        handle
            .close(true)
            .await
            .map_err(|err| format!("second close: {err}"))?;

        // With mirroring opted in, at least one record carries a payload —
        // and the approval prompt is the one event whose payload still
        // must not: its text can carry credentials, and the record above
        // deliberately looks like it does.
        let log_path = handle_log_path(&log_dir, &session_id.to_string());
        let text =
            std::fs::read_to_string(&log_path).map_err(|err| format!("log unreadable: {err}"))?;
        let mut mirrored = false;
        let mut prompt_seen = false;
        for line in text.lines() {
            let record: serde_json::Value =
                serde_json::from_str(line).map_err(|err| format!("bad NDJSON line: {err}"))?;
            let has_payload = record["fields"].get("payload").is_some();
            mirrored |= has_payload;
            if record["event"] == "prompt.approval_required" {
                prompt_seen = true;
                if has_payload {
                    return Err("an approval prompt's payload reached the log".to_string());
                }
            }
            if line.contains("hunter2") {
                return Err(format!("prompt text reached the log: {line}"));
            }
        }
        if !mirrored {
            return Err("mirror_payloads=true wrote no payload".to_string());
        }
        if !prompt_seen {
            return Err("the prompt's metadata mirror record is missing".to_string());
        }
        Ok("bounds, writer clearing, and guarded opt-in mirroring held".to_string())
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

/// The control plane under a jammed data path: a child that never reads
/// lets the kernel buffer fill, the first write parks in the writer, the
/// queue fills behind it — and the next send is refused with every byte
/// handed back, while a force-close still completes promptly. The
/// load-bearing claim is that the actor never waits on the child.
fn input_saturation_refuses_and_close_stays_prompt() -> Result<String, String> {
    let dir = scratch_dir("stdin-saturation");
    let log_dir = dir.clone();
    let outcome = on_runtime(async move {
        let recorder = Recorder::default();
        let spec = fixture_spec("deaf", &[], cooperative_hint(), log_dir, |_| {});
        let spawned = spawn_session(spec, Box::new(recorder.clone()))
            .map_err(|err| format!("spawn refused: {err}"))?;
        spawned
            .launch
            .await
            .map_err(|_| "the actor died before reporting".to_string())?
            .map_err(|err| format!("launch failed: {err}"))?;
        let handle = spawned.handle;
        wait_state(&handle, SessionState::Running).await?;

        // Each payload dwarfs any kernel terminal buffer, so the first
        // write parks on its deadline and everything behind it queues.
        // Twenty concurrent senders overfill the sixteen-slot queue plus
        // the write in flight; the overflow is refused immediately while
        // the accepted ones park — the teardown answers those.
        let chunk = Bytes::from(vec![b'x'; 1024 * 1024]);
        let mut senders = tokio::task::JoinSet::new();
        for _ in 0..20 {
            let handle = handle.clone();
            let chunk = chunk.clone();
            senders.spawn(async move { handle.send(chunk).await });
        }
        let deadline = tokio::time::Instant::now() + PATIENCE;
        let unwritten = loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, senders.join_next()).await {
                Ok(Some(Ok(Err(SessionError::Pty(PtyError::StdinBlocked { unwritten }))))) => {
                    break unwritten;
                }
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(_))) => return Err("a sender task panicked".to_string()),
                Ok(None) => return Err("every send settled without a refusal".to_string()),
                Err(_) => return Err("the queue never refused within patience".to_string()),
            }
        };
        if unwritten != chunk.as_ref() {
            return Err(format!(
                "refusal returned {} bytes, sent {}",
                unwritten.len(),
                chunk.len()
            ));
        }

        // The refusal proven, the control plane must still answer: the
        // force-close travels the command queue, never the input queue,
        // and completes within the in-flight write's own deadline plus
        // teardown — nowhere near the sum a parked data path would cost.
        let started = tokio::time::Instant::now();
        handle
            .close(true)
            .await
            .map_err(|err| format!("force close: {err}"))?;
        if started.elapsed() > Duration::from_secs(20) {
            return Err(format!(
                "forced close took {:?} behind a jammed writer",
                started.elapsed()
            ));
        }
        senders.abort_all();
        Ok(format!(
            "refused with all bytes back; closed in {:?}",
            started.elapsed()
        ))
    });
    let _ = std::fs::remove_dir_all(&dir);
    outcome
}

fn handle_log_path(dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    dir.join("sessions").join(format!("{session_id}.log"))
}

async fn wait_for_pid_file(path: &std::path::Path) -> Result<u32, String> {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            return Ok(pid);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("the grandchild pid file never appeared".to_string());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
