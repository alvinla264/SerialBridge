//! The MCP server over stdio: handshake, `tools/list`, `tools/call`, and the
//! newline-delimited JSON-RPC framing around them.
//!
//! Requests are handled strictly in arrival order: a batch in order, and one
//! line at a time otherwise. That keeps serial writes in the order the host
//! sent them (the core serializes writes in the order they arrive) and answers
//! every request exactly once. The cost is that a `send` waiting for device
//! output delays the next request on the same session; a host that needs
//! concurrency across devices can run a second session. Nothing is streamed
//! unsolicited from the core to the host.

use std::io::{BufRead, Write};

use serde_json::{json, Value};
use serial_nexus_protocol::MAX_MESSAGE_BYTES;

use crate::jsonrpc::{self, Message, Request};
use crate::tools::{Failure, Session, ToolOutcome};

/// Protocol versions this server can answer in. All three allow batching;
/// `2025-06-18` removed it, and a host that batches is still answered.
pub const PROTOCOL_VERSIONS: [&str; 3] = ["2024-11-05", "2025-03-26", "2025-06-18"];

/// Version used when the host asks for one we do not speak.
pub const LATEST_PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "SerialNexus serial devices, shared with the human via a local core. \
Call list_devices, then connect (request_control: true) to a path. read returns recent buffered \
lines labelled device/ai/human with partial lines marked. send writes the exact text (no newline \
is added) and waits for the requested device-output lines or a bounded timeout, returning what \
arrived. One AI may hold write control per device; when another holds it, connect is read-only \
until it expires or disconnects. There is no continuous stream: each call returns one bounded batch. Give ip and port together to use a device another host shares; omit both for the local core. Pairing credentials come from the host user's private configuration, never from tool arguments.";

/// A JSON-RPC dispatch error.
struct RpcError {
    code: i64,
    message: String,
}

impl RpcError {
    fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Serve MCP over `input`/`output` until the host closes its side.
pub fn serve(
    session: &Session,
    mut input: impl BufRead,
    mut output: impl Write,
) -> std::io::Result<()> {
    loop {
        let line = match serial_nexus_protocol::read_line(&mut input, MAX_MESSAGE_BYTES)? {
            Some(line) => line,
            None => return Ok(()),
        };
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches('\n').trim_end_matches('\r');
        let response =
            match jsonrpc::parse(text) {
                Message::Skip | Message::Notification { .. } => None,
                Message::Invalid { id, code, message } => {
                    Some(jsonrpc::error(id.unwrap_or(Value::Null), code, &message))
                }
                Message::Request(request) => Some(handle(session, &request)),
                Message::Batch(items) => {
                    let mut responses = Vec::new();
                    for item in items {
                        match item {
                            Message::Request(request) => responses.push(handle(session, &request)),
                            Message::Invalid { id, code, message } => responses
                                .push(jsonrpc::error(id.unwrap_or(Value::Null), code, &message)),
                            Message::Skip | Message::Notification { .. } | Message::Batch(_) => {}
                        }
                    }
                    if responses.is_empty() {
                        None
                    } else {
                        Some(Value::Array(responses))
                    }
                }
            };
        if let Some(response) = response {
            let line = jsonrpc::encode(&response);
            output.write_all(line.as_bytes())?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
}

fn handle(session: &Session, request: &Request) -> Value {
    match dispatch(session, &request.method, &request.params) {
        Ok(result) => jsonrpc::result(request.id.clone(), result),
        Err(error) => jsonrpc::error(request.id.clone(), error.code, &error.message),
    }
}

fn dispatch(session: &Session, method: &str, params: &Value) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": crate::tools::definitions() })),
        "tools/call" => call_tool(session, params),
        other => Err(RpcError::new(
            jsonrpc::METHOD_NOT_FOUND,
            format!("method not found: {other}"),
        )),
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = match requested {
        Some(version) if PROTOCOL_VERSIONS.contains(&version) => version,
        _ => LATEST_PROTOCOL,
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": crate::tools::CLIENT_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": INSTRUCTIONS,
    })
}

fn call_tool(session: &Session, params: &Value) -> Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(jsonrpc::INVALID_PARAMS, "tools/call needs a tool name"))?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    match session.call(name, &arguments) {
        Ok(outcome) => Ok(tool_result(&outcome)),
        Err(Failure::Params(message)) => Err(RpcError::new(jsonrpc::INVALID_PARAMS, message)),
        Err(Failure::Tool(message)) => Ok(tool_result(&ToolOutcome::failed(message))),
    }
}

fn tool_result(outcome: &ToolOutcome) -> Value {
    json!({
        "content": [{ "type": "text", "text": outcome.text }],
        "structuredContent": outcome.structured,
        "isError": outcome.is_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_negotiates_a_known_version() {
        assert_eq!(
            initialize(&json!({"protocolVersion": "2024-11-05"}))["protocolVersion"],
            "2024-11-05"
        );
        assert_eq!(
            initialize(&json!({"protocolVersion": "1999-01-01"}))["protocolVersion"],
            LATEST_PROTOCOL
        );
    }

    #[test]
    fn unknown_methods_are_method_not_found() {
        let session = Session::new();
        let error = dispatch(&session, "nope", &json!({})).expect_err("unknown method");
        assert_eq!(error.code, jsonrpc::METHOD_NOT_FOUND);
    }

    #[test]
    fn a_batch_is_answered_in_order() {
        let session = Session::new();
        let input = concat!(
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"},"#,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}]"#,
            "\n"
        );
        let mut output = Vec::new();
        serve(&session, input.as_bytes(), &mut output).expect("serve");
        let text = String::from_utf8(output).expect("utf8");
        let value: Value = serde_json::from_str(text.trim()).expect("json");
        let responses = value.as_array().expect("array");
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["id"], 1);
        assert_eq!(responses[1]["id"], 2);
    }
}
