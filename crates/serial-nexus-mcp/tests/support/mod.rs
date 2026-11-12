//! Test helpers: a simulated serial device (a PTY pair), a core on a temp
//! socket, an MCP client that speaks real stdio framing to the built
//! `serial-nexus-mcp` binary, and small polling utilities.
//!
//! No physical hardware is involved.

#![allow(dead_code)]

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::openpty;
use serde_json::{json, Value};
use tempfile::TempDir;

/// A PTY pair: the core opens the slave path as a serial port, the test drives
/// the master side.
pub struct SimulatedDevice {
    pub path: String,
    master: File,
    // Keep the slave open so the PTY stays alive for the whole test.
    _slave: File,
}

impl SimulatedDevice {
    pub fn new() -> Self {
        let pty = openpty(None, None).expect("openpty");
        let master: File = pty.master.into();
        let slave: File = pty.slave.into();
        let link =
            std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd())).expect("slave path");
        fcntl(master.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).expect("nonblocking");
        Self {
            path: link.to_string_lossy().into_owned(),
            master,
            _slave: slave,
        }
    }

    /// A second handle to the master side, for writing while a tool call blocks.
    pub fn master_handle(&self) -> File {
        self.master.try_clone().expect("clone master")
    }

    /// Bytes the core wrote to the device.
    pub fn write(&mut self, data: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut remaining = data;
        while !remaining.is_empty() {
            match self.master.write(remaining) {
                Ok(written) => remaining = &remaining[written..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        panic!("master write timed out");
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("master write failed: {error}"),
            }
        }
        self.master.flush().ok();
    }

    /// Read exactly `count` bytes from the master side, else panic.
    pub fn read_n(&mut self, count: usize, timeout: Duration) -> Vec<u8> {
        read_exactly(&mut self.master, count, timeout)
    }

    /// Everything the master side receives within `window`.
    pub fn read_for(&mut self, window: Duration) -> Vec<u8> {
        read_for(&mut self.master, window)
    }
}

fn read_exactly(master: &mut File, count: usize, timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    let mut got = Vec::new();
    loop {
        let mut buffer = [0u8; 512];
        match master.read(&mut buffer) {
            Ok(0) => {}
            Ok(read) => {
                got.extend_from_slice(&buffer[..read]);
                if got.len() >= count {
                    got.truncate(count);
                    return got;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("master read failed: {error} (got {} bytes)", got.len()),
        }
        if Instant::now() >= deadline {
            panic!("did not read {count} bytes; got {} bytes", got.len());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn read_for(master: &mut File, window: Duration) -> Vec<u8> {
    let deadline = Instant::now() + window;
    let mut got = Vec::new();
    loop {
        let mut buffer = [0u8; 512];
        match master.read(&mut buffer) {
            Ok(0) => {}
            Ok(read) => got.extend_from_slice(&buffer[..read]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            return got;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    got
}

/// Start a core in a background thread on a fresh temp socket.
pub fn start_core(control_idle: Duration) -> (TempDir, PathBuf) {
    let directory = TempDir::new().expect("tempdir");
    let socket = directory.path().join("core.sock");
    let config = serial_nexus_core::CoreConfig {
        socket: socket.clone(),
        grace: Duration::from_secs(5),
        control_idle,
        history_max_lines: 2000,
        history_max_bytes: 256 * 1024,
        ..serial_nexus_core::CoreConfig::default()
    };
    std::thread::spawn(move || {
        if let Err(error) = serial_nexus_core::server::run(config) {
            eprintln!("test core failed: {error}");
        }
    });
    wait_until(Duration::from_secs(5), || {
        std::os::unix::net::UnixStream::connect(&socket).is_ok()
    });
    (directory, socket)
}

pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not met within {timeout:?}");
}

/// One MCP server process, spoken to over its real stdin/stdout framing.
pub struct McpSession {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    next_id: u64,
    pending: Vec<Value>,
    stderr: Arc<Mutex<String>>,
}

impl McpSession {
    /// Spawn `serial-nexus-mcp` against an already-running core.
    pub fn spawn(socket: &Path) -> Self {
        Self::spawn_with_env(socket, &[])
    }

    /// Spawn `serial-nexus-mcp`; `extra_env` is added to the child's environment.
    pub fn spawn_with_env(socket: &Path, extra_env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_serial-nexus-mcp"));
        command
            .env("SERIAL_NEXUS_SOCKET", socket)
            .env_remove("SERIAL_NEXUS_CORE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn serial-nexus-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");

        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(text) {
                    Ok(value) => {
                        if tx.send(value).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        eprintln!("serial-nexus-mcp wrote a non-JSON line: {text} ({error})")
                    }
                }
            }
        });

        let stderr_buffer = Arc::new(Mutex::new(String::new()));
        {
            let buffer = Arc::clone(&stderr_buffer);
            std::thread::spawn(move || {
                // Publish stderr as it arrives, not only at exit: a secrecy
                // assertion made while the server is still running must be
                // able to see what it has already written.
                let mut reader = stderr;
                let mut chunk = [0u8; 4096];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => {
                            if let Ok(mut text) = buffer.lock() {
                                text.push_str(&String::from_utf8_lossy(&chunk[..read]));
                            }
                        }
                    }
                }
            });
        }

        let mut session = Self {
            child,
            stdin: Some(stdin),
            lines: rx,
            next_id: 1,
            pending: Vec::new(),
            stderr: stderr_buffer,
        };
        session.initialize();
        session
    }

    fn initialize(&mut self) {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "serial-nexus-mcp-tests", "version": "0" }
            }),
        );
        assert!(
            response.get("result").is_some(),
            "initialize failed: {response}"
        );
        self.notify("notifications/initialized", json!({}));
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write notification");
        stdin.flush().expect("flush notification");
    }

    /// Send a raw line (used to check framing and protocol errors).
    pub fn send_raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write raw");
        stdin.flush().expect("flush raw");
    }

    /// One JSON-RPC request, returning the whole response object.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.start_request(method, params);
        self.response_for(&id)
    }

    /// Write a request without waiting, returning its id (for pipelining).
    pub fn start_request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.send_raw(&line.to_string());
        json!(id)
    }

    /// The response object for `id`, waiting up to 60 seconds.
    pub fn response_for(&mut self, id: &Value) -> Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(position) = self
                .pending
                .iter()
                .position(|value| value.get("id") == Some(id))
            {
                return self.pending.remove(position);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "no response for id {id} within 60s");
            match self.lines.recv_timeout(left) {
                Ok(value) => {
                    if value.get("id") == Some(id) {
                        return value;
                    }
                    self.pending.push(value);
                }
                Err(RecvTimeoutError::Timeout) => panic!("no response for id {id} within 60s"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!(
                        "serial-nexus-mcp closed stdout (stderr: {})",
                        self.stderr_text()
                    )
                }
            }
        }
    }

    /// The next server message, for protocol-level assertions.
    pub fn next_message(&mut self) -> Value {
        self.lines
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("no server message (stderr: {})", self.stderr_text()))
    }

    pub fn tools(&mut self) -> Vec<Value> {
        let response = self.request("tools/list", json!({}));
        response["result"]["tools"]
            .as_array()
            .unwrap_or_else(|| panic!("tools/list failed: {response}"))
            .clone()
    }

    pub fn call_tool(&mut self, name: &str, arguments: Value) -> ToolReply {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(
            response.get("error").is_none(),
            "tools/call {name} was a protocol error: {response}"
        );
        let result = &response["result"];
        ToolReply {
            is_error: result["isError"].as_bool().unwrap_or(false),
            text: result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            structured: result.get("structuredContent").cloned(),
        }
    }

    /// The server process id, for tests that act on it (e.g. its stderr).
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    return;
                }
            }
        }
    }
}

pub struct ToolReply {
    pub is_error: bool,
    pub text: String,
    pub structured: Option<Value>,
}

impl ToolReply {
    pub fn json(&self) -> &Value {
        self.structured
            .as_ref()
            .unwrap_or_else(|| panic!("tool reply has no structuredContent: {}", self.text))
    }

    pub fn device_id(&self) -> u64 {
        self.json()["device_id"]
            .as_u64()
            .unwrap_or_else(|| panic!("no device_id in {} / {}", self.text, self.json()))
    }
}
