//! The method surface: the one place each method name is spelled, and the
//! typed parameter shape each carries.
//!
//! Parameter validation is types plus explicit bounds, not a schema engine:
//! every param struct is `deny_unknown_fields`, so a field the runtime does
//! not consume is a rejected call rather than a silently dropped value —
//! "trusted caller is not trusted input" satisfied by the shapes the layer
//! deserializes into. A field the wire documents for a later phase is
//! deliberately *not* accepted here where the runtime cannot honour it: env
//! and cwd on create have no path to the launch through the Phase-1 adapter
//! seam, so a client sending them learns so, rather than watching them vanish.
//!
//! The MVP subset (`planning`-scoped): the two `runtime.*` methods this PR
//! carries plus the session verbs. `runtime.setLogLevel` / `runtime.selftest`,
//! the full method surface, and writer acquisition land in later PRs against
//! this same table.

use serde::Deserialize;

/// `runtime.info` — version, adapters, capabilities, event `schema_version`.
pub const RUNTIME_INFO: &str = "runtime.info";
/// `runtime.shutdown` — drain sessions and exit.
pub const RUNTIME_SHUTDOWN: &str = "runtime.shutdown";
/// `session.create` — launch a session, return its id.
pub const SESSION_CREATE: &str = "session.create";
/// `session.attach` — begin a session-scoped subscription at head.
pub const SESSION_ATTACH: &str = "session.attach";
/// `session.send` — forward input bytes to a session.
pub const SESSION_SEND: &str = "session.send";
/// `session.resolve_approval` — answer one pending approval.
pub const SESSION_RESOLVE_APPROVAL: &str = "session.resolve_approval";
/// `session.interrupt` — interrupt the CLI without ending the session.
pub const SESSION_INTERRUPT: &str = "session.interrupt";
/// `session.resize` — change the terminal geometry.
pub const SESSION_RESIZE: &str = "session.resize";
/// `session.close` — terminate a session.
pub const SESSION_CLOSE: &str = "session.close";

/// The outbound `session.event` notification method.
pub const SESSION_EVENT: &str = "session.event";
/// The outbound `session.eof` notification method — a subscription ending.
pub const SESSION_EOF: &str = "session.eof";
/// The outbound `transport.error` notification method — a wire condition the
/// transport raises out-of-band (a frame too large, a malformed frame, a
/// subscriber disconnected for lag, stdout blocked), scoped to no session and
/// keyed by its code rather than any sequence.
pub const TRANSPORT_ERROR: &str = "transport.error";

/// The longest method name the dispatcher will look up, before it looks. A
/// method name is attacker-influenced header-like data on the wire, and this
/// bounds a denial-of-service that churns enormous names; every real method
/// above is well under it.
pub const MAX_METHOD_NAME_BYTES: usize = 128;

/// `session.create` parameters.
///
/// `adapter` names a registered adapter; `dims` is an optional `[cols, rows]`
/// geometry that outranks the adapter's own hint. `env` and `cwd` are
/// intentionally not accepted in Phase 1 (see the module note) — the launch
/// path that would consume them is the adapter env-policy work, and until it
/// lands, `deny_unknown_fields` refuses them plainly.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateParams {
    /// The registered adapter to host the session.
    pub adapter: String,
    /// Requested terminal geometry as `[cols, rows]`.
    #[serde(default)]
    pub dims: Option<[u16; 2]>,
}

/// `session.attach` parameters.
///
/// `from_seq` is parsed so a client's request is *understood* rather than
/// rejected as an unknown field — but in Phase 1 supplying it is refused with
/// `-32602` and a message naming the phase: backfill lands with the Phase-3
/// attach surface, and accepting-and-ignoring it would let a caller believe
/// it had resumed from a point it did not.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionAttachParams {
    /// The session to subscribe to.
    pub session_id: String,
    /// Resume point — unsupported in Phase 1.
    #[serde(default)]
    pub from_seq: Option<u64>,
    /// The event `schema_version` the caller expects. When present and not the
    /// runtime's, the attach fails `-32008` so a stale caller learns before it
    /// consumes an event it cannot parse.
    #[serde(default)]
    pub expected_schema_version: Option<u32>,
}

/// `session.send` parameters. `input` is forwarded to the CLI verbatim; it is
/// input only, never an approval answer (that is `session.resolve_approval`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSendParams {
    /// The session to write to.
    pub session_id: String,
    /// The input bytes, as a string.
    pub input: String,
}

/// `session.resolve_approval` parameters (the dedicated approval channel).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResolveApprovalParams {
    /// The session holding the pending approval.
    pub session_id: String,
    /// The id of the approval to resolve — a stale or unknown id is refused
    /// and the pending prompt stays pending.
    pub approval_id: String,
    /// Allow, deny, or defer to the CLI's own prompt.
    pub decision: WireDecision,
    /// Carried back to the model on a deny.
    #[serde(default)]
    pub reason: Option<String>,
}

/// The wire spelling of an approval decision.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireDecision {
    /// Let the tool call proceed.
    Allow,
    /// Refuse it.
    Deny,
    /// Defer to the CLI's own interactive prompt.
    Ask,
}

/// A parameter object naming only a session — `interrupt` and `close`'s
/// shape, and the shared shell every session verb starts from.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInterruptParams {
    /// The session to interrupt.
    pub session_id: String,
}

/// `session.resize` parameters.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResizeParams {
    /// The session to resize.
    pub session_id: String,
    /// The new geometry as `[cols, rows]`.
    pub dims: [u16; 2],
}

/// `session.close` parameters. `force` skips the adapter's shutdown hint and
/// terminates at once; absent, a graceful close is attempted first.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCloseParams {
    /// The session to close.
    pub session_id: String,
    /// Skip the graceful hint and drain.
    #[serde(default)]
    pub force: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_accepts_adapter_and_optional_dims() {
        let params: SessionCreateParams =
            serde_json::from_value(json!({ "adapter": "fixture", "dims": [120, 40] })).unwrap();
        assert_eq!(params.adapter, "fixture");
        assert_eq!(params.dims, Some([120, 40]));
    }

    #[test]
    fn create_refuses_an_unconsumed_field_rather_than_dropping_it() {
        // env has no path to the launch in Phase 1; a client sending it must
        // learn so, not have it silently ignored.
        let refused = serde_json::from_value::<SessionCreateParams>(
            json!({ "adapter": "fixture", "env": [["K", "V"]] }),
        );
        assert!(refused.is_err(), "unknown fields must be rejected");
    }

    #[test]
    fn a_decision_reads_its_three_lowercase_spellings() {
        for (text, want) in [
            ("allow", WireDecision::Allow),
            ("deny", WireDecision::Deny),
            ("ask", WireDecision::Ask),
        ] {
            let params: SessionResolveApprovalParams = serde_json::from_value(json!({
                "session_id": "s", "approval_id": "a", "decision": text
            }))
            .unwrap();
            assert_eq!(params.decision, want);
        }
    }

    #[test]
    fn every_method_name_is_within_the_cap() {
        for name in [
            RUNTIME_INFO,
            RUNTIME_SHUTDOWN,
            SESSION_CREATE,
            SESSION_ATTACH,
            SESSION_SEND,
            SESSION_RESOLVE_APPROVAL,
            SESSION_INTERRUPT,
            SESSION_RESIZE,
            SESSION_CLOSE,
            SESSION_EVENT,
            SESSION_EOF,
            TRANSPORT_ERROR,
        ] {
            assert!(name.len() <= MAX_METHOD_NAME_BYTES);
        }
    }
}
