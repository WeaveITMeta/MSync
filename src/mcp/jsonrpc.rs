//! JSON-RPC 2.0 framing for the MCP endpoint, hand-rolled on serde_json.

use serde_json::{json, Map, Value};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// A JSON-RPC protocol error: code and a caller-safe message.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }
}

/// One parsed JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Has an id: needs a response.
    Request { id: Value, method: String, params: Option<Value> },
    /// No id: never answered with a body.
    Notification { method: String },
    /// A response to a server request. This server sends none, so it is accepted and ignored.
    Response,
}

pub fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error_response(id: Value, err: &RpcError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": err.code, "message": err.message}})
}

/// The id of a message when it can be recovered (for error responses to invalid requests).
fn recover_id(obj: &Map<String, Value>) -> Value {
    match obj.get("id") {
        Some(v @ (Value::String(_) | Value::Number(_))) => v.clone(),
        _ => Value::Null,
    }
}

/// Parse a request body. On failure returns the id to answer with (null when unknown) and the error.
pub fn parse(body: &[u8]) -> Result<Incoming, (Value, RpcError)> {
    let v: Value = serde_json::from_slice(body).map_err(|_| (Value::Null, RpcError::new(PARSE_ERROR, "parse error: the body is not valid JSON")))?;
    let obj = match v {
        Value::Object(o) => o,
        Value::Array(_) => return Err((Value::Null, RpcError::new(INVALID_REQUEST, "invalid request: batch requests are not supported"))),
        _ => return Err((Value::Null, RpcError::new(INVALID_REQUEST, "invalid request: expected a JSON-RPC 2.0 object"))),
    };
    let id = recover_id(&obj);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((id, RpcError::new(INVALID_REQUEST, "invalid request: jsonrpc must be \"2.0\"")));
    }
    let has_id = obj.contains_key("id");
    if has_id && !matches!(obj.get("id"), Some(Value::String(_) | Value::Number(_))) {
        return Err((Value::Null, RpcError::new(INVALID_REQUEST, "invalid request: id must be a string or a number")));
    }
    let method = match obj.get("method") {
        Some(Value::String(m)) if !m.is_empty() => m.clone(),
        Some(_) => return Err((id, RpcError::new(INVALID_REQUEST, "invalid request: method must be a non-empty string"))),
        None => {
            if has_id && (obj.contains_key("result") || obj.contains_key("error")) {
                return Ok(Incoming::Response);
            }
            return Err((id, RpcError::new(INVALID_REQUEST, "invalid request: method is required")));
        }
    };
    let params = match obj.get("params") {
        None | Some(Value::Null) => None,
        Some(p @ Value::Object(_)) => Some(p.clone()),
        Some(_) => return Err((id, RpcError::new(INVALID_REQUEST, "invalid request: params must be an object"))),
    };
    if has_id {
        Ok(Incoming::Request { id, method, params })
    } else {
        Ok(Incoming::Notification { method })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(body: &str) -> i64 {
        parse(body.as_bytes()).unwrap_err().1.code
    }

    #[test]
    fn classifies_messages_and_errors() {
        assert_eq!(code("{nope"), PARSE_ERROR);
        assert_eq!(code(""), PARSE_ERROR);
        assert_eq!(code("[]"), INVALID_REQUEST);
        assert_eq!(code("7"), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":[1],"method":"ping"}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":5}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":"x"}"#), INVALID_REQUEST);
        assert_eq!(code(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[1]}"#), INVALID_REQUEST);
        assert_eq!(
            parse(br#"{"jsonrpc":"2.0","id":"a","method":"ping"}"#).unwrap(),
            Incoming::Request { id: json!("a"), method: "ping".into(), params: None }
        );
        assert_eq!(
            parse(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).unwrap(),
            Incoming::Notification { method: "notifications/initialized".into() }
        );
        assert_eq!(parse(br#"{"jsonrpc":"2.0","id":3,"result":{}}"#).unwrap(), Incoming::Response);
        // The id is echoed on an invalid request when it can be read.
        assert_eq!(parse(br#"{"jsonrpc":"2.0","id":9,"method":""}"#).unwrap_err().0, json!(9));
    }
}
