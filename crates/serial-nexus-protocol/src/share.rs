//! Wire types for opt-in LAN sharing.
//!
//! Sharing is controlled by one **local owner** connection over the Unix
//! socket and served to **remote clients** over TLS on one concrete bind
//! address. The local control messages extend [`Request`](crate::Request) and
//! only the core's Unix-socket server accepts them; remote connections carry
//! the same request types but the core refuses every control message there.
//!
//! Remote framing is length-prefixed JSON (a 4-byte big-endian length, then
//! that many bytes), bounded by [`MAX_REMOTE_FRAME_BYTES`]. The Unix socket
//! keeps its newline-delimited framing, so existing clients are unaffected.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read};
use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::{ClientInfo, Limits, ProtocolError, RequestMessage};

/// Version of the remote (LAN) protocol.
pub const SHARE_PROTOCOL_VERSION: u32 = 1;

/// Largest accepted remote frame, in bytes, in either direction.
pub const MAX_REMOTE_FRAME_BYTES: usize = 64 * 1024;

/// Most devices one sharing session may check.
pub const MAX_SHARED_DEVICES: usize = 16;

/// Most remote connections (authenticated or still handshaking) at once.
pub const MAX_REMOTE_CONNECTIONS: usize = 8;

/// Most writes one remote connection may have waiting for device output.
pub const MAX_PENDING_REMOTE_WRITES: usize = 16;

/// Time allowed for the TLS handshake.
pub const HANDSHAKE_TIMEOUT_SECS: u64 = 10;

/// Time allowed to send the pairing credential after the handshake.
pub const AUTH_TIMEOUT_SECS: u64 = 5;

/// One concrete unicast address to listen on. Wildcard addresses are refused:
/// sharing is opt-in per interface, never "all interfaces" by default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareBind {
    pub address: IpAddr,
    pub port: u16,
}

impl ShareBind {
    pub fn new(address: IpAddr, port: u16) -> Self {
        Self { address, port }
    }

    /// Refuse addresses that would listen on every interface or that are not
    /// usable unicast endpoints.
    pub fn validate(&self) -> Result<(), String> {
        // An IPv4-mapped IPv6 address is an IPv4 address in disguise; validate
        // the inner address so `::ffff:0.0.0.0` cannot become a wildcard bind.
        if let std::net::IpAddr::V6(v6) = self.address {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return ShareBind::new(std::net::IpAddr::V4(v4), self.port).validate();
            }
        }
        let address = self.address;
        if address.is_unspecified() {
            return Err(format!(
                "{address} is a wildcard address; choose one concrete IPv4 or IPv6 address"
            ));
        }
        if address.is_multicast() {
            return Err(format!("{address} is a multicast address"));
        }
        if let IpAddr::V4(v4) = address {
            if v4.is_broadcast() || is_interface_broadcast(v4) {
                return Err(format!("{address} is a broadcast address"));
            }
        }
        Ok(())
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.address, self.port)
    }
}

fn derived_broadcast(
    local: std::net::Ipv4Addr,
    mask: std::net::Ipv4Addr,
) -> Option<std::net::Ipv4Addr> {
    let mask = u32::from(mask);
    // A host-only prefix or a zero mask has no distinct directed broadcast.
    if mask == 0 || mask == u32::MAX {
        return None;
    }
    let derived = std::net::Ipv4Addr::from(u32::from(local) | !mask);
    (derived != local).then_some(derived)
}

/// True when the address is the directed broadcast of one of this host's
/// interfaces (for example loopback's `127.255.255.255`).
#[cfg(unix)]
fn is_interface_broadcast(address: std::net::Ipv4Addr) -> bool {
    use nix::ifaddrs::getifaddrs;
    let Ok(interfaces) = getifaddrs() else {
        return false;
    };
    interfaces.into_iter().any(|interface| {
        if interface
            .broadcast
            .as_ref()
            .and_then(nix::sys::socket::SockaddrStorage::as_sockaddr_in)
            .is_some_and(|broadcast| broadcast.ip() == address)
        {
            return true;
        }
        // Loopback and some interfaces do not expose IFF_BROADCAST; derive it
        // from the interface address and netmask.
        let Some(local) = interface
            .address
            .as_ref()
            .and_then(nix::sys::socket::SockaddrStorage::as_sockaddr_in)
        else {
            return false;
        };
        let Some(mask) = interface
            .netmask
            .as_ref()
            .and_then(nix::sys::socket::SockaddrStorage::as_sockaddr_in)
        else {
            return false;
        };
        derived_broadcast(local.ip(), mask.ip()) == Some(address)
    })
}

#[cfg(not(unix))]
fn is_interface_broadcast(_address: std::net::Ipv4Addr) -> bool {
    false
}

impl fmt::Display for ShareBind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.socket_addr())
    }
}

/// Where a sharing session listens and which detected devices it shares.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareStartParams {
    pub bind: ShareBind,
    /// Device paths to check. They must be detectable now; they need not be
    /// open locally.
    #[serde(default)]
    pub devices: Vec<String>,
    /// The device identity the owner selected for each path, when it had one.
    /// The core refuses a path whose current node no longer matches, so a
    /// replacement cannot inherit a check made against its predecessor.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub expected: BTreeMap<String, String>,
}

/// Check or uncheck specific devices in the owner's running session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareDevicesParams {
    pub devices: Vec<String>,
    /// Expected device identity per path, as captured when it was selected.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub expected: BTreeMap<String, String>,
}

/// A validated checklist that has not been applied yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharePrepared {
    pub token: String,
}

/// Apply a previously prepared checklist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareCommitParams {
    pub token: String,
}

/// Which kind of pairing credential a secret is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingRole {
    /// A human client; may write without holding AI control.
    Human,
    /// An AI client; needs write control to write.
    Ai,
}

/// A freshly generated pairing secret. Shown once, to the local owner only.
///
/// `Debug` never prints the value, so it cannot leak through a log line by
/// accident.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairingSecret {
    pub role: PairingRole,
    pub credential: String,
}

impl fmt::Debug for PairingSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PairingSecret")
            .field("role", &self.role)
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// What a remote client needs to authenticate the host it paired with.
///
/// The certificate authority is generated for one sharing session and is the
/// only root the client trusts; the leaf fingerprint is checked as well, so a
/// different certificate is never accepted silently.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairedHost {
    /// Exact endpoint to dial.
    pub address: SocketAddr,
    /// TLS name to verify: the bind IP address as text.
    pub host_name: String,
    /// Base64 (standard, padded) DER of the session's certificate authority.
    pub ca_cert_der_b64: String,
    /// Lowercase hex SHA-256 of the host's leaf certificate DER.
    pub host_cert_fingerprint_sha256: String,
}

/// Everything the local owner needs to pair remote clients. Returned once, in
/// the answer to [`Request::ShareStart`](crate::Request::ShareStart), and
/// never again or to anyone else.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareSessionInfo {
    pub session_id: String,
    pub bind: SocketAddr,
    pub host: PairedHost,
    pub human: PairingSecret,
    pub ai: PairingSecret,
}

/// A checked device and the identity it was authorized as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedDevice {
    pub path: String,
    /// Opaque identity captured at check time; changes when the device is
    /// replaced, and the core revokes access rather than reusing it.
    pub identity: String,
}

/// The owner-visible state of a session: no secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharingSnapshot {
    pub active: bool,
    /// Present only while a session is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bind: Option<SocketAddr>,
    pub checked: Vec<SharedDevice>,
    /// Remote connections that finished authentication.
    pub remote_clients: usize,
    /// Remote connections still in the TLS handshake or waiting to pair.
    pub pending_connections: usize,
}

/// Answer to `ShareStop`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareStopped {
    pub was_active: bool,
}

/// First remote frame: the pairing credential and an optional display name.
///
/// The role is not part of this message: the core derives it from which
/// credential matched, so a client can never ask for more than it paired for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAuth {
    pub credential: String,
    pub protocol: u32,
    #[serde(default)]
    pub client: Option<ClientInfo>,
}

/// Answer to [`RemoteAuth`]. Never contains a credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteAuthResult {
    /// The engine client id used for this connection's requests.
    pub client_id: u64,
    pub protocol: u32,
    pub core_version: String,
    pub role: PairingRole,
    pub limits: Limits,
    pub max_frame_bytes: usize,
    pub max_pending_writes: usize,
}

/// A remote frame after authentication: the same requests local clients use.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoteRequestMessage(pub RequestMessage);

/// Length-prefixed frame encoding used on the TLS stream.
pub fn encode_frame<T: Serialize>(message: &T) -> Result<Vec<u8>, ProtocolError> {
    let body = serde_json::to_vec(message)?;
    if body.is_empty() {
        return Err(ProtocolError::Empty);
    }
    if body.len() > MAX_REMOTE_FRAME_BYTES {
        return Err(ProtocolError::TooLarge {
            size: body.len(),
            max: MAX_REMOTE_FRAME_BYTES,
        });
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Read one length-prefixed frame. Returns `Ok(None)` on a clean EOF between
/// frames and an `InvalidData` error for a zero or oversized length.
pub fn read_frame<R: Read>(reader: &mut R, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 4];
    let mut filled = 0usize;
    while filled < 4 {
        match reader.read(&mut header[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream ended inside a frame header",
                ))
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    let length = u32::from_be_bytes(header) as usize;
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "zero-length frame",
        ));
    }
    if length > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {length} bytes exceeds the {max} byte limit"),
        ));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

impl RemoteRequestMessage {
    pub fn decode(body: &[u8]) -> Result<Self, ProtocolError> {
        let message: RequestMessage = serde_json::from_slice(body)?;
        Ok(Self(message))
    }
}

/// Incremental frame decoder for streams read with a short timeout: partial
/// frames stay buffered and are never misread as a new frame.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes read from the stream.
    pub fn push(&mut self, bytes: &[u8]) -> Result<(), ProtocolError> {
        // Individual frames are bounded by `next_frame`. The caller drains
        // complete frames between reads, so a legal pipeline of several
        // near-limit frames must not be rejected as one oversized buffer.
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    /// Remove and return one complete frame body, if buffered.
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>, ProtocolError> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }
        let length = u32::from_be_bytes([
            self.buffer[0],
            self.buffer[1],
            self.buffer[2],
            self.buffer[3],
        ]) as usize;
        if length == 0 || length > MAX_REMOTE_FRAME_BYTES {
            return Err(ProtocolError::TooLarge {
                size: length,
                max: MAX_REMOTE_FRAME_BYTES,
            });
        }
        if self.buffer.len() < 4 + length {
            return Ok(None);
        }
        let body = self.buffer[4..4 + length].to_vec();
        self.buffer.drain(..4 + length);
        Ok(Some(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_mapped_wildcards_are_refused_and_ipv6_formats_with_brackets() {
        let mapped = ShareBind::new("::ffff:0.0.0.0".parse().unwrap(), 9000);
        assert!(mapped.validate().is_err(), "{mapped}");
        let concrete = ShareBind::new("::ffff:127.0.0.1".parse().unwrap(), 9000);
        assert!(concrete.validate().is_ok());
        assert_eq!(
            ShareBind::new("::1".parse().unwrap(), 9000).to_string(),
            "[::1]:9000"
        );
    }

    #[test]
    fn wildcard_addresses_are_refused() {
        for address in ["0.0.0.0", "::", "224.0.0.1", "255.255.255.255"] {
            let bind = ShareBind::new(address.parse().unwrap(), 9000);
            assert!(bind.validate().is_err(), "{address} must be refused");
        }
        for address in ["127.0.0.1", "::1", "192.168.1.20"] {
            let bind = ShareBind::new(address.parse().unwrap(), 9000);
            assert!(bind.validate().is_ok(), "{address} must be allowed");
        }
    }

    #[test]
    fn frames_round_trip_and_are_bounded() {
        let auth = RemoteAuth {
            credential: "abc".into(),
            protocol: SHARE_PROTOCOL_VERSION,
            client: None,
        };
        let frame = encode_frame(&auth).unwrap();
        let back: RemoteAuth = serde_json::from_slice(&frame[4..]).unwrap();
        assert_eq!(back, auth);

        let mut cursor = std::io::Cursor::new(frame);
        let body = read_frame(&mut cursor, MAX_REMOTE_FRAME_BYTES)
            .unwrap()
            .unwrap();
        assert_eq!(serde_json::from_slice::<RemoteAuth>(&body).unwrap(), auth);
        assert!(read_frame(&mut cursor, MAX_REMOTE_FRAME_BYTES)
            .unwrap()
            .is_none());
    }

    #[test]
    fn zero_and_oversized_frames_are_refused() {
        let mut zero = std::io::Cursor::new(0u32.to_be_bytes().to_vec());
        assert!(read_frame(&mut zero, MAX_REMOTE_FRAME_BYTES).is_err());
        let mut huge = std::io::Cursor::new(u32::MAX.to_be_bytes().to_vec());
        assert!(read_frame(&mut huge, MAX_REMOTE_FRAME_BYTES).is_err());

        let mut decoder = FrameDecoder::new();
        decoder.push(&u32::MAX.to_be_bytes()).unwrap();
        assert!(decoder.next_frame().is_err());
    }

    #[test]
    fn two_pipelined_frames_are_not_mistaken_for_one_oversized_frame() {
        let first = encode_frame(&"a".repeat(MAX_REMOTE_FRAME_BYTES - 8)).unwrap();
        let second = encode_frame(&"b").unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&first).unwrap();
        decoder.push(&second).unwrap();
        let body = decoder.next_frame().unwrap().expect("first");
        assert_eq!(
            body,
            serde_json::to_vec(&"a".repeat(MAX_REMOTE_FRAME_BYTES - 8)).unwrap()
        );
        let body = decoder.next_frame().unwrap().expect("second");
        assert_eq!(serde_json::from_slice::<String>(&body).unwrap(), "b");
        assert!(decoder.next_frame().unwrap().is_none());
    }

    #[test]
    fn a_partial_frame_waits_for_the_rest() {
        let frame = encode_frame(&"hello").unwrap();
        let mut decoder = FrameDecoder::new();
        decoder.push(&frame[..3]).unwrap();
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.push(&frame[3..]).unwrap();
        let body = decoder.next_frame().unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<String>(&body).unwrap(), "hello");
    }

    #[test]
    fn zero_write_recovery_never_duplicates_plaintext() {
        use std::collections::VecDeque;
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};
        use std::time::Instant;

        struct Scripted {
            accepts: VecDeque<usize>,
            written: Vec<u8>,
        }
        impl tlsio::TlsConnection for Scripted {
            fn read_tls(&mut self, _reader: &mut dyn Read) -> std::io::Result<usize> {
                Ok(0)
            }
            fn write_tls(&mut self, _writer: &mut dyn Write) -> std::io::Result<usize> {
                Ok(0)
            }
            fn process_new_packets(&mut self) -> Result<(), String> {
                Ok(())
            }
            fn is_handshaking(&self) -> bool {
                false
            }
            fn read_plaintext(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                Ok(0)
            }
            fn write_plaintext(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                let next = self.accepts.pop_front().unwrap_or(buffer.len());
                let take = next.min(buffer.len());
                self.written.extend_from_slice(&buffer[..take]);
                Ok(take)
            }
        }

        fn run(accepts: &[usize], frame: &[u8]) -> Vec<u8> {
            let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let mut tcp = TcpStream::connect(address).unwrap();
            let (_peer, _) = listener.accept().unwrap();
            let mut connection = Scripted {
                accepts: accepts.iter().copied().collect(),
                written: Vec::new(),
            };
            tlsio::write_frame(
                &mut connection,
                &mut tcp,
                frame,
                Instant::now() + std::time::Duration::from_secs(2),
            )
            .unwrap();
            connection.written
        }

        let frame = b"0123456789abcdef";
        // Zero, then the whole remaining frame in one retry.
        assert_eq!(run(&[0, frame.len()], frame), frame);
        // Zero, then a partial retry; the rest follows without duplication.
        assert_eq!(run(&[0, 3], frame), frame);
        // Ordinary short writes.
        assert_eq!(run(&[2, 5, 9], frame), frame);
    }

    #[test]
    fn pairing_secret_debug_is_redacted() {
        let secret = PairingSecret {
            role: PairingRole::Human,
            credential: "super-secret-value".into(),
        };
        let text = format!("{secret:?}");
        assert!(!text.contains("super-secret-value"));
        assert!(text.contains("redacted"));
    }
}

/// TLS stream plumbing shared by the core's remote listener and the client's
/// remote adapter.
///
/// `rustls` has no common trait exposing `read_tls`/`write_tls` for both
/// connection types, so this module names the small surface both sides need.
/// Reads are short-timeout reads into a [`FrameDecoder`], which keeps a partial
/// frame buffered instead of losing it on a timeout.
pub mod tlsio {
    use std::io::{self, Read, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    use super::{FrameDecoder, ProtocolError};

    /// One read attempt's outcome.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum TlsRead {
        /// TLS records were processed (plaintext may have been buffered).
        Progress,
        /// Nothing was available before the timeout.
        Idle,
        /// The peer closed the connection.
        Closed,
    }

    /// The bits of a rustls connection used here.
    pub trait TlsConnection {
        fn read_tls(&mut self, reader: &mut dyn Read) -> io::Result<usize>;
        fn write_tls(&mut self, writer: &mut dyn Write) -> io::Result<usize>;
        fn process_new_packets(&mut self) -> Result<(), String>;
        fn is_handshaking(&self) -> bool;
        fn read_plaintext(&mut self, buffer: &mut [u8]) -> io::Result<usize>;
        /// Write as much plaintext as the TLS buffer accepts; callers loop and
        /// flush, because a write may be short or return zero when full.
        fn write_plaintext(&mut self, buffer: &[u8]) -> io::Result<usize>;
    }

    impl TlsConnection for rustls::Connection {
        fn read_tls(&mut self, reader: &mut dyn Read) -> io::Result<usize> {
            rustls::Connection::read_tls(self, reader)
        }
        fn write_tls(&mut self, writer: &mut dyn Write) -> io::Result<usize> {
            rustls::Connection::write_tls(self, writer)
        }
        fn process_new_packets(&mut self) -> Result<(), String> {
            rustls::Connection::process_new_packets(self)
                .map(|_| ())
                .map_err(|error| error.to_string())
        }
        fn is_handshaking(&self) -> bool {
            match self {
                rustls::Connection::Client(connection) => connection.is_handshaking(),
                rustls::Connection::Server(connection) => connection.is_handshaking(),
            }
        }
        fn read_plaintext(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reader().read(buffer)
        }
        fn write_plaintext(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writer().write(buffer)
        }
    }

    /// Finish the TLS handshake before `deadline`.
    pub fn handshake<C: TlsConnection>(
        connection: &mut C,
        tcp: &mut TcpStream,
        deadline: Instant,
        decoder: &mut FrameDecoder,
    ) -> io::Result<()> {
        loop {
            if !connection.is_handshaking() {
                return Ok(());
            }
            match pump(connection, tcp, decoder)? {
                TlsRead::Closed => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "peer closed during the TLS handshake",
                    ))
                }
                TlsRead::Idle | TlsRead::Progress => {}
            }
            flush(connection, tcp, deadline)?;
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "TLS handshake did not finish before its deadline",
                ));
            }
        }
    }

    /// Read from the socket with a short timeout, process TLS records and move
    /// any plaintext into `decoder`.
    pub fn pump<C: TlsConnection>(
        connection: &mut C,
        tcp: &mut TcpStream,
        decoder: &mut FrameDecoder,
    ) -> io::Result<TlsRead> {
        tcp.set_read_timeout(Some(Duration::from_millis(25)))?;
        match connection.read_tls(tcp) {
            Ok(0) => return Ok(TlsRead::Closed),
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                return Ok(TlsRead::Idle)
            }
            Err(error) => return Err(error),
        }
        connection
            .process_new_packets()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let mut buffer = [0u8; 8192];
        loop {
            match connection.read_plaintext(&mut buffer) {
                Ok(0) => break,
                Ok(read) => decoder
                    .push(&buffer[..read])
                    .map_err(|error: ProtocolError| io::Error::other(error.to_string()))?,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(TlsRead::Progress)
    }

    /// Write one frame and flush every pending TLS record.
    pub fn write_frame<C: TlsConnection>(
        connection: &mut C,
        tcp: &mut TcpStream,
        frame: &[u8],
        deadline: Instant,
    ) -> io::Result<()> {
        // rustls buffers only a bounded amount of plaintext. Write in bounded
        // chunks and flush whenever the buffer is full, so a valid maximum
        // frame is never turned into a connection failure.
        const CHUNK: usize = 16 * 1024;
        let mut offset = 0usize;
        while offset < frame.len() {
            let end = (offset + CHUNK).min(frame.len());
            match connection.write_plaintext(&frame[offset..end]) {
                Ok(0) => {
                    flush(connection, tcp, deadline)?;
                    // Account for whatever the retry accepts, exactly once.
                    match connection.write_plaintext(&frame[offset..end])? {
                        0 => {
                            return Err(io::Error::new(
                                io::ErrorKind::WriteZero,
                                "the TLS stream accepted no plaintext after a flush",
                            ))
                        }
                        written => offset += written,
                    }
                }
                Ok(written) => {
                    offset += written;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    flush(connection, tcp, deadline)?;
                }
                Err(error) => return Err(error),
            }
            flush(connection, tcp, deadline)?;
        }
        flush(connection, tcp, deadline)
    }

    /// Flush pending TLS records before `deadline`.
    pub fn flush<C: TlsConnection>(
        connection: &mut C,
        tcp: &mut TcpStream,
        deadline: Instant,
    ) -> io::Result<()> {
        loop {
            match connection.write_tls(tcp) {
                Ok(0) => return Ok(()),
                Ok(_) => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "could not flush the TLS stream before its deadline",
                        ));
                    }
                    tcp.set_write_timeout(Some(Duration::from_millis(25)))?;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod broadcast_tests {
    use super::*;

    #[test]
    fn directed_broadcast_addresses_are_refused() {
        let loopback = ShareBind::new("127.255.255.255".parse().unwrap(), 0);
        assert!(loopback.validate().is_err());
        let limited = ShareBind::new("255.255.255.255".parse().unwrap(), 0);
        assert!(limited.validate().is_err());
        // A host-only /32 has no distinct broadcast.
        assert_eq!(
            derived_broadcast(
                "10.77.0.3".parse().unwrap(),
                "255.255.255.255".parse().unwrap()
            ),
            None
        );
    }
}
