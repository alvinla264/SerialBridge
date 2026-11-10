//! Remote (LAN) client for a shared session.
//!
//! A paired client trusts exactly the session's certificate authority and pins
//! the host certificate's SHA-256 fingerprint from the pairing information, so
//! it never accepts a different certificate silently. The pairing credential
//! selects the role on the core side; this client cannot ask for another.

use std::collections::HashMap;
use std::net::{IpAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use rustls::pki_types::{CertificateDer, ServerName};
use serial_nexus_protocol::share::tlsio;
use serial_nexus_protocol::{
    decode_bytes, encode_bytes, encode_frame, DeviceRef, DeviceStatus, DevicesResult, Event,
    FrameDecoder, OpenParams, OpenResult, PairedHost, PairingRole, PairingSecret, ReadParams,
    ReadResult, RemoteAuth, RemoteAuthResult, Request, RequestMessage, ResponseData, ServerMessage,
    WaitSpec, WriteParams, WriteResult, SHARE_PROTOCOL_VERSION,
};

use crate::ClientError;

struct Pending {
    tx: Sender<Result<ResponseData, ClientError>>,
    /// Writes may already have reached the device even when the answer never
    /// arrives, so failures must report uncertainty instead of a plain close.
    is_write: bool,
}

type PendingMap = Arc<Mutex<HashMap<u64, Pending>>>;

struct Inner {
    outgoing: Sender<Vec<u8>>,
    pending: PendingMap,
    subscribers: Arc<Mutex<Vec<Sender<Event>>>>,
    connected: Arc<AtomicBool>,
    /// A handle for closing the socket when the last clone is dropped, so the
    /// reader thread ends promptly.
    tcp: TcpStream,
    auth: RemoteAuthResult,
    next_id: AtomicU64,
    request_timeout: Duration,
}

/// A connected remote client. Cloning shares the connection.
#[derive(Clone)]
pub struct RemoteClient {
    inner: Arc<Inner>,
}

impl RemoteClient {
    /// Connect to a paired host, verify its certificate, and authenticate with
    /// the pairing credential.
    pub fn connect(
        paired: &PairedHost,
        secret: &PairingSecret,
        client_name: Option<String>,
        timeout: Duration,
    ) -> Result<Self, ClientError> {
        let ca_der = decode_bytes(&paired.ca_cert_der_b64)
            .map_err(|error| ClientError::Tls(format!("bad pairing CA: {error}")))?;
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from(ca_der))
            .map_err(|error| ClientError::Tls(format!("bad pairing CA: {error}")))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|error| ClientError::Tls(error.to_string()))?
            .with_root_certificates(roots)
            .with_no_client_auth();

        let server_name = match paired.host_name.parse::<IpAddr>() {
            Ok(address) => ServerName::IpAddress(address.into()),
            Err(_) => ServerName::try_from(paired.host_name.clone())
                .map_err(|_| ClientError::HostRejected("invalid pairing host name".into()))?,
        };

        let mut tcp = TcpStream::connect_timeout(&paired.address, timeout)?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(Duration::from_millis(50)))?;
        tcp.set_write_timeout(Some(Duration::from_millis(50)))?;
        let mut connection = rustls::Connection::Client(
            rustls::ClientConnection::new(Arc::new(config), server_name)
                .map_err(|error| ClientError::Tls(error.to_string()))?,
        );

        let deadline = Instant::now() + timeout;
        let mut decoder = FrameDecoder::new();
        tlsio::handshake(&mut connection, &mut tcp, deadline, &mut decoder)
            .map_err(|error| ClientError::Tls(format!("TLS handshake failed: {error}")))?;

        // Host authentication: the pairing pinned this exact leaf certificate.
        let presented = connection
            .peer_certificates()
            .and_then(|certs| certs.first())
            .map(|cert| cert.as_ref().to_vec())
            .ok_or_else(|| ClientError::HostRejected("the host presented no certificate".into()))?;
        let fingerprint = hex(&sha256(&presented));
        if fingerprint != paired.host_cert_fingerprint_sha256 {
            return Err(ClientError::HostRejected(
                "the host certificate does not match the pairing fingerprint".into(),
            ));
        }

        let auth = RemoteAuth {
            credential: secret.credential.clone(),
            protocol: SHARE_PROTOCOL_VERSION,
            client: client_name.map(|name| serial_nexus_protocol::ClientInfo {
                kind: match secret.role {
                    PairingRole::Human => serial_nexus_protocol::ClientKind::Human,
                    PairingRole::Ai => serial_nexus_protocol::ClientKind::Ai,
                },
                name: Some(name),
            }),
        };
        let frame = encode_frame(&auth)?;
        tlsio::write_frame(&mut connection, &mut tcp, &frame, deadline)?;

        let body = loop {
            if let Some(body) = decoder.next_frame()? {
                break body;
            }
            if Instant::now() >= deadline {
                return Err(ClientError::Authentication(
                    "the host did not answer authentication in time".into(),
                ));
            }
            match tlsio::pump(&mut connection, &mut tcp, &mut decoder) {
                Ok(tlsio::TlsRead::Closed) => {
                    return Err(ClientError::Authentication(
                        "the host closed the connection during authentication".into(),
                    ))
                }
                Ok(_) => {}
                Err(error) => {
                    return Err(ClientError::Authentication(format!(
                        "authentication failed: {error}"
                    )))
                }
            }
        };
        let auth: RemoteAuthResult =
            serde_json::from_slice(&body).map_err(|error| ClientError::Protocol(error.into()))?;
        if auth.protocol != SHARE_PROTOCOL_VERSION {
            return Err(ClientError::UnsupportedProtocol {
                core: auth.protocol,
                client: SHARE_PROTOCOL_VERSION,
            });
        }

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let subscribers: Arc<Mutex<Vec<Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let (outgoing, outgoing_rx) = unbounded::<Vec<u8>>();
        let drop_handle = tcp.try_clone()?;

        {
            let pending = Arc::clone(&pending);
            let subscribers = Arc::clone(&subscribers);
            let connected = Arc::clone(&connected);
            std::thread::Builder::new()
                .name("serial-nexus-remote-reader".into())
                .spawn(move || {
                    connection_loop(
                        connection,
                        tcp,
                        outgoing_rx,
                        pending,
                        subscribers,
                        connected,
                    )
                })
                .map_err(|error| ClientError::Disconnected(error.to_string()))?;
        }

        Ok(Self {
            inner: Arc::new(Inner {
                outgoing,
                pending,
                subscribers,
                connected,
                tcp: drop_handle,
                auth,
                next_id: AtomicU64::new(1),
                request_timeout: timeout,
            }),
        })
    }

    pub fn auth(&self) -> &RemoteAuthResult {
        &self.inner.auth
    }

    pub fn role(&self) -> PairingRole {
        self.inner.auth.role
    }

    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst)
    }

    pub fn list_devices(&self) -> Result<DevicesResult, ClientError> {
        match self.request(Request::ListDevices, self.inner.request_timeout)? {
            ResponseData::Devices(devices) => Ok(devices),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    pub fn open(
        &self,
        port: impl Into<String>,
        baud: Option<u32>,
        request_control: bool,
    ) -> Result<OpenResult, ClientError> {
        match self.request(
            Request::Open(OpenParams {
                port: port.into(),
                baud,
                request_control,
            }),
            self.inner.request_timeout,
        )? {
            ResponseData::Opened(opened) => Ok(opened),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    pub fn close(&self, device_id: u64) -> Result<(), ClientError> {
        match self.request(
            Request::Close(DeviceRef { device_id }),
            self.inner.request_timeout,
        )? {
            ResponseData::Closed => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    pub fn read(&self, device_id: u64, lines: Option<usize>) -> Result<ReadResult, ClientError> {
        match self.request(
            Request::Read(ReadParams { device_id, lines }),
            self.inner.request_timeout,
        )? {
            ResponseData::Read(read) => Ok(read),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    pub fn write_bytes(
        &self,
        device_id: u64,
        data: &[u8],
        wait: Option<WaitSpec>,
    ) -> Result<WriteResult, ClientError> {
        let wait_ms = wait
            .as_ref()
            .and_then(|spec| spec.timeout_ms)
            .unwrap_or(self.inner.auth.limits.write_default_timeout_ms)
            .min(self.inner.auth.limits.write_max_timeout_ms);
        let timeout = self.inner.request_timeout + Duration::from_millis(wait_ms);
        match self.request(
            Request::Write(WriteParams {
                device_id,
                data_b64: encode_bytes(data),
                wait,
            }),
            timeout,
        )? {
            ResponseData::Written(written) => Ok(written),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    pub fn status(&self, device_id: u64) -> Result<DeviceStatus, ClientError> {
        match self.request(
            Request::Status(DeviceRef { device_id }),
            self.inner.request_timeout,
        )? {
            ResponseData::Status(status) => Ok(status),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = unbounded();
        self.inner
            .subscribers
            .lock()
            .expect("subscribers lock")
            .push(tx);
        rx
    }

    fn request(&self, request: Request, timeout: Duration) -> Result<ResponseData, ClientError> {
        if !self.is_connected() {
            return Err(ClientError::Disconnected("not connected".into()));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = bounded(1);
        let is_write = matches!(request, Request::Write(_));
        self.inner
            .pending
            .lock()
            .expect("pending lock")
            .insert(id, Pending { tx, is_write });
        let frame = encode_frame(&RequestMessage { id, request })?;
        if self.inner.outgoing.send(frame).is_err() {
            let pending = self.inner.pending.lock().expect("pending lock").remove(&id);
            if pending.is_some_and(|pending| pending.is_write) {
                return Err(ClientError::WriteUncertain(
                    "the connection ended before the write was acknowledged; the bytes may already have reached the device and were not replayed".into(),
                ));
            }
            return Err(ClientError::Disconnected("connection thread ended".into()));
        }
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                let pending = self.inner.pending.lock().expect("pending lock").remove(&id);
                if pending.is_some_and(|pending| pending.is_write) {
                    Err(ClientError::WriteUncertain(format!(
                        "no answer within {timeout:?} (request id {id}); the bytes may already have reached the device and were not replayed"
                    )))
                } else {
                    Err(ClientError::Timeout(format!(
                        "no answer within {timeout:?} (request id {id})"
                    )))
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                Err(ClientError::Disconnected("remote connection closed".into()))
            }
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.connected.store(false, Ordering::SeqCst);
        let _ = self.tcp.shutdown(std::net::Shutdown::Both);
    }
}

impl std::fmt::Debug for RemoteClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteClient")
            .field("role", &self.role())
            .field("connected", &self.is_connected())
            .finish()
    }
}

fn connection_loop(
    mut connection: rustls::Connection,
    mut tcp: TcpStream,
    outgoing: Receiver<Vec<u8>>,
    pending: PendingMap,
    subscribers: Arc<Mutex<Vec<Sender<Event>>>>,
    connected: Arc<AtomicBool>,
) {
    let mut decoder = FrameDecoder::new();
    loop {
        while let Ok(frame) = outgoing.try_recv() {
            let deadline = Instant::now() + Duration::from_secs(30);
            if tlsio::write_frame(&mut connection, &mut tcp, &frame, deadline).is_err() {
                fail(&pending, &subscribers, &connected, "write failed");
                return;
            }
        }
        loop {
            match decoder.next_frame() {
                Ok(Some(body)) => match decode_server_message(&body) {
                    Ok(message) => dispatch(message, &pending, &subscribers),
                    Err(error) => {
                        fail(&pending, &subscribers, &connected, &error.to_string());
                        return;
                    }
                },
                Ok(None) => break,
                Err(error) => {
                    fail(&pending, &subscribers, &connected, &error.to_string());
                    return;
                }
            }
        }
        match tlsio::pump(&mut connection, &mut tcp, &mut decoder) {
            Ok(tlsio::TlsRead::Closed) => {
                fail(
                    &pending,
                    &subscribers,
                    &connected,
                    "host closed the connection",
                );
                return;
            }
            Ok(_) => {}
            Err(error) => {
                fail(&pending, &subscribers, &connected, &error.to_string());
                return;
            }
        }
    }
}

fn decode_server_message(body: &[u8]) -> Result<ServerMessage, ClientError> {
    serde_json::from_slice(body).map_err(|error| ClientError::Protocol(error.into()))
}

fn dispatch(
    message: ServerMessage,
    pending: &PendingMap,
    subscribers: &Arc<Mutex<Vec<Sender<Event>>>>,
) {
    match message {
        ServerMessage::Response { id, result } => {
            if let Some(pending) = pending.lock().expect("pending lock").remove(&id) {
                let _ = pending.tx.send(Ok(result));
            }
        }
        ServerMessage::Error { id, error } => {
            if let Some(pending) = pending.lock().expect("pending lock").remove(&id) {
                let _ = pending.tx.send(Err(ClientError::from(error)));
            }
        }
        ServerMessage::Event { event } => {
            subscribers
                .lock()
                .expect("subscribers lock")
                .retain(|tx| tx.send(event.clone()).is_ok());
        }
    }
}

fn fail(
    pending: &PendingMap,
    subscribers: &Arc<Mutex<Vec<Sender<Event>>>>,
    connected: &AtomicBool,
    reason: &str,
) {
    connected.store(false, Ordering::SeqCst);
    for (_, pending) in pending.lock().expect("pending lock").drain() {
        let error = if pending.is_write {
            ClientError::WriteUncertain(format!(
                "{reason}; the bytes may already have reached the device and were not replayed"
            ))
        } else {
            ClientError::Disconnected(reason.to_string())
        };
        let _ = pending.tx.send(Err(error));
    }
    let event = Event::TransportClosed {
        reason: reason.to_string(),
    };
    subscribers
        .lock()
        .expect("subscribers lock")
        .retain(|tx| tx.send(event.clone()).is_ok());
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}
