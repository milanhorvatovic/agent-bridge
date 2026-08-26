//! One instance of every published event type.
//!
//! Three tests need the whole taxonomy at once — the round-trip sweep, the
//! inventory check, and the artifact validation — and each of them is only
//! as complete as this list. That is the point: the inventory test compares
//! this list against the generated taxonomy, so an event type added to the
//! enum without a sample here fails immediately rather than going untested.

// Each test file compiles this module into its own binary, and not every one
// of them needs every helper.
#![allow(dead_code)]

use agent_bridge_events::*;

/// Every event type this revision publishes, with a representative payload.
pub fn every_event_kind() -> Vec<EventKind> {
    let approval = EventBody::approval_required(
        "a-7f3",
        ApprovalPrompt::new("Allow filesystem write?")
            .tool("bash")
            .options(["y", "n"]),
    );
    let withdrawn = EventBody::approval_withdrawn("a-7f3");
    vec![
        EventKind::LifecycleSessionCreated(LifecycleSessionCreated {
            adapter: Some("claude".to_owned()),
        }),
        EventKind::LifecycleSessionLaunching(LifecycleSessionLaunching {}),
        EventKind::LifecycleSessionConnecting(LifecycleSessionConnecting {}),
        EventKind::LifecycleSessionRunning(LifecycleSessionRunning {}),
        EventKind::LifecycleSessionAwaitingApproval(LifecycleSessionAwaitingApproval {}),
        EventKind::LifecycleSessionInterrupted(LifecycleSessionInterrupted {}),
        EventKind::LifecycleSessionClosing(LifecycleSessionClosing {}),
        EventKind::LifecycleSessionClosed(LifecycleSessionClosed {
            exit_code: Some(0),
            duration_ms: Some(4_213),
            bytes_read: Some(18_442),
            bytes_written: Some(96),
            drained: Some(false),
            cleanup_verified: Some(true),
            remaining_processes: None,
        }),
        EventKind::LifecycleSessionCompacting(LifecycleSessionCompacting {}),
        EventKind::LifecycleTurnStarted(LifecycleTurnStarted {}),
        EventKind::LifecycleTurnCompleted(LifecycleTurnCompleted {}),
        EventKind::StreamToken(StreamToken {
            source: Some("claude".to_owned()),
            content: "Analyzing repository...".to_owned(),
        }),
        EventKind::StreamStderr(StreamStderr {
            content: "Unhandled exception".to_owned(),
        }),
        EventKind::StreamUnrecognizedOutput(StreamUnrecognizedOutput {
            content: "unfamiliar prompt format".to_owned(),
        }),
        approval.kind,
        withdrawn.kind,
        EventKind::ToolCallStarted(ToolCallStarted {
            call_id: "t-9c2".to_owned(),
            tool: "bash".to_owned(),
            command: Some("git status".to_owned()),
        }),
        EventKind::ToolCallCompleted(ToolCallCompleted {
            call_id: "t-9c2".to_owned(),
            exit_code: Some(0),
            duration_ms: Some(134),
        }),
        EventKind::ToolCallFailed(ToolCallFailed {
            call_id: "t-9c2".to_owned(),
            reason: "timeout".to_owned(),
        }),
        EventKind::ToolResult(ToolResult {
            call_id: "t-9c2".to_owned(),
            content: "On branch main".to_owned(),
        }),
        EventKind::RuntimeError(RuntimeErrorPayload {
            code: RuntimeErrorCode::LogDiskFull,
            message: "log volume is full; diagnostics are not being written".to_owned(),
            detail: serde_json::Map::new(),
        }),
        EventKind::RuntimeIdleTooLong(RuntimeIdleTooLong {
            idle_ms: Some(300_000),
            threshold_ms: Some(300_000),
        }),
        EventKind::RuntimeHealthChanged(RuntimeHealthChanged {
            status: HealthStatus::Degraded,
            previous: Some(HealthStatus::Ok),
            reason: Some("log volume below 5% free".to_owned()),
        }),
        EventKind::RuntimeNotice(RuntimeNotice {
            notification_type: "permission_prompt".to_owned(),
            message: Some("Claude needs your permission to use Bash".to_owned()),
            detail: serde_json::Map::new(),
        }),
        EventKind::TransportError(TransportErrorPayload {
            code: TransportErrorCode::SubscriberLagging,
            message: "subscriber s-3a fell behind and was disconnected".to_owned(),
            detail: [("subscriber".to_owned(), "s-3a".into())]
                .into_iter()
                .collect(),
        }),
        EventKind::PtyError(PtyErrorPayload {
            code: PtyErrorCode::EncodingReplacement,
            message: "undecodable bytes were replaced".to_owned(),
            detail: serde_json::Map::new(),
        }),
        EventKind::AdapterError(AdapterErrorPayload {
            code: AdapterErrorCode::PatternTimeout,
            message: "matching exceeded the adapter's budget".to_owned(),
            detail: serde_json::Map::new(),
        }),
        EventKind::AdapterVersionWarning(AdapterVersionWarning {
            adapter: Some("claude".to_owned()),
            detected_version: Some("2.1.201".to_owned()),
            supported_range: Some(">=2.0.0, <2.1.0".to_owned()),
        }),
        EventKind::SessionReconnecting(SessionReconnecting {
            from_seq: Some(142),
            subscriber: "s-3a".to_owned(),
        }),
        EventKind::SessionReconnected(SessionReconnected {
            replay: ReplayInfo::within_ring(142, 17),
        }),
        EventKind::SessionWriterChanged(SessionWriterChanged {
            writer: Some("s-7b".to_owned()),
            previous_writer: Some("s-3a".to_owned()),
            reason: WriterChangeReason::Acquire,
        }),
    ]
}

/// An envelope around one event, with every stamped field filled in.
pub fn envelope(seq: u64, kind: EventKind) -> Event {
    let approval_id = matches!(
        kind,
        EventKind::PromptApprovalRequired(_) | EventKind::PromptApprovalWithdrawn(_)
    )
    .then(|| "a-7f3".to_owned());
    Event {
        schema_version: SCHEMA_VERSION,
        session_id: Some("0b8ee0e4-9f4f-4e6b-8f0a-3a80cf9c17d1".to_owned()),
        seq,
        monotonic_ns: Some(1_000 * (seq + 1)),
        ts: "2026-05-16T08:00:00.123Z".to_owned(),
        approval_id,
        correlation_id: None,
        kind,
    }
}
