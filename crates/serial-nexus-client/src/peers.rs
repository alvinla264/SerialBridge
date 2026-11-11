//! The MCP host's private per-endpoint pairing file.
//!
//! A remote MCP session must not receive its credential through AI-visible
//! tool arguments. The human who configures the MCP host instead writes one
//! small JSON file, readable by its owner only, with one entry per shared
//! endpoint. This module loads it defensively: a file another user can read,
//! a symbolic link, a malformed entry or a credential paired for a different
//! address is refused before any connection is attempted, and no message ever
//! contains the credential itself.
//!
//! ```json
//! {
//!   "version": 1,
//!   "peers": [
//!     {
//!       "endpoint": { "address": "192.0.2.10", "port": 7000 },
//!       "paired": {
//!         "address": "192.0.2.10:7000",
//!         "host_name": "192.0.2.10",
//!         "ca_cert_der_b64": "<from the host terminal's pairing details>",
//!         "host_cert_fingerprint_sha256": "<64 hex characters>"
//!       },
//!       "secret": { "role": "ai", "credential": "<from the same pairing details>" }
//!     }
//!   ]
//! }
//! ```
//!
//! The file path is `SERIAL_NEXUS_MCP_PEERS` when set, else
//! `$XDG_CONFIG_HOME/serial-nexus/mcp-peers.json`, else
//! `~/.config/serial-nexus/mcp-peers.json`.

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serial_nexus_protocol::{decode_bytes, PairedHost, PairingSecret};

use crate::remote::RemoteEndpoint;

/// Environment variable naming an explicit pairing file.
pub const PEERS_ENV: &str = "SERIAL_NEXUS_MCP_PEERS";

/// Only version understood by this build.
pub const PEERS_VERSION: u32 = 1;

/// Why a pairing file could not be used. No variant carries a credential.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error(
        "no remote pairing file at {path}; create it readable by you only (chmod 600) and add one entry per shared endpoint"
    )]
    Missing { path: PathBuf },

    #[error("cannot read the remote pairing file {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "the remote pairing file {path} must be a regular file, not a symbolic link or device"
    )]
    NotRegular { path: PathBuf },

    #[error("the remote pairing file {path} is owned by uid {owner}, not by you (uid {current})")]
    WrongOwner {
        path: PathBuf,
        owner: u32,
        current: u32,
    },

    #[error(
        "the remote pairing file {path} has mode {mode:04o}, giving group or world permission; it must be readable by its owner only (chmod 600)"
    )]
    UnsafePermissions { path: PathBuf, mode: u32 },

    #[error("the remote pairing file {path} is malformed: {reason}")]
    Format { path: PathBuf, reason: String },

    #[error(
        "the remote pairing file {path} uses version {version}; this build understands version {PEERS_VERSION}"
    )]
    Version { path: PathBuf, version: u32 },

    #[error("the remote pairing file {path} pairs {endpoint} twice; keep one entry per endpoint")]
    Duplicate {
        path: PathBuf,
        endpoint: RemoteEndpoint,
    },

    #[error("no pairing for {endpoint} in {path}; add an entry for that exact address and port")]
    Unknown {
        path: PathBuf,
        endpoint: RemoteEndpoint,
    },
}

/// One paired endpoint: where it is, which host certificate and credential
/// authenticate it.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePeer {
    /// Explicit address and port this entry is for; must equal the pairing's.
    pub endpoint: RemoteEndpoint,
    /// The host's pinned certificate identity, as the host terminal showed it.
    pub paired: PairedHost,
    /// The credential this client authenticates with.
    pub secret: PairingSecret,
}

impl RemotePeer {
    /// Check the entry stands for itself: one endpoint, one pinned host.
    fn validate(&self) -> Result<(), String> {
        self.endpoint.validate()?;
        let endpoint = RemoteEndpoint::from_socket_addr(self.paired.address).canonical();
        if endpoint != self.endpoint.canonical() {
            return Err(format!(
                "the pairing is for {} but this entry is for {}",
                self.paired.address, self.endpoint
            ));
        }
        let name = self
            .paired
            .host_name
            .parse::<std::net::IpAddr>()
            .map_err(|_| {
                format!(
                    "the pairing host name {:?} is not the IP address the host reported",
                    self.paired.host_name
                )
            })?;
        if RemoteEndpoint::canonical_ip(name) != RemoteEndpoint::canonical_ip(self.endpoint.address)
        {
            return Err(format!(
                "the pairing host name {:?} does not match the endpoint address {}",
                self.paired.host_name, self.endpoint.address
            ));
        }
        let fingerprint = &self.paired.host_cert_fingerprint_sha256;
        if fingerprint.len() != 64
            || !fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(
                "host_cert_fingerprint_sha256 must be 64 lowercase hex characters".to_string(),
            );
        }
        decode_bytes(&self.paired.ca_cert_der_b64)
            .map_err(|_| "ca_cert_der_b64 is not valid base64".to_string())?;
        if self.secret.credential.trim().is_empty() {
            return Err("the pairing credential is empty".to_string());
        }
        Ok(())
    }
}

impl fmt::Debug for RemotePeer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RemotePeer")
            .field("endpoint", &self.endpoint)
            .field("role", &self.secret.role)
            .field("fingerprint", &self.paired.host_cert_fingerprint_sha256)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerFile {
    version: u32,
    peers: Vec<RemotePeer>,
}

/// Every paired endpoint from one private file.
#[derive(Clone)]
pub struct Peers {
    path: PathBuf,
    peers: Vec<RemotePeer>,
}

impl Peers {
    /// The default pairing file: the environment override, else the user's
    /// configuration directory.
    pub fn default_path() -> PathBuf {
        if let Some(path) = std::env::var_os(PEERS_ENV) {
            return PathBuf::from(path);
        }
        if let Some(base) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(base)
                .join("serial-nexus")
                .join("mcp-peers.json");
        }
        match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home)
                .join(".config")
                .join("serial-nexus")
                .join("mcp-peers.json"),
            None => PathBuf::from("mcp-peers.json"),
        }
    }

    /// Load [`Peers::default_path`].
    pub fn load_default() -> Result<Self, PeerError> {
        Self::load(Self::default_path())
    }

    /// Load a pairing file, refusing anything another user could read.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, PeerError> {
        let path = path.as_ref().to_path_buf();
        let file = open_private(&path)?;
        let metadata = file.metadata().map_err(|source| PeerError::Io {
            path: path.clone(),
            source,
        })?;
        check_private(&path, &metadata)?;

        let mut text = String::new();
        {
            use std::io::Read;
            let mut file = file;
            file.read_to_string(&mut text)
                .map_err(|source| PeerError::Io {
                    path: path.clone(),
                    source,
                })?;
        }
        let parsed: PeerFile = serde_json::from_str(&text).map_err(|error| PeerError::Format {
            path: path.clone(),
            reason: format!("{error}"),
        })?;
        if parsed.version != PEERS_VERSION {
            return Err(PeerError::Version {
                path,
                version: parsed.version,
            });
        }
        let mut seen: Vec<RemoteEndpoint> = Vec::with_capacity(parsed.peers.len());
        for peer in &parsed.peers {
            peer.validate().map_err(|reason| PeerError::Format {
                path: path.clone(),
                reason: format!("{}: {reason}", peer.endpoint),
            })?;
            let canonical = peer.endpoint.canonical();
            if seen.contains(&canonical) {
                return Err(PeerError::Duplicate {
                    path: path.clone(),
                    endpoint: peer.endpoint,
                });
            }
            seen.push(canonical);
        }
        Ok(Self {
            path,
            peers: parsed.peers,
        })
    }

    /// The pairing for exactly this address and port.
    pub fn find(&self, endpoint: &RemoteEndpoint) -> Result<&RemotePeer, PeerError> {
        let wanted = endpoint.canonical();
        self.peers
            .iter()
            .find(|peer| peer.endpoint.canonical() == wanted)
            .ok_or_else(|| PeerError::Unknown {
                path: self.path.clone(),
                endpoint: wanted,
            })
    }

    pub fn len(&self) -> usize {
        self.peers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }

    /// The file these entries came from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl fmt::Debug for Peers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Peers")
            .field("path", &self.path)
            .field(
                "endpoints",
                &self.peers.iter().map(|p| p.endpoint).collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Open without following a symlink; the ownership and mode checks then apply
/// to the file actually read.
fn open_private(path: &Path) -> Result<File, PeerError> {
    use std::os::unix::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(PeerError::Missing { path: path.into() })
        }
        Err(error) => Err(PeerError::Io {
            path: path.into(),
            source: error,
        }),
    }
}

/// Reject anything but a regular file owned by this user with no group or
/// world permission bits.
fn check_private(path: &Path, metadata: &std::fs::Metadata) -> Result<(), PeerError> {
    check_private_as(path, metadata, nix::unistd::geteuid().as_raw())
}

/// [`check_private`] with the effective uid passed in, so the ownership rule
/// can be tested without needing a second real user.
fn check_private_as(
    path: &Path,
    metadata: &std::fs::Metadata,
    current: u32,
) -> Result<(), PeerError> {
    use std::os::unix::fs::MetadataExt;
    if !metadata.file_type().is_file() {
        return Err(PeerError::NotRegular { path: path.into() });
    }
    if metadata.uid() != current {
        return Err(PeerError::WrongOwner {
            path: path.into(),
            owner: metadata.uid(),
            current,
        });
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(PeerError::UnsafePermissions {
            path: path.into(),
            mode,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_foreign_owner_is_refused() {
        // The same function production uses: the current uid matches, and any
        // other owner is a hard failure (tested without a second real user).
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::TempDir::new().expect("tempdir");
        let path = directory.path().join("peers.json");
        std::fs::write(&path, "{}").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let metadata = std::fs::metadata(&path).expect("regular file");
        let current = nix::unistd::geteuid().as_raw();
        assert!(matches!(check_private(&path, &metadata), Ok(())));
        assert!(matches!(
            check_private_as(&path, &metadata, current.wrapping_add(1)),
            Err(PeerError::WrongOwner { owner, current: claimed, .. })
                if owner == current && claimed == current.wrapping_add(1)
        ));
    }
}
