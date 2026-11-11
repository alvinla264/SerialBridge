//! Wire types and framing shared by `serial-nexus-core` and every SerialNexus client.
//!
//! The core speaks newline-delimited JSON over a Unix domain socket. One JSON
//! object per line, at most [`MAX_MESSAGE_BYTES`] per line. This crate is the
//! single place that defines those objects, so the MCP client, the human
//! terminal and the core can never drift apart.
//!
//! Clients normally do not use this crate directly: `serial-nexus-client`
//! wraps it in a typed, thread-safe API that also starts the core when needed.

use base64::Engine as _;
use serde::{Deserialize, Serialize};

pub mod share;
pub use share::*;

/// Version of the client/core protocol.
pub const PROTOCOL_VERSION: u32 = 1;

/// Largest accepted JSON line, in bytes, in either direction.
pub const MAX_MESSAGE_BYTES: usize = 1 << 20;

/// Baud rate used when a new device is opened without one. Clients also offer
/// this as their default choice.
pub const DEFAULT_BAUD: u32 = 115_200;

/// Base64 (standard alphabet, padded) of raw bytes.
pub fn encode_bytes(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// Decode base64 produced by [`encode_bytes`].
pub fn decode_bytes(data: &str) -> Result<Vec<u8>, ProtocolError> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| ProtocolError::BadBase64(e.to_string()))
}

/// Errors from framing or decoding a message.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message is {size} bytes, more than the {max} byte limit")]
    TooLarge { size: usize, max: usize },
    #[error("invalid JSON message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid base64 field: {0}")]
    BadBase64(String),
    #[error("empty message")]
    Empty,
}

/// Who a client is. The core uses this to decide whether writes need control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    /// An AI client (the MCP server). Needs write control to send bytes.
    Ai,
    /// A human-facing client (the terminal). May always write.
    Human,
    /// Anything else: can discover, open, read, but not write.
    Other,
}

/// Identity a client sends in its `hello` request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientInfo {
    pub kind: ClientKind,
    /// Free-form label shown to other clients, e.g. "claude-desktop".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ClientInfo {
    pub fn ai(name: impl Into<String>) -> Self {
        Self {
            kind: ClientKind::Ai,
            name: Some(name.into()),
        }
    }

    pub fn human(name: impl Into<String>) -> Self {
        Self {
            kind: ClientKind::Human,
            name: Some(name.into()),
        }
    }

    pub fn other(name: impl Into<String>) -> Self {
        Self {
            kind: ClientKind::Other,
            name: Some(name.into()),
        }
    }
}

/// One request line: a numeric id plus a typed request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RequestMessage {
    pub id: u64,
    #[serde(flatten)]
    pub request: Request,
}

/// A client request. Tagged by `method`, parameters under `params`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    /// Must be the first request on a connection.
    Hello(HelloParams),
    Ping,
    /// Every serial device the core can see, merged with open devices.
    ListDevices,
    /// Open a device, join an open one, and/or request write control.
    Open(OpenParams),
    /// Disconnect this client from a device (the last one closes the device).
    Close(DeviceRef),
    /// Recent history of a device.
    Read(ReadParams),
    /// Write bytes and optionally wait for device response lines.
    Write(WriteParams),
    /// One device's state.
    Status(DeviceRef),
    /// Start sharing checked devices to remote clients on one concrete
    /// address. Local owner only; the answer carries the pairing secrets.
    ShareStart(ShareStartParams),
    /// Stop sharing, disconnect every remote client and revoke credentials.
    /// Local owner only.
    ShareStop,
    /// Add detected devices to the owner's session allowlist. Local owner only.
    ShareCheck(ShareDevicesParams),
    /// Remove devices from the allowlist and revoke remote access at once.
    /// Local owner only.
    ShareUncheck(ShareDevicesParams),
    /// The session's state, without secrets. Local owner only.
    ShareStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloParams {
    pub client: ClientInfo,
    pub protocol: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRef {
    pub device_id: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenParams {
    /// Device path, e.g. `/dev/ttyUSB0`.
    pub port: String,
    /// Baud rate. Required when opening; optional when joining an open device
    /// (then the active rate is used). A conflicting rate is rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baud: Option<u32>,
    /// Ask for (or keep) AI write control. Only meaningful for AI clients.
    #[serde(default)]
    pub request_control: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadParams {
    pub device_id: u64,
    /// How many recent lines to return. The core applies its default and max.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteParams {
    pub device_id: u64,
    /// Bytes to send exactly as supplied; no line ending is added.
    pub data_b64: String,
    /// Wait for device output after the write. Without it the call returns
    /// as soon as the bytes are queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitSpec>,
}

/// What a write waits for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitSpec {
    /// Device-output lines wanted after the write.
    pub lines: usize,
    /// Give up after this long. The core applies its default and max.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// One line of a server-to-client message.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Answer to a request, matched by id.
    Response { id: u64, result: ResponseData },
    /// Failure answer to a request, matched by id.
    Error { id: u64, error: ErrorBody },
    /// Unsolicited notification.
    Event { event: Event },
}

/// Successful result payloads, tagged by `kind`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ResponseData {
    Hello(HelloResult),
    Pong,
    Devices(DevicesResult),
    Opened(OpenResult),
    Closed,
    Read(ReadResult),
    Written(WriteResult),
    Status(DeviceStatus),
    ShareStarted(ShareSessionInfo),
    Sharing(SharingSnapshot),
    ShareStopped(ShareStopped),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelloResult {
    pub client_id: u64,
    pub protocol: u32,
    pub core_version: String,
    pub socket: String,
    pub limits: Limits,
}

/// Bounded defaults the core applies, reported so clients can document them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub read_default_lines: usize,
    pub read_max_lines: usize,
    pub write_default_timeout_ms: u64,
    pub write_max_timeout_ms: u64,
    pub control_idle_ms: u64,
    pub history_max_lines: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicesResult {
    pub devices: Vec<DeviceSummary>,
}

/// A device as seen by discovery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baud: Option<u32>,
    pub client_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_holder: Option<Holder>,
}

/// Who currently holds AI write control of a device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub client_id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: ClientKind,
}

impl Holder {
    /// Short label for error messages and events.
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) => format!("{name} (client {})", self.client_id),
            None => format!("client {} ({:?})", self.client_id, self.kind),
        }
    }
}

/// What an open caller got: controller, plain reader, or human writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Holds (or just acquired) AI write control.
    Controller,
    /// Attached, read-only for AI; can read history.
    Reader,
    /// Human client: reads and writes without holding control.
    Human,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<Holder>,
    /// Milliseconds until the holder's idle control expires, if it is held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenResult {
    pub device_id: u64,
    pub port: String,
    pub baud: u32,
    pub role: Role,
    pub control: ControlState,
}

/// Where one history line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Bytes received from the serial device.
    Device,
    /// Bytes written by an AI client.
    Ai,
    /// Bytes written by a human client.
    Human,
}

/// One line. `text` is lossy UTF-8: invalid bytes become U+FFFD in the text
/// view, while the core keeps the original bytes in its history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryLine {
    pub source: Source,
    pub text: String,
    /// True for a line with no newline yet (a prompt, or a write without one).
    pub partial: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadResult {
    pub lines: Vec<HistoryLine>,
    /// Older lines discarded by the bounded buffer.
    pub dropped_lines: u64,
    /// True when the response was trimmed to fit the IPC frame limit; request
    /// fewer lines to see the rest.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteResult {
    pub bytes_written: usize,
    /// Device-output lines seen after the write (plus a partial tail on timeout).
    pub lines: Vec<HistoryLine>,
    pub timed_out: bool,
    /// True when response lines were trimmed to fit the IPC frame limit.
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub device_id: u64,
    pub path: String,
    pub baud: u32,
    pub client_count: usize,
    pub control: ControlState,
    pub history_lines: usize,
    pub dropped_lines: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Malformed or out-of-order request.
    BadRequest,
    /// Unsupported protocol version.
    Unsupported,
    /// Unknown device id or path.
    NotFound,
    /// The device is open at a different baud rate.
    BaudConflict,
    /// The requesting AI does not hold write control.
    ControlHeld,
    /// This client is not attached to that device.
    NotAttached,
    /// The device was closed (last client left, or the TUI/core closed it).
    DeviceClosed,
    /// The device failed or was unplugged.
    DeviceFailed,
    /// The client is not allowed to do this.
    PermissionDenied,
    /// The request took too long.
    Timeout,
    /// The core hit an internal error.
    Internal,
    /// The core is going away.
    ShuttingDown,
    /// The peer reached a bounded resource or pending-work limit.
    LimitExceeded,
}

/// Why a holder lost control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlReleaseReason {
    /// Five minutes (by default) passed without a request from the holder.
    Idle,
    /// The holder's connection closed.
    Disconnected,
    /// The device closed.
    DeviceClosed,
}

/// Unsolicited notifications. Every connected client receives every event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// Raw bytes from the device, in arrival order. Clients that render a
    /// terminal feed these to an ANSI parser; line-based clients use `read`.
    DeviceOutput { device_id: u64, data_b64: String },
    /// A write by some client, echoed so others see what was sent.
    Write {
        device_id: u64,
        source: Source,
        data_b64: String,
        /// True when the echo was trimmed to fit a remote frame.
        #[serde(default)]
        truncated: bool,
    },
    /// A device was closed. It must be opened again to be used.
    DeviceClosed {
        device_id: u64,
        path: String,
        reason: String,
    },
    /// A client attached to a device.
    ClientJoined {
        device_id: u64,
        client: Holder,
        role: Role,
    },
    /// A client detached from a device.
    ClientLeft { device_id: u64, client: Holder },
    /// Control was granted to a client.
    ControlGranted { device_id: u64, holder: Holder },
    /// Control was released (idle expiry, disconnect, or device close).
    ControlReleased {
        device_id: u64,
        holder: Holder,
        reason: ControlReleaseReason,
    },
    /// A control request was answered read-only because someone else holds it.
    ControlDenied {
        device_id: u64,
        requester: Holder,
        holder: Holder,
    },
    /// The core is shutting down after the last client left.
    CoreShutdown { reason: String },
    /// Client-local: the connection ended. Never sent by the core.
    TransportClosed { reason: String },
}

impl Event {
    /// The device this event is about, if any. The core uses it to keep remote
    /// clients from seeing unchecked devices.
    pub fn device_id(&self) -> Option<u64> {
        match self {
            Self::DeviceOutput { device_id, .. }
            | Self::Write { device_id, .. }
            | Self::DeviceClosed { device_id, .. }
            | Self::ClientJoined { device_id, .. }
            | Self::ClientLeft { device_id, .. }
            | Self::ControlGranted { device_id, .. }
            | Self::ControlReleased { device_id, .. }
            | Self::ControlDenied { device_id, .. } => Some(*device_id),
            Self::CoreShutdown { .. } | Self::TransportClosed { .. } => None,
        }
    }

    pub fn device_output(device_id: u64, data: &[u8]) -> Self {
        Self::DeviceOutput {
            device_id,
            data_b64: encode_bytes(data),
        }
    }

    pub fn write(device_id: u64, source: Source, data: &[u8]) -> Self {
        Self::Write {
            device_id,
            source,
            data_b64: encode_bytes(data),
            truncated: false,
        }
    }

    /// Decode the bytes carried by `DeviceOutput` or `Write`.
    pub fn bytes(&self) -> Option<Result<Vec<u8>, ProtocolError>> {
        match self {
            Self::DeviceOutput { data_b64, .. } | Self::Write { data_b64, .. } => {
                Some(decode_bytes(data_b64))
            }
            _ => None,
        }
    }
}

/// Read one newline-terminated line without buffering more than `max` bytes.
///
/// `read_until` would grow an unbounded buffer before a newline arrives; this
/// stops as soon as the limit is crossed. Returns `Ok(None)` on EOF.
pub fn read_line<R: std::io::BufRead>(
    reader: &mut R,
    max: usize,
) -> std::io::Result<Option<Vec<u8>>> {
    let mut buffer = Vec::new();
    loop {
        let (take, found_newline, eof) = {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                (0, false, true)
            } else {
                let newline = available.iter().position(|byte| *byte == b'\n');
                let take = newline
                    .map(|position| position + 1)
                    .unwrap_or(available.len());
                buffer.extend_from_slice(&available[..take]);
                (take, newline.is_some(), false)
            }
        };
        if eof {
            return if buffer.is_empty() {
                Ok(None)
            } else {
                Ok(Some(buffer))
            };
        }
        reader.consume(take);
        if buffer.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("message exceeds {max} bytes without a newline"),
            ));
        }
        if found_newline {
            return Ok(Some(buffer));
        }
    }
}

/// Serialize one message as a newline-terminated JSON line.
pub fn encode_line<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtocolError> {
    let mut line = serde_json::to_vec(message)?;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::TooLarge {
            size: line.len(),
            max: MAX_MESSAGE_BYTES,
        });
    }
    line.push(b'\n');
    Ok(line)
}

/// Decode one JSON line (as returned by `read_until(b'\n')`, with or without
/// its trailing newline).
pub fn decode_line<T: for<'de> Deserialize<'de>>(line: &[u8]) -> Result<T, ProtocolError> {
    let line = match line.last() {
        Some(b'\n') => &line[..line.len() - 1],
        _ => line,
    };
    if line.is_empty() {
        return Err(ProtocolError::Empty);
    }
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(ProtocolError::TooLarge {
            size: line.len(),
            max: MAX_MESSAGE_BYTES,
        });
    }
    Ok(serde_json::from_slice(line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_as_one_line() {
        let msg = RequestMessage {
            id: 7,
            request: Request::Write(WriteParams {
                device_id: 3,
                data_b64: encode_bytes(b"AT\r"),
                wait: Some(WaitSpec {
                    lines: 2,
                    timeout_ms: Some(500),
                }),
            }),
        };
        let line = encode_line(&msg).unwrap();
        assert!(line.ends_with(b"\n"));
        let back: RequestMessage = decode_line(&line).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.request, msg.request);
    }

    #[test]
    fn unit_requests_have_no_params() {
        let line = encode_line(&RequestMessage {
            id: 1,
            request: Request::Ping,
        })
        .unwrap();
        let text = std::str::from_utf8(&line).unwrap();
        assert_eq!(text.trim(), r#"{"id":1,"method":"ping"}"#);
    }

    #[test]
    fn events_round_trip_bytes() {
        let event = Event::device_output(9, &[0xff, 0x00, b'a']);
        let back: Event = decode_line(&encode_line(&ServerMessage::Event { event }).unwrap())
            .map(|m| match m {
                ServerMessage::Event { event } => event,
                _ => panic!("wrong variant"),
            })
            .unwrap();
        assert_eq!(back.bytes().unwrap().unwrap(), vec![0xff, 0x00, b'a']);
    }

    #[test]
    fn unfinished_frames_are_bounded() {
        let mut input = std::io::Cursor::new(vec![b'x'; 4096]);
        let error = read_line(&mut input, 1024).expect_err("over the limit");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn bounded_reader_returns_lines_and_eof() {
        let mut input = std::io::Cursor::new(b"one\ntwo".to_vec());
        assert_eq!(read_line(&mut input, 1024).unwrap().unwrap(), b"one\n");
        assert_eq!(read_line(&mut input, 1024).unwrap().unwrap(), b"two");
        assert!(read_line(&mut input, 1024).unwrap().is_none());
    }

    #[test]
    fn oversized_lines_are_refused() {
        let huge = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        assert!(matches!(
            decode_line::<ServerMessage>(&huge),
            Err(ProtocolError::TooLarge { .. })
        ));
    }

    #[test]
    fn empty_lines_are_refused() {
        assert!(matches!(
            decode_line::<ServerMessage>(b"\n"),
            Err(ProtocolError::Empty)
        ));
    }
}

/// Where the core's socket lives unless a client or the core is told otherwise.
///
/// Order: `$SERIAL_NEXUS_SOCKET`, then `$XDG_RUNTIME_DIR/serial-nexus/core.sock`,
/// then `$TMPDIR/serial-nexus-<uid>/core.sock`. The directory is private to the
/// user (0700) and the socket is 0600, so only same-user clients can connect.
pub mod paths {
    use std::path::{Path, PathBuf};

    pub const SOCKET_ENV: &str = "SERIAL_NEXUS_SOCKET";

    /// Effective user id, used to keep one core and one socket per user.
    pub fn effective_uid() -> u32 {
        // SAFETY: geteuid has no preconditions and is always safe to call.
        unsafe { libc::geteuid() }
    }

    /// The directory holding the core lock file and socket.
    pub fn default_socket_dir() -> PathBuf {
        socket_dir(
            std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
            &std::env::temp_dir(),
            effective_uid(),
        )
    }

    /// The pure form of [`default_socket_dir`]: the inputs are explicit so the
    /// naming can be tested without touching the environment.
    fn socket_dir(xdg_runtime: Option<PathBuf>, temp_dir: &Path, uid: u32) -> PathBuf {
        match xdg_runtime.filter(|dir| !dir.as_os_str().is_empty()) {
            Some(dir) => dir.join("serial-nexus"),
            None => temp_dir.join(format!("serial-nexus-{uid}")),
        }
    }

    /// Default socket path. Set [`SOCKET_ENV`] to override it.
    pub fn default_socket_path() -> PathBuf {
        if let Some(path) = std::env::var_os(SOCKET_ENV) {
            let path = PathBuf::from(path);
            if !path.as_os_str().is_empty() {
                return path;
            }
        }
        default_socket_dir().join("core.sock")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_socket_directory_is_named_after_the_product() {
            assert_eq!(
                socket_dir(
                    Some(PathBuf::from("/run/user/1000")),
                    Path::new("/tmp"),
                    1000
                ),
                PathBuf::from("/run/user/1000/serial-nexus")
            );
            assert_eq!(
                socket_dir(None, Path::new("/tmp"), 1000),
                PathBuf::from("/tmp/serial-nexus-1000")
            );
            // An empty XDG_RUNTIME_DIR falls through to the per-user temp dir,
            // as `default_socket_dir` documents.
            assert_eq!(
                socket_dir(Some(PathBuf::new()), Path::new("/tmp"), 7),
                PathBuf::from("/tmp/serial-nexus-7")
            );
        }

        #[test]
        fn the_socket_environment_variable_has_the_agreed_name() {
            assert_eq!(SOCKET_ENV, "SERIAL_NEXUS_SOCKET");
        }
    }
}
