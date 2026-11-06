//! Client-side auto-start: a client with no core running finds the
//! `serial-core` binary, launches it on the requested socket, and connects.
//! Also checks that an explicit core path is honoured (the packaging seam).

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serialbridge_client::{paths, Client, ClientConfig, ClientInfo};
use tempfile::TempDir;

fn core_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_serial-core"))
}

#[test]
fn client_starts_the_core_and_connects() {
    // Keep the spawned core's grace period short so the test leaves nothing
    // running for long. Other tests build CoreConfig directly and ignore this.
    std::env::set_var("SERIALBRIDGE_GRACE_SECS", "1");

    let directory = TempDir::new().expect("tempdir");
    let socket = directory.path().join("core.sock");
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_err());

    let mut config = ClientConfig::new(ClientInfo::ai("autostart-test"));
    config.socket = socket.clone();
    config.core_binary = Some(core_binary());
    config.start_timeout = Duration::from_secs(15);

    let client = Client::connect(config).expect("client starts the core");
    assert_eq!(
        client.hello().protocol,
        serialbridge_protocol::PROTOCOL_VERSION
    );
    assert!(client.hello().client_id > 0);
    assert!(client.list_devices().is_ok());
    assert_eq!(client.socket(), socket);
    drop(client);

    // The core honors its grace period and removes the socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        // Check the file, not a connection: connecting would itself keep the
        // core's grace period alive.
        if !socket.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("core did not exit after its grace period");
}

#[test]
fn default_socket_path_can_be_overridden() {
    std::env::set_var(paths::SOCKET_ENV, "/tmp/serialbridge-explicit.sock");
    assert_eq!(
        paths::default_socket_path(),
        PathBuf::from("/tmp/serialbridge-explicit.sock")
    );
    std::env::remove_var(paths::SOCKET_ENV);
}

#[test]
fn explicit_core_path_wins_and_missing_binary_is_reported() {
    let explicit = PathBuf::from("/nonexistent/serial-core");
    assert_eq!(
        serialbridge_client::find_core_binary(Some(&explicit)),
        Some(explicit.clone())
    );

    let directory = TempDir::new().expect("tempdir");
    let mut config = ClientConfig::new(ClientInfo::ai("missing"));
    config.socket = directory.path().join("none.sock");
    config.core_binary = Some(explicit);
    config.start_timeout = Duration::from_millis(200);
    let error = Client::connect(config).expect_err("cannot start a missing binary");
    assert!(matches!(
        error,
        serialbridge_client::ClientError::CoreStart(_)
    ));
}

#[test]
fn auto_started_core_runs_in_its_own_session() {
    std::env::set_var("SERIALBRIDGE_GRACE_SECS", "1");
    let directory = TempDir::new().expect("tempdir");
    let socket = directory.path().join("session.sock");

    let pid = serialbridge_client::spawn_core(&core_binary(), &socket, Duration::from_secs(15))
        .expect("spawn core");
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("/proc stat");
    let after_name = stat.rsplit_once(')').expect("comm").1;
    let fields: Vec<&str> = after_name.split_whitespace().collect();
    // Fields after comm: state(0) ppid(1) pgrp(2) session(3).
    let session: u32 = fields[3].parse().expect("session id");
    let our_session = unsafe { libc::getsid(0) } as u32;
    assert_ne!(
        session, our_session,
        "auto-started core shares the launcher's session"
    );

    // Connect once so the core is doing real work, then let it exit.
    let client =
        Client::connect_to_socket(&socket, ClientInfo::other("session-check")).expect("connect");
    drop(client);
}
