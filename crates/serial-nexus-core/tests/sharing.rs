//! End-to-end tests for opt-in LAN sharing: a real core, real TCP+TLS
//! listeners on loopback, the real remote client adapter, PTY serial devices
//! and the existing local Unix-socket clients.

mod support;

use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serial_nexus_client::{Client, ClientError, ClientInfo, RemoteClient};
use serial_nexus_protocol::{
    ErrorCode, Event, PairedHost, PairingRole, ReadResult, ResponseData, ServerMessage, ShareBind,
    ShareSessionInfo, WaitSpec,
};
use support::{connect, wait_until, SimulatedDevice};
use tempfile::TempDir;

struct SharedCore {
    _dir: TempDir,
    socket: PathBuf,
}

fn shared_core() -> SharedCore {
    shared_core_with(|_config| {})
}

fn shared_core_with(adjust: impl FnOnce(&mut serial_nexus_core::CoreConfig)) -> SharedCore {
    let directory = TempDir::new().expect("tempdir");
    let socket = directory.path().join("core.sock");
    let mut config = serial_nexus_core::CoreConfig {
        socket: socket.clone(),
        grace: Duration::from_secs(30),
        control_idle: Duration::from_secs(300),
        history_max_lines: 500,
        history_max_bytes: 128 * 1024,
        share_handshake_timeout: Duration::from_secs(3),
        share_auth_timeout: Duration::from_secs(3),
        ..serial_nexus_core::CoreConfig::default()
    };
    adjust(&mut config);
    std::thread::spawn(move || {
        if let Err(error) = serial_nexus_core::server::run(config) {
            eprintln!("test core failed: {error}");
        }
    });
    wait_until(Duration::from_secs(5), || {
        std::os::unix::net::UnixStream::connect(&socket).is_ok()
    });
    SharedCore {
        _dir: directory,
        socket,
    }
}

fn local_owner(core: &SharedCore) -> Client {
    connect(&core.socket, ClientInfo::human("owner-terminal"))
}

/// A loopback port that was free a moment ago.
fn free_port() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("probe port");
    listener.local_addr().expect("probe address").port()
}

fn localhost_bind() -> ShareBind {
    ShareBind::new(IpAddr::V4(Ipv4Addr::LOCALHOST), free_port())
}

fn remote(
    info: &ShareSessionInfo,
    role: PairingRole,
    name: &str,
) -> Result<RemoteClient, ClientError> {
    let secret = match role {
        PairingRole::Human => &info.human,
        PairingRole::Ai => &info.ai,
    };
    RemoteClient::connect(
        &info.host,
        secret,
        Some(name.to_string()),
        Duration::from_secs(5),
    )
}

fn wait_disconnected(client: &RemoteClient) {
    wait_until(Duration::from_secs(5), || !client.is_connected());
}

#[test]
fn sharing_is_off_until_the_owner_starts_it_and_secrets_go_only_to_them() {
    let core = shared_core();
    let mut device = SimulatedDevice::new();
    let owner = local_owner(&core);
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open locally");
    let other = connect(&core.socket, ClientInfo::human("other-local"));

    assert!(!owner.sharing_status().expect("status").active);

    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start sharing");
    assert!(!info.human.credential.is_empty());
    assert!(!info.ai.credential.is_empty());
    assert_ne!(info.human.credential, info.ai.credential);
    assert_eq!(info.human.role, PairingRole::Human);
    assert_eq!(info.ai.role, PairingRole::Ai);
    assert!(info.host.host_cert_fingerprint_sha256.len() == 64);
    assert_eq!(info.bind.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));

    // The snapshot is what the owner's UI shows: no secrets in it.
    let snapshot = owner.sharing_status().expect("status");
    assert!(snapshot.active);
    assert_eq!(snapshot.checked.len(), 1);
    let snapshot_json = serde_json::to_string(&snapshot).expect("snapshot json");
    assert!(!snapshot_json.contains(&info.human.credential));
    assert!(!snapshot_json.contains(&info.ai.credential));
    assert!(!format!("{:?}", info.human).contains(&info.human.credential));

    // A second local client cannot control or even see sharing controls it
    // did not start.
    assert!(other.start_sharing(localhost_bind(), vec![]).is_err());
    assert!(other.sharing_status().is_err());
    assert!(other.stop_sharing().is_err());

    // The owner cannot start a second listener while one is live.
    assert!(owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .is_err());

    // Remote clients get exactly the checked device, and can use it.
    let human = remote(&info, PairingRole::Human, "remote-human").expect("pair");
    let devices = human.list_devices().expect("remote list");
    assert_eq!(devices.devices.len(), 1);
    assert_eq!(devices.devices[0].path, device.path);

    let remote_open = human
        .open(&device.path, None, false)
        .expect("remote open after list");
    human
        .write_bytes(remote_open.device_id, b"hello", None)
        .expect("remote write");
    device.read_until(b"hello", Duration::from_secs(2));

    owner.stop_sharing().expect("stop sharing");
    assert!(!owner.sharing_status().expect("status").active);
    wait_disconnected(&human);
    // The old credentials cannot be used against a new session either.
    let info2 = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start again");
    let old_host = PairedHost {
        address: info2.host.address,
        host_name: info2.host.host_name.clone(),
        ca_cert_der_b64: info2.host.ca_cert_der_b64.clone(),
        host_cert_fingerprint_sha256: info2.host.host_cert_fingerprint_sha256.clone(),
    };
    assert!(RemoteClient::connect(&old_host, &info.human, None, Duration::from_secs(3)).is_err());
}

#[test]
fn remote_operations_are_limited_to_checked_devices_by_acl() {
    let core = shared_core();
    let owner = local_owner(&core);
    let shared = SimulatedDevice::new();
    let hidden = SimulatedDevice::new();
    owner
        .open(&shared.path, Some(115_200), false)
        .expect("open shared");
    let hidden_open = owner
        .open(&hidden.path, Some(115_200), false)
        .expect("open hidden");

    let info = owner
        .start_sharing(localhost_bind(), vec![shared.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");

    let listed = human.list_devices().expect("list");
    assert_eq!(listed.devices.len(), 1);
    assert_eq!(listed.devices[0].path, shared.path);

    // Not merely absent from the list: every operation on it is refused.
    let denied = human
        .open(&hidden.path, Some(115_200), false)
        .expect_err("hidden device open");
    assert!(matches!(
        denied.code(),
        Some(ErrorCode::PermissionDenied) | Some(ErrorCode::NotFound)
    ));
    assert!(human.read(hidden_open.device_id, Some(5)).is_err());
    assert!(human
        .write_bytes(hidden_open.device_id, b"x", None)
        .is_err());
    assert!(human.status(hidden_open.device_id).is_err());
    assert!(human.close(hidden_open.device_id).is_err());

    // Sharing controls are local-only, even for an authenticated human.
    let control = human.read(0, None).expect_err("no device 0");
    assert!(!format!("{control:?}").contains(&info.human.credential));
}

#[test]
fn role_is_derived_from_the_credential_and_ai_control_stays_exclusive() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    let local_ai = connect(&core.socket, ClientInfo::ai("local-ai"));
    let control = local_ai
        .open(&device.path, Some(115_200), true)
        .expect("local control");
    assert_eq!(control.role, serial_nexus_client::Role::Controller);

    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");

    // The AI credential gets an AI role even if it claims to be a human, and
    // it cannot hold control while the local AI does.
    let ai = remote(&info, PairingRole::Ai, "remote-ai").expect("pair");
    assert_eq!(ai.role(), PairingRole::Ai);
    let joined = ai
        .open(&device.path, Some(115_200), true)
        .expect("remote ai open");
    assert_eq!(joined.role, serial_nexus_client::Role::Reader);
    let denied = ai
        .write_bytes(joined.device_id, b"ai", None)
        .expect_err("control held");
    assert_eq!(denied.code(), Some(ErrorCode::ControlHeld));

    // Human credentials may write beside the AI holder.
    let human = remote(&info, PairingRole::Human, "remote-human").expect("pair");
    let human_open = human
        .open(&device.path, None, false)
        .expect("remote human open");
    human
        .write_bytes(human_open.device_id, b"human", None)
        .expect("human write");
    device.read_until(b"human", Duration::from_secs(2));

    // Releasing local control lets exactly one remote AI take it.
    local_ai.close(control.device_id).expect("release control");
    wait_until(Duration::from_secs(2), || {
        human
            .status(human_open.device_id)
            .map(|status| status.control.holder.is_none())
            .unwrap_or(false)
    });
    let second_ai = remote(&info, PairingRole::Ai, "remote-ai-2").expect("pair");
    let second = second_ai
        .open(&device.path, None, true)
        .expect("second ai open");
    assert_eq!(second.role, serial_nexus_client::Role::Controller);
    second_ai
        .write_bytes(second.device_id, b"ai-write", None)
        .expect("remote ai write");
    device.read_until(b"ai-write", Duration::from_secs(2));

    let first = ai.open(&device.path, None, true).expect("first ai rejoins");
    assert_eq!(first.role, serial_nexus_client::Role::Reader);
}

#[test]
fn unchecking_revokes_remote_access_immediately_and_keeps_other_sessions() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut first = SimulatedDevice::new();
    let mut second = SimulatedDevice::new();
    let local_human = connect(&core.socket, ClientInfo::human("local-human"));
    let first_local = local_human
        .open(&first.path, Some(115_200), false)
        .expect("open first");
    local_human
        .open(&second.path, Some(115_200), false)
        .expect("open second");

    let info = owner
        .start_sharing(
            localhost_bind(),
            vec![first.path.clone(), second.path.clone()],
        )
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let first_remote = human.open(&first.path, None, false).expect("remote first");
    let second_remote = human
        .open(&second.path, None, false)
        .expect("remote second");
    let events = human.subscribe();

    owner
        .uncheck_shared_devices(vec![first.path.clone()])
        .expect("uncheck first");

    // First device: revoke now, no pending remote work survives.
    let denied = human
        .read(first_remote.device_id, Some(5))
        .expect_err("revoked");
    assert!(matches!(
        denied.code(),
        Some(ErrorCode::PermissionDenied) | Some(ErrorCode::NotAttached)
    ));
    assert!(human
        .write_bytes(first_remote.device_id, b"x", None)
        .is_err());
    // Second device and the local human are untouched.
    human
        .write_bytes(second_remote.device_id, b"still-shared", None)
        .expect("second write");
    second.read_until(b"still-shared", Duration::from_secs(2));
    local_human
        .write_bytes(first_local.device_id, b"local-still-works", None)
        .expect("local write");
    first.read_until(b"local-still-works", Duration::from_secs(2));

    // Remote events for the unchecked device stop as well.
    while let Ok(event) = events.try_recv() {
        assert_ne!(event.device_id(), Some(first_remote.device_id));
    }
    first.write(b"after-uncheck\n");
    std::thread::sleep(Duration::from_millis(200));
    while let Ok(event) = events.try_recv() {
        assert_ne!(event.device_id(), Some(first_remote.device_id));
    }

    // A pending waited write fails when its device is unchecked.
    let device_id = second_remote.device_id;
    let pending = std::thread::spawn(move || {
        human.write_bytes(
            device_id,
            b"long-wait",
            Some(WaitSpec {
                lines: 3,
                timeout_ms: Some(3_000),
            }),
        )
    });
    second.read_until(b"long-wait", Duration::from_secs(2));
    owner
        .uncheck_shared_devices(vec![second.path.clone()])
        .expect("uncheck second");
    let outcome = pending.join().expect("thread");
    assert!(outcome.is_err(), "pending remote work must fail");
}

#[test]
fn losing_the_owner_connection_stops_the_listener_and_disconnects_remotes() {
    let core = shared_core();
    let device = SimulatedDevice::new();
    let owner = local_owner(&core);
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    human.open(&device.path, None, false).expect("open remote");
    let address = info.host.address;

    drop(owner);

    wait_disconnected(&human);
    // The listener is gone: a fresh TCP connection is refused.
    wait_until(Duration::from_secs(3), || {
        TcpStream::connect_timeout(&address, Duration::from_millis(200)).is_err()
    });

    // Sharing is off until explicitly enabled again, with new credentials.
    let new_owner = local_owner(&core);
    assert!(!new_owner.sharing_status().expect("status").active);
    wait_until(Duration::from_secs(3), || {
        new_owner.open(&device.path, Some(115_200), false).is_ok()
    });
    let fresh = new_owner
        .start_sharing(
            ShareBind::new(info.bind.ip(), free_port()),
            vec![device.path.clone()],
        )
        .expect("restart");
    assert_ne!(fresh.session_id, info.session_id);
    assert_ne!(fresh.human.credential, info.human.credential);
    assert!(RemoteClient::connect(&fresh.host, &info.human, None, Duration::from_secs(3)).is_err());
}

#[test]
fn a_bind_failure_reports_an_error_and_leaves_sharing_off() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("occupy");
    let port = listener.local_addr().expect("addr").port();
    let error = owner
        .start_sharing(
            ShareBind::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            vec![device.path.clone()],
        )
        .expect_err("bind must fail");
    assert!(!error.to_string().contains("credential"));
    assert!(!owner.sharing_status().expect("status").active);

    drop(listener);
    let info = owner
        .start_sharing(
            ShareBind::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
            vec![device.path.clone()],
        )
        .expect("start after freeing the port");
    remote(&info, PairingRole::Human, "remote").expect("pair");
}

#[test]
fn wildcard_binding_is_refused() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    for address in ["0.0.0.0", "::", "::ffff:0.0.0.0"] {
        let error = owner
            .start_sharing(
                ShareBind::new(address.parse().unwrap(), free_port()),
                vec![device.path.clone()],
            )
            .expect_err("wildcard");
        assert!(error.to_string().contains("wildcard"), "{error}");
    }
    assert!(!owner.sharing_status().expect("status").active);
}

#[test]
fn ipv6_loopback_is_supported_when_the_platform_has_it() {
    if TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)).is_err() {
        eprintln!("not exercised: this platform has no IPv6 loopback");
        return;
    }
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let probe = TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, 0)).expect("probe");
    let port = probe.local_addr().expect("addr").port();
    drop(probe);
    let info = owner
        .start_sharing(
            ShareBind::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), port),
            vec![device.path.clone()],
        )
        .expect("start on ::1");
    assert!(info.bind.is_ipv6());
    let human = remote(&info, PairingRole::Human, "remote-v6").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");
    human
        .write_bytes(opened.device_id, b"v6", None)
        .expect("write");
    device.read_until(b"v6", Duration::from_secs(2));
}

#[test]
fn a_client_that_cannot_verify_the_host_is_refused() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");

    // Correct CA, wrong pinned leaf fingerprint: explicit host auth failure.
    let mut tampered = info.host.clone();
    tampered.host_cert_fingerprint_sha256 = "00".repeat(32);
    let error = RemoteClient::connect(&tampered, &info.human, None, Duration::from_secs(3))
        .expect_err("fingerprint mismatch");
    assert!(matches!(error, ClientError::HostRejected(_)), "{error}");

    // A different CA cannot be talked into trusting the host either.
    let other_core = shared_core();
    let other_owner = local_owner(&other_core);
    let other_device = SimulatedDevice::new();
    other_owner
        .open(&other_device.path, Some(115_200), false)
        .expect("open");
    let other_info = other_owner
        .start_sharing(localhost_bind(), vec![other_device.path.clone()])
        .expect("start elsewhere");
    let mut wrong_ca = info.host.clone();
    wrong_ca.ca_cert_der_b64 = other_info.host.ca_cert_der_b64.clone();
    assert!(RemoteClient::connect(&wrong_ca, &info.human, None, Duration::from_secs(3)).is_err());

    // The real pairing still works.
    remote(&info, PairingRole::Human, "remote").expect("pair");
}

#[test]
fn bad_credentials_and_oversized_frames_are_refused() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");

    let mut wrong = info.human.clone();
    wrong.credential = "not-the-right-secret".to_string();
    let error = RemoteClient::connect(&info.host, &wrong, None, Duration::from_secs(3))
        .expect_err("wrong credential");
    assert!(matches!(error, ClientError::Authentication(_)), "{error}");

    // A raw TLS peer that sends an over-limit frame length is disconnected.
    let (mut connection, mut tcp) = raw_tls(&info);
    tcp.write_all(&u32::MAX.to_be_bytes())
        .expect("write header");
    connection.writer().flush().ok();
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut closed = false;
    while Instant::now() < deadline {
        let mut buffer = [0u8; 256];
        match connection.reader().read(&mut buffer) {
            Ok(0) => {
                closed = true;
                break;
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                closed = true;
                break;
            }
        }
        if serial_nexus_protocol::share::tlsio::pump(
            &mut connection,
            &mut tcp,
            &mut Default::default(),
        )
        .is_err()
        {
            closed = true;
            break;
        }
    }
    assert!(closed, "oversized frame must close the connection");
}

#[test]
fn connection_limits_refuse_new_peers_without_disturbing_existing_ones() {
    let core = shared_core_with(|config| config.share_max_connections = 1);
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let first = remote(&info, PairingRole::Human, "first").expect("first pair");

    // A second peer is refused while the first holds the only slot.
    assert!(RemoteClient::connect(
        &info.host,
        &info.human,
        Some("second".into()),
        Duration::from_secs(2)
    )
    .is_err());

    // The existing connection still works.
    first.list_devices().expect("first still works");
    drop(first);
    wait_until(Duration::from_secs(3), || {
        RemoteClient::connect(
            &info.host,
            &info.human,
            Some("third".into()),
            Duration::from_secs(2),
        )
        .is_ok()
    });
}

#[test]
fn stopping_sharing_during_a_waited_write_fails_it_without_replaying() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    let device_id = opened.device_id;
    let writer = human.clone();
    let pending = std::thread::spawn(move || {
        writer.write_bytes(
            device_id,
            b"once",
            Some(WaitSpec {
                lines: 1,
                timeout_ms: Some(5_000),
            }),
        )
    });
    device.read_until(b"once", Duration::from_secs(2));
    owner.stop_sharing().expect("stop");

    let result = pending.join().expect("thread");
    assert!(result.is_err(), "the wait must not survive shutdown");
    let error = result.unwrap_err();
    assert!(
        matches!(error, ClientError::WriteUncertain(_)),
        "a lost write answer must report uncertainty, got {error:?}"
    );
    assert!(error.to_string().contains("not replayed"), "{error}");
    // No automatic replay: exactly one copy of the bytes reached the device.
    let leftovers = device.read_available(Duration::from_millis(200));
    assert!(
        !String::from_utf8_lossy(&leftovers).contains("once"),
        "bytes were replayed: {leftovers:?}"
    );
    wait_disconnected(&human);
}

#[test]
fn path_reuse_does_not_inherit_authorization() {
    let core = shared_core();
    let owner = local_owner(&core);
    let directory = TempDir::new().expect("tempdir");
    let link = directory.path().join("stable-device");
    let first = SimulatedDevice::new();
    std::os::unix::fs::symlink(&first.path, &link).expect("symlink");
    let link_text = link.to_string_lossy().into_owned();
    owner
        .open(&link_text, Some(115_200), false)
        .expect("open via link");

    let info = owner
        .start_sharing(localhost_bind(), vec![link_text.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    human.open(&link_text, None, false).expect("open shared");

    // Replace the device behind the same path with a different one.
    let second = SimulatedDevice::new();
    std::fs::remove_file(&link).expect("remove link");
    std::os::unix::fs::symlink(&second.path, &link).expect("retarget link");

    let error = human
        .open(&link_text, None, false)
        .expect_err("replaced device must not inherit the authorization");
    assert!(
        matches!(
            error.code(),
            Some(ErrorCode::PermissionDenied) | Some(ErrorCode::DeviceFailed)
        ),
        "{error:?}"
    );
    // Explicit rechecking (with the new device) restores access. Opening it
    // locally first is what makes a PTY "detected" in this test.
    owner
        .open(&link_text, Some(115_200), false)
        .expect("open replacement locally");
    owner
        .check_shared_devices(vec![link_text.clone()])
        .expect("recheck");
    human
        .open(&link_text, None, false)
        .expect("open after recheck");
}

#[test]
fn secrets_never_appear_in_remote_responses_events_or_history() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let events = human.subscribe();
    let opened = human.open(&device.path, None, false).expect("open");
    human
        .write_bytes(opened.device_id, b"data\n", None)
        .expect("write");
    device.write(b"answer\n");
    device.read_until(b"data\n", Duration::from_secs(2));

    let mut seen = String::new();
    let listed = human.list_devices().expect("list");
    seen.push_str(&serde_json::to_string(&listed).expect("json"));
    let read: ReadResult = human.read(opened.device_id, Some(50)).expect("read");
    seen.push_str(&serde_json::to_string(&read).expect("json"));
    seen.push_str(&serde_json::to_string(human.auth()).expect("json"));
    while let Ok(event) = events.try_recv() {
        seen.push_str(&serde_json::to_string(&event).expect("json"));
    }
    assert!(!seen.contains(&info.human.credential));
    assert!(!seen.contains(&info.ai.credential));
    assert!(!format!("{:?}", human).contains(&info.human.credential));
}

#[test]
fn local_unix_clients_keep_working_while_sharing_is_active() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    let local_ai = connect(&core.socket, ClientInfo::ai("local-ai"));
    let local = local_ai
        .open(&device.path, Some(115_200), true)
        .expect("local open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let remote = remote(&info, PairingRole::Human, "remote").expect("pair");
    let remote_open = remote.open(&device.path, None, false).expect("remote open");

    // Both write, local control is still held by the local AI, and history is
    // shared as before.
    remote
        .write_bytes(remote_open.device_id, b"local-and-remote", None)
        .expect("remote write");
    device.read_until(b"local-and-remote", Duration::from_secs(2));
    local_ai
        .write_bytes(local.device_id, b"local", None)
        .expect("local write");
    device.read_until(b"local", Duration::from_secs(2));
    let history = local_ai.read(local.device_id, Some(50)).expect("read");
    assert!(history
        .lines
        .iter()
        .any(|line| line.text.contains("local-and-remote")));
    assert_eq!(
        local_ai
            .status(local.device_id)
            .expect("status")
            .control
            .holder
            .expect("holder")
            .client_id,
        local_ai.hello().client_id
    );
}

/// TLS handshake only, for frame-level and deadline tests.
fn raw_handshake(info: &ShareSessionInfo) -> (rustls::Connection, TcpStream) {
    let ca = serial_nexus_protocol::decode_bytes(&info.host.ca_cert_der_b64).expect("ca");
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(ca))
        .expect("root");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name: IpAddr = info.host.host_name.parse().expect("ip");
    let mut tcp =
        TcpStream::connect_timeout(&info.host.address, Duration::from_secs(2)).expect("tcp");
    tcp.set_read_timeout(Some(Duration::from_millis(25))).ok();
    tcp.set_write_timeout(Some(Duration::from_millis(25))).ok();
    let mut connection = rustls::Connection::Client(
        rustls::ClientConnection::new(
            Arc::new(config),
            rustls::pki_types::ServerName::IpAddress(name.into()),
        )
        .expect("client connection"),
    );
    serial_nexus_protocol::share::tlsio::handshake(
        &mut connection,
        &mut tcp,
        Instant::now() + Duration::from_secs(3),
        &mut Default::default(),
    )
    .expect("handshake");
    (connection, tcp)
}

fn send_auth(
    connection: &mut rustls::Connection,
    tcp: &mut TcpStream,
    secret: &serial_nexus_protocol::PairingSecret,
    client: Option<serial_nexus_protocol::ClientInfo>,
) {
    let auth = serial_nexus_protocol::RemoteAuth {
        credential: secret.credential.clone(),
        protocol: serial_nexus_protocol::SHARE_PROTOCOL_VERSION,
        client,
    };
    let frame = serial_nexus_protocol::encode_frame(&auth).expect("frame");
    serial_nexus_protocol::share::tlsio::write_frame(
        connection,
        tcp,
        &frame,
        Instant::now() + Duration::from_secs(3),
    )
    .expect("auth");
}

fn read_auth_result(
    connection: &mut rustls::Connection,
    tcp: &mut TcpStream,
    timeout: Duration,
) -> Option<serial_nexus_protocol::RemoteAuthResult> {
    let mut decoder = serial_nexus_protocol::FrameDecoder::new();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(Some(body)) = decoder.next_frame() {
            return serde_json::from_slice(&body).ok();
        }
        match serial_nexus_protocol::share::tlsio::pump(connection, tcp, &mut decoder) {
            Ok(serial_nexus_protocol::share::tlsio::TlsRead::Closed) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

/// A TLS client that authenticates, for frame-level tests.
fn raw_tls(info: &ShareSessionInfo) -> (rustls::Connection, TcpStream) {
    let (mut connection, mut tcp) = raw_handshake(info);
    send_auth(&mut connection, &mut tcp, &info.human, None);
    read_auth_result(&mut connection, &mut tcp, Duration::from_secs(3)).expect("auth result");
    (connection, tcp)
}

#[test]
fn an_unplugged_shared_device_is_reported_and_revoked() {
    let core = shared_core();
    let owner = local_owner(&core);
    let first = SimulatedDevice::new();
    let mut second = SimulatedDevice::new();
    owner
        .open(&first.path, Some(115_200), false)
        .expect("open first");
    owner
        .open(&second.path, Some(115_200), false)
        .expect("open second");
    let first_path = first.path.clone();
    let info = owner
        .start_sharing(
            localhost_bind(),
            vec![first.path.clone(), second.path.clone()],
        )
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let failed = human.open(&first.path, None, false).expect("open first");
    let healthy = human.open(&second.path, None, false).expect("open second");
    let events = human.subscribe();

    drop(first);
    let closed = support::wait_event(&events, Duration::from_secs(5), |event| {
        matches!(
            event,
            Event::DeviceClosed { device_id, .. } if *device_id == failed.device_id
        )
    });
    assert!(matches!(closed, Event::DeviceClosed { .. }));

    // The failed device is no longer usable or shared; the other one is.
    assert!(human.read(failed.device_id, Some(5)).is_err());
    wait_until(Duration::from_secs(3), || {
        !owner
            .sharing_status()
            .expect("status")
            .checked
            .iter()
            .any(|device| device.path == first_path)
    });
    human
        .write_bytes(healthy.device_id, b"still-here", None)
        .expect("healthy device keeps working");
    second.read_until(b"still-here", Duration::from_secs(2));

    // Reusing the old path needs an explicit check and a real device there.
    assert!(owner
        .check_shared_devices(vec![first_path.clone()])
        .is_err());
}

#[test]
fn a_replacement_at_the_same_path_does_not_inherit_authorization() {
    let core = shared_core();
    let owner = local_owner(&core);
    let holder = connect(&core.socket, ClientInfo::human("holder"));
    let first = SimulatedDevice::new();
    let first_path = first.path.clone();
    holder
        .open(&first.path, Some(115_200), false)
        .expect("open first");
    let info = owner
        .start_sharing(localhost_bind(), vec![first.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    human.open(&first.path, None, false).expect("open shared");

    // Close every engine attachment, then destroy the PTY so its node (and
    // likely its path) is recycled. The checked identity pins the old node.
    human.close(1).ok();
    holder.close(1).ok();
    drop(holder);
    wait_until(Duration::from_secs(3), || {
        owner
            .list_devices()
            .expect("list")
            .devices
            .iter()
            .all(|device| device.path != first_path)
    });
    drop(first);

    // Allocate a replacement, trying until the kernel reuses the same path.
    let mut replacement = None;
    for _ in 0..30 {
        let candidate = SimulatedDevice::new();
        if candidate.path == first_path {
            replacement = Some(candidate);
            break;
        }
    }
    let Some(replacement) = replacement else {
        eprintln!(
            "not exercised: the kernel did not reuse {first_path}; identity pinning is covered by the symlink test and unit identities"
        );
        return;
    };

    // The replacement must not inherit the old authorization.
    let error = human
        .open(&first_path, None, false)
        .expect_err("a replacement must not inherit authorization");
    assert!(
        matches!(
            error.code(),
            Some(ErrorCode::PermissionDenied) | Some(ErrorCode::DeviceFailed)
        ),
        "{error:?}"
    );

    // An explicit recheck of the replacement restores access.
    owner
        .open(&replacement.path, Some(115_200), false)
        .expect("open replacement");
    owner
        .check_shared_devices(vec![first_path.clone()])
        .expect("recheck replacement");
    human
        .open(&first_path, None, false)
        .expect("replacement after recheck");
}

#[test]
fn an_ai_credential_cannot_claim_a_human_role() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");

    // Authenticate with the AI credential while claiming to be a human.
    let (mut connection, mut tcp) = raw_handshake(&info);
    send_auth(
        &mut connection,
        &mut tcp,
        &info.ai,
        Some(serial_nexus_protocol::ClientInfo::human("spoof")),
    );
    let auth =
        read_auth_result(&mut connection, &mut tcp, Duration::from_secs(3)).expect("auth result");
    assert_eq!(auth.role, PairingRole::Ai);

    // The engine treats it as an AI: no write without control.
    let frame = serial_nexus_protocol::encode_frame(&serial_nexus_protocol::RequestMessage {
        id: 1,
        request: serial_nexus_protocol::Request::Open(serial_nexus_protocol::OpenParams {
            port: device.path.clone(),
            baud: None,
            request_control: false,
        }),
    })
    .expect("frame");
    serial_nexus_protocol::share::tlsio::write_frame(
        &mut connection,
        &mut tcp,
        &frame,
        Instant::now() + Duration::from_secs(3),
    )
    .expect("open");
    let mut decoder = serial_nexus_protocol::FrameDecoder::new();
    let opened = read_response(&mut connection, &mut tcp, &mut decoder, 1).expect("open answer");
    let device_id = match opened {
        ServerMessage::Response {
            result: ResponseData::Opened(opened),
            ..
        } => opened.device_id,
        other => panic!("unexpected open answer: {other:?}"),
    };
    let frame = serial_nexus_protocol::encode_frame(&serial_nexus_protocol::RequestMessage {
        id: 2,
        request: serial_nexus_protocol::Request::Write(serial_nexus_protocol::WriteParams {
            device_id,
            data_b64: serial_nexus_protocol::encode_bytes(b"spoofed"),
            wait: None,
        }),
    })
    .expect("frame");
    serial_nexus_protocol::share::tlsio::write_frame(
        &mut connection,
        &mut tcp,
        &frame,
        Instant::now() + Duration::from_secs(3),
    )
    .expect("write");
    let answer = read_response(&mut connection, &mut tcp, &mut decoder, 2).expect("write answer");
    match answer {
        ServerMessage::Error { error, .. } => assert_eq!(error.code, ErrorCode::ControlHeld),
        other => panic!("AI without control could write: {other:?}"),
    }
}

/// Read messages until the answer with `id`, skipping unsolicited events.
fn read_response(
    connection: &mut rustls::Connection,
    tcp: &mut TcpStream,
    decoder: &mut serial_nexus_protocol::FrameDecoder,
    id: u64,
) -> Option<ServerMessage> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        while let Ok(Some(body)) = decoder.next_frame() {
            let message: ServerMessage = serde_json::from_slice(&body).ok()?;
            match &message {
                ServerMessage::Response { id: answer, .. }
                | ServerMessage::Error { id: answer, .. }
                    if *answer == id =>
                {
                    return Some(message)
                }
                ServerMessage::Event { .. } => continue,
                _ => continue,
            }
        }
        match serial_nexus_protocol::share::tlsio::pump(connection, tcp, decoder) {
            Ok(serial_nexus_protocol::share::tlsio::TlsRead::Closed) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    None
}

#[test]
fn an_oversized_frame_over_authenticated_tls_closes_only_that_connection() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let (mut connection, mut tcp) = raw_tls(&info);

    connection
        .writer()
        .write_all(&u32::MAX.to_be_bytes())
        .expect("write header");
    let _ = serial_nexus_protocol::share::tlsio::flush(
        &mut connection,
        &mut tcp,
        Instant::now() + Duration::from_secs(3),
    );
    let mut decoder = serial_nexus_protocol::FrameDecoder::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut closed = false;
    while Instant::now() < deadline {
        match serial_nexus_protocol::share::tlsio::pump(&mut connection, &mut tcp, &mut decoder) {
            Ok(serial_nexus_protocol::share::tlsio::TlsRead::Closed) => {
                closed = true;
                break;
            }
            Ok(_) => {}
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    assert!(closed, "an oversized application frame must close the peer");

    // A fresh remote connection is unaffected.
    remote(&info, PairingRole::Human, "other").expect("pair");
}

#[test]
fn a_tls_peer_that_withholds_authentication_is_dropped_after_the_deadline() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let (mut connection, mut tcp) = raw_handshake(&info);

    let deadline = Instant::now() + Duration::from_secs(8);
    let mut closed = false;
    while Instant::now() < deadline {
        let mut decoder = serial_nexus_protocol::FrameDecoder::new();
        match serial_nexus_protocol::share::tlsio::pump(&mut connection, &mut tcp, &mut decoder) {
            Ok(serial_nexus_protocol::share::tlsio::TlsRead::Closed) => {
                closed = true;
                break;
            }
            Ok(_) => {}
            Err(_) => {
                closed = true;
                break;
            }
        }
    }
    assert!(closed, "withholding authentication must hit the deadline");
    remote(&info, PairingRole::Human, "other").expect("pair");
}

#[test]
fn a_slow_remote_peer_hits_the_bounded_serial_queue_and_queued_work_is_revoked() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    // Do not drain the PTY: the writer blocks after the small kernel buffer,
    // so most of the queue below is still waiting.
    for _ in 0..40 {
        human
            .write_bytes(opened.device_id, &[b'x'; 8 * 1024], None)
            .expect("queued write");
    }
    // A marker queued behind the backlog must never reach the device after a
    // successful uncheck: only the write already in progress may go.
    human
        .write_bytes(opened.device_id, b"FINAL", None)
        .expect("queue final marker");

    // Keep going until the bounded queue refuses work.
    let mut refused = false;
    for _ in 0..200 {
        match human.write_bytes(opened.device_id, &[b'x'; 8 * 1024], None) {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error.code(), Some(ErrorCode::LimitExceeded), "{error:?}");
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "the write queue must be bounded");
    owner
        .uncheck_shared_devices(vec![device.path.clone()])
        .expect("uncheck");
    let received = device.read_available(Duration::from_millis(300));
    assert!(
        !String::from_utf8_lossy(&received).contains("FINAL"),
        "a revoked remote write was still sent after uncheck"
    );
    human.list_devices().expect("connection stays usable");
}

#[test]
fn an_oversized_pending_history_answer_does_not_drop_the_connection() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    // ~120 KiB of history is under the local answer budget but over one
    // remote frame.
    for _ in 0..200 {
        device.write(format!("{}\r\n", "y".repeat(600)).as_bytes());
    }
    std::thread::sleep(Duration::from_millis(300));
    let result = human.read(opened.device_id, Some(2_000));
    match result {
        Ok(read) => assert!(read.truncated),
        Err(error) => assert_eq!(error.code(), Some(ErrorCode::BadRequest), "{error:?}"),
    }
    // The connection and its device attachment survive the large answer.
    human.list_devices().expect("connection still usable");
    human.status(opened.device_id).expect("still attached");
}

#[test]
fn a_checked_device_survives_an_ordinary_close_and_reopen() {
    let core = shared_core();
    let owner = local_owner(&core);
    let holder = connect(&core.socket, ClientInfo::human("holder"));
    let device = SimulatedDevice::new();
    let opened = holder
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");

    holder.close(opened.device_id).expect("last detach");
    wait_until(Duration::from_secs(3), || {
        !owner
            .list_devices()
            .expect("list")
            .devices
            .iter()
            .any(|entry| entry.path == device.path && entry.open)
    });
    // The device is still checked: closing is not a replacement.
    let status = owner.sharing_status().expect("status");
    assert_eq!(status.checked.len(), 1);
    assert_eq!(status.checked[0].path, device.path);

    // A remote client may open it again (this also clears the closure mark).
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    human
        .open(&device.path, None, false)
        .expect("reopen the same live device");
}

#[test]
fn a_checked_but_never_opened_device_rejects_a_recycled_node() {
    // Regular files give a deterministic "the pinned node was unlinked, a new
    // node appeared at the same path" without relying on kernel PTY recycling.
    let directory = TempDir::new().expect("tempdir");
    let path = directory.path().join("device-node");
    std::fs::write(&path, b"old").expect("old node");
    let path_text = path.to_string_lossy().into_owned();
    let identity =
        serial_nexus_core::device::DeviceIdentity::capture(&path_text).expect("identity");
    let prepared = serial_nexus_core::share::prepare(
        1,
        &serial_nexus_protocol::ShareStartParams {
            bind: localhost_bind(),
            devices: vec![path_text.clone()],
        },
        vec![identity],
        serial_nexus_core::share::ShareLimits {
            handshake_timeout: Duration::from_secs(1),
            auth_timeout: Duration::from_secs(1),
            max_connections: 1,
            max_pending_writes: 1,
        },
    )
    .expect("prepare");
    assert!(prepared.session.authorize(&path_text).is_ok());

    std::fs::remove_file(&path).expect("unlink pinned node");
    std::fs::write(&path, b"new").expect("replacement node");
    assert!(
        prepared.session.authorize(&path_text).is_err(),
        "a recycled node must not inherit authorization"
    );
    assert!(prepared.session.snapshot().checked.is_empty());
}

#[test]
fn stopping_sharing_drops_queued_remote_writes_before_the_writer_gets_them() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    // Backlog the serial writer, with a marker queued behind it.
    for _ in 0..40 {
        human
            .write_bytes(opened.device_id, &[b'x'; 8 * 1024], None)
            .expect("queued write");
    }
    human
        .write_bytes(opened.device_id, b"STOP-MARKER", None)
        .expect("queue marker");
    owner.stop_sharing().expect("stop");

    let received = device.read_available(Duration::from_millis(400));
    assert!(
        !String::from_utf8_lossy(&received).contains("STOP-MARKER"),
        "a queued remote write ran after sharing stopped"
    );
}

#[test]
fn a_rejected_write_is_not_added_to_history() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    let local = owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    // Fill the serial queue without draining the PTY.
    let mut refused = false;
    for _ in 0..200 {
        match human.write_bytes(opened.device_id, &[b'x'; 8 * 1024], None) {
            Ok(_) => {}
            Err(error) => {
                assert_eq!(error.code(), Some(ErrorCode::LimitExceeded));
                refused = true;
                break;
            }
        }
    }
    assert!(refused, "queue must be bounded");
    let rejected = human.write_bytes(opened.device_id, b"REJECTED-MARKER\n", None);
    assert!(rejected.is_err(), "the full queue must refuse the write");
    let history = owner.read(local.device_id, Some(500)).expect("read");
    assert!(
        !history
            .lines
            .iter()
            .any(|line| line.text.contains("REJECTED-MARKER")),
        "a rejected write must not be recorded as written"
    );
}

#[test]
fn an_expired_entry_does_not_leak_events_for_a_later_device_instance() {
    let core = shared_core();
    let owner = local_owner(&core);
    let first = SimulatedDevice::new();
    let first_path = first.path.clone();
    owner.open(&first.path, Some(115_200), false).expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![first.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let failed = human.open(&first.path, None, false).expect("open");
    let events = human.subscribe();

    drop(first);
    support::wait_event(&events, Duration::from_secs(5), |event| {
        matches!(
            event,
            Event::DeviceClosed { device_id, .. } if *device_id == failed.device_id
        )
    });
    wait_until(Duration::from_secs(3), || {
        owner.sharing_status().expect("status").checked.is_empty()
    });

    // A later instance at the same path is not shared: its closure must not
    // reach the remote client.
    let mut replacement = None;
    for _ in 0..30 {
        let candidate = SimulatedDevice::new();
        if candidate.path == first_path {
            replacement = Some(candidate);
            break;
        }
    }
    let Some(replacement) = replacement else {
        eprintln!("not exercised: the kernel did not reuse {first_path}");
        return;
    };
    let local = owner
        .open(&replacement.path, Some(115_200), false)
        .expect("local open without rechecking");
    owner.close(local.device_id).ok();
    std::thread::sleep(Duration::from_millis(300));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(
                event,
                Event::DeviceClosed { device_id, .. } if device_id != failed.device_id
            ),
            "an unshared device instance leaked a closure event: {event:?}"
        );
    }
}

#[test]
fn queued_writes_from_a_detached_remote_client_are_revoked() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    // Queue a long backlog and a marker with the PTY not being drained.
    for _ in 0..40 {
        human
            .write_bytes(opened.device_id, &[b'x'; 8 * 1024], None)
            .expect("queued write");
    }
    human
        .write_bytes(opened.device_id, b"DETACHED-MARKER", None)
        .expect("queue marker");
    // The client detaches from the device, then the owner unchecks it.
    human.close(opened.device_id).expect("detach");
    owner
        .uncheck_shared_devices(vec![device.path.clone()])
        .expect("uncheck");

    let received = device.read_available(Duration::from_millis(300));
    assert!(
        !String::from_utf8_lossy(&received).contains("DETACHED-MARKER"),
        "a queued write from a detached remote client ran after uncheck"
    );
}

#[test]
fn an_oversized_waited_write_answer_stays_a_successful_write() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    let waiting = human.clone();
    let device_id = opened.device_id;
    let pending = std::thread::spawn(move || {
        waiting.write_bytes(
            device_id,
            b"EXECUTED-ONCE",
            Some(WaitSpec {
                lines: 200,
                timeout_ms: Some(5_000),
            }),
        )
    });
    device.read_until(b"EXECUTED-ONCE", Duration::from_secs(3));
    for _ in 0..200 {
        device.write(format!("{}\r\n", "z".repeat(600)).as_bytes());
    }
    let written = pending
        .join()
        .expect("thread")
        .expect("the write succeeded and its answer must say so");
    assert_eq!(written.bytes_written, b"EXECUTED-ONCE".len());
    assert!(written.truncated, "the answer must be trimmed to fit");
    assert!(!written.timed_out);
}

#[test]
fn unchecked_and_new_session_devices_are_not_discoverable() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    assert_eq!(human.list_devices().expect("list").devices.len(), 1);

    owner
        .uncheck_shared_devices(vec![device.path.clone()])
        .expect("uncheck");
    assert!(
        human.list_devices().expect("list").devices.is_empty(),
        "an unchecked but locally open device must not be listed"
    );

    owner.stop_sharing().expect("stop");
    let fresh = owner
        .start_sharing(ShareBind::new(info.bind.ip(), free_port()), vec![])
        .expect("empty session");
    let second = remote(&fresh, PairingRole::Human, "remote-2").expect("pair");
    assert!(
        second.list_devices().expect("list").devices.is_empty(),
        "a new empty session must not inherit discovery from the old one"
    );
}

#[test]
fn many_empty_waited_lines_keep_a_successful_write() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    let waiting = human.clone();
    let device_id = opened.device_id;
    let pending = std::thread::spawn(move || {
        waiting.write_bytes(
            device_id,
            b"EMPTY-LINES",
            Some(WaitSpec {
                lines: 2_000,
                timeout_ms: Some(5_000),
            }),
        )
    });
    device.read_until(b"EMPTY-LINES", Duration::from_secs(3));
    device.write(&[b'\n'; 2_000]);
    let written = pending
        .join()
        .expect("thread")
        .expect("a completed write must stay a successful result");
    assert_eq!(written.bytes_written, b"EMPTY-LINES".len());
    assert!(!written.timed_out);
    assert!(written.truncated);
}

#[test]
fn near_limit_waited_write_answers_survive_the_tls_send() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    // Bodies just under the 64 KiB frame limit previously passed encoding but
    // failed to fit rustls's plaintext buffer, closing the connection after the
    // device write had executed.
    for length in (65_340usize..=65_430).step_by(10) {
        let waiting = human.clone();
        let device_id = opened.device_id;
        let pending = std::thread::spawn(move || {
            waiting.write_bytes(
                device_id,
                b"EXECUTED-ONCE",
                Some(WaitSpec {
                    lines: 1,
                    timeout_ms: Some(5_000),
                }),
            )
        });
        device.read_until(b"EXECUTED-ONCE", Duration::from_secs(3));
        device.write(format!("{}\r\n", "q".repeat(length)).as_bytes());
        let written = pending
            .join()
            .expect("thread")
            .unwrap_or_else(|error| panic!("length {length} lost its write answer: {error:?}"));
        assert_eq!(written.bytes_written, b"EXECUTED-ONCE".len());
        assert!(!written.timed_out);
    }
    human.status(opened.device_id).expect("connection survived");
}

#[test]
fn a_vanished_checked_device_can_be_unchecked_and_frees_its_slot() {
    let core = shared_core();
    let owner = local_owner(&core);
    let holder = connect(&core.socket, ClientInfo::human("holder"));
    let gone = SimulatedDevice::new();
    let gone_path = gone.path.clone();
    let opened = holder.open(&gone.path, Some(115_200), false).expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![gone.path.clone()])
        .expect("start");
    holder.close(opened.device_id).expect("last detach");
    wait_until(Duration::from_secs(3), || {
        !owner
            .list_devices()
            .expect("list")
            .devices
            .iter()
            .any(|entry| entry.path == gone.path && entry.open)
    });
    drop(gone);

    let snapshot = owner
        .uncheck_shared_devices(vec![gone_path.clone()])
        .expect("uncheck a vanished device");
    assert!(
        snapshot.checked.is_empty(),
        "the vanished entry must be removed: {:?}",
        snapshot.checked
    );

    // Its allowlist slot is free for a new device.
    let replacement = SimulatedDevice::new();
    owner
        .open(&replacement.path, Some(115_200), false)
        .expect("open replacement");
    let snapshot = owner
        .check_shared_devices(vec![replacement.path.clone()])
        .expect("check replacement");
    assert_eq!(snapshot.checked.len(), 1);
    assert_eq!(snapshot.checked[0].path, replacement.path);
    assert_ne!(snapshot.session_id, None);
    drop(info);
}

#[test]
fn a_detached_waited_write_fails_and_never_returns_post_revocation_output() {
    let core = shared_core();
    let owner = local_owner(&core);
    let mut device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    let waiting = human.clone();
    let device_id = opened.device_id;
    let pending = std::thread::spawn(move || {
        waiting.write_bytes(
            device_id,
            b"detach-wait",
            Some(WaitSpec {
                lines: 1,
                timeout_ms: Some(5_000),
            }),
        )
    });
    device.read_until(b"detach-wait", Duration::from_secs(2));
    human.close(device_id).expect("detach from device");
    owner
        .uncheck_shared_devices(vec![device.path.clone()])
        .expect("uncheck");
    device.write(b"PRIVATE-AFTER-UNCHECK\n");

    let result = pending.join().expect("thread");
    assert!(
        result.is_err(),
        "a detached waited write must fail, not return post-revocation output: {result:?}"
    );
    if let Ok(written) = result {
        assert!(
            !written
                .lines
                .iter()
                .any(|line| line.text.contains("PRIVATE-AFTER-UNCHECK")),
            "post-revocation output leaked to a detached waiter"
        );
    }
}

#[test]
fn remote_sharing_administration_requests_are_refused_on_the_wire() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let (mut connection, mut tcp) = raw_tls(&info);
    let mut decoder = serial_nexus_protocol::FrameDecoder::new();

    for (id, request) in [
        (1, serial_nexus_protocol::Request::ShareStop),
        (2, serial_nexus_protocol::Request::ShareStatus),
        (
            3,
            serial_nexus_protocol::Request::ShareCheck(serial_nexus_protocol::ShareDevicesParams {
                devices: vec![device.path.clone()],
            }),
        ),
    ] {
        let frame = serial_nexus_protocol::encode_frame(&serial_nexus_protocol::RequestMessage {
            id,
            request,
        })
        .expect("frame");
        serial_nexus_protocol::share::tlsio::write_frame(
            &mut connection,
            &mut tcp,
            &frame,
            Instant::now() + Duration::from_secs(3),
        )
        .expect("send");
        let answer = read_response(&mut connection, &mut tcp, &mut decoder, id)
            .expect("administration answer");
        match answer {
            ServerMessage::Error { error, .. } => {
                assert_eq!(error.code, ErrorCode::PermissionDenied)
            }
            other => panic!("remote administration was not refused: {other:?}"),
        }
    }
    // The real owner still controls the session.
    owner.sharing_status().expect("owner status");
}

#[test]
fn refused_oversized_remote_requests_do_not_leak_pending_entries() {
    let core = shared_core();
    let owner = local_owner(&core);
    let device = SimulatedDevice::new();
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open");
    let info = owner
        .start_sharing(localhost_bind(), vec![device.path.clone()])
        .expect("start");
    let human = remote(&info, PairingRole::Human, "remote").expect("pair");
    let opened = human.open(&device.path, None, false).expect("open");

    for _ in 0..5 {
        let error = human
            .write_bytes(opened.device_id, &vec![b'x'; 60_000], None)
            .expect_err("too large for one remote frame");
        assert!(
            matches!(error, ClientError::Protocol(_) | ClientError::Server(_)),
            "{error:?}"
        );
        assert_eq!(
            human.pending_request_count(),
            0,
            "a refused request must not retain pending bookkeeping"
        );
    }
    human.list_devices().expect("connection still usable");
}
