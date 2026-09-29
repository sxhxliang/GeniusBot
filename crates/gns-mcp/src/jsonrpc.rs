//! JSON-RPC 2.0 message helpers.

use crate::error::McpError;
use serde_json::{Value, json};

pub(crate) fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub(crate) fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub(crate) fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(crate) fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// What one incoming message is.
pub(crate) enum Incoming {
    /// A reply to one of our requests.
    Response { id: i64, result: Result<Value, McpError> },
    /// A request from the server that expects an answer.
    Request { id: Value, method: String },
    /// A notification from the server.
    Notification { method: String },
}

pub(crate) fn classify(message: Value) -> Option<Incoming> {
    let obj = message.as_object()?;
    let method = obj.get("method").and_then(Value::as_str).map(str::to_owned);
    let id = obj.get("id").cloned().filter(|v| !v.is_null());
    match (method, id) {
        (Some(method), Some(id)) => Some(Incoming::Request { id, method }),
        (Some(method), None) => Some(Incoming::Notification { method }),
        (None, Some(id)) => {
            let id = id.as_i64()?;
            let result = if let Some(err) = obj.get("error") {
                Err(McpError::Rpc {
                    code: err.get("code").and_then(Value::as_i64).unwrap_or(-1),
                    message: err.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_owned(),
                })
            } else {
                Ok(obj.get("result").cloned().unwrap_or(Value::Null))
            };
            Some(Incoming::Response { id, result })
        }
        (None, None) => None,
    }
}
