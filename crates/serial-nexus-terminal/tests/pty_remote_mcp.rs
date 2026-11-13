//! The real `serial-nexus-mcp` binary reaching a checked device on a remote
//! host through the real foreground terminal's gateway: two network clients
//! (an AI MCP session and a second human) on one shared device, secrecy of the
//! pairing credential, revocation on uncheck, and no silent fallback when the
//! gateway or the host's session goes away.
//!
//! The host core starts sharing with a detected device that was never opened
//! locally, so the remote first open chooses the baud rate. Everything below
//! the MCP server is a real process or a real TLS connection; only the serial
//! endpoint is a simulated PTY.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use serial_nexus_client::{Client, ClientInfo, RemoteClient, ShareBind};
use serial_nexus_protocol::ShareSessionInfo;
use support::tui::TuiSession;
use support::{start_core, wait_until, PortListerGuard, SimulatedDevice};
use tempfile::TempDir;

/// The workspace builds every binary before its integration tests run, so the
/// terminal tests can drive the MCP binary at the same target directory.
fn mcp_binary() -> PathBuf {
    let test_binary = std::env::current_exe().expect("current test executable");
    let target = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("target directory");
    let binary = target.join("serial-nexus-mcp");
    assert!(
        binary.exists(),
        "{} is missing: run cargo test --workspace so the MCP binary is built",
        binary.display()
    );
    binary
}

/// One real MCP server process, spoken to over its stdio framing.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    next_id: u64,
}

impl Mcp {
    fn spawn(env: &[(&str, &str)]) -> Self {
        let mut command = Command::new(mcp_binary());
        command
            .env_remove("SERIAL_NEXUS_CORE")
            .env_remove("SERIAL_NEXUS_SOCKET")
            .env_remove("SERIAL_NEXUS_GATEWAY_SOCKET")
            .env_remove("SERIAL_NEXUS_MCP_PEERS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in env {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("spawn serial-nexus-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout_pipe = child.stdout.take().expect("stdout");
        let stderr_pipe = child.stderr.take().expect("stderr");

        let (tx, rx) = channel();
        let seen_stdout = Arc::new(Mutex::new(String::new()));
        {
            let seen = Arc::clone(&seen_stdout);
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stdout_pipe);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    if let Ok(mut text) = seen.lock() {
                        text.push_str(&line);
                    }
                    let text = line.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(text) {
                        if tx.send(value).is_err() {
                            return;
                        }
                    }
                }
            });
        }
        let seen_stderr = Arc::new(Mutex::new(String::new()));
        {
            let seen = Arc::clone(&seen_stderr);
            std::thread::spawn(move || {
                let mut reader = stderr_pipe;
                let mut chunk = [0u8; 4096];
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => {
                            if let Ok(mut text) = seen.lock() {
                                text.push_str(&String::from_utf8_lossy(&chunk[..read]));
                            }
                        }
                    }
                }
            });
        }

        let mut mcp = Self {
            child,
            stdin: Some(stdin),
            lines: rx,
            stdout: seen_stdout,
            stderr: seen_stderr,
            next_id: 1,
        };
        mcp.initialize();
        mcp
    }

    fn initialize(&mut self) {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "remote-gateway-e2e", "version": "0" }
            }),
        );
        assert!(response.get("result").is_some(), "initialize: {response}");
        self.notify("notifications/initialized", json!({}));
    }

    fn notify(&mut self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write notification");
        stdin.flush().expect("flush notification");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write request");
        stdin.flush().expect("flush request");
        loop {
            let message = self
                .lines
                .recv_timeout(Duration::from_secs(20))
                .unwrap_or_else(|_| {
                    panic!(
                        "no answer to {method}; stdout: {}; stderr: {}",
                        self.stdout_text(),
                        self.stderr_text()
                    )
                });
            if message.get("id").and_then(Value::as_u64) == Some(id) {
                return message;
            }
        }
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> (bool, String, Option<Value>) {
        let response = self.request("tools/call", json!({"name": name, "arguments": arguments}));
        assert!(
            response.get("error").is_none(),
            "tools/call {name} was a protocol error: {response}"
        );
        let result = &response["result"];
        (
            result["isError"].as_bool().unwrap_or(false),
            result["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            result.get("structuredContent").cloned(),
        )
    }

    fn stdout_text(&self) -> String {
        self.stdout
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|text| text.clone())
            .unwrap_or_default()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
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

fn remote_args(info: &ShareSessionInfo, extra: Value) -> Value {
    let mut args = match extra {
        Value::Object(map) => Value::Object(map),
        other => panic!("expected object arguments, got {other}"),
    };
    args["ip"] = json!(info.host.address.ip().to_string());
    args["port"] = json!(info.host.address.port());
    args
}

fn write_peers(directory: &Path, info: &ShareSessionInfo) -> PathBuf {
    let path = directory.join("mcp-peers.json");
    let peers = json!({
        "version": 1,
        "peers": [{
            "endpoint": {
                "address": info.host.address.ip().to_string(),
                "port": info.host.address.port(),
            },
            "paired": info.host,
            "secret": { "role": "ai", "credential": info.ai.credential },
        }],
    });
    std::fs::write(&path, serde_json::to_vec(&peers).expect("peers json")).expect("write peers");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("private peers file");
    path
}

#[test]
fn the_real_mcp_binary_reaches_a_remote_host_through_the_real_terminal_gateway() {
    // The host's core, its detected (never opened) device and the owner.
    let (host_core, host_socket) =
        start_core(Duration::from_secs(30), Duration::from_secs(300), 500);
    let mut device = SimulatedDevice::new();
    let _lister = PortListerGuard::from_path(device.path.clone());
    let owner = Client::connect_to_socket(&host_socket, ClientInfo::human("host-terminal"))
        .expect("owner connects");
    let info = owner
        .start_sharing(
            ShareBind::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            vec![device.path.clone()],
        )
        .expect("sharing starts for the detected device");

    // A local core and the real foreground terminal, which hosts the gateway.
    let (_local_core, local_socket) =
        start_core(Duration::from_secs(30), Duration::from_secs(300), 500);
    let xdg = TempDir::new().expect("xdg");
    let mut terminal = TuiSession::spawn(&local_socket, xdg.path(), 30, 110);
    terminal.wait_for_text("focus: sidebar");
    let gateway = local_socket.with_file_name("gateway.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !gateway.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        gateway.exists(),
        "no gateway at {}; terminal screen:\n{}",
        gateway.display(),
        terminal.text()
    );

    let peers_directory = TempDir::new().expect("peers directory");
    let peers = write_peers(peers_directory.path(), &info);
    let peers = peers.to_string_lossy().into_owned();
    let gateway_text = gateway.to_string_lossy().into_owned();
    let mut mcp = Mcp::spawn(&[
        ("SERIAL_NEXUS_GATEWAY_SOCKET", &gateway_text),
        ("SERIAL_NEXUS_MCP_PEERS", &peers),
        ("HOME", &xdg.path().to_string_lossy()),
        ("XDG_CONFIG_HOME", &xdg.path().to_string_lossy()),
    ]);

    // The checked device is discoverable on the remote host without a local
    // open, and the reply names that endpoint.
    let (is_error, text, structured) = mcp.call_tool("list_devices", remote_args(&info, json!({})));
    assert!(!is_error, "{text}");
    assert!(text.contains(&device.path), "{text}");
    assert!(
        text.contains(&format!("remote {}", info.host.address)),
        "{text}"
    );
    let structured = structured.expect("structured content");
    assert_eq!(
        structured["remote_endpoint"]["port"],
        json!(info.host.address.port())
    );

    // The first remote open chooses the baud; later clients join it.
    let (is_error, text, structured) = mcp.call_tool(
        "connect",
        remote_args(
            &info,
            json!({ "path": device.path, "baud": 115_200, "request_control": true }),
        ),
    );
    assert!(!is_error, "{text}");
    let device_id = structured.expect("structured connect")["device_id"]
        .as_u64()
        .expect("device id");

    let (is_error, text, _) = mcp.call_tool(
        "send",
        remote_args(
            &info,
            json!({ "device": device_id, "text": "ai-through-gateway", "lines": 0 }),
        ),
    );
    assert!(!is_error, "{text}");

    // A second human pairs over TLS and joins the same open device at the
    // rate the first open chose.
    let human = RemoteClient::connect(
        &info.host,
        &info.human,
        Some("second-human".into()),
        Duration::from_secs(5),
    )
    .expect("human pairs");
    let human_open = human
        .open(&device.path, None, false)
        .expect("human joins the open device");
    assert_eq!(
        human_open.baud, 115_200,
        "a later client joins at the rate the first open chose"
    );
    // Several humans may pair the same credential; each is its own network
    // client and its writes stay whole beside the AI's.
    let second_human = RemoteClient::connect(
        &info.host,
        &info.human,
        Some("third-human".into()),
        Duration::from_secs(5),
    )
    .expect("a second human pairs the same credential");
    let second_open = second_human
        .open(&device.path, None, false)
        .expect("the second human joins");
    let ai_text = "A".repeat(1_500);
    let human_text = "B".repeat(1_500);
    let second_text = "C".repeat(1_500);
    {
        let line = json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {
                "name": "send",
                "arguments": remote_args(&info, json!({ "device": device_id, "text": ai_text, "lines": 0 })),
            },
        });
        let stdin = mcp.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write concurrent send");
        stdin.flush().expect("flush concurrent send");
    }
    let human_write = {
        let human = human.clone();
        let text = human_text.clone();
        std::thread::spawn(move || human.write_bytes(human_open.device_id, text.as_bytes(), None))
    };
    let second_write = {
        let human = second_human.clone();
        let text = second_text.clone();
        std::thread::spawn(move || human.write_bytes(second_open.device_id, text.as_bytes(), None))
    };
    let mut answer = None;
    while answer.is_none() {
        let message = mcp
            .lines
            .recv_timeout(Duration::from_secs(20))
            .expect("answer to the concurrent send");
        if message.get("id").and_then(Value::as_u64) == Some(99) {
            answer = Some(message);
        }
    }
    let answer = answer.expect("answer");
    let result = &answer["result"];
    assert!(!result["isError"].as_bool().unwrap_or(false), "{result}");
    human_write
        .join()
        .expect("human write thread")
        .expect("human write");
    second_write
        .join()
        .expect("second human write thread")
        .expect("second human write");

    // Every whole request reached the one device exactly once, and the device
    // stream never carried either pairing credential.
    let mut stream = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let has_every_request = |stream: &[u8]| {
        [&ai_text, &human_text, &second_text]
            .into_iter()
            .all(|text| {
                stream
                    .windows(text.len())
                    .any(|window| window == text.as_bytes())
            })
    };
    while !has_every_request(&stream) && Instant::now() < deadline {
        stream.extend(device.try_read());
        std::thread::sleep(Duration::from_millis(5));
    }
    let stream = String::from_utf8_lossy(&stream).into_owned();
    assert_eq!(
        stream.matches(&ai_text).count(),
        1,
        "AI request stays whole"
    );
    assert_eq!(
        stream.matches(&human_text).count(),
        1,
        "human request stays whole"
    );
    assert_eq!(
        stream.matches(&second_text).count(),
        1,
        "second human request stays whole"
    );
    assert!(!stream.contains(&info.ai.credential));
    assert!(!stream.contains(&info.human.credential));

    // Unchecking revokes the remote device at once. The same MCP process
    // still reaches the local core, so the failure is not a silent fallback.
    owner
        .uncheck_shared_devices(vec![device.path.clone()])
        .expect("uncheck");
    let revoked = human
        .read(human_open.device_id, Some(5))
        .expect_err("the detached human loses the device at once");
    assert!(
        !format!("{revoked:?}").contains(&info.human.credential),
        "a revocation error never echoes the credential"
    );
    assert!(
        human.write_bytes(human_open.device_id, b"x", None).is_err(),
        "a revoked device refuses writes"
    );
    let (_, text, _) = mcp.call_tool("list_devices", remote_args(&info, json!({})));
    assert!(
        !text.contains(&device.path),
        "an unchecked device must not be discoverable: {text}"
    );
    let (is_error, text, _) = mcp.call_tool(
        "connect",
        remote_args(&info, json!({ "path": device.path, "baud": 115_200 })),
    );
    assert!(
        is_error,
        "an unchecked device must not be openable through a crafted request: {text}"
    );
    let (is_error, text, _) = mcp.call_tool("list_devices", json!({}));
    assert!(!is_error, "local calls keep working: {text}");
    assert!(
        text.contains(&device.path),
        "the local core still lists it: {text}"
    );

    // The credential never appeared in the server's output or diagnostics.
    let output = mcp.stdout_text();
    let diagnostics = mcp.stderr_text();
    for secret in [
        &info.ai.credential,
        &info.human.credential,
        &info.host.ca_cert_der_b64,
    ] {
        assert!(
            !output.contains(secret),
            "MCP stdout leaked a pairing value"
        );
        assert!(
            !diagnostics.contains(secret),
            "MCP stderr leaked a pairing value"
        );
    }

    // Restarting sharing on the host rotates every session secret, so the old
    // private pairing file is refused rather than silently reaching a new
    // session.
    owner.stop_sharing().expect("sharing stops");
    wait_until(Duration::from_secs(5), || !human.is_connected());
    let rotated = owner
        .start_sharing(
            ShareBind::new(info.bind.ip(), info.bind.port()),
            vec![device.path.clone()],
        )
        .expect("sharing restarts");
    assert_eq!(rotated.bind, info.bind);
    assert_ne!(rotated.session_id, info.session_id);
    assert_ne!(rotated.ai.credential, info.ai.credential);
    assert_ne!(rotated.host.ca_cert_der_b64, info.host.ca_cert_der_b64);
    let (is_error, text, _) = mcp.call_tool("list_devices", remote_args(&info, json!({})));
    assert!(
        is_error,
        "a rotated session must refuse the stale pairing, not reach it: {text}"
    );
    assert!(
        !text.contains(&info.ai.credential) && !text.contains(&info.human.credential),
        "a stale pairing must not echo its credential: {text}"
    );

    // When the foreground terminal leaves, its gateway goes with it; MCP has
    // no core or direct-dial fallback.
    terminal.quit();
    let (is_error, text, _) = mcp.call_tool("list_devices", json!({}));
    assert!(
        is_error,
        "MCP must fail without the terminal gateway instead of starting a core: {text}"
    );

    drop(host_core);
}
