//! The frames the runtime sends without being asked: the `session.event`
//! notification per bus event, the `session.eof` that ends a subscription,
//! and the transport-condition frames the wire emits directly (a frame too
//! large, a malformed frame, the stdout-blocked farewell).
//!
//! A bus event goes out as `session.event` with its params the event envelope
//! **verbatim** — the same serialization the schema publishes, not a
//! transport-shaped twin. The transport-condition frames are the one place
//! the transport *synthesizes* an event rather than relaying one: they are
//! not routed through the bus (a client need not have subscribed to hear its
//! own connection failing), so this module stamps them a `session.event`
//! carrying a `transport.error` envelope with `session_id: null`.

use agent_bridge_events::{
    Event, EventKind, SCHEMA_VERSION, TransportErrorCode, TransportErrorPayload,
};
use bytes::Bytes;
use serde_json::Value;

use crate::framing::encode;
use crate::method::{SESSION_EOF, SESSION_EVENT};
use crate::rpc::Notification;
use crate::timestamp::rfc3339_now;

/// Frame one bus event as a `session.event` notification. The event serializes
/// to its published envelope; the notification wraps it unchanged.
#[must_use]
pub fn event_frame(event: &Event) -> Bytes {
    let params = serde_json::to_value(event).unwrap_or(Value::Null);
    encode(&Notification::new(SESSION_EVENT, params).encode())
}

/// Why a subscription ended, for the `session.eof` notification. The wire
/// names the same two ends the bus distinguishes: a session that reached its
/// close, and a subscriber the runtime disconnected for lag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EofReason {
    /// The session was sealed — it reached `Closed` and its stream ended. The
    /// design's `session_closed` eof carries the child's `exit_code`, echoed
    /// from the `lifecycle.session.closed` event that preceded it (absent when
    /// the child was killed and has no code). `events_lost` is present and
    /// non-zero only when the seal ended the stream while the subscriber still
    /// had accepted events it could not be handed — a loss the bus counted, so
    /// the wire names it rather than dropping it silently.
    SessionClosed {
        /// The child's exit code, when it exited with one.
        exit_code: Option<i32>,
        /// Accepted events the subscriber never received before the seal.
        events_lost: Option<u64>,
    },
    /// The runtime disconnected this subscriber for failing to keep up.
    SubscriberLagging,
}

impl EofReason {
    /// The wire spelling of the reason.
    const fn as_str(&self) -> &'static str {
        match self {
            EofReason::SessionClosed { .. } => "session_closed",
            EofReason::SubscriberLagging => "subscriber_lagging",
        }
    }
}

/// Frame the `session.eof` that ends a subscription. `session.eof` is a
/// transport notification, not a taxonomy event (the event taxonomy
/// deliberately has none): the end of a *subscription* is not something that
/// happened to the session, so it is spelled here, on the wire, rather than in
/// the event stream every other subscriber shares.
#[must_use]
pub fn eof_frame(session_id: &str, reason: EofReason) -> Bytes {
    let mut params = serde_json::Map::new();
    params.insert("session_id".into(), Value::from(session_id));
    params.insert("reason".into(), Value::from(reason.as_str()));
    if let EofReason::SessionClosed {
        exit_code,
        events_lost,
    } = reason
    {
        if let Some(code) = exit_code {
            params.insert("exit_code".into(), Value::from(code));
        }
        if let Some(lost) = events_lost.filter(|&lost| lost > 0) {
            params.insert("events_lost".into(), Value::from(lost));
        }
    }
    encode(&Notification::new(SESSION_EOF, Value::Object(params)).encode())
}

/// Frame a synthesized global `transport.error` — the wire condition the
/// transport itself raises and then acts on (closes, or exits). Not bus
/// traffic: emitted straight onto the connection whether or not the client
/// subscribed to anything.
#[must_use]
pub fn transport_error_frame(code: TransportErrorCode, message: &str) -> Bytes {
    let event = Event {
        schema_version: SCHEMA_VERSION,
        session_id: None,
        // A synthesized transport condition belongs to no session's sequence
        // domain; `session_id: null` is what says so, and the client keys a
        // transport.error by its type and code, never by seq.
        seq: 0,
        monotonic_ns: None,
        ts: rfc3339_now(),
        approval_id: None,
        correlation_id: None,
        kind: EventKind::TransportError(TransportErrorPayload {
            code,
            message: message.to_owned(),
            detail: serde_json::Map::new(),
        }),
    };
    event_frame(&event)
}

/// Frame a session-scoped `transport.error` the bus recorded beside a stream
/// it ended — today, the `subscriber_lagging` payload that precedes a lag
/// `session.eof`. The payload is the bus's, carried onto the wire unchanged;
/// only the envelope is added, scoped to the session whose subscription
/// ended.
#[must_use]
pub fn session_transport_error_frame(session_id: &str, payload: &TransportErrorPayload) -> Bytes {
    let event = Event {
        schema_version: SCHEMA_VERSION,
        session_id: Some(session_id.to_owned()),
        seq: 0,
        monotonic_ns: None,
        ts: rfc3339_now(),
        approval_id: None,
        correlation_id: None,
        kind: EventKind::TransportError(payload.clone()),
    };
    event_frame(&event)
}

/// The pre-encoded stdout-blocked farewell handed to the bounded writer: the
/// single best-effort frame it attempts on its way down when the parent has
/// stopped reading. Best-effort by nature — a truly non-reading parent never
/// receives it — which is why the writer's fatal signal and log carry the
/// same fact independently.
#[must_use]
pub fn stdout_blocked_farewell() -> Bytes {
    transport_error_frame(
        TransportErrorCode::StdoutBlocked,
        "the caller stopped reading stdout; the runtime is exiting",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bus_event_frames_as_a_session_event_carrying_the_envelope_verbatim() {
        let event = Event {
            schema_version: SCHEMA_VERSION,
            session_id: Some("abc".into()),
            seq: 4,
            monotonic_ns: None,
            ts: "2026-05-16T08:00:00.123Z".into(),
            approval_id: None,
            correlation_id: None,
            kind: EventKind::LifecycleSessionRunning(Default::default()),
        };
        let frame = event_frame(&event);
        let body = frame_body(&frame);
        assert_eq!(body["method"], SESSION_EVENT);
        assert_eq!(body["params"]["type"], "lifecycle.session.running");
        assert_eq!(body["params"]["seq"], 4);
        assert_eq!(body["params"]["session_id"], "abc");
    }

    #[test]
    fn an_eof_frame_names_the_session_the_reason_and_the_exit_code() {
        let body = frame_body(&eof_frame(
            "abc",
            EofReason::SessionClosed {
                exit_code: Some(7),
                events_lost: None,
            },
        ));
        assert_eq!(body["method"], SESSION_EOF);
        assert_eq!(body["params"]["reason"], "session_closed");
        assert_eq!(body["params"]["session_id"], "abc");
        assert_eq!(body["params"]["exit_code"], 7);
        // A lossless close carries no events_lost key.
        assert!(body["params"].get("events_lost").is_none());
    }

    #[test]
    fn a_seal_with_loss_names_the_dropped_count_and_a_lag_eof_carries_neither() {
        let lossy = frame_body(&eof_frame(
            "abc",
            EofReason::SessionClosed {
                exit_code: None,
                events_lost: Some(4),
            },
        ));
        assert_eq!(lossy["params"]["events_lost"], 4);
        // A killed child has no exit code to report.
        assert!(lossy["params"].get("exit_code").is_none());

        let lagging = frame_body(&eof_frame("abc", EofReason::SubscriberLagging));
        assert_eq!(lagging["params"]["reason"], "subscriber_lagging");
        assert!(lagging["params"].get("exit_code").is_none());
        assert!(lagging["params"].get("events_lost").is_none());
    }

    #[test]
    fn a_transport_error_frame_is_an_unscoped_transport_error_event() {
        let body = frame_body(&transport_error_frame(
            TransportErrorCode::FrameTooLarge,
            "too big",
        ));
        assert_eq!(body["method"], SESSION_EVENT);
        assert_eq!(body["params"]["type"], "transport.error");
        assert_eq!(body["params"]["payload"]["code"], "frame_too_large");
        assert!(body["params"]["session_id"].is_null());
    }

    /// The JSON body inside a frame, for asserting on the wire shape.
    fn frame_body(frame: &Bytes) -> Value {
        let text = std::str::from_utf8(frame).unwrap();
        let body = text.split_once("\r\n\r\n").unwrap().1;
        serde_json::from_str(body).unwrap()
    }
}
