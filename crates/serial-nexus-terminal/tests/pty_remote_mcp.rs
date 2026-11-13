//! The real `serial-nexus-mcp` binary reaching a checked device on a remote
//! host through the real foreground terminal's gateway: three network clients
//! on one shared device, secrecy of every pairing value, revocation on uncheck,
//! an interrupted write that is reported uncertain and never replayed, rotated
//! credentials, and no silent fallback when the gateway or the host's session
//! goes away.
//!
//! The host core starts sharing with a detected device that was never opened
//! locally, so the first remote open chooses the baud rate. Everything below
//! the MCP server is a real process or a real TLS connection; only the serial
//! endpoint is a simulated PTY.

mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use serial_nexus_client::{Client, ClientInfo, RemoteClient, ShareBind};
use serial_nexus_protocol::ShareSessionInfo;
use std::sync::MutexGuard;
use support::tui::TuiSession;
use support::{start_core, wait_until, PortListerGuard, SimulatedDevice};
use tempfile::TempDir;

static SERIAL_TESTS: Mutex<()> = Mutex::new(());

/// The injected discovery lister is process-wide, so the tests here take
/// turns rather than publish two different device lists.
fn serial_test() -> MutexGuard<'static, ()> {
    SERIAL_TESTS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Find the MCP binary Cargo built beside this test executable.
///
/// Cargo does not always place an integration-test executable directly under
/// `target/debug/deps`: with a restored build cache it can run from
/// `target/debug/build/<package>/<hash>/out`. Walk up from the test executable
/// to the first directory that holds the MCP binary, which a workspace test
/// run puts at `target/debug/serial-nexus-mcp`.
fn find_binary_from(start: &Path) -> Option<PathBuf> {
    start.ancestors().skip(1).find_map(|directory| {
        let candidate = directory.join("serial-nexus-mcp");
        candidate.is_file().then_some(candidate)
    })
}

fn mcp_binary() -> PathBuf {
    let test_binary = std::env::current_exe().expect("current test executable");
    find_binary_from(&test_binary).unwrap_or_else(|| {
        panic!(
            "serial-nexus-mcp is not in any parent of {}: run cargo test --workspace so the MCP binary is built",
            test_binary.display()
        )
    })
}

/// One real MCP server process, spoken to over its stdio framing.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    secrets: Arc<Mutex<Vec<String>>>,
    stdout_reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    next_id: u64,
}

impl Mcp {
    fn spawn(env: &[(&str, &str)], secrets: &[&str]) -> Self {
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
        let stdout_reader = {
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
            })
        };
        let seen_stderr = Arc::new(Mutex::new(String::new()));
        let stderr_reader = {
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
            })
        };

        let mut mcp = Self {
            child,
            stdin: Some(stdin),
            lines: rx,
            stdout: seen_stdout,
            stderr: seen_stderr,
            secrets: Arc::new(Mutex::new(
                secrets.iter().map(|secret| (*secret).to_string()).collect(),
            )),
            stdout_reader: Some(stdout_reader),
            stderr_reader: Some(stderr_reader),
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
        assert!(response.get("result").is_some(), "initialize failed");
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
                        self.redacted(&self.stdout_text()),
                        self.redacted(&self.stderr_text())
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
            "tools/call {name} was a protocol error"
        );
        let result = &response["result"];
        (
            result["isError"].as_bool().unwrap_or(false),
            self.redacted(result["content"][0]["text"].as_str().unwrap_or_default()),
            result.get("structuredContent").cloned(),
        )
    }

    /// Replace every pairing value with a marker, so a failing assertion or a
    /// timeout panic cannot republish a credential the test is meant to catch.
    fn redacted(&self, text: &str) -> String {
        let mut text = text.to_string();
        if let Ok(secrets) = self.secrets.lock() {
            for secret in secrets.iter() {
                text = text.replace(secret.as_str(), "<pairing-secret>");
            }
        }
        text
    }

    fn add_secrets(&self, secrets: &[&str]) {
        if let Ok(mut known) = self.secrets.lock() {
            for secret in secrets {
                known.push((*secret).to_string());
            }
        }
    }

    /// Close stdin, wait for the server to exit and join both collectors, so
    /// the final transcript includes its shutdown output.
    fn finish(&mut self) {
        self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break;
                }
            }
        }
        if let Some(reader) = self.stdout_reader.take() {
            let _ = reader.join();
        }
        if let Some(reader) = self.stderr_reader.take() {
            let _ = reader.join();
        }
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
        self.finish();
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

/// Write the private pairing file for `info`; the mode is owner-only.
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
    // Create the file owner-only from the start: writing it first and
    // chmodding afterwards would leave the credential briefly readable.
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)
        .expect("private peers file");
    file.write_all(&serde_json::to_vec(&peers).expect("peers json"))
        .expect("write peers");
    path
}

/// Everything the device receives until it has been quiet for `quiet`.
fn drain_serial(device: &mut SimulatedDevice, quiet: Duration) -> Vec<u8> {
    let mut got = Vec::new();
    let mut last_data = Instant::now();
    loop {
        let chunk = device.try_read();
        if chunk.is_empty() {
            if last_data.elapsed() >= quiet {
                return got;
            }
        } else {
            got.extend(chunk);
            last_data = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Remove ANSI control sequences from a terminal transcript: CSI, OSC, and
/// the short two-byte escapes. Text between them is kept in order.
fn strip_terminal_sequences(bytes: &[u8]) -> String {
    let mut text = String::new();
    let mut chars = bytes.iter().copied().peekable();
    while let Some(byte) = chars.next() {
        if byte != 0x1b {
            text.push(byte as char);
            continue;
        }
        match chars.next() {
            Some(b'[') => {
                // CSI: parameters then a final byte in 0x40..=0x7e.
                for next in chars.by_ref() {
                    if (0x40..=0x7e).contains(&next) {
                        break;
                    }
                }
            }
            Some(b']') => {
                // OSC: ends with BEL or ESC \.
                let mut previous = 0;
                for next in chars.by_ref() {
                    if next == 0x07 || (previous == 0x1b && next == b'\\') {
                        break;
                    }
                    previous = next;
                }
            }
            Some(_) => {}
            None => {}
        }
    }
    text
}

/// True when a pairing value appears in a PTY transcript, either literally,
/// with control sequences removed, or with every non-base64 character
/// removed (so wrapping, spacing and cursor moves between its characters
/// cannot hide it).
fn transcript_leaks(transcript: &[u8], secret: &str) -> bool {
    let raw = String::from_utf8_lossy(transcript);
    if raw.contains(secret) {
        return true;
    }
    let stripped = strip_terminal_sequences(transcript);
    if stripped.contains(secret) {
        return true;
    }
    let squash = |text: &str| -> String {
        text.chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '/' | '='))
            .collect()
    };
    squash(&stripped).contains(&squash(secret))
}

/// Read one specific MCP answer, ignoring unrelated traffic.
fn answer_for(mcp: &mut Mcp, id: u64) -> Value {
    loop {
        let message = mcp
            .lines
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("no answer to request {id}"));
        if message.get("id").and_then(Value::as_u64) == Some(id) {
            return message;
        }
    }
}

#[test]
fn the_real_mcp_binary_reaches_a_remote_host_through_the_real_terminal_gateway() {
    let _guard = serial_test();
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
    wait_until(Duration::from_secs(5), || gateway.exists());

    let peers_directory = TempDir::new().expect("peers directory");
    let peers = write_peers(peers_directory.path(), &info);
    let peers = peers.to_string_lossy().into_owned();
    let gateway_text = gateway.to_string_lossy().into_owned();
    let mut mcp = Mcp::spawn(
        &[
            ("SERIAL_NEXUS_GATEWAY_SOCKET", &gateway_text),
            ("SERIAL_NEXUS_MCP_PEERS", &peers),
            ("HOME", &xdg.path().to_string_lossy()),
            ("XDG_CONFIG_HOME", &xdg.path().to_string_lossy()),
        ],
        &[
            &info.ai.credential,
            &info.human.credential,
            &info.host.ca_cert_der_b64,
        ],
    );

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

    // The first remote open chooses a non-default baud; later clients join it.
    let (is_error, text, structured) = mcp.call_tool(
        "connect",
        remote_args(
            &info,
            json!({ "path": device.path, "baud": 57_600, "request_control": true }),
        ),
    );
    assert!(!is_error, "{text}");
    let structured = structured.expect("structured connect");
    assert_eq!(structured["baud"], json!(57_600), "{text}");
    let device_id = structured["device_id"].as_u64().expect("device id");

    let (is_error, text, _) = mcp.call_tool(
        "send",
        remote_args(
            &info,
            json!({ "device": device_id, "text": "ai-through-gateway", "lines": 0 }),
        ),
    );
    assert!(!is_error, "{text}");

    // Two humans pair the one human credential, join at the rate the first
    // open chose, and write beside the AI.
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
        human_open.baud, 57_600,
        "a later client joins at the rate the first open chose"
    );
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
    let answer = answer_for(&mut mcp, 99);
    let result = &answer["result"];
    assert!(!result["isError"].as_bool().unwrap_or(false));
    human_write
        .join()
        .expect("human write thread")
        .expect("human write");
    second_write
        .join()
        .expect("second human write thread")
        .expect("second human write");

    // Every whole request reached the one device exactly once: one copy each,
    // byte for byte, with no extra or duplicated bytes in the stream.
    let expected_len =
        "ai-through-gateway".len() + ai_text.len() + human_text.len() + second_text.len();
    let mut stream = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while stream.len() < expected_len && Instant::now() < deadline {
        stream.extend(device.try_read());
        std::thread::sleep(Duration::from_millis(5));
    }
    // Extra bytes are a failure too: drain a short quiet window after the
    // expected total arrived, then require the exact total.
    stream.extend(drain_serial(&mut device, Duration::from_millis(200)));
    assert_eq!(
        stream.len(),
        expected_len,
        "the device got bytes no request sent"
    );
    let stream = String::from_utf8_lossy(&stream).into_owned();
    assert_eq!(stream.matches("ai-through-gateway").count(), 1);
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
        remote_args(&info, json!({ "path": device.path, "baud": 57_600 })),
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

    // Re-check the device so an in-flight write can be interrupted mid-wait.
    owner
        .check_shared_devices(vec![device.path.clone()])
        .expect("check again");

    // Uncertain write: a waited send is pending when the owner stops sharing.
    // The answer is an error that says the bytes may have arrived, the device
    // holds exactly one copy, and nothing is sent again.
    let (is_error, text, structured) = mcp.call_tool(
        "connect",
        remote_args(
            &info,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!is_error, "{text}");
    let device_id = structured.expect("structured reconnect")["device_id"]
        .as_u64()
        .expect("device id");
    {
        let line = json!({
            "jsonrpc": "2.0",
            "id": 98,
            "method": "tools/call",
            "params": {
                "name": "send",
                "arguments": remote_args(&info, json!({
                    "device": device_id,
                    "text": "uncertain-request",
                    "lines": 3,
                    "timeout_ms": 8_000,
                })),
            },
        });
        let stdin = mcp.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write pending send");
        stdin.flush().expect("flush pending send");
    }
    let mut before_stop = Vec::new();
    let arrival = Instant::now() + Duration::from_secs(5);
    while !String::from_utf8_lossy(&before_stop).contains("uncertain-request")
        && Instant::now() < arrival
    {
        before_stop.extend(device.try_read());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        String::from_utf8_lossy(&before_stop).contains("uncertain-request"),
        "the pending write never reached the device"
    );
    owner.stop_sharing().expect("sharing stops mid-write");
    let answer = answer_for(&mut mcp, 98);
    let result = &answer["result"];
    assert!(
        result["isError"].as_bool().unwrap_or(false),
        "a write whose answer was lost must be reported"
    );
    let uncertainty = result["content"][0]["text"].as_str().unwrap_or_default();
    let uncertainty = mcp.redacted(uncertainty).to_lowercase();
    assert!(
        uncertainty.contains("may have reached") || uncertainty.contains("uncertain"),
        "the error must say the write may have reached the device: {uncertainty}"
    );
    let after_stop = drain_serial(&mut device, Duration::from_millis(300));
    let before_stop = String::from_utf8_lossy(&before_stop).into_owned();
    let after_stop = String::from_utf8_lossy(&after_stop).into_owned();
    assert_eq!(
        before_stop.matches("uncertain-request").count(),
        1,
        "the pending write reached the device before sharing stopped"
    );
    assert!(
        after_stop.is_empty(),
        "an uncertain write is never retried: {:?}",
        mcp.redacted(&after_stop)
    );
    for value in [&before_stop, &after_stop] {
        assert!(!value.contains(&info.ai.credential));
        assert!(!value.contains(&info.human.credential));
    }

    // Restarting sharing rotates every session secret, so the old private
    // pairing file is refused rather than silently reaching a new session.
    wait_until(Duration::from_secs(5), || !human.is_connected());
    let rotated = owner
        .start_sharing(
            ShareBind::new(info.bind.ip(), info.bind.port()),
            vec![device.path.clone()],
        )
        .expect("sharing restarts");
    assert_eq!(rotated.bind, info.bind);
    assert!(rotated.session_id != info.session_id);
    assert!(rotated.ai.credential != info.ai.credential);
    assert!(rotated.human.credential != info.human.credential);
    assert!(rotated.host.ca_cert_der_b64 != info.host.ca_cert_der_b64);
    mcp.add_secrets(&[
        &rotated.ai.credential,
        &rotated.human.credential,
        &rotated.host.ca_cert_der_b64,
    ]);
    let old_human = RemoteClient::connect(
        &rotated.host,
        &info.human,
        Some("stale-human".into()),
        Duration::from_secs(5),
    );
    assert!(old_human.is_err(), "an old human credential is refused");
    let (is_error, text, _) = mcp.call_tool("list_devices", remote_args(&info, json!({})));
    assert!(
        is_error,
        "a rotated session must refuse the stale pairing, not reach it: {text}"
    );
    assert!(
        !text.contains(&info.ai.credential) && !text.contains(&info.human.credential),
        "a stale pairing must not echo its credential: {text}"
    );

    // With the rotated details the same MCP process reaches the new session,
    // and the interrupted request is still not sent a second time.
    write_peers(peers_directory.path(), &rotated);
    let (is_error, text, _) = mcp.call_tool("list_devices", remote_args(&rotated, json!({})));
    assert!(!is_error, "the rotated pairing is accepted: {text}");
    assert!(text.contains(&device.path), "{text}");
    let after_restart = drain_serial(&mut device, Duration::from_millis(300));
    assert!(
        after_restart.is_empty(),
        "an uncertain write is not replayed on a later session: {:?}",
        mcp.redacted(&String::from_utf8_lossy(&after_restart))
    );

    // When the foreground terminal leaves while that remote session is
    // healthy, its gateway goes with it. MCP neither dials the host directly
    // nor starts a local core. The transcript is joined after the child exits,
    // so shutdown output is part of the secrecy check below.
    let transcript = terminal.quit_capturing();
    let (is_error, text, _) = mcp.call_tool("list_devices", remote_args(&rotated, json!({})));
    assert!(
        is_error,
        "MCP must not reach a remote host without the terminal gateway: {text}"
    );
    let (is_error, text, _) = mcp.call_tool("list_devices", json!({}));
    assert!(
        is_error,
        "MCP must fail without the terminal gateway instead of starting a core: {text}"
    );

    // No pairing value appeared in the complete terminal PTY transcript
    // (including shutdown, unwrapped from control sequences) or in the
    // MCP process transcript: close stdin and join both collectors so its
    // shutdown output is included too.
    mcp.finish();
    let output = mcp.stdout_text();
    let diagnostics = mcp.stderr_text();
    for secret in [
        &info.ai.credential,
        &info.human.credential,
        &info.host.ca_cert_der_b64,
        &rotated.ai.credential,
        &rotated.human.credential,
        &rotated.host.ca_cert_der_b64,
    ] {
        assert!(
            !output.contains(secret),
            "MCP stdout leaked a pairing value"
        );
        assert!(
            !diagnostics.contains(secret),
            "MCP stderr leaked a pairing value"
        );
        assert!(
            !transcript_leaks(&transcript, secret),
            "the terminal PTY transcript leaked a pairing value"
        );
    }

    drop(host_core);
}

#[test]
fn a_write_larger_than_the_serial_buffer_keeps_the_device_to_itself() {
    // A request of 44,000 bytes is base64-safe inside one 64 KiB remote frame and is
    // far larger than a Linux PTY's buffer, so one serial write needs many
    // syscalls. Nothing reads the device until the human's request is queued,
    // so the AI request cannot finish first.
    let _guard = serial_test();
    const REQUEST: usize = 44_000;
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
        .expect("sharing starts");

    let (_local_core, local_socket) =
        start_core(Duration::from_secs(30), Duration::from_secs(300), 500);
    let xdg = TempDir::new().expect("xdg");
    let mut terminal = TuiSession::spawn(&local_socket, xdg.path(), 30, 110);
    terminal.wait_for_text("focus: sidebar");
    let gateway = local_socket.with_file_name("gateway.sock");
    wait_until(Duration::from_secs(5), || gateway.exists());

    let peers_directory = TempDir::new().expect("peers directory");
    let peers = write_peers(peers_directory.path(), &info);
    let peers = peers.to_string_lossy().into_owned();
    let gateway_text = gateway.to_string_lossy().into_owned();
    let mut mcp = Mcp::spawn(
        &[
            ("SERIAL_NEXUS_GATEWAY_SOCKET", &gateway_text),
            ("SERIAL_NEXUS_MCP_PEERS", &peers),
            ("HOME", &xdg.path().to_string_lossy()),
            ("XDG_CONFIG_HOME", &xdg.path().to_string_lossy()),
        ],
        &[
            &info.ai.credential,
            &info.human.credential,
            &info.host.ca_cert_der_b64,
        ],
    );
    let (is_error, text, structured) = mcp.call_tool(
        "connect",
        remote_args(
            &info,
            json!({ "path": device.path, "baud": 57_600, "request_control": true }),
        ),
    );
    assert!(!is_error, "{text}");
    let device_id = structured.expect("structured connect")["device_id"]
        .as_u64()
        .expect("device id");

    let ai_text = "D".repeat(REQUEST);
    {
        let line = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "send",
                "arguments": remote_args(&info, json!({ "device": device_id, "text": ai_text, "lines": 0 })),
            },
        });
        let stdin = mcp.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{line}").expect("write large send");
        stdin.flush().expect("flush large send");
    }
    // A write without a wait is answered once the core queued it. The serial
    // writer has begun and is blocked by the device's buffer because nothing
    // reads the device yet, so this acknowledgement proves the AI request is
    // still incomplete.
    let answer = answer_for(&mut mcp, 7);
    assert!(
        !answer["result"]["isError"].as_bool().unwrap_or(false),
        "the large AI send was queued"
    );

    // A human joins and writes another 44,000 bytes while the AI request is
    // still incomplete.
    let human = RemoteClient::connect(
        &info.host,
        &info.human,
        Some("large-human".into()),
        Duration::from_secs(5),
    )
    .expect("human pairs");
    let human_open = human
        .open(&device.path, None, false)
        .expect("human joins the open device");
    let human_text = "E".repeat(REQUEST);
    // The human's acknowledgement means its request is queued behind the
    // unfinished AI request. Reading the device now cannot change that order.
    human
        .write_bytes(human_open.device_id, human_text.as_bytes(), None)
        .expect("human write is admitted mid-write");

    let mut stream = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(15);
    while stream.len() < 2 * REQUEST && Instant::now() < deadline {
        stream.extend(device.try_read());
        std::thread::sleep(Duration::from_millis(5));
    }
    let expected = format!("{ai_text}{human_text}");
    assert!(
        stream == expected.as_bytes(),
        "the serial writer finished the AI request before the human's: {} bytes",
        stream.len()
    );

    drop(human);
    terminal.quit();
    drop(host_core);
}

#[test]
fn the_mcp_binary_is_found_from_a_nested_build_directory() {
    let root = TempDir::new().expect("temp");
    let debug = root.path().join("debug");
    let nested = debug.join("build/serial-nexus-terminal/0123456789abcdef/out");
    std::fs::create_dir_all(&nested).expect("nested build directory");
    std::fs::write(debug.join("serial-nexus-mcp"), b"binary").expect("binary");
    let test_executable = nested.join("pty_remote_mcp-0123456789abcdef");
    std::fs::write(&test_executable, b"test").expect("test executable");

    assert_eq!(
        find_binary_from(&test_executable),
        Some(debug.join("serial-nexus-mcp"))
    );
    assert_eq!(
        find_binary_from(&nested.join("not-built")),
        Some(debug.join("serial-nexus-mcp")),
        "a stale path under the same build tree still finds it"
    );
}
