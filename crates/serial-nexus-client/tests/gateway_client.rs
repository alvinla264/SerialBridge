use std::io::{BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serial_nexus_client::{Client, ClientInfo, GatewayConfig, GatewayServer};
use serial_nexus_protocol::{
    decode_bytes, decode_line, encode_line, read_line, ControlState, GatewayConnect,
    GatewayHandshake, GatewayTarget, HelloResult, Limits, OpenResult, Request, RequestMessage,
    ResponseData, Role, ServerMessage, GATEWAY_PROTOCOL_VERSION, MAX_GATEWAY_MESSAGE_BYTES,
    MAX_MESSAGE_BYTES, PROTOCOL_VERSION,
};
use tempfile::tempdir;

#[test]
fn gateway_server_rejects_an_unsupported_protocol_version() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gateway.sock");
    let _server = GatewayServer::start(GatewayConfig {
        socket: path.clone(),
        core_socket: dir.path().join("missing-core.sock"),
    })
    .unwrap();
    let mut stream = UnixStream::connect(&path).unwrap();
    stream
        .write_all(
            &encode_line(&GatewayConnect {
                protocol: GATEWAY_PROTOCOL_VERSION + 1,
                target: GatewayTarget::Local,
                pairing_file: None,
            })
            .unwrap(),
        )
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let line = read_line(&mut reader, MAX_GATEWAY_MESSAGE_BYTES)
        .unwrap()
        .unwrap();
    let acknowledgement: GatewayHandshake = decode_line(&line).unwrap();
    assert_eq!(acknowledgement.protocol, GATEWAY_PROTOCOL_VERSION);
    assert!(acknowledgement
        .error
        .unwrap()
        .contains("gateway protocol mismatch"));
}

#[test]
fn closing_an_ipc_client_releases_its_upstream_connection() {
    let dir = tempdir().unwrap();
    let core_path = dir.path().join("core.sock");
    let gateway_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&core_path).unwrap();
    listener.set_nonblocking(true).unwrap();

    let running = Arc::new(AtomicBool::new(true));
    let active = Arc::new(AtomicUsize::new(0));
    let handlers = Arc::new(Mutex::new(Vec::new()));
    let server_running = Arc::clone(&running);
    let server_active = Arc::clone(&active);
    let server_handlers = Arc::clone(&handlers);
    let core = thread::spawn(move || {
        while server_running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    server_active.fetch_add(1, Ordering::SeqCst);
                    let active = Arc::clone(&server_active);
                    server_handlers.lock().unwrap().push(thread::spawn(move || {
                        let mut writer = stream;
                        let mut reader = BufReader::new(writer.try_clone().unwrap());
                        let line = read_line(&mut reader, MAX_MESSAGE_BYTES).unwrap().unwrap();
                        let hello: RequestMessage = decode_line(&line).unwrap();
                        assert!(matches!(hello.request, Request::Hello(_)));
                        writer
                            .write_all(
                                &encode_line(&ServerMessage::Response {
                                    id: hello.id,
                                    result: ResponseData::Hello(HelloResult {
                                        client_id: 9,
                                        protocol: PROTOCOL_VERSION,
                                        core_version: "gateway-test".into(),
                                        socket: "mock-core".into(),
                                        limits: Limits {
                                            read_default_lines: 50,
                                            read_max_lines: 2000,
                                            write_default_timeout_ms: 1000,
                                            write_max_timeout_ms: 30_000,
                                            control_idle_ms: 300_000,
                                            history_max_lines: 2000,
                                        },
                                    }),
                                })
                                .unwrap(),
                            )
                            .unwrap();
                        let mut byte = [0u8; 1];
                        while reader.read(&mut byte).unwrap_or(0) != 0 {}
                        active.fetch_sub(1, Ordering::SeqCst);
                    }));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("mock core accept: {error}"),
            }
        }
        for handler in server_handlers.lock().unwrap().drain(..) {
            handler.join().unwrap();
        }
    });

    let gateway = GatewayServer::start(GatewayConfig {
        socket: gateway_path.clone(),
        core_socket: core_path,
    })
    .unwrap();
    let client = Client::connect_to_gateway(
        &gateway_path,
        GatewayTarget::Local,
        ClientInfo::ai("session-lifecycle-test"),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while active.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(active.load(Ordering::SeqCst), 1);

    drop(client);
    let deadline = Instant::now() + Duration::from_secs(2);
    while active.load(Ordering::SeqCst) != 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        active.load(Ordering::SeqCst),
        0,
        "closing the MCP-side gateway socket must drop its upstream core client"
    );

    drop(gateway);
    running.store(false, Ordering::SeqCst);
    core.join().unwrap();
}

#[test]
fn dropping_gateway_makes_an_inflight_write_uncertain_without_replay() {
    let dir = tempdir().unwrap();
    let core_path = dir.path().join("core.sock");
    let gateway_path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&core_path).unwrap();
    let request_seen = Arc::new(AtomicBool::new(false));
    let upstream_closed = Arc::new(AtomicBool::new(false));
    let server_request_seen = Arc::clone(&request_seen);
    let server_upstream_closed = Arc::clone(&upstream_closed);
    let core = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let line = read_line(&mut reader, MAX_MESSAGE_BYTES).unwrap().unwrap();
        let hello: RequestMessage = decode_line(&line).unwrap();
        assert!(matches!(hello.request, Request::Hello(_)));
        stream
            .write_all(
                &encode_line(&ServerMessage::Response {
                    id: hello.id,
                    result: ResponseData::Hello(HelloResult {
                        client_id: 11,
                        protocol: PROTOCOL_VERSION,
                        core_version: "gateway-shutdown-test".into(),
                        socket: "mock-core".into(),
                        limits: Limits {
                            read_default_lines: 50,
                            read_max_lines: 2000,
                            write_default_timeout_ms: 1000,
                            write_max_timeout_ms: 30_000,
                            control_idle_ms: 300_000,
                            history_max_lines: 2000,
                        },
                    }),
                })
                .unwrap(),
            )
            .unwrap();
        let line = read_line(&mut reader, MAX_MESSAGE_BYTES).unwrap().unwrap();
        let open: RequestMessage = decode_line(&line).unwrap();
        assert!(matches!(open.request, Request::Open(_)));
        stream
            .write_all(
                &encode_line(&ServerMessage::Response {
                    id: open.id,
                    result: ResponseData::Opened(OpenResult {
                        device_id: 1,
                        port: "mock-device".into(),
                        baud: 9_600,
                        role: Role::Controller,
                        control: ControlState {
                            holder: None,
                            expires_in_ms: None,
                        },
                    }),
                })
                .unwrap(),
            )
            .unwrap();
        let line = read_line(&mut reader, MAX_MESSAGE_BYTES).unwrap().unwrap();
        let request: RequestMessage = decode_line(&line).unwrap();
        let Request::Write(write) = request.request else {
            panic!("expected one upstream write request");
        };
        assert_eq!(decode_bytes(&write.data_b64).unwrap(), b"single-attempt");
        server_request_seen.store(true, Ordering::SeqCst);
        let duplicate = read_line(&mut reader, MAX_MESSAGE_BYTES).unwrap();
        assert!(duplicate.is_none(), "gateway replayed a submitted write");
        server_upstream_closed.store(true, Ordering::SeqCst);
    });

    let gateway = GatewayServer::start(GatewayConfig {
        socket: gateway_path.clone(),
        core_socket: core_path,
    })
    .unwrap();
    let client = Client::connect_to_gateway(
        &gateway_path,
        GatewayTarget::Local,
        ClientInfo::ai("gateway-shutdown-test"),
    )
    .unwrap();
    let opened = client.open("mock-device", Some(9_600), true).unwrap();
    let request_client = client.clone();
    let request = thread::spawn(move || {
        request_client.write_bytes(opened.device_id, b"single-attempt", None)
    });
    let deadline = Instant::now() + Duration::from_secs(2);
    while !request_seen.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        request_seen.load(Ordering::SeqCst),
        "upstream request was not submitted"
    );

    drop(gateway);
    let deadline = Instant::now() + Duration::from_secs(2);
    while !upstream_closed.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        upstream_closed.load(Ordering::SeqCst),
        "dropping the gateway must shut down an upstream with a pending request"
    );
    let error = request.join().unwrap().unwrap_err();
    assert!(
        error.is_write_uncertain(),
        "write must remain uncertain: {error}"
    );
    assert!(!client.is_connected());
    drop(client);
    core.join().unwrap();
}

#[test]
fn gateway_server_refuses_oversized_prefaces() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gateway.sock");
    let _server = GatewayServer::start(GatewayConfig {
        socket: path.clone(),
        core_socket: dir.path().join("missing-core.sock"),
    })
    .unwrap();
    let mut stream = UnixStream::connect(&path).unwrap();
    stream
        .write_all(&vec![b'x'; MAX_GATEWAY_MESSAGE_BYTES + 1])
        .unwrap();
    let mut reader = BufReader::new(stream);
    let line = read_line(&mut reader, MAX_GATEWAY_MESSAGE_BYTES)
        .unwrap()
        .unwrap();
    let acknowledgement: GatewayHandshake = decode_line(&line).unwrap();
    assert!(acknowledgement
        .error
        .unwrap()
        .contains("invalid gateway handshake"));
}

#[test]
fn gateway_protocol_mismatch_is_reported_before_core_hello() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let line = read_line(&mut reader, MAX_GATEWAY_MESSAGE_BYTES)
            .unwrap()
            .unwrap();
        let _: GatewayConnect = decode_line(&line).unwrap();
        stream
            .write_all(
                &encode_line(&GatewayHandshake {
                    protocol: GATEWAY_PROTOCOL_VERSION,
                    error: Some("gateway protocol mismatch: use protocol 2".into()),
                })
                .unwrap(),
            )
            .unwrap();
    });

    let error =
        Client::connect_to_gateway(&path, GatewayTarget::Local, ClientInfo::ai("version-test"))
            .unwrap_err();
    assert!(error.to_string().contains("gateway protocol mismatch"));
    server.join().unwrap();
}

#[test]
fn write_reply_lost_with_gateway_is_reported_as_uncertain() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let _ = read_line(&mut reader, MAX_GATEWAY_MESSAGE_BYTES).unwrap();
        stream
            .write_all(
                &encode_line(&GatewayHandshake {
                    protocol: GATEWAY_PROTOCOL_VERSION,
                    error: None,
                })
                .unwrap(),
            )
            .unwrap();
        let line = read_line(&mut reader, MAX_GATEWAY_MESSAGE_BYTES)
            .unwrap()
            .unwrap();
        let hello: RequestMessage = decode_line(&line).unwrap();
        stream
            .write_all(
                &encode_line(&ServerMessage::Response {
                    id: hello.id,
                    result: ResponseData::Hello(HelloResult {
                        client_id: 1,
                        protocol: PROTOCOL_VERSION,
                        core_version: "test".into(),
                        socket: "test-gateway".into(),
                        limits: Limits {
                            read_default_lines: 50,
                            read_max_lines: 2000,
                            write_default_timeout_ms: 1000,
                            write_max_timeout_ms: 30_000,
                            control_idle_ms: 300_000,
                            history_max_lines: 2000,
                        },
                    }),
                })
                .unwrap(),
            )
            .unwrap();
        let line = read_line(&mut reader, MAX_GATEWAY_MESSAGE_BYTES)
            .unwrap()
            .unwrap();
        let write: RequestMessage = decode_line(&line).unwrap();
        assert!(matches!(write.request, Request::Write(_)));
        // The gateway has accepted a write but exits before its answer arrives.
    });

    let client = Client::connect_to_gateway(
        &path,
        GatewayTarget::Local,
        ClientInfo::ai("uncertain-write-test"),
    )
    .unwrap();
    let error = client.write_bytes(7, b"ON", None).unwrap_err();
    assert!(error.is_write_uncertain(), "{error:?}");
    server.join().unwrap();
}

#[test]
fn gateway_client_sends_versioned_remote_target_before_the_core_hello() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("gateway.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server_path = path.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let line = read_line(&mut reader, MAX_GATEWAY_MESSAGE_BYTES)
            .unwrap()
            .unwrap();
        let connect: GatewayConnect = decode_line(&line).unwrap();
        assert_eq!(connect.protocol, GATEWAY_PROTOCOL_VERSION);
        assert_eq!(
            connect.target,
            GatewayTarget::Remote {
                address: "192.0.2.9".parse().unwrap(),
                port: 7443,
                scope_id: 0,
            }
        );
        stream
            .write_all(
                &encode_line(&GatewayHandshake {
                    protocol: GATEWAY_PROTOCOL_VERSION,
                    error: None,
                })
                .unwrap(),
            )
            .unwrap();

        let line = read_line(&mut reader, MAX_GATEWAY_MESSAGE_BYTES)
            .unwrap()
            .unwrap();
        let hello: RequestMessage = decode_line(&line).unwrap();
        assert!(matches!(hello.request, Request::Hello(_)));
        let response = ServerMessage::Response {
            id: hello.id,
            result: ResponseData::Hello(HelloResult {
                client_id: 44,
                protocol: PROTOCOL_VERSION,
                core_version: "test".into(),
                socket: server_path.display().to_string(),
                limits: Limits {
                    read_default_lines: 50,
                    read_max_lines: 2000,
                    write_default_timeout_ms: 1000,
                    write_max_timeout_ms: 30_000,
                    control_idle_ms: 300_000,
                    history_max_lines: 2000,
                },
            }),
        };
        stream
            .try_clone()
            .unwrap()
            .write_all(&encode_line(&response).unwrap())
            .unwrap();
    });

    let client = Client::connect_to_gateway(
        &path,
        GatewayTarget::Remote {
            address: "192.0.2.9".parse().unwrap(),
            port: 7443,
            scope_id: 0,
        },
        ClientInfo::ai("gateway-test"),
    )
    .unwrap();
    assert_eq!(client.hello().client_id, 44);
    drop(client);
    server.join().unwrap();
}
