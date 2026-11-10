//! Gated acceptance test for the real board + LAN seam. Never flashes.
//!
//! This test is `#[ignore]`d so CI and ordinary `cargo test` skip it. The
//! coordinator runs it on the machine with the STM32 attached, against one
//! concrete LAN address, following `docs/board-gate.md`.
//!
//! Environment:
//! - `SER13_BOARD_SERIAL` (required): real serial device path, e.g.
//!   `/dev/serial/by-id/usb-STMicroelectronics_...`.
//! - `SER13_BOARD_BIND` (required): one concrete `<LAN-IP>:<port>`; port 0 lets
//!   the kernel choose and the test reports the actual port.
//! - `SER13_BOARD_BAUD` (optional, default 115200).
//! - `SER13_BOARD_PROBE` (optional): exact bytes to write through sharing, e.g.
//!   `\r`. Unset means the gate is read-only and sends nothing to the board.
//! - `SER13_BOARD_EXPECT_ECHO` (optional): a string that must appear in device
//!   output after the probe (firmware echo). Unset skips the echo assertion.

use std::net::SocketAddr;
use std::time::Duration;

use serial_nexus_client::{Client, ClientInfo, ErrorCode, RemoteClient};
use serial_nexus_protocol::PairingRole;
use tempfile::TempDir;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

#[test]
#[ignore = "needs a real STM32 board and one LAN address; see docs/board-gate.md"]
fn board_gate_over_real_lan() {
    let serial = env("SER13_BOARD_SERIAL")
        .expect("set SER13_BOARD_SERIAL to the real board's serial device");
    let bind: SocketAddr = env("SER13_BOARD_BIND")
        .expect("set SER13_BOARD_BIND to one concrete LAN address, e.g. 192.168.1.20:0")
        .parse()
        .expect("SER13_BOARD_BIND must be <ip>:<port>");
    let baud: u32 = env("SER13_BOARD_BAUD")
        .unwrap_or_else(|| "115200".to_string())
        .parse()
        .expect("SER13_BOARD_BAUD must be a number");
    let probe = env("SER13_BOARD_PROBE").map(|value| value.into_bytes());
    let expect_echo = env("SER13_BOARD_EXPECT_ECHO");

    let directory = TempDir::new().expect("tempdir");
    let socket = directory.path().join("core.sock");
    let config = serial_nexus_core::CoreConfig {
        socket: socket.clone(),
        grace: Duration::from_secs(30),
        control_idle: Duration::from_secs(300),
        ..serial_nexus_core::CoreConfig::default()
    };
    let core_socket = socket.clone();
    std::thread::spawn(move || {
        if let Err(error) = serial_nexus_core::server::run(config) {
            eprintln!("board-gate core failed: {error}");
        }
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the core did not start listening"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let owner = Client::connect_to_socket(&core_socket, ClientInfo::human("board-gate-owner"))
        .expect("owner connects");
    // Open locally first so a PTY-like or non-enumerated path is detectable;
    // real boards are enumerated anyway and this also proves local access.
    let local = owner
        .open(&serial, Some(baud), false)
        .expect("local open of the real board");
    eprintln!(
        "1/7 local open: {} device_id={} baud={}",
        local.port, local.device_id, local.baud
    );

    let info = owner
        .start_sharing(
            serial_nexus_protocol::ShareBind::new(bind.ip(), bind.port()),
            vec![serial.clone()],
        )
        .expect("start sharing on the concrete LAN address");
    eprintln!(
        "2/7 sharing: bind={} session={}",
        info.bind, info.session_id
    );
    if bind.port() != 0 {
        assert_eq!(info.bind.port(), bind.port());
    }

    let human = RemoteClient::connect(
        &info.host,
        &info.human,
        Some("board-gate-remote".to_string()),
        Duration::from_secs(10),
    )
    .expect("remote human pairs over TLS and authenticates the host");
    eprintln!("3/7 remote pair: role={:?}", human.role());
    assert_eq!(human.role(), PairingRole::Human);

    let listed = human.list_devices().expect("remote list");
    assert!(
        listed
            .devices
            .iter()
            .any(|device| device.path == local.port && device.open),
        "the checked board must be visible to the remote client: {:?}",
        listed.devices
    );
    eprintln!("4/7 remote list: {:?}", listed.devices);

    let opened = human
        .open(&local.port, None, false)
        .expect("remote joins the open board");
    eprintln!(
        "5/7 remote join: role={:?} baud={}",
        opened.role, opened.baud
    );

    // Read-only by default: the board's existing firmware output is enough to
    // prove the LAN serial path. A probe is sent only when explicitly asked.
    let read = human
        .read(opened.device_id, Some(200))
        .expect("remote read");
    eprintln!(
        "6/7 remote read: {} lines (dropped {})",
        read.lines.len(),
        read.dropped_lines
    );
    assert!(
        !read.lines.is_empty(),
        "the board printed nothing; power-cycle the board (never flash) and rerun"
    );

    if let Some(bytes) = &probe {
        let written = human
            .write_bytes(opened.device_id, bytes, None)
            .expect("remote write");
        assert_eq!(written.bytes_written, bytes.len());
        eprintln!("    probe sent: {} bytes", bytes.len());
        if let Some(expected) = &expect_echo {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let read = human.read(opened.device_id, Some(200)).expect("re-read");
                if read.lines.iter().any(|line| line.text.contains(expected)) {
                    eprintln!("    firmware echoed {expected:?}");
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    panic!("firmware did not echo {expected:?} within 5s");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    // Revocation must be visible on the real seam.
    owner
        .uncheck_shared_devices(vec![local.port.clone()])
        .expect("uncheck the board");
    let denied = human
        .read(opened.device_id, Some(5))
        .expect_err("remote read after uncheck must fail");
    assert!(matches!(
        denied.code(),
        Some(ErrorCode::NotAttached)
            | Some(ErrorCode::PermissionDenied)
            | Some(ErrorCode::NotFound)
    ));
    eprintln!("7/7 uncheck revoked remote access");

    // Local serial access keeps working after sharing is stopped.
    owner.stop_sharing().expect("stop sharing");
    let local_read = owner
        .read(local.device_id, Some(50))
        .expect("local read still works");
    eprintln!(
        "    sharing stopped; local history {} lines",
        local_read.lines.len()
    );
    owner.close(local.device_id).ok();
}
