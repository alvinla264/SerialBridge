//! `serial-core`: the SerialBridge core.
//!
//! One core runs per local user and owns every open serial device. Clients
//! (the AI's MCP server and the human terminal) connect over a private Unix
//! socket and use the shared API in `serialbridge-client`.
//!
//! Modules:
//! - [`config`]: bounded defaults and environment/argument overrides;
//! - [`device`]: serial I/O threads (8N1, no flow control, configurable baud);
//! - [`engine`]: devices, clients, write control, history and timers;
//! - [`enumerate`]: serial device discovery without libudev;
//! - [`history`]: bounded in-memory line history;
//! - [`server`]: the socket server and its one-core-per-user lock.

pub mod config;
pub mod device;
pub mod engine;
pub mod enumerate;
pub mod history;
pub mod server;

pub use config::CoreConfig;
