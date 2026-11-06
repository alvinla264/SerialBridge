//! The five MCP tools and their translation onto the shared core client.
//!
//! Tools are thin: every device rule (one AI writer, baud conflicts, history,
//! partial lines, waited writes, idle expiry) is enforced by `serial-core`.
//! This module only formats arguments, reports outcomes, and remembers which
//! devices this MCP session connected to so later calls can take a path or an
//! id.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};
use serialbridge_client::{
    Client, ClientConfig, ClientError, ClientInfo, ErrorCode, HistoryLine, Role, Source, WaitSpec,
};

/// Client name this server introduces itself with.
pub const CLIENT_NAME: &str = "serial-mcp";

/// A failed tool call, split by whether the caller's arguments were wrong.
#[derive(Debug)]
pub enum Failure {
    /// Invalid tool arguments: a JSON-RPC `-32602` error.
    Params(String),
    /// The call ran and failed: a tool result with `isError: true`.
    Tool(String),
}

impl Failure {
    pub fn message(&self) -> &str {
        match self {
            Self::Params(message) | Self::Tool(message) => message,
        }
    }
}

/// A successful or failed tool outcome, ready to become an MCP result.
#[derive(Debug)]
pub struct ToolOutcome {
    pub text: String,
    pub structured: Value,
    pub is_error: bool,
}

impl ToolOutcome {
    fn ok(text: String, structured: Value) -> Self {
        Self {
            text,
            structured,
            is_error: false,
        }
    }

    /// A failed call: the message is what the host and its human read.
    pub fn failed(message: String) -> Self {
        Self {
            text: format!("error: {message}"),
            structured: json!({ "error": message }),
            is_error: true,
        }
    }
}

struct Connection {
    port: String,
    baud: u32,
}

/// One MCP server run: a lazily started core connection plus the devices this
/// session has connected to.
pub struct Session {
    client: Mutex<Option<Client>>,
    connections: Mutex<BTreeMap<u64, Connection>>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    pub fn new() -> Self {
        Self {
            client: Mutex::new(None),
            connections: Mutex::new(BTreeMap::new()),
        }
    }

    /// The core connection, starting the core on the first call and replacing
    /// a connection that has since closed.
    fn client(&self) -> Result<Client, Failure> {
        let mut guard = self.client.lock().expect("client lock");
        if let Some(client) = guard.as_ref() {
            if client.is_connected() {
                return Ok(client.clone());
            }
        }
        let client = Client::connect(ClientConfig::new(ClientInfo::ai(CLIENT_NAME)))
            .map_err(|error| Failure::Tool(describe_error(&error, None)))?;
        *guard = Some(client.clone());
        Ok(client)
    }

    /// Drop a device from this session's map once the core says it is gone.
    /// Turn a core error into a tool failure, dropping a device id the core
    /// says is gone so a later reconnect's id is not shadowed.
    fn failure(
        &self,
        error: &ClientError,
        device: Option<&str>,
        device_id: Option<u64>,
    ) -> Failure {
        if error.is_device_gone() {
            if let Some(device_id) = device_id {
                self.forget(device_id);
            }
        }
        Failure::Tool(describe_error(error, device))
    }

    fn forget(&self, device_id: u64) {
        self.connections
            .lock()
            .expect("connections lock")
            .remove(&device_id);
    }

    fn remember(&self, device_id: u64, port: String, baud: u32) {
        let mut connections = self.connections.lock().expect("connections lock");
        // A reopened device gets a new id; the old id is dead and must not
        // shadow the path.
        connections.retain(|_, connection| connection.port != port);
        connections.insert(device_id, Connection { port, baud });
    }

    fn connection(&self, device_id: u64) -> Option<String> {
        self.connections
            .lock()
            .expect("connections lock")
            .get(&device_id)
            .map(|connection| connection.port.clone())
    }

    fn connection_baud(&self, device_id: u64) -> Option<u32> {
        self.connections
            .lock()
            .expect("connections lock")
            .get(&device_id)
            .map(|connection| connection.baud)
    }

    /// Resolve a `device` argument: a numeric device id, or the path of a
    /// device this session (or the core) has open. Paths are canonicalized the
    /// way the core canonicalizes them, so a symlink such as a
    /// `/dev/serial/by-id/...` alias still finds the device it opened.
    fn resolve_device(&self, device: &str) -> Result<u64, Failure> {
        if let Ok(id) = device.parse::<u64>() {
            return Ok(id);
        }
        let canonical = std::fs::canonicalize(device)
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        let matches = |path: &str| path == device || canonical.as_deref() == Some(path);
        let known = {
            let connections = self.connections.lock().expect("connections lock");
            connections
                .iter()
                .find(|(_, connection)| matches(&connection.port))
                .map(|(id, _)| *id)
        };
        if let Some(id) = known {
            return Ok(id);
        }
        let client = self.client()?;
        let devices = client
            .list_devices()
            .map_err(|error| Failure::Tool(describe_error(&error, None)))?;
        devices
            .devices
            .iter()
            .find(|summary| summary.open && matches(&summary.path))
            .and_then(|summary| summary.device_id)
            .ok_or_else(|| {
                Failure::Tool(format!(
                    "no open serial device at {device}; call connect first"
                ))
            })
    }

    /// Run one tool.
    pub fn call(&self, name: &str, arguments: &Value) -> Result<ToolOutcome, Failure> {
        let arguments = match arguments {
            Value::Null => &json!({}),
            Value::Object(_) => arguments,
            _ => return Err(Failure::Params("arguments must be an object".into())),
        };
        let outcome = match name {
            "list_devices" => {
                reject_unknown_arguments(arguments, &[])?;
                self.list_devices()
            }
            "connect" => {
                reject_unknown_arguments(arguments, &["port", "baud", "request_control"])?;
                self.connect(arguments)
            }
            "disconnect" => {
                reject_unknown_arguments(arguments, &["device"])?;
                self.disconnect(arguments)
            }
            "read" => {
                reject_unknown_arguments(arguments, &["device", "lines"])?;
                self.read(arguments)
            }
            "send" => {
                reject_unknown_arguments(arguments, &["device", "text", "lines", "timeout_ms"])?;
                self.send(arguments)
            }
            other => {
                return Err(Failure::Params(format!(
                "unknown tool {other:?}; available: list_devices, connect, disconnect, read, send"
            )))
            }
        };
        // A failed call is still a tool result; only bad arguments are a
        // JSON-RPC error.
        outcome.or_else(|failure| match failure {
            Failure::Params(message) => Err(Failure::Params(message)),
            Failure::Tool(message) => Ok(ToolOutcome::failed(message)),
        })
    }

    fn list_devices(&self) -> Result<ToolOutcome, Failure> {
        let client = self.client()?;
        let devices = client
            .list_devices()
            .map_err(|error| Failure::Tool(describe_error(&error, None)))?;
        let mut text = String::new();
        if devices.devices.is_empty() {
            text.push_str("no serial devices found; plug one in and call list_devices again");
        } else {
            text.push_str("devices (path, state):");
            for device in &devices.devices {
                text.push('\n');
                text.push_str("- ");
                text.push_str(&device.path);
                if let Some(description) = &device.description {
                    text.push_str(" [");
                    text.push_str(description);
                    text.push(']');
                }
                if device.open {
                    text.push_str(&format!(
                        ": open at {} baud (device {}), {} {}",
                        device.baud.unwrap_or(0),
                        device.device_id.unwrap_or(0),
                        device.client_count,
                        plural(device.client_count, "client", "clients"),
                    ));
                    if let Some(holder) = &device.control_holder {
                        text.push_str(", write control: ");
                        text.push_str(&holder.label());
                    } else {
                        text.push_str(", no AI holds write control");
                    }
                } else {
                    text.push_str(": closed");
                }
            }
        }
        Ok(ToolOutcome::ok(text, json!({ "devices": devices.devices })))
    }

    fn connect(&self, arguments: &Value) -> Result<ToolOutcome, Failure> {
        let port = required_string(arguments, "port")?;
        let baud = match optional_u64(arguments, "baud")? {
            Some(value) => Some(
                u32::try_from(value)
                    .map_err(|_| Failure::Params(format!("baud {value} is out of range")))?,
            ),
            None => None,
        };
        let request_control = optional_bool(arguments, "request_control")?.unwrap_or(false);
        let client = self.client()?;
        let opened = client
            .open(port, baud, request_control)
            .map_err(|error| Failure::Tool(describe_error(&error, None)))?;
        self.remember(opened.device_id, opened.port.clone(), opened.baud);

        let holder = opened.control.holder.clone();
        let write_control = opened.role == Role::Controller;
        let mut text = format!(
            "connected to {} (device {}) at {} baud",
            opened.port, opened.device_id, opened.baud
        );
        match opened.role {
            Role::Controller => text.push_str("; write control: granted"),
            Role::Human => text.push_str("; write control: human client"),
            Role::Reader => {
                text.push_str(", read-only");
                match (&holder, request_control) {
                    (Some(holder), _) => {
                        text.push_str(&format!(
                            ": another AI client holds write control ({})",
                            holder.label()
                        ));
                        if let Some(left) = opened.control.expires_in_ms {
                            text.push_str(&format!(", expiring in {}", human_duration(left)));
                        }
                        text.push_str(
                            ". Call connect again with request_control: true to claim it when it is free.",
                        );
                    }
                    (None, true) => text.push_str(
                        ": write control was not granted, and no other AI holds it (only AI clients hold control).",
                    ),
                    (None, false) => text.push_str(
                        ": write control was not requested; call connect again with request_control: true to write.",
                    ),
                }
            }
        }
        Ok(ToolOutcome::ok(
            text,
            json!({
                "device_id": opened.device_id,
                "port": opened.port,
                "baud": opened.baud,
                "role": role_name(opened.role),
                "write_control": write_control,
                "requested_control": request_control,
                "holder": holder,
                "expires_in_ms": opened.control.expires_in_ms,
            }),
        ))
    }

    fn disconnect(&self, arguments: &Value) -> Result<ToolOutcome, Failure> {
        let device = required_device(arguments)?;
        let device_id = self.resolve_device(&device)?;
        let client = self.client()?;
        let status = client.status(device_id).ok();
        let port = status
            .as_ref()
            .map(|status| status.path.clone())
            .or_else(|| self.connection(device_id))
            .unwrap_or_else(|| device.clone());
        client.close(device_id).map_err(|error| {
            Failure::Tool(describe_error(
                &error,
                Some(&device_label(&device, Some(port.clone()))),
            ))
        })?;
        self.forget(device_id);
        let device_closed = status
            .as_ref()
            .is_none_or(|status| status.client_count <= 1);
        let text = if device_closed {
            format!("disconnected from {port} (device {device_id}); the device is now closed")
        } else {
            let left = status
                .as_ref()
                .map(|status| status.client_count - 1)
                .unwrap_or(0);
            format!(
                "disconnected from {port} (device {device_id}); left the device open for {}",
                plural(left, "other client", "other clients")
            )
        };
        Ok(ToolOutcome::ok(
            text,
            json!({
                "device_id": device_id,
                "port": port,
                "device_closed": device_closed,
            }),
        ))
    }

    fn read(&self, arguments: &Value) -> Result<ToolOutcome, Failure> {
        let device = required_device(arguments)?;
        let device_id = self.resolve_device(&device)?;
        let lines = match optional_u64(arguments, "lines")? {
            Some(0) => {
                return Err(Failure::Params(
                    "lines must be at least 1; omit it for the core default".into(),
                ))
            }
            Some(lines) => Some(
                usize::try_from(lines)
                    .map_err(|_| Failure::Params(format!("lines {lines} is too large")))?,
            ),
            None => None,
        };
        let client = self.client()?;
        let read = client.read(device_id, lines).map_err(|error| {
            self.failure(
                &error,
                Some(&device_label(&device, self.connection(device_id))),
                Some(device_id),
            )
        })?;
        let port = self
            .connection(device_id)
            .unwrap_or_else(|| format!("device {device_id}"));
        let mut counts = (0usize, 0usize, 0usize);
        for line in &read.lines {
            match line.source {
                Source::Device => counts.0 += 1,
                Source::Ai => counts.1 += 1,
                Source::Human => counts.2 += 1,
            }
        }
        let baud = self
            .connection_baud(device_id)
            .map(|baud| format!(" at {baud} baud"))
            .unwrap_or_default();
        let mut text = format!(
            "{port} (device {device_id}{baud}): {} (device {}, ai {}, human {}); dropped older lines: {}; truncated: {}",
            plural(read.lines.len(), "line", "lines"),
            counts.0,
            counts.1,
            counts.2,
            read.dropped_lines,
            if read.truncated {
                "yes (the oldest lines were trimmed by the core; ask for fewer lines)"
            } else {
                "no"
            }
        );
        text.push('\n');
        text.push_str(&format_lines(&read.lines));
        Ok(ToolOutcome::ok(
            text,
            json!({
                "device_id": device_id,
                "port": port,
                "lines": read.lines,
                "dropped_lines": read.dropped_lines,
                "truncated": read.truncated,
            }),
        ))
    }

    fn send(&self, arguments: &Value) -> Result<ToolOutcome, Failure> {
        let device = required_device(arguments)?;
        let text = required_string(arguments, "text")?;
        let device_id = self.resolve_device(&device)?;
        let requested_lines = optional_u64(arguments, "lines")?.unwrap_or(1);
        let client = self.client()?;
        let limits = client.hello().limits;
        // The core caps its wait target silently; do it here so the reply can
        // say what it actually waited for.
        let lines = requested_lines.min(limits.read_max_lines as u64);
        let capped = lines != requested_lines;
        let requested_timeout =
            optional_u64(arguments, "timeout_ms")?.unwrap_or(limits.write_default_timeout_ms);
        let timeout = requested_timeout.min(limits.write_max_timeout_ms);
        let bounded = timeout != requested_timeout;
        let wait = if lines == 0 {
            None
        } else {
            Some(WaitSpec {
                lines: usize::try_from(lines)
                    .map_err(|_| Failure::Params(format!("lines {lines} is too large")))?,
                timeout_ms: Some(timeout),
            })
        };
        // On timeout the core includes the device's current partial tail,
        // which may have started before this send. Snapshot it so the reply
        // can say so instead of presenting an old prompt as a response.
        let prior_partial = if wait.is_some() {
            client.read(device_id, Some(1)).ok().and_then(|history| {
                history
                    .lines
                    .last()
                    .filter(|line| line.partial && line.source == Source::Device)
                    .map(|line| line.text.clone())
            })
        } else {
            None
        };
        let started = Instant::now();
        let written = client
            .write_bytes(device_id, text.as_bytes(), wait.clone())
            .map_err(|error| {
                self.failure(
                    &error,
                    Some(&device_label(&device, self.connection(device_id))),
                    Some(device_id),
                )
            })?;
        let elapsed = started.elapsed().as_millis() as u64;
        let port = self
            .connection(device_id)
            .unwrap_or_else(|| format!("device {device_id}"));
        let mut message = format!(
            "sent {} to {port} (device {device_id})",
            plural(written.bytes_written, "byte", "bytes")
        );
        match &wait {
            None => message.push_str("; no response requested (lines: 0)"),
            Some(wait) => {
                let arrived = written.lines.len();
                if written.timed_out {
                    message.push_str(&format!(
                        "; timed out after {} waiting for {} ({} arrived)",
                        human_duration(timeout),
                        device_lines(wait.lines as u64),
                        arrived
                    ));
                } else {
                    message.push_str(&format!(
                        "; received {} of {} after the send (elapsed {})",
                        arrived,
                        device_lines(wait.lines as u64),
                        human_duration(elapsed)
                    ));
                }
                if capped {
                    message.push_str(&format!(
                        "; lines capped to the core maximum of {} (requested {requested_lines})",
                        limits.read_max_lines
                    ));
                }
                if bounded {
                    message.push_str(&format!(
                        "; timeout bounded to the core maximum of {}",
                        human_duration(limits.write_max_timeout_ms)
                    ));
                }
                if written.truncated {
                    message.push_str("; response truncated, only the newest lines were kept");
                }
                if written.timed_out
                    && written.lines.iter().any(|line| {
                        line.partial && prior_partial.as_deref() != Some(line.text.as_str())
                    })
                {
                    message.push_str("; a partial tail may have started before this send");
                }
                if !written.lines.is_empty() {
                    message.push('\n');
                    message.push_str(&format_send_lines(
                        &written.lines,
                        prior_partial.as_deref(),
                        written.timed_out,
                    ));
                } else if written.timed_out {
                    message.push_str("\n  (no device output arrived before the timeout)");
                }
            }
        }
        Ok(ToolOutcome::ok(
            message,
            json!({
                "device_id": device_id,
                "port": port,
                "bytes_written": written.bytes_written,
                "lines": send_lines(&written.lines, prior_partial.as_deref(), written.timed_out),
                "timed_out": written.timed_out,
                "truncated": written.truncated,
                "waited_for_lines": lines,
                "requested_lines": requested_lines,
                "lines_capped": capped,
                "timeout_ms": timeout,
                "bounded_timeout": bounded,
                "elapsed_ms": elapsed,
            }),
        ))
    }
}

/// The MCP `tools/list` answer.
pub fn definitions() -> Value {
    json!([
        {
            "name": "list_devices",
            "description": "List every serial device the local SerialBridge core can see, including open ones with their active baud rate, client count and the AI client that holds write control. Use the paths it reports with connect.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        },
        {
            "name": "connect",
            "description": "Connect this session to a serial device, or join one already open, and optionally ask for AI write control. Without baud, an open device keeps its active rate and a closed device opens at 115200. An explicit different baud is rejected. When another AI holds write control, the connection is read-only and this call names the holder; call connect again with request_control true to claim control once it is free. One session can connect to several devices.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "port": {
                        "type": "string",
                        "description": "Device path, e.g. /dev/ttyUSB0 or /dev/serial/by-id/..."
                    },
                    "baud": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Baud rate. Omit to join at the active rate, or open a new device at 115200."
                    },
                    "request_control": {
                        "type": "boolean",
                        "default": false,
                        "description": "Ask for AI write control (required before send)."
                    }
                },
                "required": ["port"],
                "additionalProperties": false
            }
        },
        {
            "name": "disconnect",
            "description": "Disconnect this session from a device. The last client leaving closes the device and releases its baud rate and write control.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": { "type": ["string", "integer"], "description": "Device id from connect, or its path." }
                },
                "required": ["device"],
                "additionalProperties": false
            }
        },
        {
            "name": "read",
            "description": "Read recent buffered history of a device: device output, AI writes and human writes, newest last, each labelled by source. Partial lines (no newline yet) are marked. Reports dropped older lines and truncation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": { "type": ["string", "integer"], "description": "Device id from connect, or its path." },
                    "lines": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "How many of the most recent lines to return (default 50, core maximum 2000)."
                    }
                },
                "required": ["device"],
                "additionalProperties": false
            }
        },
        {
            "name": "send",
            "description": "Send exact text to a device (no newline is added) and wait for device-output lines received after the send. Requires AI write control; fails clearly with ControlHeld otherwise. On timeout it returns what arrived, including a marked partial line. lines: 0 sends without waiting. No continuous stream: each call returns one bounded batch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "device": { "type": ["string", "integer"], "description": "Device id from connect, or its path." },
                    "text": {
                        "type": "string",
                        "description": "Exact bytes to send, as text (JSON escapes such as \\r and \\n are sent as those bytes)."
                    },
                    "lines": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 1,
                        "description": "Device-output lines to wait for after the send. 0 returns as soon as the text is queued."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "How long to wait for those lines (default 1000 ms, core maximum 30000 ms)."
                    }
                },
                "required": ["device", "text"],
                "additionalProperties": false
            }
        }
    ])
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Controller => "controller",
        Role::Reader => "reader",
        Role::Human => "human",
    }
}

fn source_name(source: Source) -> &'static str {
    match source {
        Source::Device => "device",
        Source::Ai => "ai",
        Source::Human => "human",
    }
}

/// Send results add `predates_send`, so a host can tell an old prompt from
/// output that arrived after its write.
fn send_lines(lines: &[HistoryLine], prior_partial: Option<&str>, timed_out: bool) -> Vec<Value> {
    lines
        .iter()
        .map(|line| {
            json!({
                "source": source_name(line.source),
                "text": line.text,
                "partial": line.partial,
                "predates_send": predates_send(line, prior_partial, timed_out),
            })
        })
        .collect()
}

fn predates_send(line: &HistoryLine, prior_partial: Option<&str>, timed_out: bool) -> bool {
    timed_out
        && line.partial
        && line.source == Source::Device
        && prior_partial == Some(line.text.as_str())
}

/// Like [`format_lines`], but marks a partial tail the device already had
/// before this send.
fn format_send_lines(
    lines: &[HistoryLine],
    prior_partial: Option<&str>,
    timed_out: bool,
) -> String {
    let mut text = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str("  [");
        text.push_str(source_name(line.source));
        text.push(']');
        if predates_send(line, prior_partial, timed_out) {
            text.push_str(" (partial, present before this send)");
        } else if line.partial {
            text.push_str(" (partial)");
        }
        text.push(' ');
        text.push_str(&line.text);
    }
    text
}

fn format_lines(lines: &[HistoryLine]) -> String {
    if lines.is_empty() {
        return "  (no lines)".to_string();
    }
    let mut text = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        text.push_str("  [");
        text.push_str(source_name(line.source));
        text.push(']');
        if line.partial {
            text.push_str(" (partial)");
        }
        text.push(' ');
        text.push_str(&line.text);
    }
    text
}

fn device_lines(count: u64) -> String {
    match count {
        1 => "1 device line".to_string(),
        count => format!("{count} device lines"),
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

fn device_label(argument: &str, known: Option<String>) -> String {
    match known {
        Some(port) if port != argument => format!("{port} (device {argument})"),
        _ => argument.to_string(),
    }
}

/// Human duration for messages: `950 ms`, `3.2 s`, `4m 58s`.
fn human_duration(millis: u64) -> String {
    if millis < 1_000 {
        format!("{millis} ms")
    } else if millis < 60_000 {
        format!("{:.1} s", millis as f64 / 1_000.0)
    } else {
        format!("{}m {:02}s", millis / 60_000, (millis % 60_000) / 1_000)
    }
}

/// Turn a core error into a message an AI host (and its human) can act on.
fn describe_error(error: &ClientError, device: Option<&str>) -> String {
    let target = device.unwrap_or("that device");
    match error {
        ClientError::Server(body) => match body.code {
            ErrorCode::BaudConflict => format!(
                "{}; omit baud to join at the active rate",
                body.message
            ),
            ErrorCode::ControlHeld => format!(
                "{}; this session is read-only for {target}, so call connect again with request_control: true once control is free",
                body.message
            ),
            ErrorCode::NotAttached => format!(
                "this session is not connected to {target}; call connect first"
            ),
            ErrorCode::NotFound => format!(
                "{}; call list_devices to see what is available, then connect",
                body.message
            ),
            ErrorCode::DeviceClosed | ErrorCode::DeviceFailed => format!(
                "{}; the device is gone, so call connect again after it is back",
                body.message
            ),
            ErrorCode::PermissionDenied | ErrorCode::BadRequest => {
                format!("{} ({:?})", body.message, body.code)
            }
            ErrorCode::Timeout => format!("serial-core timed out: {}", body.message),
            _ => body.message.clone(),
        },
        ClientError::CoreNotFound => {
            "serial-core was not found; install it next to serial-mcp or set SERIALBRIDGE_CORE"
                .to_string()
        }
        ClientError::CoreStart(reason) => format!("serial-core did not start: {reason}"),
        ClientError::Disconnected(reason) => format!(
            "lost the connection to serial-core ({reason}); the next call starts it again"
        ),
        ClientError::Timeout(reason) => format!("serial-core did not answer in time: {reason}"),
        ClientError::UnsupportedProtocol { core, client } => format!(
            "serial-core speaks protocol {core}, this serial-mcp speaks {client}; upgrade one of them"
        ),
        other => format!("serial-core error: {other}"),
    }
}

/// The `device` argument: an id (number or digits) or a path.
fn required_device(arguments: &Value) -> Result<String, Failure> {
    match arguments.get("device") {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(value @ Value::Number(_)) => as_integral(value)
            .map(|value| value.to_string())
            .ok_or_else(|| {
                Failure::Params("device id must be a whole number, or a path string".into())
            }),
        Some(_) => Err(Failure::Params(
            "device must be a device id (number) or a path (string)".into(),
        )),
        None => Err(Failure::Params("device is required".into())),
    }
}

fn required_string(arguments: &Value, name: &str) -> Result<String, Failure> {
    match arguments.get(name) {
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(Failure::Params(format!("{name} must be a string"))),
        None => Err(Failure::Params(format!("{name} is required"))),
    }
}

fn optional_bool(arguments: &Value, name: &str) -> Result<Option<bool>, Failure> {
    match arguments.get(name) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(Failure::Params(format!("{name} must be true or false"))),
    }
}

fn optional_u64(arguments: &Value, name: &str) -> Result<Option<u64>, Failure> {
    match arguments.get(name) {
        None => Ok(None),
        Some(value) => as_integral(value)
            .map(Some)
            .ok_or_else(|| Failure::Params(format!("{name} must be a non-negative integer"))),
    }
}

/// A JSON Schema `integer`: `1` and `1.0` both count, `1.5` does not.
fn as_integral(value: &Value) -> Option<u64> {
    let number = value.as_number()?;
    if let Some(value) = number.as_u64() {
        return Some(value);
    }
    let float = number.as_f64()?;
    if float.is_finite() && float >= 0.0 && float.fract() == 0.0 && float <= u64::MAX as f64 {
        Some(float as u64)
    } else {
        None
    }
}

/// Refuse misspelled or extra arguments instead of silently ignoring them:
/// a typo must not open a device at the wrong rate.
fn reject_unknown_arguments(arguments: &Value, allowed: &[&str]) -> Result<(), Failure> {
    let object = arguments.as_object().cloned().unwrap_or_default();
    let unknown: Vec<&String> = object
        .keys()
        .filter(|key| !allowed.contains(&key.as_str()))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let unknown = unknown
        .iter()
        .map(|key| format!("{key:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let allowed = if allowed.is_empty() {
        "this tool takes no arguments".to_string()
    } else {
        format!("allowed arguments: {}", allowed.join(", "))
    };
    Err(Failure::Params(format!(
        "unknown argument(s) {unknown}; {allowed}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_are_unique_and_well_formed() {
        let tools = definitions();
        let tools = tools.as_array().expect("array");
        let mut names = Vec::new();
        for tool in tools {
            names.push(tool["name"].as_str().expect("name"));
            assert!(tool["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty()));
            assert_eq!(tool["inputSchema"]["type"], "object");
        }
        names.sort();
        assert_eq!(
            names,
            ["connect", "disconnect", "list_devices", "read", "send"]
        );
    }

    #[test]
    fn only_a_device_tail_can_predate_the_send() {
        let device_line = HistoryLine {
            source: Source::Device,
            text: "AT".into(),
            partial: true,
        };
        let ai_line = HistoryLine {
            source: Source::Ai,
            text: "AT".into(),
            partial: true,
        };
        assert!(predates_send(&device_line, Some("AT"), true));
        assert!(!predates_send(&device_line, Some("AT"), false));
        assert!(!predates_send(&ai_line, Some("AT"), true));
        assert!(!predates_send(&device_line, Some("other"), true));
    }

    #[test]
    fn reconnect_replaces_a_stale_id_for_the_same_port() {
        let session = Session::new();
        session.remember(1, "/dev/ttyUSB0".into(), 115_200);
        session.remember(2, "/dev/ttyUSB0".into(), 9_600);
        assert_eq!(session.connection(1), None);
        assert_eq!(session.connection(2).as_deref(), Some("/dev/ttyUSB0"));
        assert_eq!(session.connection_baud(2), Some(9_600));
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(950), "950 ms");
        assert_eq!(human_duration(3_200), "3.2 s");
        assert_eq!(human_duration(298_000), "4m 58s");
    }

    #[test]
    fn arguments_are_validated() {
        assert!(matches!(
            required_string(&json!({}), "port"),
            Err(Failure::Params(message)) if message.contains("port")
        ));
        assert!(matches!(
            optional_u64(&json!({"lines": -1}), "lines"),
            Err(Failure::Params(_))
        ));
        assert_eq!(
            optional_bool(&json!({"request_control": true}), "request_control").unwrap(),
            Some(true)
        );
    }
}
