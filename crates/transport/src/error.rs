//! The JSON-RPC error object, and the total map from the runtime's typed
//! errors onto the protocol's code table.
//!
//! The runtime's layers already own their own code assignment:
//! [`RegistryError::jsonrpc_code`] and [`SessionError::jsonrpc_code`] each
//! choose the code beside the variant, so a new failure mode cannot land
//! without someone picking its code. This module is where those choices meet
//! the wire — it turns a typed error into the `{ code, message, data }` a
//! client receives, and it never reconstructs a code from a string.
//!
//! `-32004` is deliberately absent from every path here: the reconnect gap is
//! a payload field, never an error code, and pairing the two is a
//! contradiction the workspace gate is built to catch.

use agent_bridge_core::{BusError, RegistryError, SessionError, SessionState};
use serde::Serialize;
use serde_json::{Value, json};

/// A JSON-RPC error object: a numeric `code`, a human-readable `message`, and
/// optional structured `data`. `data` carries the detail a remediation needs
/// — the session state an operation was refused in, the caps a create hit —
/// that a bare message would bury in prose.
#[derive(Debug, Clone, Serialize)]
pub struct JsonRpcError {
    /// The protocol code. Standard JSON-RPC codes and the runtime's
    /// `-32000`..=`-32099` extensions.
    pub code: i32,
    /// What went wrong, for a human.
    pub message: String,
    /// Structured context for the code, when there is any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpcError {
    /// A code and message with no structured data.
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// Attach structured detail.
    #[must_use]
    fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// `-32700`: the frame was not JSON.
    #[must_use]
    pub fn parse_error() -> Self {
        Self::new(-32700, "frame is not valid JSON")
    }

    /// `-32600`: the frame was JSON but not a well-formed request.
    #[must_use]
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(-32600, message)
    }

    /// `-32601`: the method is not on the allowlist, or its name exceeded the
    /// cap that bounds header churn.
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self::new(-32601, format!("unknown method: {method}"))
    }

    /// `-32602`: the params did not match the method's shape, or carried a
    /// value the method cannot yet honour (a Phase-3 field, an out-of-range
    /// dimension).
    #[must_use]
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(-32602, message)
    }

    /// `-32603`: an internal condition with no more specific code — the
    /// wire's own catch-all, kept distinct from the runtime extensions.
    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(-32603, message)
    }

    /// `-32008`: the caller pinned an event `schema_version` the runtime does
    /// not serve, learned at attach before it consumes a single event. Carries
    /// both versions so a client can log or upgrade against them.
    #[must_use]
    pub fn schema_version_mismatch(expected: u32, actual: u32) -> Self {
        Self::new(
            -32008,
            format!("caller expected event schema_version {expected}, runtime serves {actual}"),
        )
        .with_data(json!({ "expected": expected, "actual": actual }))
    }
}

/// Map a registry failure onto its wire error. The code is the registry's own
/// choice; this adds the structured detail a client acts on.
#[must_use]
pub fn from_registry(error: &RegistryError) -> JsonRpcError {
    let base = JsonRpcError::new(error.jsonrpc_code(), error.to_string());
    match error {
        RegistryError::CapReached { limit } => base.with_data(json!({ "hard_cap": limit })),
        RegistryError::Session(session) => attach_session_detail(base, session),
        RegistryError::AdapterNotFound(_) | RegistryError::SessionNotFound(_) => base,
    }
}

/// Map a session failure onto its wire error, code chosen beside the variant
/// in the session layer.
#[must_use]
pub fn from_session(error: &SessionError) -> JsonRpcError {
    attach_session_detail(
        JsonRpcError::new(error.jsonrpc_code(), error.to_string()),
        error,
    )
}

/// Map a bus failure onto the wire. The bus is typed but codeless — the
/// mapping onto the protocol table lives here, where the wire is:
/// an unknown session reads as session-not-found, a sealed one as
/// session-closed, and a backfill past head as invalid params (the shape the
/// `from_seq` parameter would take once Phase 3 accepts it).
#[must_use]
pub fn from_bus(error: &BusError) -> JsonRpcError {
    match error {
        BusError::UnknownSession(_) => JsonRpcError::new(-32002, error.to_string()),
        BusError::Sealed(_) => JsonRpcError::new(-32003, error.to_string()),
        BusError::FromSeqBeyondHead { head, .. } => {
            JsonRpcError::invalid_params(error.to_string()).with_data(json!({ "head": head }))
        }
        BusError::PublisherExists(_) => JsonRpcError::internal(error.to_string()),
    }
}

/// The `data` a session error carries: the state a stateful refusal happened
/// in, and the bound a rejected geometry broke. Everything else states its
/// case in the message alone.
fn attach_session_detail(base: JsonRpcError, error: &SessionError) -> JsonRpcError {
    match error {
        SessionError::InvalidStateForOperation { state, op } => {
            base.with_data(json!({ "state": state_name(*state), "operation": op }))
        }
        SessionError::InvalidDimensions {
            max_cols, max_rows, ..
        } => base.with_data(json!({ "max_cols": max_cols, "max_rows": max_rows })),
        _ => base,
    }
}

/// The lifecycle state's wire spelling, matching the `lifecycle.*` event
/// names a client already knows the session by.
fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::Created => "created",
        SessionState::Launching => "launching",
        SessionState::Connecting => "connecting",
        SessionState::Running => "running",
        SessionState::AwaitingApproval => "awaiting_approval",
        SessionState::Interrupted => "interrupted",
        SessionState::Closing => "closing",
        SessionState::Closed => "closed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_errors_carry_the_registry_chosen_codes() {
        assert_eq!(
            from_registry(&RegistryError::AdapterNotFound("x".into())).code,
            -32001
        );
        let cap = from_registry(&RegistryError::CapReached { limit: 32 });
        assert_eq!(cap.code, -32009);
        assert_eq!(cap.data.unwrap()["hard_cap"], 32);
    }

    #[test]
    fn a_stateful_refusal_reports_the_state_it_was_refused_in() {
        let error = from_session(&SessionError::InvalidStateForOperation {
            state: SessionState::Closed,
            op: "resize",
        });
        assert_eq!(error.code, -32006);
        let data = error.data.unwrap();
        assert_eq!(data["state"], "closed");
        assert_eq!(data["operation"], "resize");
    }

    #[test]
    fn an_approval_mismatch_maps_to_the_reserved_approval_code() {
        assert_eq!(from_session(&SessionError::ApprovalIdMismatch).code, -32007);
    }

    #[test]
    fn bus_conditions_map_onto_the_session_facing_codes() {
        assert_eq!(from_bus(&BusError::UnknownSession("s".into())).code, -32002);
        assert_eq!(from_bus(&BusError::Sealed("s".into())).code, -32003);
        assert_eq!(
            from_bus(&BusError::FromSeqBeyondHead {
                session_id: "s".into(),
                from_seq: 9,
                head: 3
            })
            .code,
            -32602
        );
    }
}
