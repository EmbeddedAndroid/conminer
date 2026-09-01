//! MCP wire protocol: JSON-RPC 2.0 plus the MCP envelope.
//!
//! Implemented directly rather than through an SDK. The protocol is small, and
//! owning it means the §12.4 conformance tests can assert the exact bytes on the
//! wire — including that every tool error is a *structured* payload an agent can
//! branch on, rather than a stringly-typed protocol error.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC: &str = "2.0";
/// The MCP revision this server speaks.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub jsonrpc: String,
    /// Absent for notifications.
    #[serde(default)]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC,
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC,
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// JSON-RPC reserved codes.
pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// A server-initiated notification (§8: novel template, stage transition).
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC,
            method: method.into(),
            params,
        }
    }

    /// `notifications/resources/updated` — what a supervising agent subscribes
    /// to so it reacts without polling.
    pub fn resource_updated(uri: impl Into<String>, detail: Value) -> Self {
        Self::new(
            "notifications/resources/updated",
            serde_json::json!({ "uri": uri.into(), "detail": detail }),
        )
    }
}

/// The result shape of `tools/call`.
///
/// MCP wraps tool output in content blocks. conminer always returns exactly one
/// JSON text block plus `structuredContent`, so an agent can read either without
/// parsing prose.
pub fn tool_result(value: Value, is_error: bool) -> Value {
    serde_json::json!({
        "content": [{"type": "text", "text": text_block(&value, is_error)}],
        "structuredContent": value,
        "isError": is_error,
    })
}

/// The human-readable block that accompanies `structuredContent`.
///
/// This used to be `to_string_pretty` of the WHOLE payload, which meant every
/// response carried its data twice -- and the second copy was the inflated one.
/// Measured on a real `list_templates` (limit 25) against a board that had just
/// booted: 6,632 bytes of pretty-printed duplicate against 4,265 bytes of actual
/// structured payload. Roughly 60% of EVERY tool response, on every tool, was a
/// copy of the other 40%.
///
/// So the text block is now a pointer, not a transcript. Agents read
/// `structuredContent` (conminer advertises it, and errors are structured there
/// too); a summary line is enough for a human tailing the wire.
///
/// Set `CONMINER_MCP_TEXT_CONTENT=full` to get the old behaviour back for a
/// client that can only read content blocks. That escape hatch is why this is a
/// default rather than a removal.
fn text_block(value: &Value, is_error: bool) -> String {
    if std::env::var("CONMINER_MCP_TEXT_CONTENT").as_deref() == Ok("full") {
        return serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    }
    // An error is small and is the one thing worth saying in full: a caller
    // staring at a failure should not have to go digging for the reason.
    if is_error {
        return serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    }
    summarize(value)
}

/// One line describing a successful payload, without reproducing it.
fn summarize(value: &Value) -> String {
    let Some(obj) = value.as_object() else {
        return "see structuredContent".into();
    };
    // Name the collections and their sizes -- "38 templates" is what a human
    // scanning the wire actually wants, and it costs a dozen bytes.
    let mut parts: Vec<String> = Vec::new();
    for (k, v) in obj {
        match v {
            Value::Array(a) => parts.push(format!("{k}={}", a.len())),
            Value::Number(n) if k != "server_now" => parts.push(format!("{k}={n}")),
            Value::String(st) if st.len() <= 40 => parts.push(format!("{k}={st}")),
            Value::Bool(b) => parts.push(format!("{k}={b}")),
            _ => {}
        }
        if parts.len() >= 8 {
            break;
        }
    }
    if parts.is_empty() {
        "see structuredContent".into()
    } else {
        format!("{} (see structuredContent)", parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_without_an_id_is_a_notification() {
        let r: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        assert!(r.is_notification());
        let r: Request =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        assert!(!r.is_notification());
    }

    #[test]
    fn responses_omit_the_half_they_do_not_use() {
        let ok = serde_json::to_value(Response::ok(1.into(), serde_json::json!({"a":1}))).unwrap();
        assert!(ok.get("error").is_none());
        let err =
            serde_json::to_value(Response::err(1.into(), INVALID_PARAMS, "bad", None)).unwrap();
        assert!(err.get("result").is_none());
        assert_eq!(err["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn tool_results_carry_both_text_and_structured_content() {
        let v = tool_result(serde_json::json!({"templates": []}), false);
        assert_eq!(v["content"][0]["type"], "text");
        assert!(v["structuredContent"]["templates"].is_array());
        assert_eq!(v["isError"], false);
    }

    /// The payload must appear ONCE.
    ///
    /// Measured on a real list_templates(limit=25): 6,632 bytes of
    /// pretty-printed text block against 4,265 bytes of structured payload --
    /// ~60% of every response, on every tool, was a duplicate of the rest. The
    /// text block is a pointer now.
    #[test]
    fn a_successful_payload_is_not_duplicated_into_the_text_block() {
        let big: Vec<Value> = (0..25)
            .map(|i| serde_json::json!({"id": i, "text": "kernel: <*> probe deferred <*>"}))
            .collect();
        let v = tool_result(serde_json::json!({"templates": big, "returned": 25}), false);

        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(
            !text.contains("probe deferred"),
            "the text block must not reproduce the payload: {text}"
        );
        // But it must still say something useful about what came back.
        assert!(
            text.contains("templates=25"),
            "say what was returned: {text}"
        );
        // And the data itself is untouched.
        assert_eq!(
            v["structuredContent"]["templates"]
                .as_array()
                .unwrap()
                .len(),
            25
        );
    }

    /// Errors stay legible in the text block: they are small, and a caller
    /// staring at a failure should not have to go digging for the reason.
    #[test]
    fn errors_are_still_readable_in_the_text_block() {
        let v = tool_result(
            serde_json::json!({"error": {"code": "UNKNOWN_DEVICE", "message": "no such device"}}),
            true,
        );
        let text = v["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("UNKNOWN_DEVICE"),
            "errors must stay readable: {text}"
        );
    }
}
