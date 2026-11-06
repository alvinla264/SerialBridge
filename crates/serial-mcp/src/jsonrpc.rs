//! The MCP stdio transport's JSON-RPC 2.0 framing.
//!
//! MCP over stdio sends one JSON-RPC message per line, so a message may not
//! contain a raw newline (`serde_json` escapes them inside strings). This
//! module only parses and builds those messages; the dispatcher lives in
//! [`crate::server`].

use serde_json::{json, Value};

/// A request that expects a response.
#[derive(Clone, Debug)]
pub struct Request {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// What one input line turned out to be.
#[derive(Clone, Debug)]
pub enum Message {
    /// A caller request: always answered.
    Request(Request),
    /// A notification: never answered.
    Notification { method: String },
    /// A broken line; answered with `id` (or null) and a JSON-RPC error.
    Invalid {
        id: Option<Value>,
        code: i64,
        message: String,
    },
    /// A JSON-RPC batch: handled in order, answered as one array (or not at
    /// all when it contains only notifications).
    Batch(Vec<Message>),
    /// Nothing to do (an empty line between messages).
    Skip,
}

/// Parse one line of the stdio transport.
pub fn parse(line: &str) -> Message {
    if line.trim().is_empty() {
        return Message::Skip;
    }
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Message::Invalid {
                id: None,
                code: PARSE_ERROR,
                message: format!("invalid JSON: {error}"),
            }
        }
    };
    match value {
        Value::Array(items) => {
            if items.is_empty() {
                return Message::Invalid {
                    id: None,
                    code: INVALID_REQUEST,
                    message: "an empty batch is not a valid request".into(),
                };
            }
            Message::Batch(items.into_iter().map(parse_value).collect())
        }
        other => parse_value(other),
    }
}

fn parse_value(value: Value) -> Message {
    let Value::Object(object) = value else {
        return Message::Invalid {
            id: None,
            code: INVALID_REQUEST,
            message: "an MCP message must be a JSON object (no nested batches)".into(),
        };
    };
    // MCP request ids are strings or integers, never null; anything else
    // cannot be echoed safely, so the answer uses a null id.
    let id = match object.get("id") {
        None => None,
        Some(value @ Value::String(_)) => Some(value.clone()),
        Some(value @ Value::Number(_)) if is_integral(value) => Some(value.clone()),
        Some(_) => {
            return Message::Invalid {
                id: None,
                code: INVALID_REQUEST,
                message: "id must be a string or an integer".into(),
            }
        }
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Message::Invalid {
            id,
            code: INVALID_REQUEST,
            message: r#"jsonrpc must be "2.0""#.into(),
        };
    }
    let method = match object.get("method").and_then(Value::as_str) {
        Some(method) if !method.is_empty() => method.to_string(),
        _ => {
            return Message::Invalid {
                id,
                code: INVALID_REQUEST,
                message: "missing method".into(),
            }
        }
    };
    let Some(id) = id else {
        return Message::Notification { method };
    };
    // A request's params must be structured; a scalar would be silently
    // discarded by every handler.
    let params = match object.get("params") {
        None => Value::Null,
        Some(params @ (Value::Object(_) | Value::Array(_))) => params.clone(),
        Some(_) => {
            return Message::Invalid {
                id: Some(id),
                code: INVALID_REQUEST,
                message: "params must be an object or an array".into(),
            }
        }
    };
    Message::Request(Request { id, method, params })
}

fn is_integral(value: &Value) -> bool {
    let Some(number) = value.as_number() else {
        return false;
    };
    if number.as_i64().is_some() || number.as_u64().is_some() {
        return true;
    }
    number.as_f64().is_some_and(|float| float.fract() == 0.0)
}

/// JSON-RPC error codes used here.
pub const PARSE_ERROR: i64 = -32_700;
pub const INVALID_REQUEST: i64 = -32_600;
pub const METHOD_NOT_FOUND: i64 = -32_601;
pub const INVALID_PARAMS: i64 = -32_602;

/// A JSON-RPC success response.
pub fn result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// A JSON-RPC error response.
pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

/// One response line (without its newline).
pub fn encode(value: &Value) -> String {
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_notifications_and_errors_are_distinguished() {
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#),
            Message::Request(Request { id, method, .. }) if id == json!(1) && method == "ping"
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
            Message::Notification { method } if method == "notifications/initialized"
        ));
        assert!(matches!(
            parse("nope"),
            Message::Invalid {
                code: PARSE_ERROR,
                ..
            }
        ));
        assert!(matches!(
            parse("[1,2]"),
            Message::Batch(items)
                if items.len() == 2
                    && items.iter().all(|item| matches!(
                        item,
                        Message::Invalid {
                            code: INVALID_REQUEST,
                            ..
                        }
                    ))
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#),
            Message::Invalid {
                code: INVALID_REQUEST,
                ..
            }
        ));
        // Envelopes JSON-RPC forbids are rejected before dispatch.
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":true,"method":"ping"}"#),
            Message::Invalid {
                id: None,
                code: INVALID_REQUEST,
                ..
            }
        ));
        for invalid in [
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#,
        ] {
            assert!(
                matches!(
                    parse(invalid),
                    Message::Invalid {
                        id: None,
                        code: INVALID_REQUEST,
                        ..
                    }
                ),
                "{invalid} was accepted"
            );
        }
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":1.0,"method":"ping"}"#),
            Message::Request(Request { .. })
        ));
        assert!(matches!(
            parse(r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":42}"#),
            Message::Invalid {
                code: INVALID_REQUEST,
                ..
            }
        ));
        assert!(matches!(parse("   "), Message::Skip));
    }

    #[test]
    fn responses_are_single_line_json() {
        let value = error(json!(null), METHOD_NOT_FOUND, "no such method");
        let line = encode(&value);
        assert!(!line.contains('\n'));
        let value: Value = serde_json::from_str(&line).expect("json");
        assert_eq!(value["error"]["code"], METHOD_NOT_FOUND);
    }

    #[test]
    fn batches_are_parsed_in_order_and_must_not_be_empty() {
        match parse(r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"n"}]"#)
        {
            Message::Batch(items) => {
                assert_eq!(items.len(), 2);
                assert!(matches!(items[0], Message::Request(_)));
                assert!(matches!(items[1], Message::Notification { .. }));
            }
            other => panic!("expected a batch: {other:?}"),
        }
        assert!(matches!(
            parse("[]"),
            Message::Invalid {
                code: INVALID_REQUEST,
                ..
            }
        ));
        assert!(matches!(
            parse(r#"[[{"jsonrpc":"2.0","id":1,"method":"ping"}]]"#),
            Message::Batch(items) if matches!(items[0], Message::Invalid { code: INVALID_REQUEST, .. })
        ));
    }
}
