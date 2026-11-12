//! Remote MCP transport: real stdio tool calls, real cores on loopback, PTY
//! serial devices, two endpoints, and the private pairing file.
//!
//! These tests never touch physical hardware.

mod support;

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};
use serial_nexus_client::{Client, ClientInfo};
use serial_nexus_protocol::{PairingSecret, ShareBind, ShareSessionInfo};
use support::{start_core, wait_until, McpSession, SimulatedDevice};
use tempfile::TempDir;

/// One core with an owner terminal connection that is sharing `devices`.
struct SharedHost {
    directory: TempDir,
    socket: PathBuf,
    owner: Client,
    info: ShareSessionInfo,
}

impl SharedHost {
    fn start(devices: &[String]) -> Self {
        Self::start_on(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), devices)
    }

    fn start_on(address: IpAddr, devices: &[String]) -> Self {
        let (directory, socket) = start_core(Duration::from_secs(30));
        Self::start_at(directory, socket, address, devices)
    }

    /// Share `devices` from an already-running core. The path is what the
    /// core sees; two cores in different mount namespaces may see the same
    /// path as different nodes.
    fn start_at(directory: TempDir, socket: PathBuf, address: IpAddr, devices: &[String]) -> Self {
        let owner = Client::connect_to_socket(&socket, ClientInfo::human("owner-terminal"))
            .expect("owner connects");
        // A PTY is not an enumerated port, so the host opens each device it
        // wants to share first, exactly as a human terminal would.
        for path in devices {
            owner
                .open(path, Some(115_200), false)
                .expect("host opens the device to share");
        }
        let info = owner
            .start_sharing(ShareBind::new(address, 0), devices.to_vec())
            .expect("start sharing");
        Self {
            directory,
            socket,
            owner,
            info,
        }
    }

    fn endpoint(&self) -> (IpAddr, u16) {
        (self.info.bind.ip(), self.info.bind.port())
    }
}

fn peer_entry(info: &ShareSessionInfo, secret: &PairingSecret) -> Value {
    json!({
        "endpoint": { "address": info.bind.ip().to_string(), "port": info.bind.port() },
        "paired": info.host,
        "secret": { "role": secret.role, "credential": secret.credential },
    })
}

fn write_peers(directory: &TempDir, entries: Vec<Value>) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.path().join("mcp-peers.json");
    let document = json!({ "version": 1, "peers": entries });
    let mut file = std::fs::File::create(&path).expect("create peers");
    file.write_all(document.to_string().as_bytes())
        .expect("write");
    file.flush().expect("flush");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    path
}

fn spawn(socket: &Path, peers: &Path) -> McpSession {
    let peers = peers.to_string_lossy().into_owned();
    McpSession::spawn_with_env(socket, &[("SERIAL_NEXUS_MCP_PEERS", &peers)])
}

fn remote_args(host: &SharedHost, rest: Value) -> Value {
    let (address, port) = host.endpoint();
    let mut object = rest.as_object().cloned().unwrap_or_default();
    object.insert("ip".into(), json!(address.to_string()));
    object.insert("port".into(), json!(port));
    Value::Object(object)
}

/// The core binary built beside this test binary, for tests that need a real
/// core *process*: one that can be killed, or started in its own mount
/// namespace.
fn core_binary() -> PathBuf {
    let core = Path::new(env!("CARGO_BIN_EXE_serial-nexus-mcp"))
        .parent()
        .expect("target dir")
        .join("serial-nexus-core");
    assert!(
        core.is_file(),
        "serial-nexus-core binary missing at {core:?}"
    );
    core
}

/// Wait until a spawned core answers on its socket, or say why it never did.
fn wait_for_core_socket(socket: &Path, child: &mut Child) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if UnixStream::connect(socket).is_ok() {
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("core exited before its socket appeared: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("core socket {socket:?} did not appear within 10s");
}

fn core_command(socket: &Path) -> Command {
    let mut command = Command::new(core_binary());
    command
        .env("SERIAL_NEXUS_SOCKET", socket)
        .env("SERIAL_NEXUS_GRACE_SECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

/// Start the core binary as a child process on a fresh temp socket, so a test
/// can end it abruptly.
fn start_core_process(directory: &Path) -> (Child, PathBuf) {
    let socket = directory.join("core.sock");
    let mut child = core_command(&socket).spawn().expect("spawn core process");
    wait_for_core_socket(&socket, &mut child);
    (child, socket)
}

/// Whether this machine lets an ordinary user create a user and mount
/// namespace. GitHub's Ubuntu images may deny it; the same-path test says so
/// and skips rather than pretending to pass.
fn user_namespaces_available() -> bool {
    Command::new("unshare")
        .args(["-rm", "--", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Start the core binary in its own mount namespace with `device` bound over
/// `mountpoint`, so two hosts can present the *same* device path backed by
/// different nodes.
fn start_core_with_device_at(
    directory: &Path,
    device: &str,
    mountpoint: &Path,
) -> (Child, PathBuf) {
    let socket = directory.join("core.sock");
    let mut child = Command::new("unshare")
        .args([
            "-rm",
            "--",
            "/bin/sh",
            "-c",
            "mount --bind \"$SNX_TEST_DEVICE\" \"$SNX_TEST_MOUNTPOINT\" && exec \"$SNX_TEST_CORE\"",
        ])
        .env("SNX_TEST_DEVICE", device)
        .env("SNX_TEST_MOUNTPOINT", mountpoint)
        .env("SNX_TEST_CORE", core_binary())
        .env("SERIAL_NEXUS_SOCKET", &socket)
        .env("SERIAL_NEXUS_GRACE_SECS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn core in a user and mount namespace");
    wait_for_core_socket(&socket, &mut child);
    (child, socket)
}

fn call(session: &mut McpSession, name: &str, arguments: Value) -> Value {
    session.request(
        "tools/call",
        json!({ "name": name, "arguments": arguments }),
    )
}

/// The text of a tool result, and whether it was an error.
fn reply(response: &Value) -> (String, bool) {
    let result = &response["result"];
    (
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        result["isError"].as_bool().unwrap_or(false),
    )
}

fn assert_no_secret(session: &McpSession, text: &str, info: &ShareSessionInfo) {
    assert!(!text.contains(&info.ai.credential), "secret in {text}");
    assert!(!text.contains(&info.human.credential), "secret in {text}");
    // stderr is captured as it arrives; give the collector a moment so this
    // check can see anything the server has already written.
    std::thread::sleep(Duration::from_millis(50));
    let stderr = session.stderr_text();
    assert!(!stderr.contains(&info.ai.credential), "secret in stderr");
    assert!(!stderr.contains(&info.human.credential), "secret in stderr");
}

#[test]
fn local_calls_are_unchanged_when_no_pairing_is_configured() {
    let (directory, socket) = start_core(Duration::from_secs(30));
    let mut device = SimulatedDevice::new();
    // No peers file exists at all.
    let missing = directory.path().join("absent.json");
    let mut session = spawn(&socket, &missing);

    let listed = session.call_tool("list_devices", json!({}));
    assert!(!listed.is_error, "{}", listed.text);
    assert!(!listed.text.contains("remote"), "{}", listed.text);

    let connected = session.call_tool(
        "connect",
        json!({ "port": device.path, "request_control": true }),
    );
    assert!(!connected.is_error, "{}", connected.text);
    assert!(!connected.text.contains("remote"), "{}", connected.text);

    let id = connected.device_id();
    let sent = session.call_tool("send", json!({ "device": id, "text": "PING", "lines": 0 }));
    assert!(!sent.is_error, "{}", sent.text);
    assert_eq!(device.read_n(4, Duration::from_secs(5)), b"PING");
}

#[test]
fn a_remote_session_reaches_a_shared_device_and_reports_its_endpoint() {
    let mut device = SimulatedDevice::new();
    let host = SharedHost::start(std::slice::from_ref(&device.path));
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);
    let (address, port) = host.endpoint();

    let listed = session.call_tool("list_devices", remote_args(&host, json!({})));
    assert!(!listed.is_error, "{}", listed.text);
    assert!(
        listed.text.contains(&format!("remote {address}:{port}")),
        "{}",
        listed.text
    );
    assert!(listed.text.contains(&device.path), "{}", listed.text);

    let connected = session.call_tool(
        "connect",
        remote_args(
            &host,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!connected.is_error, "{}", connected.text);
    assert!(connected.text.contains("remote"), "{}", connected.text);
    assert_eq!(
        connected.json()["remote_endpoint"]["port"].as_u64(),
        Some(u64::from(port))
    );
    let id = connected.device_id();

    let sent = session.call_tool(
        "send",
        remote_args(&host, json!({ "device": id, "text": "HELLO", "lines": 0 })),
    );
    assert!(!sent.is_error, "{}", sent.text);
    assert_eq!(device.read_n(5, Duration::from_secs(5)), b"HELLO");

    device.write(b"remote echo\r\n");
    let read = session.call_tool(
        "read",
        remote_args(&host, json!({ "device": device.path, "lines": 5 })),
    );
    assert!(!read.is_error, "{}", read.text);
    assert!(read.text.contains("remote echo"), "{}", read.text);
    assert_no_secret(&session, &read.text, &host.info);
}

#[test]
fn ip_and_port_are_required_together_and_validated() {
    let host = SharedHost::start(&[]);
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);
    let (address, port) = host.endpoint();

    for arguments in [
        json!({ "ip": address.to_string() }),
        json!({ "port": port }),
        json!({ "ip": "not-an-address", "port": port }),
        json!({ "ip": address.to_string(), "port": 0 }),
        json!({ "ip": address.to_string(), "port": 70_000 }),
        json!({ "ip": "0.0.0.0", "port": port }),
        json!({ "ip": address.to_string(), "port": format!("{port}") }),
    ] {
        let response = call(&mut session, "list_devices", arguments.clone());
        assert!(
            response.get("error").is_some(),
            "{arguments} must be a parameter error: {response}"
        );
    }

    // A remote connect needs the device path separately from the TCP port.
    let response = call(
        &mut session,
        "connect",
        json!({ "ip": address.to_string(), "port": port }),
    );
    assert!(response.get("error").is_some(), "{response}");
}

#[test]
fn an_unknown_endpoint_is_a_clear_error_without_credentials() {
    let host = SharedHost::start(&[]);
    let empty = write_peers(&host.directory, Vec::new());
    let mut session = spawn(&host.socket, &empty);
    let (address, port) = host.endpoint();

    let reply = session.call_tool("list_devices", remote_args(&host, json!({})));
    assert!(reply.is_error);
    assert!(
        reply.text.contains(&format!("{address}:{port}")),
        "{}",
        reply.text
    );
    assert!(reply.text.contains("no pairing"), "{}", reply.text);
    assert_no_secret(&session, &reply.text, &host.info);
}

#[test]
fn a_human_credential_cannot_authenticate_the_mcp_role() {
    let host = SharedHost::start(&[]);
    let peers = write_peers(
        &host.directory,
        vec![peer_entry(&host.info, &host.info.human)],
    );
    let mut session = spawn(&host.socket, &peers);

    let reply = session.call_tool("list_devices", remote_args(&host, json!({})));
    assert!(reply.is_error, "{}", reply.text);
    assert!(
        reply.text.to_lowercase().contains("human"),
        "{}",
        reply.text
    );
    assert_no_secret(&session, &reply.text, &host.info);
}

#[test]
fn a_host_that_cannot_prove_the_pinned_certificate_is_refused() {
    let host = SharedHost::start(&[]);
    let mut entry = peer_entry(&host.info, &host.info.ai);
    entry["paired"]["host_cert_fingerprint_sha256"] = json!("ab".repeat(32));
    let peers = write_peers(&host.directory, vec![entry]);
    let mut session = spawn(&host.socket, &peers);

    let reply = session.call_tool("list_devices", remote_args(&host, json!({})));
    assert!(reply.is_error, "{}", reply.text);
    assert!(
        reply.text.to_lowercase().contains("certificate"),
        "{}",
        reply.text
    );
    assert_no_secret(&session, &reply.text, &host.info);
}

#[test]
fn a_pairing_file_others_can_read_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let host = SharedHost::start(&[]);
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    std::fs::set_permissions(&peers, std::fs::Permissions::from_mode(0o644)).expect("chmod");

    let mut session = spawn(&host.socket, &peers);
    let reply = session.call_tool("list_devices", remote_args(&host, json!({})));
    assert!(reply.is_error, "{}", reply.text);
    assert!(
        reply.text.contains("permission") || reply.text.contains("mode"),
        "{}",
        reply.text
    );
    assert_no_secret(&session, &reply.text, &host.info);
}

#[test]
fn two_hosts_with_equal_device_ids_keep_separate_sessions() {
    // Each host has its own serial node (one PTY cannot be opened by two cores
    // at once), and each core hands out device id 1: the endpoint, not the id,
    // must identify the session. The identical-path case is covered by the
    // session bookkeeping unit test.
    let mut first_device = SimulatedDevice::new();
    let mut second_device = SimulatedDevice::new();
    let first = SharedHost::start(&[first_device.path.clone()]);
    let second = SharedHost::start(&[second_device.path.clone()]);
    let peers = write_peers(
        &first.directory,
        vec![
            peer_entry(&first.info, &first.info.ai),
            peer_entry(&second.info, &second.info.ai),
        ],
    );
    let mut session = spawn(&first.socket, &peers);

    let a = session.call_tool(
        "connect",
        remote_args(
            &first,
            json!({ "path": first_device.path, "request_control": true }),
        ),
    );
    assert!(!a.is_error, "{}", a.text);
    let b = session.call_tool(
        "connect",
        remote_args(
            &second,
            json!({ "path": second_device.path, "request_control": true }),
        ),
    );
    assert!(!b.is_error, "{}", b.text);
    assert_eq!(a.device_id(), b.device_id());

    first_device.write(b"first host\r\n");
    second_device.write(b"second host\r\n");
    first_device.read_for(Duration::from_millis(200));
    second_device.read_for(Duration::from_millis(200));

    let (a_address, a_port) = first.endpoint();
    let (b_address, b_port) = second.endpoint();
    let read_a = session.call_tool(
        "read",
        remote_args(&first, json!({ "device": a.device_id(), "lines": 5 })),
    );
    assert!(!read_a.is_error, "{}", read_a.text);
    assert!(
        read_a.text.contains(&format!("{a_address}:{a_port}")),
        "{}",
        read_a.text
    );
    assert!(read_a.text.contains("first host"), "{}", read_a.text);

    // Disconnecting from one endpoint leaves the other attached.
    let disconnected = session.call_tool(
        "disconnect",
        remote_args(&first, json!({ "device": a.device_id() })),
    );
    assert!(!disconnected.is_error, "{}", disconnected.text);
    let read_b = session.call_tool(
        "read",
        remote_args(&second, json!({ "device": b.device_id(), "lines": 5 })),
    );
    assert!(!read_b.is_error, "{}", read_b.text);
    assert!(
        read_b.text.contains(&format!("{b_address}:{b_port}")),
        "{}",
        read_b.text
    );
    assert!(read_b.text.contains("second host"), "{}", read_b.text);

    // And sending to the second host reaches only its device.
    let sent = session.call_tool(
        "send",
        remote_args(
            &second,
            json!({ "device": b.device_id(), "text": "B-ONLY", "lines": 0 }),
        ),
    );
    assert!(!sent.is_error, "{}", sent.text);
    assert_eq!(second_device.read_n(6, Duration::from_secs(5)), b"B-ONLY");
    assert!(first_device.read_for(Duration::from_millis(100)).is_empty());
}

#[test]
fn one_ai_writer_is_shared_between_local_and_remote_sessions() {
    let device = SimulatedDevice::new();
    let host = SharedHost::start(std::slice::from_ref(&device.path));
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut local = spawn(&host.socket, &peers);
    let mut remote = spawn(&host.socket, &peers);

    let local_connect = local.call_tool(
        "connect",
        json!({ "port": device.path, "request_control": true }),
    );
    assert!(!local_connect.is_error, "{}", local_connect.text);

    let remote_connect = remote.call_tool(
        "connect",
        remote_args(
            &host,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!remote_connect.is_error, "{}", remote_connect.text);
    assert!(
        remote_connect.text.contains("serial-nexus-mcp"),
        "the local holder must be named: {}",
        remote_connect.text
    );
    assert_eq!(remote_connect.json()["role"], "reader");

    let remote_send = remote.call_tool(
        "send",
        remote_args(
            &host,
            json!({ "device": remote_connect.device_id(), "text": "NOPE", "lines": 0 }),
        ),
    );
    assert!(remote_send.is_error, "{}", remote_send.text);
    assert!(
        remote_send.text.to_lowercase().contains("write control"),
        "{}",
        remote_send.text
    );

    // The local controller can still write.
    let local_send = local.call_tool(
        "send",
        json!({ "device": local_connect.device_id(), "text": "OK", "lines": 0 }),
    );
    assert!(!local_send.is_error, "{}", local_send.text);
}

#[test]
fn unchecking_a_device_revokes_remote_access_without_exposing_secrets() {
    let mut device = SimulatedDevice::new();
    let host = SharedHost::start(std::slice::from_ref(&device.path));
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);

    let connected = session.call_tool(
        "connect",
        remote_args(
            &host,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!connected.is_error, "{}", connected.text);
    let id = connected.device_id();

    host.owner
        .uncheck_shared_devices(vec![device.path.clone()])
        .expect("uncheck");
    wait_until(Duration::from_secs(5), || {
        let reply = call(
            &mut session,
            "read",
            remote_args(&host, json!({ "device": id, "lines": 1 })),
        );
        reply["result"]["isError"].as_bool().unwrap_or(false)
    });

    let read = session.call_tool(
        "read",
        remote_args(&host, json!({ "device": device.path, "lines": 1 })),
    );
    assert!(read.is_error, "{}", read.text);
    let reopen = session.call_tool(
        "connect",
        remote_args(&host, json!({ "path": device.path })),
    );
    assert!(reopen.is_error, "{}", reopen.text);
    assert_no_secret(
        &session,
        &format!("{}{}", read.text, reopen.text),
        &host.info,
    );

    device.write(b"still here\r\n");
}

#[test]
fn a_write_stopped_by_owner_termination_is_reported_and_never_replayed() {
    let mut device = SimulatedDevice::new();
    let host = SharedHost::start(std::slice::from_ref(&device.path));
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);
    let connected = session.call_tool(
        "connect",
        remote_args(
            &host,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!connected.is_error, "{}", connected.text);
    let id = connected.device_id();

    // A send that would wait 30 s for a device line that never comes, stopped
    // by the owner losing the session: the write may have reached the device.
    let request = session.start_request(
        "tools/call",
        json!({
            "name": "send",
            "arguments": remote_args(&host, json!({
                "device": id,
                "text": "PARTIAL-WRITE",
                "lines": 1,
                "timeout_ms": 30_000,
            })),
        }),
    );
    assert_eq!(
        device.read_n(13, Duration::from_secs(5)),
        b"PARTIAL-WRITE",
        "the accepted write reaches the device before termination"
    );
    host.owner.stop_sharing().expect("stop sharing");
    let response = session.response_for(&request);
    let (text, is_error) = reply(&response);
    assert!(is_error, "{text}");
    assert!(
        text.to_lowercase().contains("uncertain")
            || text.to_lowercase().contains("may have reached"),
        "{}",
        text
    );
    assert!(
        text.to_lowercase().contains("not sent again")
            || text.to_lowercase().contains("not replayed"),
        "{}",
        text
    );
    assert_no_secret(&session, &text, &host.info);

    // Dropping the owner ends sharing; a later call must not fall back to the
    // local core and must not reveal the credential.
    let (address, port) = host.endpoint();
    drop(host.owner);
    let after = session.call_tool(
        "list_devices",
        json!({ "ip": address.to_string(), "port": port }),
    );
    assert!(after.is_error, "{}", after.text);
    assert!(!after.text.contains(&device.path), "{}", after.text);
    assert_no_secret(&session, &after.text, &host.info);
}

#[test]
fn rotated_credentials_are_reported_and_a_repair_is_used_on_the_next_call() {
    let device = SimulatedDevice::new();
    let host = SharedHost::start(std::slice::from_ref(&device.path));
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);

    let connected = session.call_tool(
        "connect",
        remote_args(&host, json!({ "path": device.path })),
    );
    assert!(!connected.is_error, "{}", connected.text);

    // Restart sharing: a fresh CA, leaf certificate and credentials.
    host.owner.stop_sharing().expect("stop");
    let fresh = host
        .owner
        .start_sharing(
            // Same address and port, so only the fresh certificate and
            // credentials differ: the old pairing is now stale.
            ShareBind::new(
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                host.info.bind.port(),
            ),
            vec![device.path.clone()],
        )
        .expect("restart");

    let stale = session.call_tool(
        "list_devices",
        json!({ "ip": host.info.bind.ip().to_string(), "port": host.info.bind.port() }),
    );
    assert!(stale.is_error, "{}", stale.text);
    assert_no_secret(&session, &stale.text, &host.info);

    // The human re-pairs quietly; the next call with the new endpoint works.
    let repaired = write_peers(&host.directory, vec![peer_entry(&fresh, &fresh.ai)]);
    assert_eq!(repaired, peers);
    let listed = session.call_tool(
        "list_devices",
        json!({ "ip": fresh.bind.ip().to_string(), "port": fresh.bind.port() }),
    );
    assert!(!listed.is_error, "{}", listed.text);
    assert_no_secret(&session, &listed.text, &fresh);
}

#[test]
fn concurrent_remote_human_and_ai_writes_keep_each_request_whole() {
    let mut device = SimulatedDevice::new();
    let host = SharedHost::start(std::slice::from_ref(&device.path));
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);
    let connected = session.call_tool(
        "connect",
        remote_args(
            &host,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!connected.is_error, "{}", connected.text);
    let id = connected.device_id();

    // A human remote client writes at the same time as the AI's MCP send.
    let human = serial_nexus_client::RemoteClient::connect(
        &host.info.host,
        &host.info.human,
        Some("human-remote".into()),
        Duration::from_secs(5),
    )
    .expect("human pairs");
    let opened = human
        .open(&device.path, None, false)
        .expect("human joins the open device");

    let ai_text = "A".repeat(1_500);
    let human_text = "B".repeat(1_500);
    let request = session.start_request(
        "tools/call",
        json!({
            "name": "send",
            "arguments": remote_args(&host, json!({
                "device": id,
                "text": ai_text,
                "lines": 0,
            })),
        }),
    );
    let human_write = {
        let human = human.clone();
        let text = human_text.clone();
        std::thread::spawn(move || human.write_bytes(opened.device_id, text.as_bytes(), None))
    };
    let response = session.response_for(&request);
    let (text, is_error) = reply(&response);
    assert!(!is_error, "{text}");
    human_write
        .join()
        .expect("human write thread")
        .expect("human write");

    let stream = device.read_for(Duration::from_secs(2));
    let stream = String::from_utf8_lossy(&stream).into_owned();
    assert!(
        stream.contains(&ai_text),
        "the AI request must stay whole: {} bytes",
        stream.len()
    );
    assert!(
        stream.contains(&human_text),
        "the human request must stay whole: {} bytes",
        stream.len()
    );
    // Exactly one copy of each: no request is repeated.
    assert_eq!(stream.matches(&ai_text).count(), 1);
    assert_eq!(stream.matches(&human_text).count(), 1);
}

#[test]
fn the_transport_dials_only_the_paired_endpoint_and_the_paired_role() {
    use serial_nexus_client::{
        ClientError, PairingRole, PairingSecret, RemoteClient, RemoteEndpoint,
    };
    let device = SimulatedDevice::new();
    let host = SharedHost::start(std::slice::from_ref(&device.path));
    let (address, port) = host.endpoint();
    let endpoint = RemoteEndpoint::new(address, port);

    // An explicitly specified address and port dials that endpoint.
    let explicit = RemoteClient::connect_to(
        endpoint,
        &host.info.host,
        &host.info.ai,
        Some("explicit".into()),
        Duration::from_secs(5),
    )
    .expect("explicit endpoint connects");
    assert_eq!(explicit.role(), PairingRole::Ai);

    // Any other endpoint is refused before anything is sent.
    let elsewhere = RemoteEndpoint::new(address, port.wrapping_add(1));
    let error = RemoteClient::connect_to(
        elsewhere,
        &host.info.host,
        &host.info.ai,
        None,
        Duration::from_secs(2),
    )
    .expect_err("a different endpoint must be refused");
    assert!(matches!(error, ClientError::EndpointMismatch(_)), "{error}");

    // A credential used with the wrong role is refused even though the host
    // matched it: the answer says the role the pairing promised is not the
    // role the host authenticated.
    let mislabelled = PairingSecret {
        role: PairingRole::Human,
        credential: host.info.ai.credential.clone(),
    };
    let error = RemoteClient::connect_to(
        endpoint,
        &host.info.host,
        &mislabelled,
        None,
        Duration::from_secs(5),
    )
    .expect_err("a role mismatch must be refused");
    assert!(matches!(error, ClientError::Authentication(_)), "{error}");
    assert!(!error.to_string().contains(&host.info.ai.credential));
}

#[test]
fn a_remote_device_path_is_never_resolved_on_this_machine() {
    // The alias exists only on the client here and points at a different PTY
    // than the path the host shared. Resolving it locally would address the
    // wrong device; the transport must refuse instead.
    let mut device_a = SimulatedDevice::new();
    let mut device_b = SimulatedDevice::new();
    let host = SharedHost::start(&[device_a.path.clone(), device_b.path.clone()]);
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);

    for path in [&device_a.path, &device_b.path] {
        let connected = session.call_tool(
            "connect",
            remote_args(&host, json!({ "path": path, "request_control": true })),
        );
        assert!(!connected.is_error, "{}", connected.text);
    }

    let alias = host.directory.path().join("client-only-alias");
    std::os::unix::fs::symlink(&device_b.path, &alias).expect("symlink");
    let alias = alias.to_string_lossy().into_owned();
    let reply = session.call_tool(
        "send",
        remote_args(
            &host,
            json!({ "device": alias, "text": "WRONG", "lines": 0 }),
        ),
    );
    assert!(reply.is_error, "{}", reply.text);
    assert!(reply.text.contains(&alias), "{}", reply.text);
    assert!(
        device_b.read_for(Duration::from_millis(300)).is_empty(),
        "the write must not reach the locally resolved device"
    );
    assert!(device_a.read_for(Duration::from_millis(100)).is_empty());
}

#[test]
fn a_misplaced_credential_in_the_pairing_file_is_not_echoed() {
    let host = SharedHost::start(&[]);
    let (address, port) = host.endpoint();
    let credential = host.info.ai.credential.clone();

    // The bearer pasted into the wrong field: serde's own message would quote
    // it, so the loader must not forward that message.
    let mut entry = peer_entry(&host.info, &host.info.ai);
    entry["secret"]["role"] = json!(credential);
    let peers = write_peers(&host.directory, vec![entry]);
    let mut session = spawn(&host.socket, &peers);
    let reply = session.call_tool(
        "list_devices",
        json!({ "ip": address.to_string(), "port": port }),
    );
    assert!(reply.is_error, "{}", reply.text);
    assert!(!reply.text.contains(&credential), "credential leaked");
    assert_no_secret(&session, &reply.text, &host.info);
}

#[test]
fn a_credential_pasted_as_the_host_name_is_not_echoed() {
    let host = SharedHost::start(&[]);
    let (address, port) = host.endpoint();
    let credential = host.info.ai.credential.clone();
    let mut entry = peer_entry(&host.info, &host.info.ai);
    entry["paired"]["host_name"] = json!(credential);
    let peers = write_peers(&host.directory, vec![entry]);
    let mut session = spawn(&host.socket, &peers);
    let reply = session.call_tool(
        "list_devices",
        json!({ "ip": address.to_string(), "port": port }),
    );
    assert!(reply.is_error, "{}", reply.text);
    assert!(!reply.text.contains(&credential), "credential leaked");
}

#[test]
fn losing_the_owner_connection_stops_a_pending_write_without_replay() {
    let mut device = SimulatedDevice::new();
    let host = SharedHost::start(std::slice::from_ref(&device.path));
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);
    let connected = session.call_tool(
        "connect",
        remote_args(
            &host,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!connected.is_error, "{}", connected.text);
    let id = connected.device_id();

    let request = session.start_request(
        "tools/call",
        json!({
            "name": "send",
            "arguments": remote_args(&host, json!({
                "device": id,
                "text": "OWNER-LOSS",
                "lines": 1,
                "timeout_ms": 30_000,
            })),
        }),
    );
    assert_eq!(
        device.read_n(10, Duration::from_secs(5)),
        b"OWNER-LOSS",
        "the write reaches the device before the owner goes away"
    );
    // The owning terminal's connection dies: the session, listener and
    // credentials go with it.
    drop(host.owner);
    let response = session.response_for(&request);
    let (text, is_error) = reply(&response);
    assert!(is_error, "{text}");
    assert!(
        text.to_lowercase().contains("uncertain")
            || text.to_lowercase().contains("may have reached"),
        "{}",
        text
    );
    assert_no_secret(&session, &text, &host.info);
    assert!(
        device.read_for(Duration::from_millis(300)).is_empty(),
        "the write must not be replayed"
    );
}

#[test]
fn an_ipv6_endpoint_is_reached_with_the_same_pair() {
    let mut device = SimulatedDevice::new();
    let host = SharedHost::start_on(
        IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        std::slice::from_ref(&device.path),
    );
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);

    let connected = session.call_tool(
        "connect",
        remote_args(
            &host,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!connected.is_error, "{}", connected.text);
    assert!(
        connected
            .text
            .contains(&format!("[::1]:{}", host.endpoint().1)),
        "{}",
        connected.text
    );
    let sent = session.call_tool(
        "send",
        remote_args(
            &host,
            json!({ "device": connected.device_id(), "text": "V6", "lines": 0 }),
        ),
    );
    assert!(!sent.is_error, "{}", sent.text);
    assert_eq!(device.read_n(2, Duration::from_secs(5)), b"V6");
}

#[test]
fn a_fifo_in_place_of_the_pairing_file_is_refused_without_blocking() {
    use nix::sys::stat::Mode;
    let host = SharedHost::start(&[]);
    let (address, port) = host.endpoint();
    let peers = host.directory.path().join("mcp-peers.json");
    nix::unistd::mkfifo(&peers, Mode::from_bits_truncate(0o600)).expect("mkfifo");
    let mut session = spawn(&host.socket, &peers);

    // A blocking open would wait for a writer forever; the tool call must
    // answer, and the next request on the same session must also answer.
    let reply = session.call_tool(
        "list_devices",
        json!({ "ip": address.to_string(), "port": port }),
    );
    assert!(reply.is_error, "{}", reply.text);
    assert!(
        reply.text.contains("regular file") || reply.text.contains("symbolic"),
        "{}",
        reply.text
    );
    let ping = session.request("ping", json!({}));
    assert!(ping.get("result").is_some(), "{ping}");
}

#[test]
fn two_hosts_sharing_the_same_device_path_stay_isolated_end_to_end() {
    // Both hosts see the identical path string for different physical nodes:
    // each core lives in its own mount namespace, with its own PTY bound over
    // one shared placeholder. The MCP session may only ever reach the host it
    // named, so a write for one path can never land on the other host's node.
    let mount_directory = TempDir::new().expect("mount directory");
    let shared_path = mount_directory.path().join("ttyUSB0");
    std::fs::File::create(&shared_path).expect("mount placeholder");
    let shared = shared_path.to_string_lossy().into_owned();

    if !user_namespaces_available() {
        eprintln!(
            "skipping same-path two-host isolation: this machine denies unprivileged user namespaces"
        );
        return;
    }

    let mut first_device = SimulatedDevice::new();
    let mut second_device = SimulatedDevice::new();
    let first_directory = TempDir::new().expect("first core directory");
    let second_directory = TempDir::new().expect("second core directory");
    let (_first_core, first_socket) =
        start_core_with_device_at(first_directory.path(), &first_device.path, &shared_path);
    let (_second_core, second_socket) =
        start_core_with_device_at(second_directory.path(), &second_device.path, &shared_path);

    let first = SharedHost::start_at(
        first_directory,
        first_socket,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        std::slice::from_ref(&shared),
    );
    let second = SharedHost::start_at(
        second_directory,
        second_socket,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        std::slice::from_ref(&shared),
    );
    let peers = write_peers(
        &first.directory,
        vec![
            peer_entry(&first.info, &first.info.ai),
            peer_entry(&second.info, &second.info.ai),
        ],
    );
    let mut session = spawn(&first.socket, &peers);
    let (first_address, first_port) = first.endpoint();
    let (second_address, second_port) = second.endpoint();
    assert_ne!((first_address, first_port), (second_address, second_port));

    let listed_first = session.call_tool("list_devices", remote_args(&first, json!({})));
    assert!(!listed_first.is_error, "{}", listed_first.text);
    assert!(listed_first.text.contains(&shared), "{}", listed_first.text);
    assert!(
        listed_first
            .text
            .contains(&format!("remote {first_address}:{first_port}")),
        "{}",
        listed_first.text
    );
    let listed_second = session.call_tool("list_devices", remote_args(&second, json!({})));
    assert!(!listed_second.is_error, "{}", listed_second.text);
    assert!(
        listed_second.text.contains(&shared),
        "{}",
        listed_second.text
    );
    assert!(
        listed_second
            .text
            .contains(&format!("remote {second_address}:{second_port}")),
        "{}",
        listed_second.text
    );

    let opened_first = session.call_tool(
        "connect",
        remote_args(&first, json!({ "path": shared, "request_control": true })),
    );
    assert!(!opened_first.is_error, "{}", opened_first.text);
    let opened_second = session.call_tool(
        "connect",
        remote_args(&second, json!({ "path": shared, "request_control": true })),
    );
    assert!(!opened_second.is_error, "{}", opened_second.text);
    // Both cores hand out the same id for the same path: only the endpoint may
    // separate the two sessions.
    assert_eq!(opened_first.device_id(), opened_second.device_id());

    let sent_first = session.call_tool(
        "send",
        remote_args(
            &first,
            json!({ "device": shared, "text": "FIRST-ONLY", "lines": 0 }),
        ),
    );
    assert!(!sent_first.is_error, "{}", sent_first.text);
    assert_eq!(
        first_device.read_n(10, Duration::from_secs(5)),
        b"FIRST-ONLY"
    );
    assert!(
        second_device
            .read_for(Duration::from_millis(300))
            .is_empty(),
        "the first host's write must not reach the second host's device"
    );

    let sent_second = session.call_tool(
        "send",
        remote_args(
            &second,
            json!({ "device": shared, "text": "SECOND-ONLY", "lines": 0 }),
        ),
    );
    assert!(!sent_second.is_error, "{}", sent_second.text);
    assert_eq!(
        second_device.read_n(11, Duration::from_secs(5)),
        b"SECOND-ONLY"
    );
    assert!(
        first_device.read_for(Duration::from_millis(300)).is_empty(),
        "the second host's write must not reach the first host's device"
    );

    // History belongs to the endpoint that wrote it.
    let read_first = session.call_tool(
        "read",
        remote_args(&first, json!({ "device": shared, "lines": 10 })),
    );
    assert!(!read_first.is_error, "{}", read_first.text);
    assert!(
        read_first.text.contains("FIRST-ONLY"),
        "{}",
        read_first.text
    );
    assert!(
        !read_first.text.contains("SECOND-ONLY"),
        "{}",
        read_first.text
    );
    let read_second = session.call_tool(
        "read",
        remote_args(&second, json!({ "device": shared, "lines": 10 })),
    );
    assert!(!read_second.is_error, "{}", read_second.text);
    assert!(
        read_second.text.contains("SECOND-ONLY"),
        "{}",
        read_second.text
    );
    assert!(
        !read_second.text.contains("FIRST-ONLY"),
        "{}",
        read_second.text
    );
    assert_no_secret(
        &session,
        &format!("{}{}", read_first.text, read_second.text),
        &first.info,
    );
}

#[test]
fn an_abrupt_core_loss_during_a_remote_write_is_uncertain_and_not_replayed() {
    // The host process dies without closing anything politely: the client sees
    // an abrupt transport loss while its write is unanswered. The call must
    // report an uncertain outcome, must not replay the bytes and must not fall
    // back to a local device.
    let directory = TempDir::new().expect("core directory");
    let mut device = SimulatedDevice::new();
    let (mut core, socket) = start_core_process(directory.path());
    let host = SharedHost::start_at(
        directory,
        socket,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        std::slice::from_ref(&device.path),
    );
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);
    let connected = session.call_tool(
        "connect",
        remote_args(
            &host,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!connected.is_error, "{}", connected.text);
    let id = connected.device_id();

    let request = session.start_request(
        "tools/call",
        json!({
            "name": "send",
            "arguments": remote_args(&host, json!({
                "device": id,
                "text": "ABRUPT-LOSS",
                "lines": 1,
                "timeout_ms": 30_000,
            })),
        }),
    );
    assert_eq!(
        device.read_n(11, Duration::from_secs(5)),
        b"ABRUPT-LOSS",
        "the accepted write reaches the device before the host disappears"
    );
    core.kill().expect("kill the host process");
    core.wait().expect("reap the host process");

    let response = session.response_for(&request);
    let (text, is_error) = reply(&response);
    assert!(is_error, "{text}");
    assert!(
        text.to_lowercase().contains("uncertain")
            || text.to_lowercase().contains("may have reached"),
        "{}",
        text
    );
    assert!(
        text.to_lowercase().contains("not sent again")
            || text.to_lowercase().contains("not replayed"),
        "{}",
        text
    );
    assert_no_secret(&session, &text, &host.info);
    assert!(
        device.read_for(Duration::from_millis(300)).is_empty(),
        "the write must not be replayed after the transport loss"
    );

    // The endpoint is gone: a later remote call fails and names no local
    // device, and the local core is not silently used in its place.
    let after = session.call_tool("list_devices", remote_args(&host, json!({})));
    assert!(after.is_error, "{}", after.text);
    assert!(!after.text.contains(&device.path), "{}", after.text);
    assert_no_secret(&session, &after.text, &host.info);
    assert!(
        device.read_for(Duration::from_millis(200)).is_empty(),
        "no fallback write may reach the device"
    );
}

#[test]
fn the_stderr_capture_sees_writes_while_the_server_is_still_running() {
    // The secrecy assertions read stderr before the server exits, so the
    // capture must publish as it arrives rather than at EOF.
    let (_directory, socket) = start_core(Duration::from_secs(30));
    let session = spawn(&socket, Path::new("/nonexistent/peers.json"));
    let marker = "SER14-STDERR-MARKER";
    {
        let mut stderr = std::fs::OpenOptions::new()
            .append(true)
            .open(format!("/proc/{}/fd/2", session.pid()))
            .expect("open the running server's stderr");
        stderr.write_all(marker.as_bytes()).expect("write marker");
        stderr.flush().ok();
    }
    wait_until(Duration::from_secs(5), || {
        session.stderr_text().contains(marker)
    });
    assert!(session.stderr_text().contains(marker));
}

#[test]
fn remote_listing_uses_the_canonical_path_for_a_host_symlink_alias() {
    let mut device = SimulatedDevice::new();
    let aliases = TempDir::new().expect("host alias directory");
    let alias = aliases.path().join("board-by-id");
    std::os::unix::fs::symlink(&device.path, &alias).expect("host alias symlink");
    let alias = alias.to_string_lossy().into_owned();
    // The host terminal itself opens and checks the alias; the core returns
    // the canonical serial-node path in discovery.
    let host = SharedHost::start(std::slice::from_ref(&alias));
    let peers = write_peers(&host.directory, vec![peer_entry(&host.info, &host.info.ai)]);
    let mut session = spawn(&host.socket, &peers);
    let listed = session.call_tool("list_devices", remote_args(&host, json!({})));
    assert!(!listed.is_error, "{}", listed.text);
    assert!(
        listed.text.contains(&device.path),
        "canonical path missing: {}",
        listed.text
    );
    assert!(
        !listed.text.contains(&alias),
        "host alias should not replace canonical path: {}",
        listed.text
    );

    // A user can pass the canonical path that discovery returned; remote
    // tools must target that host node rather than assuming the input alias is
    // the same string the core reports.
    let connected = session.call_tool(
        "connect",
        remote_args(
            &host,
            json!({ "path": device.path, "request_control": true }),
        ),
    );
    assert!(!connected.is_error, "{}", connected.text);
    let sent = session.call_tool(
        "send",
        remote_args(
            &host,
            json!({ "device": connected.device_id(), "text": "CANONICAL", "lines": 0 }),
        ),
    );
    assert!(!sent.is_error, "{}", sent.text);
    assert_eq!(device.read_n(9, Duration::from_secs(5)), b"CANONICAL");
}
