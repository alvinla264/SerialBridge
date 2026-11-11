//! `serial-nexus-mcp`: the AI's client of SerialNexus, exposed to an MCP host over
//! stdio.
//!
//! The server is launched by the AI host. It speaks newline-delimited
//! JSON-RPC 2.0 on stdin/stdout and connects to the one local `serial-nexus-core`
//! through `serial-nexus-client` (starting it when nothing answers). All
//! device rules - one AI writer per device, baud conflicts, history, partial
//! lines, waited writes, idle expiry - belong to the core; this crate only
//! offers `list_devices`, `connect`, `disconnect`, `read` and `send`.
//!
//! A tool call may name an `ip` and `port` instead of using the local core, in
//! which case it reaches a device another host shares. The credential for that
//! endpoint comes from the human's private pairing file and never travels
//! through tool arguments or results.
//!
//! ```text
//! AI host  <-- MCP over stdio -->  serial-nexus-mcp  <-- Unix socket -->  serial-nexus-core
//! ```

pub mod jsonrpc;
pub mod server;
pub mod tools;

pub use server::serve;
pub use tools::Session;
