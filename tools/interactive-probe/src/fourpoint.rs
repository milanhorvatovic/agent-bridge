//! The four-point verification of the hook channel against a real session:
//!
//!   1. Hooks fire and their payload reaches the probe's listener.
//!   2. The allow/deny/ask approval round-trip behaves as designed: allow
//!      executes, deny blocks with our reason, ask degrades to the CLI's own
//!      permission dialog.
//!   3. The transcript channel writes, under the same environment hygiene as
//!      everywhere else.
//!   4. A Ctrl+C byte interrupts generation and the session survives.
//!
//! **Windows is the reason this exists.** There, the console is ConPTY and
//! the hook channel is a named pipe, where POSIX has a tty and a Unix domain
//! socket — and Ctrl+C semantics differ. None of the four could be checked
//! from POSIX, so all four are re-verified rather than assumed to carry over.
//! The run that matters happens on Windows 11 **client** hardware; hosted CI
//! offers only Windows Server, and letting Server stand in for client is the
//! mistake this verification exists to avoid.
//!
//! It nonetheless runs on **every** platform, deliberately, for two reasons.
//! The POSIX run produces the baseline that point 2's word "parity" refers
//! to — a parity claim with nothing to compare against is a slogan. And it
//! means the runner is exercised long before anyone spends a Windows session
//! on it, so a Windows red is news about ConPTY rather than news about this
//! code. Each step line names the platform and the transport it ran over.
//!
//! Each point reports green or red independently and a red is a recorded
//! finding, never a silent waiver: a partial result is the honest outcome,
//! and it stays legible.
//!
//! Points 1 and 3 come free with the shared rig's `establish` step
//! (SessionStart over the channel; transcript liveness). This module adds
//! points 2 and 4, which need deliberate stimuli the ordinary probe run does
//! not issue.

use std::time::{Duration, Instant};

use crate::hooks::Decision;
use crate::rig::{LiveSession, ProbeConfig, TURN_TIMEOUT, TYPE_SETTLE, launch};
use crate::{Failure, print_step};

/// The interrupt primitive under test: the Ctrl+C byte, written into the
/// child's terminal. Not a signal — the point is that the byte alone
/// suffices, on ConPTY as on a tty.
const CTRL_C: u8 = 0x03;

/// Escape, which dismisses the CLI's permission dialog.
const ESCAPE: &[u8] = b"\x1b";

/// How long the long task is allowed to stream before it is interrupted.
const GENERATION_WARMUP: Duration = Duration::from_millis(2_500);

/// Silence this long counts as "generation stopped". Comfortably longer than
/// the gap between two streamed tokens, comfortably shorter than the task.
const INTERRUPT_QUIET_FOR: Duration = Duration::from_millis(1_500);

/// How long the interrupt gets to take effect before the point is red.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the `ask` decision gets to raise the CLI's permission dialog.
const ASK_DIALOG_TIMEOUT: Duration = Duration::from_secs(60);

/// What the capture of a four-point run is labelled with.
const SCENARIO: &str =
    "four-point hook-channel verification: allow/deny/ask round-trip + Ctrl+C interrupt";

/// The console and hook transport this run exercised, named in every step
/// line so a report is never ambiguous about what was actually verified.
const CONSOLE: &str = if cfg!(windows) { "ConPTY" } else { "tty" };
const TRANSPORT: &str = if cfg!(windows) {
    "named pipe"
} else {
    "unix domain socket"
};

/// A single point's outcome. `Red` is data, not a panic: the report lists
/// every point's verdict so a reviewer sees exactly what held.
pub enum PointOutcome {
    Green(String),
    Red(String),
}

impl PointOutcome {
    fn status(&self) -> &'static str {
        match self {
            PointOutcome::Green(_) => "green",
            PointOutcome::Red(_) => "red",
        }
    }

    fn detail(&self) -> &str {
        match self {
            PointOutcome::Green(detail) | PointOutcome::Red(detail) => detail,
        }
    }
}

/// Run all four points. The process exits non-zero if any point is red, so
/// CI can gate on it, but every point is attempted and reported first — a
/// red item must be visible, not hidden behind an early exit.
pub fn run(config: &ProbeConfig) -> Result<(), Failure> {
    print_step(
        "four_point_venue",
        "pass",
        &format!(
            "console={CONSOLE} hook-transport=\u{2018}{TRANSPORT}\u{2019} os={}{}",
            std::env::consts::OS,
            if cfg!(windows) {
                " — this is the run that matters; confirm the host is Windows 11 client, not Server"
            } else {
                " — this is the POSIX baseline point 2's parity claim compares against"
            }
        ),
    );

    let mut session = launch(config)?;
    let info = match session.establish() {
        Ok(info) => info,
        Err(failure) => {
            // The session exists but is unusable; kill it rather than leave a
            // live CLI (and its quota) behind a failed step.
            session.abandon(SCENARIO, &failure);
            return Err(failure);
        }
    };

    // Point 1: SessionStart already arrived over the channel during
    // `establish` — that IS a hook firing under this console and reaching
    // the listener. Confirm the channel is live by counting what arrived.
    let mark = session.hook_mark();
    let point1 = if mark > 0 {
        PointOutcome::Green(format!(
            "{mark} hook payload(s) reached the listener over the {TRANSPORT} under {CONSOLE} (incl. SessionStart)"
        ))
    } else {
        PointOutcome::Red(format!(
            "no hook payloads reached the listener over the {TRANSPORT}"
        ))
    };
    report_point(1, "hooks-fire", &point1);

    // Point 2: allow/deny/ask. Each decision drives one turn that asks the
    // CLI to run a tool; the round-trip shape is asserted, never the model's
    // exact words (live-lane policy).
    let point2 = check_approval_parity(&mut session);
    report_point(2, "approval-parity", &point2);

    // Point 3: the content channel writes under this console.
    let point3 = check_transcript(&mut session, &info);
    report_point(3, "transcript", &point3);

    // Point 4: Ctrl+C interrupt. Start a long generation, send the 0x03
    // byte, assert generation stops and the session still answers.
    let point4 = check_interrupt(&mut session);
    report_point(4, "interrupt", &point4);

    let reds: Vec<String> = [&point1, &point2, &point3, &point4]
        .iter()
        .enumerate()
        .filter(|(_, outcome)| matches!(outcome, PointOutcome::Red(_)))
        .map(|(index, _)| (index + 1).to_string())
        .collect();

    // Shut down regardless of the verdict: the four points are already
    // reported, and a red point must not also leak a live session.
    let shutdown = session.finish(SCENARIO);

    if !reds.is_empty() {
        // The red points are the finding and own the exit code. A shutdown
        // that also failed is surfaced as a warning rather than swallowed,
        // but it must not mask them.
        if let Err(failure) = shutdown {
            print_step("shutdown", "warn", &failure.detail);
        }
        return Err(Failure::new(
            "four_point",
            50,
            format!(
                "point(s) [{}] red under {CONSOLE} — recorded as findings above, not waived",
                reds.join(", ")
            ),
        ));
    }
    shutdown?;
    print_step(
        "four_point",
        "pass",
        &format!("all four points green under {CONSOLE} over the {TRANSPORT}"),
    );
    Ok(())
}

fn report_point(n: u8, name: &str, outcome: &PointOutcome) {
    print_step(
        &format!("four_point_{n}_{name}"),
        outcome.status(),
        outcome.detail(),
    );
}

/// The prompt used for every approval-decision turn: a trivial, side-effect
/// free shell command the model will reach for a tool to run.
fn tool_prompt(tag: &str) -> String {
    format!("Run this shell command and nothing else: echo {tag}")
}

/// Does this Notification payload announce a permission prompt? Claude Code
/// 2.1.x sets `notification_type: "permission_prompt"`; the `message` check
/// is a fallback for versions that carried only prose.
fn is_permission_notification(payload: &serde_json::Value) -> bool {
    let says_permission = |field: &str| {
        payload
            .get(field)
            .and_then(|value| value.as_str())
            .is_some_and(|text| text.to_lowercase().contains("permission"))
    };
    says_permission("notification_type") || says_permission("message")
}

/// Point 2: the three decisions, each asserted by the event shape it is
/// *defined* to produce, and nothing else.
///
/// - **allow** — `PreToolUse` then `PostToolUse`: the tool ran.
/// - **deny** — `PreToolUse` and *no* `PostToolUse`: the tool was blocked.
///   The model's prose about being blocked is not asserted; a live lane
///   never matches model output.
/// - **ask** — `PreToolUse` then a permission `Notification`. This decision
///   deliberately hands control to the CLI's own dialog, which blocks for a
///   human, so the turn **never reaches `Stop`**. Waiting for one would hang
///   until the turn timeout. The dialog is dismissed with Escape to return
///   the session to idle, and `ask` runs last so a dismissal that misbehaves
///   cannot poison the other two.
fn check_approval_parity(session: &mut LiveSession) -> PointOutcome {
    let mut notes = Vec::new();

    // allow and deny both complete a turn, so both are driven by run_turn.
    for (decision, tag) in [
        (Decision::Allow, "parity-allow"),
        (Decision::Deny, "parity-deny"),
    ] {
        session.listener.set_decision(decision);
        let turn = match session.run_turn(&tool_prompt(tag), TURN_TIMEOUT) {
            Ok(turn) => turn,
            Err(detail) => return PointOutcome::Red(format!("{decision:?} turn failed: {detail}")),
        };
        let names = turn.hook_names();
        if !names.contains(&"PreToolUse") {
            return PointOutcome::Red(format!(
                "{decision:?}: no PreToolUse hook fired, so the approver was never consulted; hooks: [{}]",
                names.join(", ")
            ));
        }
        let executed = names.contains(&"PostToolUse");
        match (decision, executed) {
            (Decision::Allow, false) => {
                return PointOutcome::Red(format!(
                    "allow: PreToolUse fired but no PostToolUse — the approved tool never executed; hooks: [{}]",
                    names.join(", ")
                ));
            }
            (Decision::Deny, true) => {
                return PointOutcome::Red(format!(
                    "deny: PostToolUse fired — the denied tool executed anyway; hooks: [{}]",
                    names.join(", ")
                ));
            }
            _ => notes.push(format!(
                "{decision:?} → [{}] in {}ms",
                names.join(", "),
                turn.duration.as_millis()
            )),
        }
    }

    // ask: assert the permission Notification, then dismiss the dialog.
    session.listener.set_decision(Decision::Ask);
    let mark = session.hook_mark();
    if let Err(err) = session
        .writer
        .type_line(&tool_prompt("parity-ask"), TYPE_SETTLE)
    {
        return PointOutcome::Red(format!("ask: submitting the prompt failed: {err}"));
    }
    if let Err(detail) = session.wait_for_hook_where(
        "Notification",
        is_permission_notification,
        mark,
        ASK_DIALOG_TIMEOUT,
    ) {
        return PointOutcome::Red(format!(
            "ask: no permission Notification — the decision did not degrade to the CLI's dialog: {detail}"
        ));
    }
    let ask_hooks = session.hooks_since(mark);
    let ask_names: Vec<&str> = ask_hooks.iter().map(|(name, _)| name.as_str()).collect();
    if ask_names.contains(&"PostToolUse") {
        return PointOutcome::Red(format!(
            "ask: PostToolUse fired — the tool ran without anyone approving it; hooks: [{}]",
            ask_names.join(", ")
        ));
    }
    notes.push(format!("Ask → [{}] + dialog", ask_names.join(", ")));

    if let Err(err) = session.writer.send(ESCAPE) {
        return PointOutcome::Red(format!("ask: dismissing the dialog failed: {err}"));
    }
    session.listener.set_decision(Decision::NoOpinion);
    // Give the dismissal time to land before the next point drives the
    // session; a dialog still up would swallow the next prompt.
    if let Err(detail) =
        session.wait_until_quiet(Duration::from_millis(750), Duration::from_secs(20))
    {
        notes.push(format!("ask: session still noisy after Escape ({detail})"));
    }

    PointOutcome::Green(format!(
        "allow/deny/ask round-trips completed over the {TRANSPORT}, each asserted by hook shape: {}",
        notes.join("; ")
    ))
}

fn check_transcript(session: &mut LiveSession, info: &crate::rig::SessionInfo) -> PointOutcome {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let size = std::fs::metadata(&info.transcript_path).map_or(0, |meta| meta.len());
        if size > info.transcript_baseline {
            return PointOutcome::Green(format!(
                "{} grew {} -> {size} bytes under {CONSOLE}",
                info.transcript_path.display(),
                info.transcript_baseline
            ));
        }
        if Instant::now() >= deadline {
            return PointOutcome::Red(format!(
                "{} did not grow past {} bytes — the content channel is dead under {CONSOLE}",
                info.transcript_path.display(),
                info.transcript_baseline
            ));
        }
        if let Err(detail) = session.tracker.ensure_live("the transcript to grow") {
            return PointOutcome::Red(detail);
        }
        if let Err(detail) = session.tracker.pump(Duration::from_millis(200)) {
            return PointOutcome::Red(detail);
        }
    }
}

/// Point 4: start a long generation, send the Ctrl+C byte, and assert two
/// things — the streaming stopped, and the session survived.
///
/// "Stopped" is measured as **output quiescence**, not as a `Stop` hook.
/// An interrupted turn fires no `Stop`: the turn did not end, it was
/// abandoned, and the CLI parks at "what should I do instead". Waiting for
/// `Stop` here would report a working interrupt as broken. Quiescence is
/// also content-free, where matching the CLI's "Interrupted" banner would
/// pin the probe to a string the CLI may reword.
///
/// The task is chosen so that *not* interrupting is loud: a model counting
/// to 500 streams continuously for far longer than the quiet window.
fn check_interrupt(session: &mut LiveSession) -> PointOutcome {
    session.listener.set_decision(Decision::NoOpinion);
    if let Err(err) = session.writer.type_line(
        "Count slowly from 1 to 500, one number per line, with no other text.",
        TYPE_SETTLE,
    ) {
        return PointOutcome::Red(format!("submitting the long task failed: {err}"));
    }

    // Let generation get going. If nothing streams, there is nothing to
    // interrupt and the point cannot be judged.
    let before = session.tracker.chunks_seen();
    if let Err(detail) = session.tracker.pump(GENERATION_WARMUP) {
        return PointOutcome::Red(detail);
    }
    let streamed = session.tracker.chunks_seen() - before;
    if streamed == 0 {
        return PointOutcome::Red(format!(
            "no output streamed in the {}ms before the interrupt — nothing was interrupted, so the point is unproven",
            GENERATION_WARMUP.as_millis()
        ));
    }

    let interrupted_at = Instant::now();
    if let Err(err) = session.writer.send(&[CTRL_C]) {
        return PointOutcome::Red(format!("sending the Ctrl+C byte failed: {err}"));
    }
    let quiet_after = match session.wait_until_quiet(INTERRUPT_QUIET_FOR, INTERRUPT_TIMEOUT) {
        Ok(_) => interrupted_at.elapsed(),
        Err(detail) => {
            return PointOutcome::Red(format!(
                "generation did not halt after the Ctrl+C byte: {detail}"
            ));
        }
    };

    // Survival: a follow-up turn still completes. This also proves the
    // interrupt stopped the *generation* and not the *process*.
    match session.run_turn("Reply with exactly: alive", TURN_TIMEOUT) {
        Ok(turn) => PointOutcome::Green(format!(
            "{streamed} chunks streamed, then the Ctrl+C byte (0x03) quiesced output {}ms later (no Stop hook fires for an abandoned turn — that is expected); the session survived and answered a follow-up in {}ms",
            quiet_after.as_millis(),
            turn.duration.as_millis()
        )),
        Err(detail) => PointOutcome::Red(format!(
            "generation halted, but the session did not survive the interrupt: {detail}"
        )),
    }
}
