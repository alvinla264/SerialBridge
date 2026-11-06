//! A reusable Rust client for `serial-core`.
//!
//! Both SerialBridge clients (the AI's MCP server and the human terminal) use
//! this crate. It hides the Unix socket, request ids, framing, connection
//! lifecycle and core auto-start behind one small thread-safe type:
//!
//! ```no_run
//! use serialbridge_client::{Client, ClientConfig, WaitSpec};
//! use serialbridge_protocol::ClientInfo;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Starts serial-core if it is not running, then says hello.
//! let client = Client::connect(ClientConfig::new(ClientInfo::ai("my-agent")))?;
//!
//! for device in client.list_devices()?.devices {
//!     println!("{} open={} baud={:?}", device.path, device.open, device.baud);
//! }
//!
//! let opened = client.open("/dev/ttyUSB0", Some(115_200), true)?;
//! println!("open at {} baud with role {:?}", opened.baud, opened.role);
//!
//! // Wait up to the default (1s) for one device line after the write.
//! let written = client.write_bytes(
//!     opened.device_id,
//!     b"version\r",
//!     Some(WaitSpec { lines: 1, timeout_ms: Some(2_000) }),
//! )?;
//! for line in written.lines {
//!     println!("{:?}: {}", line.source, line.text);
//! }
//!
//! // Live updates, e.g. for a terminal view.
//! let events = client.subscribe();
//! # let _ = events;
//! client.close(opened.device_id)?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Lifecycle and auto-start
//!
//! `Client::connect` first tries the socket. If no core answers and auto-start
//! is enabled, it finds the `serial-core` executable, starts it detached, and
//! polls until the socket accepts. The core locks its socket directory, so two
//! clients racing to start it still end up with one core. Clients that ship the
//! core elsewhere (packaging, tests) set [`ClientConfig::core_binary`] or the
//! `SERIALBRIDGE_CORE` environment variable to an explicit path.
//!
//! The core closes a serial device when its last client disconnects, and the
//! core itself exits 30 seconds (by default) after its last client leaves.
//! Dropping a [`Client`] closes its connection.
//!
//! ## Write control
//!
//! An AI client opens a device with `request_control = true` to get write
//! control. If another AI holds it, the call succeeds read-only and the result
//! says who holds it. Writing without control fails with
//! [`ErrorCode::ControlHeld`](serialbridge_protocol::ErrorCode::ControlHeld).
//! Control expires after five minutes without a request from its holder; the
//! holder calls `open` again (same arguments) to reclaim it. Human clients
//! always write without taking control.

mod autostart;
mod client;
mod error;

pub use autostart::{find_core_binary, spawn_core, CORE_ENV};
pub use client::{Client, ClientConfig};
pub use error::ClientError;
pub use serialbridge_protocol::{
    paths, ClientInfo, ClientKind, ControlReleaseReason, ControlState, DeviceStatus, DeviceSummary,
    ErrorBody, ErrorCode, Event, HistoryLine, Holder, Limits, OpenResult, ReadResult, Role, Source,
    WaitSpec, WriteResult, PROTOCOL_VERSION,
};
