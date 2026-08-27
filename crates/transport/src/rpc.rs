//! The JSON-RPC 2.0 envelopes this transport reads and writes.
//!
//! Deliberately small: v1 clients send **requests** (the runtime never calls
//! back), and the runtime sends **responses** and **notifications**. The one
//! subtlety is the `id`, which JSON-RPC lets be a string, a number, or null;
//! it is carried as a raw [`serde_json::Value`] and echoed back verbatim so a
//! client correlates its own calls however it chose to.

use serde::Serialize;
use serde_json::Value;

use crate::error::JsonRpcError;

/// A parsed inbound request.
///
/// Parsing is lenient about what it *reads* and strict about what it
/// *accepts*: a frame that is not a JSON object, or is missing `method`,
/// fails as an invalid request rather than panicking, and the id is recovered
/// where present so even a rejected call can be answered against its own id.
#[derive(Debug)]
pub struct Request {
    /// The correlation id as the client sent it, or `None` when the frame
    /// carried none. An absent id marks a JSON-RPC *notification* — a
    /// fire-and-forget call that must receive no response — which the
    /// dispatcher handles distinctly from a request.
    pub id: Option<Value>,
    /// The method name. Length is bounded by the dispatcher, not here.
    pub method: String,
    /// The parameters, if any. Each handler deserializes this into its own
    /// typed shape.
    pub params: Option<Value>,
}

/// Why a frame could not be read as a request, already shaped as the wire
/// error to return. Carrying the recovered id means a malformed call is still
/// answered against the id the client used, when it supplied one.
#[derive(Debug)]
pub struct ParseRejection {
    /// The id to answer against — recovered from the frame when it was a JSON
    /// object carrying one, else null.
    pub id: Value,
    /// The error to send back.
    pub error: JsonRpcError,
}

impl Request {
    /// Read one frame as a request, or produce the rejection to answer with.
    ///
    /// The failure shapes are the ones the base protocol names: a frame that
    /// is not JSON at all is a parse error (`-32700`), and a frame that is JSON
    /// but not a well-formed 2.0 request object — not an object, missing the
    /// `"jsonrpc": "2.0"` marker, or missing a string `method` — is an invalid
    /// request (`-32600`), answered against the recovered id where one is
    /// present.
    pub fn parse(frame: &[u8]) -> Result<Self, ParseRejection> {
        let value: Value = serde_json::from_slice(frame).map_err(|_| ParseRejection {
            id: Value::Null,
            error: JsonRpcError::parse_error(),
        })?;
        let id = value.get("id").cloned();
        let reject = |message: &str| ParseRejection {
            id: id.clone().unwrap_or(Value::Null),
            error: JsonRpcError::invalid_request(message),
        };
        let Some(object) = value.as_object() else {
            return Err(reject("a request must be a JSON object"));
        };
        // A 2.0 server requires the version marker; a 1.0 or version-less frame
        // is refused rather than silently accepted.
        if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(reject("a request must carry \"jsonrpc\": \"2.0\""));
        }
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .ok_or_else(|| reject("a request must carry a string `method`"))?
            .to_owned();
        Ok(Self {
            id,
            method,
            params: object.get("params").cloned(),
        })
    }
}

/// A response to one request: the result, or the error, tagged against the
/// request's id. The two are mutually exclusive by construction — a builder
/// per outcome, never both fields set — which is exactly the invariant
/// JSON-RPC states and a two-`Option` struct would let a caller break.
#[derive(Debug, Serialize)]
pub struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

impl Response {
    /// A successful result for `id`.
    #[must_use]
    pub fn result(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error answer for `id`.
    #[must_use]
    pub fn error(id: Value, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(error),
        }
    }

    /// Serialize to bytes for framing. Infallible in practice — every field
    /// is a plain JSON value — but a serializer failure degrades to an empty
    /// vector the framer drops rather than a panic on the response path.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// A server-to-client notification: a method and its params, with no id and
/// so no reply. The two the MVP emits are `session.event` (one per bus event
/// on an attached subscription) and `session.eof` (that subscription ending).
#[derive(Debug, Serialize)]
pub struct Notification {
    jsonrpc: &'static str,
    method: &'static str,
    params: Value,
}

impl Notification {
    /// A notification of `method` carrying `params`.
    #[must_use]
    pub fn new(method: &'static str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method,
            params,
        }
    }

    /// Serialize to bytes for framing.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_well_formed_request_parses_with_its_id_and_params() {
        let frame = br#"{"jsonrpc":"2.0","id":7,"method":"runtime.info","params":{}}"#;
        let request = Request::parse(frame).expect("valid request");
        assert_eq!(request.id, Some(json!(7)));
        assert_eq!(request.method, "runtime.info");
        assert_eq!(request.params, Some(json!({})));
    }

    #[test]
    fn a_frame_without_an_id_parses_as_a_notification() {
        let frame = br#"{"jsonrpc":"2.0","method":"runtime.info"}"#;
        let request = Request::parse(frame).expect("valid notification");
        assert_eq!(request.id, None, "an absent id marks a notification");
    }

    #[test]
    fn a_frame_missing_the_jsonrpc_marker_is_an_invalid_request() {
        let rejection =
            Request::parse(br#"{"id":1,"method":"runtime.info"}"#).expect_err("must reject");
        assert_eq!(rejection.id, json!(1));
        assert_eq!(rejection.error.code, -32600);
    }

    #[test]
    fn a_non_json_frame_is_a_parse_error_answered_against_null() {
        let rejection = Request::parse(b"not json").expect_err("must reject");
        assert_eq!(rejection.id, Value::Null);
        assert_eq!(rejection.error.code, -32700);
    }

    #[test]
    fn a_json_frame_without_a_method_is_an_invalid_request_keeping_its_id() {
        // The id is recovered even though the request is rejected, so the
        // client can still match the error to the call it made.
        let rejection =
            Request::parse(br#"{"jsonrpc":"2.0","id":"abc"}"#).expect_err("must reject");
        assert_eq!(rejection.id, json!("abc"));
        assert_eq!(rejection.error.code, -32600);
    }

    #[test]
    fn a_result_and_an_error_serialize_to_the_two_disjoint_shapes() {
        let ok = Response::result(json!(1), json!({"version": "0"}));
        let encoded: Value = serde_json::from_slice(&ok.encode()).unwrap();
        assert_eq!(encoded["result"]["version"], "0");
        assert!(encoded.get("error").is_none());

        let err = Response::error(json!(1), JsonRpcError::parse_error());
        let encoded: Value = serde_json::from_slice(&err.encode()).unwrap();
        assert_eq!(encoded["error"]["code"], -32700);
        assert!(encoded.get("result").is_none());
    }
}
