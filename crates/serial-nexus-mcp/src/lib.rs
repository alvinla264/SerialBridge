//! `serial-nexus-mcp`: the AI's client of SerialNexus, exposed to an MCP host over
//! stdio.
//!
//! The server is launched by the AI host. It speaks newline-delimited
//! JSON-RPC 2.0 on stdin/stdout and routes every local or explicit remote
//! endpoint through the running foreground terminal's bounded, versioned
//! per-user gateway. MCP never auto-starts the core or dials remote hosts.
//! Each MCP session/endpoint gets its own upstream client; gateway exit closes
//! local and outgoing remote sessions. The gateway has no sharing controls.
//!
//! Remote pairing credentials stay in the laptop user's private pairing file;
//! only its path and the requested endpoint cross local IPC, never a
//! credential. Sharing is opt-in and local-only MCP operations do not require
//! it. Same-user programs are trusted and can impersonate the terminal or
//! enable sharing; the gateway is a supported routing/lifetime rule, not a
//! same-UID security barrier.
//!
//! ```text
//! AI host <-- MCP stdio --> serial-nexus-mcp <-- bounded IPC --> terminal gateway
//!                                                      <-- core socket / verified TLS --> upstream
//! ```

pub mod jsonrpc;
pub mod server;
pub mod tools;

pub use server::serve;
pub use tools::Session;
