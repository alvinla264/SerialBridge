//! Per-user IPC hosted by the foreground terminal. Each accepted MCP session
//! gets its own local-core or paired-remote client; sharing administration is
//! deliberately not part of this protocol.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::RecvTimeoutError;
use serial_nexus_protocol::{
    decode_bytes, decode_line, encode_line, paths, ClientInfo, ErrorBody, ErrorCode, Event,
    GatewayConnect, GatewayHandshake, GatewayTarget, HelloParams, HelloResult, Request,
    RequestMessage, ResponseData, ServerMessage, GATEWAY_PROTOCOL_VERSION,
    MAX_GATEWAY_MESSAGE_BYTES, MAX_MESSAGE_BYTES, PROTOCOL_VERSION,
};

use crate::{Client, ClientError, Peers, RemoteClient, RemoteEndpoint};

const MAX_GATEWAY_CLIENTS: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 1024;
const REMOTE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const GATEWAY_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// Paths used by a terminal-owned gateway. The gateway never starts the core.
#[derive(Clone, Debug)]
pub struct GatewayConfig {
    pub socket: PathBuf,
    pub core_socket: PathBuf,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            socket: paths::default_gateway_socket_path(),
            core_socket: paths::default_socket_path(),
        }
    }
}

/// Running foreground-terminal gateway. Dropping it immediately closes every
/// accepted IPC connection, which drops each upstream client as well.
pub struct GatewayServer {
    socket: PathBuf,
    running: Arc<AtomicBool>,
    sessions: Arc<Mutex<HashMap<u64, GatewaySessionControl>>>,
    accept_thread: Option<JoinHandle<()>>,
    _lock: File,
}

impl GatewayServer {
    /// Bind the single supported terminal gateway for this user. A second
    /// supported terminal receives `AddrInUse`; stale sockets are removed only
    /// after the advisory singleton lock has been acquired.
    pub fn start(config: GatewayConfig) -> io::Result<Self> {
        let parent = config.socket.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "gateway socket has no parent")
        })?;
        let existed = parent.exists();
        fs::create_dir_all(parent)?;
        if !existed {
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let directory_metadata = fs::metadata(parent)?;
        if directory_metadata.uid() != paths::effective_uid()
            || directory_metadata.mode() & 0o022 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "gateway socket directory must be owned by this user and not writable by others (found uid {}, mode {:04o}, expected uid {})",
                    directory_metadata.uid(),
                    directory_metadata.mode() & 0o777,
                    paths::effective_uid()
                ),
            ));
        }

        let lock_path = config.socket.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&lock_path)?;
        fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600))?;
        // SAFETY: flock is called with a live file descriptor and the lock file
        // stays open in `GatewayServer` until all gateway threads have stopped.
        let locked = unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "a SerialNexus terminal gateway is already running",
                ));
            }
            return Err(error);
        }

        match fs::remove_file(&config.socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&config.socket)?;
        fs::set_permissions(&config.socket, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;

        let running = Arc::new(AtomicBool::new(true));
        let sessions: Arc<Mutex<HashMap<u64, GatewaySessionControl>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let next_session = Arc::new(AtomicU64::new(1));
        let thread_running = Arc::clone(&running);
        let thread_sessions = Arc::clone(&sessions);
        let thread_next_session = Arc::clone(&next_session);
        let core_socket = config.core_socket;
        let accept_thread = thread::Builder::new()
            .name("serial-nexus-gateway".into())
            .spawn(move || {
                while thread_running.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            if peer_uid(&stream) != Some(paths::effective_uid()) {
                                let _ = stream.shutdown(std::net::Shutdown::Both);
                                continue;
                            }
                            let mut active = thread_sessions.lock().expect("gateway sessions lock");
                            if !thread_running.load(Ordering::SeqCst)
                                || active.len() >= MAX_GATEWAY_CLIENTS
                            {
                                let _ = stream.shutdown(std::net::Shutdown::Both);
                                continue;
                            }
                            let id = thread_next_session.fetch_add(1, Ordering::Relaxed);
                            let tracked = match stream.try_clone() {
                                Ok(tracked) => tracked,
                                Err(_) => {
                                    let _ = stream.shutdown(std::net::Shutdown::Both);
                                    continue;
                                }
                            };
                            let control = GatewaySessionControl {
                                socket: Arc::new(tracked),
                                active: Arc::new(AtomicBool::new(true)),
                                upstream: Arc::new(Mutex::new(None)),
                            };
                            active.insert(id, control.clone());
                            drop(active);

                            let sessions = Arc::clone(&thread_sessions);
                            let core_socket = core_socket.clone();
                            let worker_control = control.clone();
                            if thread::Builder::new()
                                .name("serial-nexus-gateway-client".into())
                                .spawn(move || {
                                    serve_connection(stream, &core_socket, worker_control);
                                    sessions.lock().expect("gateway sessions lock").remove(&id);
                                })
                                .is_err()
                            {
                                control.shutdown();
                                thread_sessions
                                    .lock()
                                    .expect("gateway sessions lock")
                                    .remove(&id);
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(ACCEPT_POLL);
                        }
                        Err(_) => break,
                    }
                }
            })?;

        Ok(Self {
            socket: config.socket,
            running,
            sessions,
            accept_thread: Some(accept_thread),
            _lock: lock,
        })
    }

    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }
}

impl Drop for GatewayServer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        let sessions: Vec<_> = self
            .sessions
            .lock()
            .expect("gateway sessions lock")
            .values()
            .cloned()
            .collect();
        for session in sessions {
            session.shutdown();
        }
        if let Some(thread) = self.accept_thread.take() {
            let _ = thread.join();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

#[derive(Clone)]
struct GatewaySessionControl {
    socket: Arc<UnixStream>,
    active: Arc<AtomicBool>,
    upstream: Arc<Mutex<Option<Backend>>>,
}

impl GatewaySessionControl {
    fn install_upstream(&self, backend: Backend) -> bool {
        let mut upstream = self.upstream.lock().expect("gateway upstream lock");
        if !self.active.load(Ordering::SeqCst) {
            return false;
        }
        *upstream = Some(backend);
        true
    }

    fn clear_upstream(&self) {
        self.upstream.lock().expect("gateway upstream lock").take();
    }

    fn shutdown(&self) {
        self.active.store(false, Ordering::SeqCst);
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        let upstream = self.upstream.lock().expect("gateway upstream lock").clone();
        if let Some(upstream) = upstream {
            upstream.shutdown_transport();
        }
    }
}

#[derive(Clone)]
enum Backend {
    Local(Client),
    Remote(RemoteClient),
}

impl Backend {
    fn is_connected(&self) -> bool {
        match self {
            Self::Local(client) => client.is_connected(),
            Self::Remote(client) => client.is_connected(),
        }
    }

    fn shutdown_transport(&self) {
        match self {
            Self::Local(client) => client.shutdown_transport(),
            Self::Remote(client) => client.shutdown_transport(),
        }
    }

    fn subscribe(&self) -> crossbeam_channel::Receiver<Event> {
        match self {
            Self::Local(client) => client.subscribe_bounded(EVENT_QUEUE_CAPACITY),
            Self::Remote(client) => client.subscribe_bounded(EVENT_QUEUE_CAPACITY),
        }
    }

    fn hello(&self, target: &GatewayTarget) -> HelloResult {
        match self {
            Self::Local(client) => client.hello(),
            Self::Remote(client) => HelloResult {
                client_id: client.auth().client_id,
                protocol: PROTOCOL_VERSION,
                core_version: client.auth().core_version.clone(),
                socket: match target {
                    GatewayTarget::Remote { address, port, .. } => format!("{address}:{port}"),
                    GatewayTarget::Local => unreachable!(),
                },
                limits: client.limits().clone(),
            },
        }
    }

    fn request(&self, request: Request) -> Result<ResponseData, ClientError> {
        match request {
            Request::Ping => Ok(ResponseData::Pong),
            Request::ListDevices => match self {
                Self::Local(client) => client.list_devices().map(ResponseData::Devices),
                Self::Remote(client) => client.list_devices().map(ResponseData::Devices),
            },
            Request::Open(params) => match self {
                Self::Local(client) => client
                    .open(params.port, params.baud, params.request_control)
                    .map(ResponseData::Opened),
                Self::Remote(client) => client
                    .open(params.port, params.baud, params.request_control)
                    .map(ResponseData::Opened),
            },
            Request::Close(reference) => match self {
                Self::Local(client) => client.close(reference.device_id),
                Self::Remote(client) => client.close(reference.device_id),
            }
            .map(|()| ResponseData::Closed),
            Request::Read(params) => match self {
                Self::Local(client) => client.read(params.device_id, params.lines),
                Self::Remote(client) => client.read(params.device_id, params.lines),
            }
            .map(ResponseData::Read),
            Request::Write(params) => {
                let bytes = decode_bytes(&params.data_b64).map_err(|error| {
                    ClientError::Server(ErrorBody::new(ErrorCode::BadRequest, error.to_string()))
                })?;
                match self {
                    Self::Local(client) => {
                        client.write_bytes(params.device_id, &bytes, params.wait)
                    }
                    Self::Remote(client) => {
                        client.write_bytes(params.device_id, &bytes, params.wait)
                    }
                }
                .map(ResponseData::Written)
            }
            Request::Status(reference) => match self {
                Self::Local(client) => client.status(reference.device_id),
                Self::Remote(client) => client.status(reference.device_id),
            }
            .map(ResponseData::Status),
            Request::Hello(_) => Err(ClientError::Server(ErrorBody::new(
                ErrorCode::BadRequest,
                "hello is only valid once per gateway connection",
            ))),
            Request::ShareStart(_)
            | Request::ShareStop
            | Request::ShareCheck(_)
            | Request::ShareReplace(_)
            | Request::SharePrepare(_)
            | Request::ShareCommit(_)
            | Request::ShareUncheck(_)
            | Request::ShareStatus => Err(ClientError::Server(ErrorBody::new(
                ErrorCode::PermissionDenied,
                "sharing administration is not available through the terminal gateway",
            ))),
        }
    }
}

fn serve_connection(
    mut stream: UnixStream,
    core_socket: &std::path::Path,
    control: GatewaySessionControl,
) {
    if stream
        .set_read_timeout(Some(GATEWAY_HANDSHAKE_TIMEOUT))
        .is_err()
    {
        return;
    }
    let reader_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let mut reader = BufReader::new(reader_stream);
    let connect = match read_gateway_connect(&mut reader) {
        Ok(connect) if connect.protocol == GATEWAY_PROTOCOL_VERSION => {
            if write_gateway_handshake(&mut stream, None).is_err() {
                return;
            }
            connect
        }
        Ok(connect) => {
            let message = format!(
                "gateway protocol mismatch: client uses {}, gateway uses {}",
                connect.protocol, GATEWAY_PROTOCOL_VERSION
            );
            let _ = write_gateway_handshake(&mut stream, Some(message));
            return;
        }
        Err(error) => {
            let _ = write_gateway_handshake(
                &mut stream,
                Some(format!("invalid gateway handshake: {error}")),
            );
            return;
        }
    };

    let hello_line = match serial_nexus_protocol::read_line(&mut reader, MAX_MESSAGE_BYTES) {
        Ok(Some(line)) => line,
        _ => return,
    };
    let hello_message: RequestMessage = match decode_line(&hello_line) {
        Ok(message) => message,
        Err(_) => return,
    };
    let Request::Hello(HelloParams { client, protocol }) = hello_message.request else {
        return;
    };
    if protocol != PROTOCOL_VERSION {
        let error = ErrorBody::new(ErrorCode::Unsupported, "unsupported core protocol version");
        let _ = write_server_message(
            &mut stream,
            &ServerMessage::Error {
                id: hello_message.id,
                error,
            },
        );
        return;
    }
    if stream.set_read_timeout(None).is_err() {
        return;
    }
    let client_name = client.name.unwrap_or_else(|| "serial-nexus-mcp".into());
    let backend = match connect_backend(
        &connect.target,
        connect.pairing_file.as_deref(),
        core_socket,
        client_name,
    ) {
        Ok(backend) => backend,
        Err(error) => {
            let _ = write_server_message(
                &mut stream,
                &ServerMessage::Error {
                    id: hello_message.id,
                    error: connect_error(error),
                },
            );
            return;
        }
    };
    if !control.install_upstream(backend.clone()) {
        backend.shutdown_transport();
        return;
    }
    let _upstream_guard = GatewayUpstreamGuard(control.clone());
    let hello = backend.hello(&connect.target);
    if write_server_message(
        &mut stream,
        &ServerMessage::Response {
            id: hello_message.id,
            result: ResponseData::Hello(hello),
        },
    )
    .is_err()
    {
        return;
    }

    let events = backend.subscribe();
    let writer = match stream.try_clone() {
        Ok(stream) => Arc::new(Mutex::new(stream)),
        Err(_) => return,
    };
    let event_writer = Arc::clone(&writer);
    let event_backend = backend.clone();
    let event_session_active = Arc::clone(&control.active);
    let event_thread = thread::Builder::new()
        .name("serial-nexus-gateway-events".into())
        .spawn(move || relay_events(events, event_writer, event_backend, event_session_active));

    while let Ok(Some(line)) = serial_nexus_protocol::read_line(&mut reader, MAX_MESSAGE_BYTES) {
        let message: RequestMessage = match decode_line(&line) {
            Ok(message) => message,
            Err(_) => break,
        };
        let is_write = matches!(message.request, Request::Write(_));
        let answer = match backend.request(message.request) {
            Ok(result) => ServerMessage::Response {
                id: message.id,
                result,
            },
            Err(error) => ServerMessage::Error {
                id: message.id,
                error: request_error(error, is_write),
            },
        };
        let mut writer = writer.lock().expect("gateway writer lock");
        if write_server_message_locked(&mut writer, &answer).is_err() {
            break;
        }
    }
    control.active.store(false, Ordering::SeqCst);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    if let Ok(thread) = event_thread {
        let _ = thread.join();
    }
}

struct GatewayUpstreamGuard(GatewaySessionControl);

impl Drop for GatewayUpstreamGuard {
    fn drop(&mut self) {
        self.0.active.store(false, Ordering::SeqCst);
        self.0.clear_upstream();
    }
}

fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    getsockopt(stream, PeerCredentials)
        .ok()
        .map(|credentials| credentials.uid())
}

fn write_gateway_handshake(stream: &mut UnixStream, error: Option<String>) -> io::Result<()> {
    let line = encode_line(&GatewayHandshake {
        protocol: GATEWAY_PROTOCOL_VERSION,
        error,
    })
    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if line.len() > MAX_GATEWAY_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gateway acknowledgement exceeds the message limit",
        ));
    }
    stream.write_all(&line)
}

fn read_gateway_connect<R: io::BufRead>(reader: &mut R) -> io::Result<GatewayConnect> {
    let line = serial_nexus_protocol::read_line(reader, MAX_GATEWAY_MESSAGE_BYTES)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing gateway hello"))?;
    decode_line(&line).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn connect_backend(
    target: &GatewayTarget,
    pairing_file: Option<&std::path::Path>,
    core_socket: &std::path::Path,
    client_name: String,
) -> Result<Backend, ClientError> {
    let client = ClientInfo::ai(client_name.clone());
    match target {
        GatewayTarget::Local => Client::connect_to_socket(core_socket, client).map(Backend::Local),
        GatewayTarget::Remote {
            address,
            port,
            scope_id,
        } => {
            let endpoint = RemoteEndpoint::with_scope(*address, *port, *scope_id).canonical();
            let peers = match pairing_file {
                Some(path) => Peers::load(path),
                None => Peers::load_default(),
            }
            .map_err(|error| {
                ClientError::Authentication(format!("could not load MCP pairing file: {error}"))
            })?;
            let peer = peers.find(&endpoint).map_err(|error| {
                ClientError::Authentication(format!("no pairing for {endpoint}: {error}"))
            })?;
            if peer.secret.role != serial_nexus_protocol::PairingRole::Ai {
                return Err(ClientError::Authentication(format!(
                    "the pairing for {endpoint} uses a {:?} credential; MCP requires the AI role",
                    peer.secret.role
                )));
            }
            RemoteClient::connect_to(
                endpoint,
                &peer.paired,
                &peer.secret,
                Some(client_name),
                REMOTE_CONNECT_TIMEOUT,
            )
            .map(Backend::Remote)
        }
    }
}

fn connect_error(error: ClientError) -> ErrorBody {
    match error {
        ClientError::Server(error) => error,
        error @ ClientError::UnsupportedProtocol { .. } => {
            ErrorBody::new(ErrorCode::Unsupported, error.to_string())
        }
        error @ ClientError::Timeout(_) => ErrorBody::new(ErrorCode::Timeout, error.to_string()),
        error @ ClientError::Authentication(_) | error @ ClientError::HostRejected(_) => {
            ErrorBody::new(ErrorCode::PermissionDenied, error.to_string())
        }
        error => ErrorBody::new(ErrorCode::ShuttingDown, error.to_string()),
    }
}

fn request_error(error: ClientError, is_write: bool) -> ErrorBody {
    let uncertain = is_write
        && (error.is_write_uncertain()
            || matches!(
                &error,
                ClientError::Io(_)
                    | ClientError::Timeout(_)
                    | ClientError::Disconnected(_)
                    | ClientError::WriteUncertain(_)
            ));
    let mut body = match error {
        ClientError::Server(error) => error,
        ClientError::Timeout(message) => ErrorBody::new(ErrorCode::Timeout, message),
        ClientError::WriteUncertain(message) => ErrorBody::new(ErrorCode::Timeout, message),
        ClientError::Disconnected(message) => ErrorBody::new(ErrorCode::ShuttingDown, message),
        ClientError::Io(error) => ErrorBody::new(ErrorCode::ShuttingDown, error.to_string()),
        other => ErrorBody::new(ErrorCode::Internal, other.to_string()),
    };
    if uncertain {
        // `ClientError::from` prioritizes Timeout codes over uncertainty; use
        // a non-timeout code so the client cannot mistake this write for a
        // retryable timeout.
        body.code = ErrorCode::Internal;
        body.details = Some(serde_json::json!({"may_have_reached_device": true}));
    }
    body
}

fn relay_events(
    events: crossbeam_channel::Receiver<Event>,
    writer: Arc<Mutex<UnixStream>>,
    backend: Backend,
    session_active: Arc<AtomicBool>,
) {
    loop {
        match events.recv_timeout(Duration::from_millis(100)) {
            Ok(event) => {
                let disconnected = matches!(
                    &event,
                    Event::TransportClosed { .. } | Event::CoreShutdown { .. }
                );
                let message = ServerMessage::Event { event };
                let write_result = {
                    let mut writer = writer.lock().expect("gateway writer lock");
                    write_server_message_locked(&mut writer, &message)
                };
                if write_result.is_err() {
                    return;
                }
                if disconnected {
                    if let Ok(writer) = writer.lock() {
                        let _ = writer.shutdown(std::net::Shutdown::Both);
                    }
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !session_active.load(Ordering::SeqCst) {
                    return;
                }
                if !backend.is_connected() {
                    if let Ok(writer) = writer.lock() {
                        let _ = writer.shutdown(std::net::Shutdown::Both);
                    }
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn write_server_message(stream: &mut UnixStream, message: &ServerMessage) -> io::Result<()> {
    write_server_message_locked(stream, message)
}

fn write_server_message_locked(stream: &mut UnixStream, message: &ServerMessage) -> io::Result<()> {
    let line =
        encode_line(message).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if line.len() > MAX_MESSAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "gateway response exceeds the message limit",
        ));
    }
    stream.write_all(&line)?;
    stream.flush()
}

#[cfg(test)]
mod uncertainty_tests {
    use super::{request_error, ClientError};
    use serial_nexus_protocol::ErrorCode;

    #[test]
    fn a_gateway_error_preserves_the_may_have_reached_device_marker() {
        let body = request_error(
            ClientError::WriteUncertain("accepted write lost its answer".into()),
            true,
        );
        assert_eq!(body.code, ErrorCode::Internal);
        let error = ClientError::from(body);
        assert!(error.is_write_uncertain(), "{error:?}");
        assert!(matches!(error, ClientError::WriteUncertain(_)));
    }
}
