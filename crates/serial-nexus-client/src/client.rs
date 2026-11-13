use std::collections::{BTreeMap, HashMap};
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender, TrySendError};
use serial_nexus_protocol::{
    encode_bytes, paths, ClientInfo, DeviceRef, DeviceStatus, DevicesResult, Event, GatewayConnect,
    GatewayHandshake, GatewayTarget, HelloParams, HelloResult, Limits, OpenParams, OpenResult,
    ReadParams, ReadResult, Request, RequestMessage, ResponseData, ServerMessage, ShareBind,
    ShareCommitParams, ShareDevicesParams, ShareSessionInfo, ShareStartParams, SharingSnapshot,
    WaitSpec, WriteParams, WriteResult, GATEWAY_PROTOCOL_VERSION, MAX_GATEWAY_MESSAGE_BYTES,
    MAX_MESSAGE_BYTES, PROTOCOL_VERSION,
};

use crate::autostart::{find_core_binary, spawn_core};
use crate::ClientError;

const DEFAULT_LIMITS: Limits = Limits {
    read_default_lines: 50,
    read_max_lines: 2000,
    write_default_timeout_ms: 1000,
    write_max_timeout_ms: 30_000,
    control_idle_ms: 300_000,
    history_max_lines: 2000,
};

fn ensure_local_peer_uid(actual: u32, expected: u32) -> Result<(), ClientError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ClientError::LocalPeerRejected { actual, expected })
    }
}

/// How to reach, identify with, and (if needed) start the core.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// Unix socket of the core.
    pub socket: PathBuf,
    /// Explicit `serial-nexus-core` executable; `None` uses the standard search
    /// (see [`find_core_binary`](crate::find_core_binary)).
    pub core_binary: Option<PathBuf>,
    /// Start the core when nothing answers on the socket.
    pub auto_start: bool,
    /// How long to wait for a freshly started core to accept connections.
    pub start_timeout: Duration,
    /// Who this client is.
    pub client: ClientInfo,
    /// Timeout for ordinary requests. Writes add their own wait time on top.
    pub request_timeout: Duration,
}

impl ClientConfig {
    /// Defaults for a client of `kind`: standard socket, auto-start on, 5s
    /// start timeout, 10s requests.
    pub fn new(client: ClientInfo) -> Self {
        Self {
            socket: paths::default_socket_path(),
            core_binary: None,
            auto_start: true,
            start_timeout: Duration::from_secs(5),
            client,
            request_timeout: Duration::from_secs(10),
        }
    }
}

/// A connected, thread-safe core client.
///
/// Cloning is cheap and shares the connection; all clones see the same request
/// ids and event subscribers. Dropping the last clone closes the connection,
/// which lets the core release control and (after its grace period) exit.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

type PendingMap = Arc<Mutex<HashMap<u64, Pending>>>;

struct Pending {
    sender: Sender<Result<ResponseData, ClientError>>,
    is_write: bool,
}

struct Inner {
    writer: Mutex<UnixStream>,
    pending: PendingMap,
    subscribers: Arc<Mutex<Vec<Sender<Event>>>>,
    next_id: AtomicU64,
    connected: Arc<AtomicBool>,
    hello: Mutex<HelloResult>,
    socket: PathBuf,
    request_timeout: Duration,
}

impl Client {
    /// Connect, starting the core first if needed.
    pub fn connect(config: ClientConfig) -> Result<Self, ClientError> {
        let stream = match UnixStream::connect(&config.socket) {
            Ok(stream) => stream,
            Err(first_error) => {
                if !config.auto_start {
                    return Err(first_error.into());
                }
                let binary = find_core_binary(config.core_binary.as_deref())
                    .ok_or(ClientError::CoreNotFound)?;
                spawn_core(&binary, &config.socket, config.start_timeout)?;
                UnixStream::connect(&config.socket).map_err(|second_error| {
                    ClientError::CoreStart(format!(
                        "started {} but could not connect to {}: {second_error} (first attempt: {first_error})",
                        binary.display(),
                        config.socket.display()
                    ))
                })?
            }
        };
        Self::from_stream(stream, config)
    }

    /// Connect through the running local terminal gateway without starting
    /// the core or making a remote connection outside that gateway.
    pub fn connect_to_gateway(
        socket: impl Into<PathBuf>,
        target: GatewayTarget,
        client: ClientInfo,
    ) -> Result<Self, ClientError> {
        Self::connect_to_gateway_with_pairing(socket, target, None, client)
    }

    /// Gateway connection with an optional owner-only pairing-file path. The
    /// path crosses the local IPC; pairing credentials are read only by the
    /// gateway and never sent by the MCP process.
    pub fn connect_to_gateway_with_pairing(
        socket: impl Into<PathBuf>,
        target: GatewayTarget,
        pairing_file: Option<PathBuf>,
        client: ClientInfo,
    ) -> Result<Self, ClientError> {
        let socket = socket.into();
        let mut stream = UnixStream::connect(&socket)?;
        let peer =
            nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
                .map_err(|error| ClientError::LocalPeerVerification(error.to_string()))?;
        ensure_local_peer_uid(peer.uid(), paths::effective_uid())?;
        let line = serial_nexus_protocol::encode_line(&GatewayConnect {
            protocol: GATEWAY_PROTOCOL_VERSION,
            target,
            pairing_file,
        })?;
        if line.len() > MAX_GATEWAY_MESSAGE_BYTES {
            return Err(ClientError::Protocol(
                serial_nexus_protocol::ProtocolError::TooLarge {
                    size: line.len(),
                    max: MAX_GATEWAY_MESSAGE_BYTES,
                },
            ));
        }
        stream.write_all(&line)?;
        let mut handshake_reader = BufReader::new(stream.try_clone()?);
        let handshake_line =
            serial_nexus_protocol::read_line(&mut handshake_reader, MAX_GATEWAY_MESSAGE_BYTES)?
                .ok_or_else(|| {
                    ClientError::GatewayHandshake(
                        "gateway closed before acknowledging the protocol".into(),
                    )
                })?;
        let handshake: GatewayHandshake = serial_nexus_protocol::decode_line(&handshake_line)
            .map_err(|error| {
                ClientError::GatewayHandshake(format!("invalid gateway acknowledgement: {error}"))
            })?;
        if handshake.protocol != GATEWAY_PROTOCOL_VERSION {
            return Err(ClientError::GatewayHandshake(format!(
                "gateway speaks protocol {}, this client speaks {}",
                handshake.protocol, GATEWAY_PROTOCOL_VERSION
            )));
        }
        if let Some(error) = handshake.error {
            return Err(ClientError::GatewayHandshake(error));
        }
        let mut config = ClientConfig::new(client);
        config.socket = socket;
        config.auto_start = false;
        Self::from_stream(stream, config)
    }

    /// Connect to an already-running core without auto-start. Intended for
    /// tests, embedding, and wrappers that manage the core themselves.
    pub fn connect_to_socket(
        socket: impl Into<PathBuf>,
        client: ClientInfo,
    ) -> Result<Self, ClientError> {
        let socket = socket.into();
        let stream = UnixStream::connect(&socket)?;
        let mut config = ClientConfig::new(client);
        config.socket = socket;
        config.auto_start = false;
        Self::from_stream(stream, config)
    }

    fn from_stream(stream: UnixStream, config: ClientConfig) -> Result<Self, ClientError> {
        let peer =
            nix::sys::socket::getsockopt(&stream, nix::sys::socket::sockopt::PeerCredentials)
                .map_err(|error| ClientError::LocalPeerVerification(error.to_string()))?;
        ensure_local_peer_uid(peer.uid(), paths::effective_uid())?;
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let subscribers: Arc<Mutex<Vec<Sender<Event>>>> = Arc::new(Mutex::new(Vec::new()));
        let connected = Arc::new(AtomicBool::new(true));

        // The reader owns one half; the writer stays here.
        let read_half = stream.try_clone()?;
        {
            let pending = Arc::clone(&pending);
            let subscribers = Arc::clone(&subscribers);
            let connected = Arc::clone(&connected);
            std::thread::Builder::new()
                .name("serial-nexus-client-reader".into())
                .spawn(move || read_loop(read_half, pending, subscribers, connected))
                .map_err(|error| {
                    ClientError::Disconnected(format!("could not start reader: {error}"))
                })?;
        }

        let client = Self {
            inner: Arc::new(Inner {
                writer: Mutex::new(stream),
                pending,
                subscribers,
                next_id: AtomicU64::new(1),
                connected,
                hello: Mutex::new(HelloResult {
                    client_id: 0,
                    protocol: PROTOCOL_VERSION,
                    core_version: String::new(),
                    socket: config.socket.display().to_string(),
                    limits: DEFAULT_LIMITS,
                }),
                socket: config.socket.clone(),
                request_timeout: config.request_timeout,
            }),
        };

        let hello = client.request(
            Request::Hello(HelloParams {
                client: config.client,
                protocol: PROTOCOL_VERSION,
            }),
            config.request_timeout,
        )?;
        let hello = match hello {
            ResponseData::Hello(hello) => hello,
            other => {
                return Err(ClientError::UnexpectedResponse(format!(
                    "hello answered with {other:?}"
                )))
            }
        };
        if hello.protocol != PROTOCOL_VERSION {
            return Err(ClientError::UnsupportedProtocol {
                core: hello.protocol,
                client: PROTOCOL_VERSION,
            });
        }
        *client.inner.hello.lock().expect("hello lock") = hello;
        Ok(client)
    }

    /// Close the transport immediately. Pending requests fail; this is used
    /// when a caller needs to drop a connection whose request is blocked.
    #[doc(hidden)]
    pub fn shutdown_transport(&self) {
        self.inner.connected.store(false, Ordering::SeqCst);
        if let Ok(writer) = self.inner.writer.lock() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
    }

    /// The core's hello answer: id, version and bounded limits.
    pub fn hello(&self) -> HelloResult {
        self.inner.hello.lock().expect("hello lock").clone()
    }

    /// Socket this client is connected to.
    pub fn socket(&self) -> &Path {
        &self.inner.socket
    }

    /// False once the connection has closed.
    pub fn is_connected(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst)
    }

    /// All serial devices the core can see, including open ones.
    pub fn list_devices(&self) -> Result<DevicesResult, ClientError> {
        match self.request(Request::ListDevices, self.timeout())? {
            ResponseData::Devices(devices) => Ok(devices),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Open a device, join an open one, and/or request AI write control.
    ///
    /// `baud` is required for a closed device and optional (active rate) when
    /// joining. A differing rate on an open device is rejected with
    /// [`ErrorCode::BaudConflict`](serial_nexus_protocol::ErrorCode::BaudConflict).
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
            self.timeout(),
        )? {
            ResponseData::Opened(opened) => Ok(opened),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Detach from a device. The last client detaching closes the device.
    pub fn close(&self, device_id: u64) -> Result<(), ClientError> {
        match self.request(Request::Close(DeviceRef { device_id }), self.timeout())? {
            ResponseData::Closed => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Recent device history: up to `lines` lines (core default when `None`).
    pub fn read(&self, device_id: u64, lines: Option<usize>) -> Result<ReadResult, ClientError> {
        match self.request(
            Request::Read(ReadParams { device_id, lines }),
            self.timeout(),
        )? {
            ResponseData::Read(read) => Ok(read),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Send bytes exactly as given (no line ending is added), optionally
    /// waiting for device response lines received after the write.
    pub fn write_bytes(
        &self,
        device_id: u64,
        data: &[u8],
        wait: Option<WaitSpec>,
    ) -> Result<WriteResult, ClientError> {
        let limits = self.limits();
        let wait_ms = wait
            .as_ref()
            .and_then(|wait| wait.timeout_ms)
            .unwrap_or(limits.write_default_timeout_ms)
            .min(limits.write_max_timeout_ms);
        let timeout = self.timeout() + Duration::from_millis(wait_ms);
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

    /// State of one device (control holder, clients, history size).
    pub fn status(&self, device_id: u64) -> Result<DeviceStatus, ClientError> {
        match self.request(Request::Status(DeviceRef { device_id }), self.timeout())? {
            ResponseData::Status(status) => Ok(status),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Start sharing checked devices on one concrete bind address.
    ///
    /// Local owner only: the core refuses this from any other connection or
    /// client kind. The answer carries the pairing secrets exactly once.
    pub fn start_sharing(
        &self,
        bind: ShareBind,
        devices: Vec<String>,
    ) -> Result<ShareSessionInfo, ClientError> {
        self.start_sharing_expected(bind, devices, BTreeMap::new())
    }

    /// Start sharing and require each path's current node to match the device
    /// identity the owner selected. A replacement is refused, not shared.
    pub fn start_sharing_expected(
        &self,
        bind: ShareBind,
        devices: Vec<String>,
        expected: BTreeMap<String, String>,
    ) -> Result<ShareSessionInfo, ClientError> {
        match self.request(
            Request::ShareStart(ShareStartParams {
                bind,
                devices,
                expected,
            }),
            self.timeout(),
        )? {
            ResponseData::ShareStarted(info) => Ok(info),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Stop sharing, disconnect every remote client and revoke its credentials.
    pub fn stop_sharing(&self) -> Result<(), ClientError> {
        match self.request(Request::ShareStop, self.timeout())? {
            ResponseData::ShareStopped(_) => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Check more detected devices in the owner's session.
    pub fn check_shared_devices(
        &self,
        devices: Vec<String>,
    ) -> Result<SharingSnapshot, ClientError> {
        self.check_shared_devices_expected(devices, BTreeMap::new())
    }

    /// Check devices and require each current node to match the identity the
    /// owner selected.
    pub fn check_shared_devices_expected(
        &self,
        devices: Vec<String>,
        expected: BTreeMap<String, String>,
    ) -> Result<SharingSnapshot, ClientError> {
        match self.request(
            Request::ShareCheck(ShareDevicesParams { devices, expected }),
            self.timeout(),
        )? {
            ResponseData::Sharing(snapshot) => Ok(snapshot),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Replace the whole allowlist atomically, requiring each selected
    /// device's identity. A rejected batch changes nothing.
    pub fn replace_shared_devices_expected(
        &self,
        devices: Vec<String>,
        expected: BTreeMap<String, String>,
    ) -> Result<SharingSnapshot, ClientError> {
        match self.request(
            Request::ShareReplace(ShareDevicesParams { devices, expected }),
            self.timeout(),
        )? {
            ResponseData::Sharing(snapshot) => Ok(snapshot),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Validate a full checklist without applying it. Pair with
    /// [`commit_shared_devices`] after the preferences are safely published.
    pub fn prepare_shared_devices_expected(
        &self,
        devices: Vec<String>,
        expected: BTreeMap<String, String>,
    ) -> Result<String, ClientError> {
        match self.request(
            Request::SharePrepare(ShareDevicesParams { devices, expected }),
            self.timeout(),
        )? {
            ResponseData::SharePrepared(prepared) => Ok(prepared.token),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Apply the checklist previously validated by
    /// [`prepare_shared_devices_expected`].
    pub fn commit_shared_devices(&self, token: String) -> Result<SharingSnapshot, ClientError> {
        match self.request(
            Request::ShareCommit(ShareCommitParams { token }),
            self.timeout(),
        )? {
            ResponseData::Sharing(snapshot) => Ok(snapshot),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Uncheck devices: remote access is revoked immediately.
    pub fn uncheck_shared_devices(
        &self,
        devices: Vec<String>,
    ) -> Result<SharingSnapshot, ClientError> {
        match self.request(
            Request::ShareUncheck(ShareDevicesParams {
                devices,
                expected: BTreeMap::new(),
            }),
            self.timeout(),
        )? {
            ResponseData::Sharing(snapshot) => Ok(snapshot),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// The session's state without secrets; `active == false` when off.
    pub fn sharing_status(&self) -> Result<SharingSnapshot, ClientError> {
        match self.request(Request::ShareStatus, self.timeout())? {
            ResponseData::Sharing(snapshot) => Ok(snapshot),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Live notifications: device bytes, writes, control changes, closures.
    /// Each subscriber gets every event from now on; dropping the receiver
    /// unsubscribes it.
    pub fn subscribe(&self) -> Receiver<Event> {
        let (tx, rx) = unbounded();
        self.inner
            .subscribers
            .lock()
            .expect("subscribers lock")
            .push(tx);
        rx
    }

    /// Subscribe with bounded memory. Events are dropped for this receiver
    /// when its queue is full; the core connection itself is not blocked.
    pub fn subscribe_bounded(&self, capacity: usize) -> Receiver<Event> {
        let (tx, rx) = bounded(capacity.max(1));
        self.inner
            .subscribers
            .lock()
            .expect("subscribers lock")
            .push(tx);
        rx
    }

    fn limits(&self) -> Limits {
        self.inner.hello.lock().expect("hello lock").limits.clone()
    }

    fn timeout(&self) -> Duration {
        let limits = self.limits();
        self.inner
            .request_timeout
            .max(Duration::from_millis(limits.write_max_timeout_ms))
    }

    fn request(&self, request: Request, timeout: Duration) -> Result<ResponseData, ClientError> {
        if !self.is_connected() {
            return Err(ClientError::Disconnected("not connected".into()));
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        let is_write = matches!(&request, Request::Write(_));
        let line = serial_nexus_protocol::encode_line(&RequestMessage { id, request })?;
        let (tx, rx) = bounded(1);
        self.inner.pending.lock().expect("pending lock").insert(
            id,
            Pending {
                sender: tx,
                is_write,
            },
        );
        {
            let mut writer = self.inner.writer.lock().expect("writer lock");
            if let Err(error) = writer.write_all(&line).and_then(|_| writer.flush()) {
                self.inner.pending.lock().expect("pending lock").remove(&id);
                self.inner.connected.store(false, Ordering::SeqCst);
                return Err(if is_write {
                    ClientError::WriteUncertain(format!(
                        "write transport failed after submission; bytes may have reached the device: {error}"
                    ))
                } else {
                    error.into()
                });
            }
        }
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                self.inner.pending.lock().expect("pending lock").remove(&id);
                let reason = format!("no answer within {timeout:?} (request id {id})");
                Err(if is_write {
                    ClientError::WriteUncertain(format!(
                        "{reason}; bytes may have reached the device"
                    ))
                } else {
                    ClientError::Timeout(reason)
                })
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => Err(if is_write {
                ClientError::WriteUncertain(
                    "connection closed before the write answer; bytes may have reached the device"
                        .into(),
                )
            } else {
                ClientError::Disconnected("connection closed".into())
            }),
        }
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("socket", &self.inner.socket)
            .field(
                "client_id",
                &self
                    .inner
                    .hello
                    .lock()
                    .map(|hello| hello.client_id)
                    .unwrap_or(0),
            )
            .field("connected", &self.is_connected())
            .finish()
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.connected.store(false, Ordering::SeqCst);
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
    }
}

fn read_loop(
    stream: UnixStream,
    pending: PendingMap,
    subscribers: Arc<Mutex<Vec<Sender<Event>>>>,
    connected: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(stream);
    loop {
        let line = match serial_nexus_protocol::read_line(&mut reader, MAX_MESSAGE_BYTES) {
            Ok(Some(line)) => line,
            Ok(None) => {
                fail(
                    &pending,
                    &subscribers,
                    &connected,
                    "core closed the connection",
                );
                return;
            }
            Err(error) => {
                fail(
                    &pending,
                    &subscribers,
                    &connected,
                    &format!("read error: {error}"),
                );
                return;
            }
        };
        let message: ServerMessage = match serial_nexus_protocol::decode_line(&line) {
            Ok(message) => message,
            Err(error) => {
                fail(
                    &pending,
                    &subscribers,
                    &connected,
                    &format!("bad message: {error}"),
                );
                return;
            }
        };
        match message {
            ServerMessage::Response { id, result } => {
                if let Some(pending) = pending.lock().expect("pending lock").remove(&id) {
                    let _ = pending.sender.send(Ok(result));
                }
            }
            ServerMessage::Error { id, error } => {
                if let Some(pending) = pending.lock().expect("pending lock").remove(&id) {
                    let _ = pending.sender.send(Err(ClientError::from(error)));
                }
            }
            ServerMessage::Event { event } => broadcast_event(&subscribers, event),
        }
    }
}

fn fail(
    pending: &PendingMap,
    subscribers: &Arc<Mutex<Vec<Sender<Event>>>>,
    connected: &Arc<AtomicBool>,
    reason: &str,
) {
    connected.store(false, Ordering::SeqCst);
    for (_, request) in pending.lock().expect("pending lock").drain() {
        let error = if request.is_write {
            ClientError::WriteUncertain(format!(
                "gateway disconnected before the write answer; bytes may have reached the device: {reason}"
            ))
        } else {
            ClientError::Disconnected(reason.to_string())
        };
        let _ = request.sender.send(Err(error));
    }
    let event = Event::TransportClosed {
        reason: reason.to_string(),
    };
    broadcast_event(subscribers, event);
}

fn broadcast_event(subscribers: &Arc<Mutex<Vec<Sender<Event>>>>, event: Event) {
    subscribers
        .lock()
        .expect("subscribers lock")
        .retain(|tx| match tx.try_send(event.clone()) {
            Ok(()) | Err(TrySendError::Full(_)) => true,
            Err(TrySendError::Disconnected(_)) => false,
        });
}

#[cfg(test)]
mod local_socket_peer_tests {
    use super::{ensure_local_peer_uid, ClientError};

    #[test]
    fn local_socket_peer_must_have_the_current_uid() {
        assert!(matches!(
            ensure_local_peer_uid(2000, 1000),
            Err(ClientError::LocalPeerRejected {
                actual: 2000,
                expected: 1000
            })
        ));
        assert!(ensure_local_peer_uid(1000, 1000).is_ok());
    }
}
