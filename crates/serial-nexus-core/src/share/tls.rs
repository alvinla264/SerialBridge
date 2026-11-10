//! Session TLS material for LAN sharing.
//!
//! Each sharing session gets a fresh certificate authority and a leaf
//! certificate for the one bind address. The owner pairs a remote client with
//! the CA certificate and the leaf's SHA-256 fingerprint; the remote client
//! trusts only that CA and additionally pins that fingerprint, so a different
//! certificate is never accepted silently.
//!
//! Crypto comes from `rustls` with its `ring` provider and `rcgen` for
//! certificate construction; neither is reimplemented here.

use std::net::IpAddr;
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;

/// Everything the listener needs to serve TLS, plus what pairing must trust.
pub struct SessionTls {
    pub server_config: Arc<ServerConfig>,
    pub ca_cert_der: Vec<u8>,
    pub host_cert_fingerprint_sha256: String,
}

/// Build the session's CA, host certificate and server configuration.
pub fn build(session_id: &str, address: IpAddr) -> Result<SessionTls, String> {
    let ca_key = KeyPair::generate().map_err(|error| error.to_string())?;
    let mut ca_params = CertificateParams::new(Vec::new()).map_err(|error| error.to_string())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    ca_params.distinguished_name.push(
        DnType::CommonName,
        format!("SerialNexus sharing {session_id}"),
    );
    let ca_cert = ca_params
        .self_signed(&ca_key)
        .map_err(|error| error.to_string())?;

    let host_key = KeyPair::generate().map_err(|error| error.to_string())?;
    let mut host_params = CertificateParams::new(Vec::new()).map_err(|error| error.to_string())?;
    host_params.is_ca = IsCa::NoCa;
    host_params.subject_alt_names = vec![SanType::IpAddress(address)];
    host_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    host_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    host_params.distinguished_name.push(
        DnType::CommonName,
        format!("SerialNexus sharing host {session_id}"),
    );
    let host_cert = host_params
        .signed_by(&host_key, &ca_cert, &ca_key)
        .map_err(|error| error.to_string())?;

    let leaf_der = CertificateDer::from(host_cert.der().to_vec());
    let fingerprint = hex(&sha256(leaf_der.as_ref()));
    let ca_der = ca_cert.der().to_vec();

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server_config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf_der, CertificateDer::from(ca_der.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(host_key.serialize_der())),
        )
        .map_err(|error| error.to_string())?;

    Ok(SessionTls {
        server_config: Arc::new(server_config),
        ca_cert_der: ca_der,
        host_cert_fingerprint_sha256: fingerprint,
    })
}

/// Lowercase hex SHA-256, used for pins and credential digests.
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// Lowercase hex of a byte slice.
pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}
