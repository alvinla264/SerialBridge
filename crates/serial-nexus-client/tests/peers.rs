//! The private per-endpoint pairing file: only the owner may use it, and an
//! endpoint matches exactly one pairing.

use std::io::Write;
use std::net::{IpAddr, SocketAddr};

use serial_nexus_client::{PairingRole, Peers, RemoteEndpoint};
use tempfile::TempDir;

const CREDENTIAL: &str = "c2VjcmV0LWNyZWRlbnRpYWwtZm9yLXRlc3Rz";

fn peer_json(address: &str, port: u16, role: &str) -> String {
    format!(
        r#"{{"version":1,"peers":[{entry}]}}"#,
        entry = entry_for(address, port, role, CREDENTIAL)
    )
}

fn entry_for(address: &str, port: u16, role: &str, credential: &str) -> String {
    let socket = SocketAddr::new(address.parse::<IpAddr>().expect("ip"), port).to_string();
    format!(
        r#"{{"endpoint":{{"address":"{address}","port":{port}}},"paired":{{"address":"{socket}","host_name":"{address}","ca_cert_der_b64":"Y2E=","host_cert_fingerprint_sha256":"{fingerprint}"}},"secret":{{"role":"{role}","credential":"{credential}"}}}}"#,
        fingerprint = "ab".repeat(32)
    )
}

fn write_file(directory: &TempDir, contents: &str, mode: u32) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.path().join("mcp-peers.json");
    let mut file = std::fs::File::create(&path).expect("create peers file");
    file.write_all(contents.as_bytes())
        .expect("write peers file");
    file.flush().expect("flush");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    path
}

#[test]
fn a_private_file_matches_an_endpoint_exactly() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), 0o600);
    let peers = Peers::load(&path).expect("load");

    let endpoint = RemoteEndpoint::new("127.0.0.1".parse().unwrap(), 7443);
    let peer = peers.find(&endpoint).expect("find");
    assert_eq!(peer.endpoint, endpoint);
    assert_eq!(peer.secret.role, PairingRole::Ai);
    assert_eq!(
        peer.paired.address,
        SocketAddr::from(([127, 0, 0, 1], 7443))
    );

    // The Debug form never prints the credential.
    let text = format!("{peer:?} {peers:?}");
    assert!(!text.contains(CREDENTIAL), "{text}");
}

#[test]
fn ipv6_endpoints_match_and_read_naturally() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("::1", 7000, "ai"), 0o600);
    let peers = Peers::load(&path).expect("load");
    let endpoint = RemoteEndpoint::new("::1".parse().unwrap(), 7000);
    assert!(peers.find(&endpoint).is_ok());
    assert_eq!(endpoint.to_string(), "[::1]:7000");

    // A different port or address is a different pairing.
    let wrong_port = RemoteEndpoint::new("::1".parse().unwrap(), 7001);
    let error = peers.find(&wrong_port).expect_err("no pairing");
    assert!(error.to_string().contains("[::1]:7001"), "{error}");
}

#[test]
fn a_mapped_ipv6_endpoint_matches_its_ipv4_pairing() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), 0o600);
    let peers = Peers::load(&path).expect("load");
    let mapped: IpAddr = "::ffff:127.0.0.1".parse().unwrap();
    assert!(peers.find(&RemoteEndpoint::new(mapped, 7443)).is_ok());
}

#[test]
fn group_or_world_readable_files_are_refused() {
    let directory = TempDir::new().expect("tempdir");
    for mode in [0o644, 0o640, 0o604, 0o660] {
        let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), mode);
        let error = Peers::load(&path).expect_err("unsafe mode must be refused");
        let text = error.to_string();
        assert!(text.contains("permission"), "{text}");
        assert!(!text.contains(CREDENTIAL), "{text}");
    }
}

#[test]
fn an_executable_but_private_file_is_accepted() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), 0o700);
    assert!(Peers::load(&path).is_ok());
}

#[test]
fn a_group_of_entries_matches_each_endpoint_unambiguously() {
    let directory = TempDir::new().expect("tempdir");
    let contents = format!(
        r#"{{"version":1,"peers":[{a},{b}]}}"#,
        a = entry_for("192.0.2.10", 7000, "ai", "Y3JlZGVudGlhbC1h"),
        b = entry_for("192.0.2.11", 7000, "ai", "Y3JlZGVudGlhbC1i")
    );
    let path = write_file(&directory, &contents, 0o600);
    let peers = Peers::load(&path).expect("load");
    assert_eq!(peers.len(), 2);
    let a = RemoteEndpoint::new("192.0.2.10".parse().unwrap(), 7000);
    let b = RemoteEndpoint::new("192.0.2.11".parse().unwrap(), 7000);
    assert_ne!(
        peers.find(&a).unwrap().secret.credential,
        peers.find(&b).unwrap().secret.credential
    );
}

#[test]
fn duplicate_endpoints_are_refused_as_ambiguous() {
    let directory = TempDir::new().expect("tempdir");
    let contents = format!(
        r#"{{"version":1,"peers":[{a},{b}]}}"#,
        a = entry_for("192.0.2.10", 7000, "ai", CREDENTIAL),
        b = entry_for("192.0.2.10", 7000, "ai", CREDENTIAL)
    );
    let path = write_file(&directory, &contents, 0o600);
    let error = Peers::load(&path).expect_err("duplicate must be refused");
    assert!(error.to_string().contains("192.0.2.10"), "{error}");
}

#[test]
fn malformed_and_mismatched_entries_are_refused_without_echoing_secrets() {
    let directory = TempDir::new().expect("tempdir");

    // Pairing address and endpoint disagree.
    let mismatched = format!(
        r#"{{"version":1,"peers":[{{"endpoint":{{"address":"192.0.2.10","port":7000}},"paired":{{"address":"192.0.2.11:7000","host_name":"192.0.2.10","ca_cert_der_b64":"Y2E=","host_cert_fingerprint_sha256":"{}"}},"secret":{{"role":"ai","credential":"{CREDENTIAL}"}}}}]}}"#,
        "ab".repeat(32)
    );
    let path = write_file(&directory, &mismatched, 0o600);
    let error = Peers::load(&path).expect_err("mismatch");
    let text = error.to_string();
    assert!(
        text.contains("192.0.2.11") || text.contains("192.0.2.10"),
        "{text}"
    );
    assert!(!text.contains(CREDENTIAL), "{text}");

    // The pairing TLS name is not the endpoint address.
    let wrong_name = format!(
        r#"{{"version":1,"peers":[{{"endpoint":{{"address":"192.0.2.10","port":7000}},"paired":{{"address":"192.0.2.10:7000","host_name":"192.0.2.99","ca_cert_der_b64":"Y2E=","host_cert_fingerprint_sha256":"{}"}},"secret":{{"role":"ai","credential":"{CREDENTIAL}"}}}}]}}"#,
        "ab".repeat(32)
    );
    let path = write_file(&directory, &wrong_name, 0o600);
    let error = Peers::load(&path).expect_err("wrong tls name");
    assert!(!error.to_string().contains(CREDENTIAL));

    // A truncated fingerprint is refused.
    let short = format!(
        r#"{{"version":1,"peers":[{{"endpoint":{{"address":"192.0.2.10","port":7000}},"paired":{{"address":"192.0.2.10:7000","host_name":"192.0.2.10","ca_cert_der_b64":"Y2E=","host_cert_fingerprint_sha256":"abcd"}},"secret":{{"role":"ai","credential":"{CREDENTIAL}"}}}}]}}"#
    );
    let path = write_file(&directory, &short, 0o600);
    assert!(Peers::load(&path).is_err());

    // Invalid JSON says so without containing the file's secret.
    let path = write_file(&directory, "{\"version\":1,\"peers\":[", 0o600);
    let error = Peers::load(&path).expect_err("invalid json");
    assert!(!error.to_string().contains(CREDENTIAL));

    // An unknown version is refused rather than guessed at.
    let path = write_file(
        &directory,
        &peer_json("192.0.2.10", 7000, "ai").replace("\"version\":1", "\"version\":9"),
        0o600,
    );
    assert!(Peers::load(&path).is_err());
}

#[test]
fn a_missing_file_has_a_clear_message_and_no_secret() {
    let directory = TempDir::new().expect("tempdir");
    let path = directory.path().join("absent.json");
    let error = Peers::load(&path).expect_err("missing");
    let text = error.to_string();
    assert!(text.contains("absent.json"), "{text}");
    assert!(text.contains("600"), "{text}");
}

#[test]
fn a_symbolic_link_is_refused() {
    let directory = TempDir::new().expect("tempdir");
    let real = write_file(&directory, &peer_json("127.0.0.1", 7443, "ai"), 0o600);
    let link = directory.path().join("link.json");
    std::os::unix::fs::symlink(&real, &link).expect("symlink");
    let error = Peers::load(&link).expect_err("symlink");
    assert!(error.to_string().contains("link.json"), "{error}");
}

#[test]
fn endpoint_validation_refuses_unusable_addresses() {
    for address in ["0.0.0.0", "::", "224.0.0.1", "255.255.255.255"] {
        let endpoint = RemoteEndpoint::new(address.parse().unwrap(), 7000);
        assert!(endpoint.validate().is_err(), "{address} must be refused");
    }
    let zero_port = RemoteEndpoint::new("127.0.0.1".parse().unwrap(), 0);
    assert!(zero_port.validate().is_err());
    for address in ["127.0.0.1", "::1", "192.0.2.10"] {
        let endpoint = RemoteEndpoint::new(address.parse().unwrap(), 7000);
        assert!(endpoint.validate().is_ok(), "{address} must be allowed");
    }
}

#[test]
fn role_is_matched_as_configured() {
    let directory = TempDir::new().expect("tempdir");
    let path = write_file(&directory, &peer_json("127.0.0.1", 7443, "human"), 0o600);
    let peers = Peers::load(&path).expect("load");
    let peer = peers
        .find(&RemoteEndpoint::new("127.0.0.1".parse().unwrap(), 7443))
        .unwrap();
    assert_eq!(peer.secret.role, PairingRole::Human);
}
