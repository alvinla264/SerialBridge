//! End-to-end tests: the built `serial-nexus-mcp` binary, spoken to over its real
//! stdio framing, against a real core and simulated serial devices (PTY pairs).

mod support;

use std::time::Duration;

use serde_json::{json, Value};
use serial_nexus_client::{Client, ClientInfo};
use support::{start_core, McpSession, SimulatedDevice, ToolReply};

fn connect_with_control(mcp: &mut McpSession, port: &str) -> ToolReply {
    let reply = mcp.call_tool("connect", json!({"port": port, "request_control": true}));
    assert!(!reply.is_error, "connect failed: {}", reply.text);
    reply
}

/// Poll `read` until a line containing `needle` appears.
fn read_until(mcp: &mut McpSession, device_id: u64, needle: &str) -> ToolReply {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let reply = mcp.call_tool("read", json!({"device": device_id, "lines": 50}));
        if !reply.is_error && reply.text.contains(needle) {
            return reply;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "history never contained {needle:?}: {}",
            reply.text
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn local_tool_without_gateway_does_not_autostart_core() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("core.sock");
    let marker = directory.path().join("core-started");
    let fake_core = directory.path().join("fake-core");
    std::fs::write(
        &fake_core,
        format!("#!/bin/sh\ntouch '{}'\nsleep 5\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake_core, std::fs::Permissions::from_mode(0o700)).unwrap();
    let fake_core = fake_core.to_string_lossy().into_owned();
    let mut mcp =
        McpSession::spawn_without_gateway_with_env(&socket, &[("SERIAL_NEXUS_CORE", &fake_core)]);

    let result = mcp.call_tool("list_devices", json!({}));
    assert!(result.is_error, "{}", result.text);
    assert!(result.text.contains("gateway"), "{}", result.text);
    assert!(
        !marker.exists(),
        "MCP must not start the configured core binary"
    );
    assert!(!socket.exists(), "MCP must not create the core socket");
}

#[test]
fn local_tool_requires_terminal_gateway_even_when_core_is_running() {
    let (_directory, socket) = start_core(Duration::from_secs(30));
    let mut mcp = McpSession::spawn_without_gateway(&socket);

    let result = mcp.call_tool("list_devices", json!({}));
    assert!(
        result.is_error,
        "local calls without the terminal must fail"
    );
    assert!(
        result.text.contains("gateway"),
        "unexpected local failure: {}",
        result.text
    );
}

#[test]
fn stdio_handshake_and_tools_list() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut mcp = McpSession::spawn(&socket);

    let initialize = mcp.request("ping", json!({}));
    assert_eq!(initialize["result"], json!({}));

    let tools = mcp.tools();
    let mut names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    names.sort();
    assert_eq!(
        names,
        ["connect", "disconnect", "list_devices", "read", "send"]
    );
    for tool in &tools {
        assert!(
            tool["description"]
                .as_str()
                .is_some_and(|text| !text.is_empty()),
            "tool without description: {tool}"
        );
        assert_eq!(
            tool["inputSchema"]["type"], "object",
            "tool schema not an object: {tool}"
        );
        let mut properties: Vec<&str> = tool["inputSchema"]["properties"]
            .as_object()
            .expect("schema properties")
            .keys()
            .map(String::as_str)
            .collect();
        properties.sort_unstable();
        let expected: &[&str] = match tool["name"].as_str().unwrap() {
            "list_devices" => &["ip", "port"],
            "connect" => &["baud", "ip", "path", "port", "request_control"],
            "disconnect" => &["device", "ip", "port"],
            "read" => &["device", "ip", "lines", "port"],
            "send" => &["device", "ip", "lines", "port", "text", "timeout_ms"],
            name => panic!("unexpected tool {name}"),
        };
        assert_eq!(properties, expected, "tool schema changed: {tool}");
    }
}

#[test]
fn initialize_reports_a_supported_protocol_version() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut mcp = McpSession::spawn(&socket);
    let response = mcp.request(
        "initialize",
        json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"}
        }),
    );
    let result = &response["result"];
    assert_eq!(result["protocolVersion"], "2024-11-05");
    assert_eq!(result["serverInfo"]["name"], "serial-nexus-mcp");
    assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
}

#[test]
fn protocol_errors_are_clear_and_notifications_get_no_reply() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut mcp = McpSession::spawn(&socket);

    // A notification is never answered; the next thing on the wire is the
    // response to the request that follows it.
    mcp.notify("notifications/cancelled", json!({"requestId": 99}));
    let ping = mcp.request("ping", json!({}));
    assert_eq!(ping["result"], json!({}));

    mcp.send_raw("this is not json");
    let error = mcp.next_message();
    assert_eq!(error["error"]["code"], -32700, "parse error: {error}");
    assert!(error["id"].is_null());

    mcp.send_raw(&json!({"jsonrpc": "2.0", "id": 7, "method": "no/such"}).to_string());
    let error = mcp.response_for(&json!(7));
    assert_eq!(error["error"]["code"], -32601, "unknown method: {error}");

    mcp.send_raw(
        &json!({"jsonrpc": "2.0", "id": 8, "method": "tools/call",
                "params": {"name": "teleport", "arguments": {}}})
        .to_string(),
    );
    let error = mcp.response_for(&json!(8));
    assert_eq!(error["error"]["code"], -32602, "unknown tool: {error}");
    assert!(error["error"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("teleport")));

    // Envelopes JSON-RPC forbids are refused before any dispatch.
    mcp.send_raw(&json!({"jsonrpc": "2.0", "id": true, "method": "ping"}).to_string());
    let error = mcp.next_message();
    assert_eq!(error["error"]["code"], -32600, "bad id: {error}");
    assert!(error["id"].is_null());

    mcp.send_raw(&json!({"jsonrpc": "2.0", "id": 9, "method": "ping", "params": 42}).to_string());
    let error = mcp.response_for(&json!(9));
    assert_eq!(error["error"]["code"], -32600, "scalar params: {error}");
}

#[test]
fn misspelled_or_null_arguments_are_refused_without_side_effects() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);

    let typo = mcp.request(
        "tools/call",
        json!({
            "name": "connect",
            "arguments": {"port": device.path, "baud_rate": 9600, "request_control": true}
        }),
    );
    assert_eq!(typo["error"]["code"], -32602, "{typo}");
    assert!(typo["error"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("baud_rate")));

    let null = mcp.request(
        "tools/call",
        json!({
            "name": "connect",
            "arguments": {"port": device.path, "request_control": null}
        }),
    );
    assert_eq!(null["error"]["code"], -32602, "{null}");

    // Neither call may have opened the device.
    let listing = mcp.call_tool("list_devices", json!({}));
    assert!(!listing.is_error, "{}", listing.text);
    assert!(
        !listing.json()["devices"]
            .as_array()
            .expect("devices")
            .iter()
            .any(|entry| entry["path"] == device.path.as_str() && entry["open"] == true),
        "a rejected call opened the device: {}",
        listing.text
    );
}

#[test]
fn list_devices_shows_the_connected_device() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);

    let listing = mcp.call_tool("list_devices", json!({}));
    assert!(!listing.is_error, "{}", listing.text);
    assert!(
        !listing.json()["devices"]
            .as_array()
            .expect("device array")
            .iter()
            .any(|entry| entry["path"] == device.path.as_str()),
        "a PTY is not a discovered serial port: {}",
        listing.text
    );

    let opened = connect_with_control(&mut mcp, &device.path);
    let device_id = opened.device_id();
    let listing = mcp.call_tool("list_devices", json!({}));
    let entry = listing.json()["devices"]
        .as_array()
        .expect("device array")
        .iter()
        .find(|entry| entry["path"] == opened.json()["port"])
        .unwrap_or_else(|| panic!("open device missing from {}", listing.text));
    assert_eq!(entry["open"], true);
    assert_eq!(entry["device_id"], device_id);
    assert_eq!(entry["baud"], 115_200);
}

#[test]
fn connect_defaults_baud_and_repeating_it_keeps_control() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);

    let opened = connect_with_control(&mut mcp, &device.path);
    assert_eq!(opened.json()["baud"], 115_200);
    assert_eq!(opened.json()["role"], "controller");
    assert_eq!(opened.json()["write_control"], true);
    assert!(opened.text.contains("115200"), "{}", opened.text);
    assert!(opened.text.contains("granted"), "{}", opened.text);

    // Repeating connect without disconnecting keeps the same device and the
    // control it already holds.
    let again = connect_with_control(&mut mcp, &device.path);
    assert_eq!(again.device_id(), opened.device_id());
    assert_eq!(again.json()["role"], "controller");
    assert_eq!(again.json()["write_control"], true);

    // Opening a second device in the same session is independent.
    let other = SimulatedDevice::new();
    let second = connect_with_control(&mut mcp, &other.path);
    assert_ne!(second.device_id(), opened.device_id());
    assert_eq!(second.json()["write_control"], true);
}

#[test]
fn second_session_is_read_only_and_conflicting_baud_is_rejected() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut controller = McpSession::spawn(&socket);
    let mut reader = McpSession::spawn(&socket);

    let opened = controller.call_tool(
        "connect",
        json!({"port": device.path, "baud": 9600, "request_control": true}),
    );
    assert!(!opened.is_error, "{}", opened.text);
    assert_eq!(opened.json()["baud"], 9600);

    let conflict = reader.call_tool("connect", json!({"port": device.path, "baud": 19_200}));
    assert!(conflict.is_error, "conflicting baud was accepted");
    assert!(
        conflict.text.contains("9600"),
        "conflict should name the active rate: {}",
        conflict.text
    );

    // Omitted baud joins at the active rate, read-only because someone else
    // holds write control.
    let joined = reader.call_tool("connect", json!({"port": device.path}));
    assert!(!joined.is_error, "{}", joined.text);
    assert_eq!(joined.device_id(), opened.device_id());
    assert_eq!(joined.json()["baud"], 9600);
    assert_eq!(joined.json()["role"], "reader");
    assert_eq!(joined.json()["write_control"], false);
    assert!(
        joined.text.contains("read-only"),
        "read-only must be explicit: {}",
        joined.text
    );
    assert!(
        joined.text.contains("write control"),
        "the holder must be named: {}",
        joined.text
    );

    // Writing without control is a clear failure, not a silent drop.
    let denied = reader.call_tool("send", json!({"device": opened.device_id(), "text": "x"}));
    assert!(denied.is_error, "read-only session wrote: {}", denied.text);
    assert!(
        denied.text.to_lowercase().contains("write control"),
        "{}",
        denied.text
    );
}

#[test]
fn read_reports_sources_partial_lines_and_truncation() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);
    let device_id = opened.device_id();

    device.write(b"boot ok\n");
    read_until(&mut mcp, device_id, "[device] boot ok");

    let human = Client::connect_to_socket(&socket, ClientInfo::human("test-human")).expect("human");
    let human_open = human
        .open(device.path.clone(), None, false)
        .expect("human open");
    human
        .write_bytes(human_open.device_id, b"help\n", None)
        .expect("human write");

    let sent = mcp.call_tool(
        "send",
        json!({"device": device_id, "text": "abc", "lines": 0}),
    );
    assert!(!sent.is_error, "{}", sent.text);

    // The AI write has no newline, so the core keeps it as a partial line.
    let history = read_until(&mut mcp, device_id, "[human] help");
    assert!(
        history.text.contains("[device] boot ok"),
        "{}",
        history.text
    );
    assert!(
        history.text.contains("[ai] (partial) abc"),
        "{}",
        history.text
    );
    let sources: Vec<&str> = history.json()["lines"]
        .as_array()
        .expect("lines")
        .iter()
        .map(|line| line["source"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(sources, ["device", "human", "ai"]);

    // A tail without a newline is reported as partial, in place.
    device.write(b"login: ");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let history = mcp.call_tool("read", json!({"device": device_id, "lines": 10}));
        let lines = history.json()["lines"].as_array().expect("lines").clone();
        if lines
            .iter()
            .any(|line| line["text"] == "login: " && line["partial"] == true)
        {
            assert!(
                history.text.contains("(partial)"),
                "partial lines must be marked: {}",
                history.text
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "partial line never appeared: {}",
            history.text
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // Asking for fewer lines reports nothing beyond what fits; `truncated`
    // says the core had to trim.
    let small = mcp.call_tool("read", json!({"device": device_id, "lines": 1}));
    assert!(!small.is_error, "{}", small.text);
    assert_eq!(small.json()["lines"].as_array().expect("lines").len(), 1);
    assert!(small.json()["truncated"].is_boolean());
}

#[test]
fn send_writes_exact_bytes_without_a_newline() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);

    let reply = mcp.call_tool(
        "send",
        json!({"device": opened.device_id(), "text": "ver\r", "lines": 0}),
    );
    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(reply.json()["bytes_written"], 4);
    assert_eq!(device.read_n(4, Duration::from_secs(2)), b"ver\r");
    assert_eq!(
        device.read_for(Duration::from_millis(150)),
        b"",
        "send must not append a newline"
    );

    // A newline inside the text is sent exactly as an escaped JSON string.
    let reply = mcp.call_tool(
        "send",
        json!({"device": opened.device_id(), "text": "a\nb", "lines": 0}),
    );
    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(reply.json()["bytes_written"], 3);
    assert_eq!(device.read_n(3, Duration::from_secs(2)), b"a\nb");
}

#[test]
fn send_waits_for_the_requested_device_output_lines() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);
    let device_id = opened.device_id();

    let mut master = device.master_handle();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        use std::io::Write as _;
        master.write_all(b"part1\npart2\n").expect("device output");
        master.flush().ok();
    });

    let reply = mcp.call_tool(
        "send",
        json!({"device": device_id, "text": "ver\r", "lines": 2, "timeout_ms": 5_000}),
    );
    writer.join().expect("writer");

    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(reply.json()["timed_out"], false);
    let lines = reply.json()["lines"].as_array().expect("lines");
    let texts: Vec<&str> = lines
        .iter()
        .map(|line| line["text"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(texts, ["part1", "part2"]);
    assert!(lines.iter().all(|line| line["source"] == "device"));
    assert!(reply.text.contains("part1"), "{}", reply.text);
    assert_eq!(device.read_n(4, Duration::from_secs(2)), b"ver\r");
}

#[test]
fn send_timeout_returns_what_arrived_including_a_marked_partial_line() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);
    let device_id = opened.device_id();

    // No output at all: a clear timeout.
    let reply = mcp.call_tool(
        "send",
        json!({"device": device_id, "text": "ping", "lines": 1, "timeout_ms": 200}),
    );
    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(reply.json()["timed_out"], true);
    assert_eq!(reply.json()["lines"].as_array().expect("lines").len(), 0);
    assert!(
        reply.text.contains("timed out"),
        "timeouts must be reported: {}",
        reply.text
    );

    // Output without a newline after the send: a partial line is returned.
    let mut master = device.master_handle();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        use std::io::Write as _;
        master.write_all(b"prompt> ").expect("partial output");
        master.flush().ok();
    });
    let reply = mcp.call_tool(
        "send",
        json!({"device": device_id, "text": "login", "lines": 1, "timeout_ms": 400}),
    );
    writer.join().expect("writer");

    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(reply.json()["timed_out"], true);
    let lines = reply.json()["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 1, "{}", reply.text);
    assert_eq!(lines[0]["text"], "prompt> ");
    assert_eq!(lines[0]["partial"], true);
    assert!(
        reply.text.contains("timed out") && reply.text.contains("(partial)"),
        "{}",
        reply.text
    );
}

#[test]
fn disconnect_reports_release_and_later_calls_fail_clearly() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut controller = McpSession::spawn(&socket);
    let mut reader = McpSession::spawn(&socket);

    let opened = connect_with_control(&mut controller, &device.path);
    let device_id = opened.device_id();
    let joined = reader.call_tool("connect", json!({"port": device.path}));
    assert!(!joined.is_error, "{}", joined.text);
    assert_eq!(joined.json()["write_control"], false);

    let closed = controller.call_tool("disconnect", json!({"device": device_id}));
    assert!(!closed.is_error, "{}", closed.text);
    assert_eq!(closed.device_id(), device_id);
    assert_eq!(closed.json()["device_closed"], false);
    assert!(closed.text.contains("disconnected"), "{}", closed.text);

    // Disconnecting released control, so the other AI session can claim it.
    let claimed = reader.call_tool(
        "connect",
        json!({"port": device.path, "request_control": true}),
    );
    assert!(!claimed.is_error, "{}", claimed.text);
    assert_eq!(claimed.json()["write_control"], true);
    assert_eq!(claimed.json()["role"], "controller");

    // The first session is no longer attached; its reads say so.
    let read = reader.call_tool("read", json!({"device": device_id, "lines": 5}));
    assert!(!read.is_error, "{}", read.text);
    let stale = controller.call_tool("read", json!({"device": device_id, "lines": 5}));
    assert!(
        stale.is_error,
        "detached session read history: {}",
        stale.text
    );
    assert!(
        stale.text.to_lowercase().contains("connect"),
        "error should say to connect first: {}",
        stale.text
    );

    // Last client disconnecting closes the device.
    let closed = reader.call_tool("disconnect", json!({"device": claimed.device_id()}));
    assert!(!closed.is_error, "{}", closed.text);
    assert_eq!(closed.json()["device_closed"], true);
}

#[test]
fn device_errors_are_reported_not_hidden() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut mcp = McpSession::spawn(&socket);

    let missing = mcp.call_tool("connect", json!({"port": "/dev/serial-nexus-nonexistent"}));
    assert!(missing.is_error, "missing device was opened");
    assert!(
        missing.text.contains("serial-nexus-nonexistent"),
        "the error must name the port: {}",
        missing.text
    );

    let dev = SimulatedDevice::new();
    let opened = connect_with_control(&mut mcp, &dev.path);
    let device_id = opened.device_id();
    // Unplug: closing the PTY pair makes the core's serial I/O fail.
    drop(dev);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let read = mcp.call_tool("read", json!({"device": device_id, "lines": 5}));
        if read.is_error {
            assert!(
                read.text.to_lowercase().contains("device"),
                "error should mention the device: {}",
                read.text
            );
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "device failure was never reported: {}",
            read.text
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn mcp_never_autostarts_a_configured_core_without_the_terminal_gateway() {
    let directory = tempfile::TempDir::new().expect("tempdir");
    let socket = directory.path().join("no-gateway-core.sock");
    let core = std::path::Path::new(env!("CARGO_BIN_EXE_serial-nexus-mcp"))
        .parent()
        .expect("target dir")
        .join("serial-nexus-core");
    assert!(
        core.is_file(),
        "serial-nexus-core binary missing at {core:?}"
    );
    let mut mcp = McpSession::spawn_without_gateway_with_env(
        &socket,
        &[("SERIAL_NEXUS_CORE", core.to_str().expect("utf8 path"))],
    );
    let listing = mcp.call_tool("list_devices", json!({}));
    assert!(listing.is_error, "{}", listing.text);
    assert!(listing.text.contains("gateway"), "{}", listing.text);
    assert!(!socket.exists(), "MCP must not launch the configured core");
}

#[test]
fn integral_numbers_are_accepted_and_fractions_rejected() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);
    let device_id = opened.device_id();

    let read = mcp.call_tool("read", json!({"device": device_id as f64, "lines": 1.0}));
    assert!(!read.is_error, "1.0 should be an integer: {}", read.text);
    assert_eq!(read.device_id(), device_id);

    let fraction = mcp.request(
        "tools/call",
        json!({"name": "read", "arguments": {"device": 1.5}}),
    );
    assert_eq!(fraction["error"]["code"], -32602, "{fraction}");
    assert!(fraction["error"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("whole number")));
}

#[test]
fn fresh_device_output_is_not_labelled_as_predating_a_send() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);
    let device_id = opened.device_id();

    // Leave an AI-source partial line whose text matches the device output
    // that follows. It must not be mistaken for the device's tail.
    let sent = mcp.call_tool(
        "send",
        json!({"device": device_id, "text": "AT", "lines": 0}),
    );
    assert!(!sent.is_error, "{}", sent.text);

    let mut master = device.master_handle();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        use std::io::Write as _;
        master.write_all(b"AT").expect("device output");
        master.flush().ok();
    });
    // Two lines wanted, only one partial arrives: the core times out and
    // appends its current tail, which started after this send.
    let reply = mcp.call_tool(
        "send",
        json!({"device": device_id, "text": "cmd", "lines": 2, "timeout_ms": 300}),
    );
    writer.join().expect("writer");

    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(reply.json()["timed_out"], true);
    let lines = reply.json()["lines"].as_array().expect("lines");
    let tail = lines
        .iter()
        .find(|line| line["text"] == "AT")
        .unwrap_or_else(|| panic!("fresh tail missing: {}", reply.text));
    assert_eq!(tail["source"], "device");
    assert_eq!(tail["partial"], true);
    assert_eq!(tail["predates_send"], false);
    assert!(
        !reply.text.contains("present before this send"),
        "fresh output was called old: {}",
        reply.text
    );
}

#[test]
fn pipelined_sends_reach_the_device_in_arrival_order() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);
    let device_id = opened.device_id();

    let commands: Vec<String> = (0..50).map(|index| format!("{index:03};")).collect();
    let ids: Vec<Value> = commands
        .iter()
        .map(|text| {
            mcp.start_request(
                "tools/call",
                json!({
                    "name": "send",
                    "arguments": {"device": device_id, "text": text, "lines": 0}
                }),
            )
        })
        .collect();
    for id in &ids {
        let response = mcp.response_for(id);
        assert!(response["result"]["isError"] != true, "{response}");
    }

    let expected = commands.concat();
    let received = device.read_n(expected.len(), Duration::from_secs(5));
    assert_eq!(
        String::from_utf8_lossy(&received),
        expected,
        "pipelined sends were reordered on the wire"
    );
}

#[test]
fn batches_are_answered_and_notifications_are_silent() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut mcp = McpSession::spawn(&socket);

    let batch = json!([
        {"jsonrpc": "2.0", "id": "a", "method": "ping"},
        {"jsonrpc": "2.0", "method": "notifications/initialized"},
        {"jsonrpc": "2.0", "id": "b", "method": "tools/list"}
    ]);
    mcp.send_raw(&batch.to_string());
    let response = mcp.next_message();
    let responses = response.as_array().expect("batch response array");
    assert_eq!(responses.len(), 2, "{response}");
    assert_eq!(responses[0]["id"], "a");
    assert_eq!(responses[1]["id"], "b");

    // A notification-only batch produces no response at all.
    mcp.send_raw(&json!([{"jsonrpc": "2.0", "method": "notifications/initialized"}]).to_string());
    let ping = mcp.request("ping", json!({}));
    assert_eq!(ping["result"], json!({}), "{ping}");
}

#[test]
fn a_device_stays_addressable_by_the_path_used_to_connect() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);
    let device_id = opened.device_id();

    let (directory, name) = device.path.rsplit_once('/').expect("path");
    let alias = format!("{directory}/./{name}");
    assert_ne!(alias, device.path);

    let read = mcp.call_tool("read", json!({"device": alias, "lines": 5}));
    assert!(!read.is_error, "alias read failed: {}", read.text);
    assert_eq!(read.device_id(), device_id);

    let sent = mcp.call_tool("send", json!({"device": alias, "text": "ping", "lines": 0}));
    assert!(!sent.is_error, "alias send failed: {}", sent.text);
    assert_eq!(device.read_n(4, Duration::from_secs(2)), b"ping");

    let closed = mcp.call_tool("disconnect", json!({"device": alias}));
    assert!(!closed.is_error, "alias disconnect failed: {}", closed.text);
    assert_eq!(closed.device_id(), device_id);
    assert_eq!(closed.json()["device_closed"], true);
}

#[test]
fn timeout_distinguishes_an_older_partial_tail_from_new_output() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let mut device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);
    let device_id = opened.device_id();

    // The device's prompt predates the send and stays unfinished.
    device.write(b"OLD> ");
    read_until(&mut mcp, device_id, "OLD>");

    let reply = mcp.call_tool(
        "send",
        json!({"device": device_id, "text": "cmd", "lines": 1, "timeout_ms": 150}),
    );
    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(reply.json()["timed_out"], true);
    let lines = reply.json()["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["text"], "OLD> ");
    assert_eq!(lines[0]["partial"], true);
    assert_eq!(lines[0]["predates_send"], true);
    assert!(
        reply.text.contains("present before this send"),
        "an old prompt must not look like a response: {}",
        reply.text
    );
}

#[test]
fn an_oversized_line_count_is_capped_and_reported() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);
    let opened = connect_with_control(&mut mcp, &device.path);

    let reply = mcp.call_tool(
        "send",
        json!({
            "device": opened.device_id(),
            "text": "x",
            "lines": 5_000,
            "timeout_ms": 100
        }),
    );
    assert!(!reply.is_error, "{}", reply.text);
    assert_eq!(reply.json()["waited_for_lines"], 2_000);
    assert_eq!(reply.json()["requested_lines"], 5_000);
    assert_eq!(reply.json()["lines_capped"], true);
    assert!(reply.text.contains("capped"), "{}", reply.text);
}

#[test]
fn structured_content_matches_the_text_for_every_tool() {
    let (_dir, socket) = start_core(Duration::from_secs(300));
    let device = SimulatedDevice::new();
    let mut mcp = McpSession::spawn(&socket);

    let connected = connect_with_control(&mut mcp, &device.path);
    let reply: Value = connected.json().clone();
    assert!(reply.get("device_id").is_some());
    assert!(reply.get("baud").is_some());
    assert!(reply.get("role").is_some());
    assert!(reply.get("write_control").is_some());

    let listed = mcp.call_tool("list_devices", json!({}));
    assert!(listed.json()["devices"].is_array());
    let read = mcp.call_tool("read", json!({"device": connected.device_id()}));
    assert!(read.json()["lines"].is_array());
    assert!(read.json()["dropped_lines"].is_u64());
    assert!(read.json()["truncated"].is_boolean());
    let sent = mcp.call_tool(
        "send",
        json!({"device": connected.device_id(), "text": "x", "lines": 0}),
    );
    assert!(sent.json()["bytes_written"].is_u64());
    assert!(sent.json()["timed_out"].is_boolean());
}
