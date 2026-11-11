//! Opt-in LAN sharing: session allowlist, pairing credentials, TLS listener and
//! the remote-connection handler.
//!
//! A [`SharingSession`] exists only while the local owner connection that
//! started it is alive, and only for one concrete bind address. It owns an
//! in-memory allowlist of detected device identities, two fresh pairing
//! credentials (one human, one AI) and a per-session TLS certificate
//! authority. Remote requests reach the same engine as local clients, so the
//! allowlist, role and write-control rules apply to every operation, not just
//! to what a UI happens to show.

mod tls;

use std::collections::BTreeMap;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded};
use ring::rand::SecureRandom;
use serial_nexus_protocol::share::tlsio;
use serial_nexus_protocol::{
    encode_bytes, encode_frame, ErrorBody, ErrorCode, FrameDecoder, PairedHost, PairingRole,
    PairingSecret, RemoteAuth, RemoteAuthResult, RemoteRequestMessage, ServerMessage,
    ShareSessionInfo, ShareStartParams, SharedDevice, SharingSnapshot, MAX_REMOTE_CONNECTIONS,
    MAX_REMOTE_FRAME_BYTES, MAX_SHARED_DEVICES, SHARE_PROTOCOL_VERSION,
};
use serial_nexus_protocol::{ClientKind, ResponseData};

use crate::device::DeviceIdentity;
use crate::engine::{Engine, PendingWrite, WriteStart};

/// Bounded sharing defaults, taken from [`crate::CoreConfig`].
#[derive(Clone, Debug)]
pub struct ShareLimits {
    pub handshake_timeout: Duration,
    pub auth_timeout: Duration,
    pub max_connections: usize,
    pub max_pending_writes: usize,
}

impl Default for ShareLimits {
    fn default() -> Self {
        Self {
            handshake_timeout: Duration::from_secs(serial_nexus_protocol::HANDSHAKE_TIMEOUT_SECS),
            auth_timeout: Duration::from_secs(serial_nexus_protocol::AUTH_TIMEOUT_SECS),
            max_connections: MAX_REMOTE_CONNECTIONS,
            max_pending_writes: serial_nexus_protocol::MAX_PENDING_REMOTE_WRITES,
        }
    }
}

struct Credential {
    digest: [u8; 32],
    role: PairingRole,
}

impl Credential {
    fn generate(role: PairingRole) -> (Self, String) {
        let mut bytes = [0u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .expect("system randomness");
        let credential = encode_bytes(&bytes);
        (
            Self {
                digest: tls::sha256(credential.as_bytes()),
                role,
            },
            credential,
        )
    }

    /// Constant-time check: the role comes from which stored credential
    /// matched, never from what the client claims.
    fn matches(&self, presented: &str) -> bool {
        let digest = tls::sha256(presented.as_bytes());
        use subtle::ConstantTimeEq;
        digest.ct_eq(&self.digest).into()
    }
}

struct RemoteSocket {
    stream: TcpStream,
}

/// One entry in the session allowlist.
struct AuthorizedDevice {
    identity: DeviceIdentity,
    /// Set when the device closed, failed or changed. An expired entry still
    /// names the path so that remote clients attached to it receive the
    /// `DeviceClosed` event, but it grants no access and is not "checked".
    expired: bool,
}

struct SessionState {
    /// Canonical path to the identity authorized at check time.
    devices: BTreeMap<String, AuthorizedDevice>,
    /// Paths whose shared device just closed. Only these paths may deliver a
    /// `DeviceClosed` event to remote clients; a later open clears the mark so
    /// unrelated device instances at the same path stay invisible.
    vacated: std::collections::BTreeSet<String>,
    authenticated: usize,
    pending: usize,
}

/// One live sharing session.
pub struct SharingSession {
    id: String,
    bind: SocketAddr,
    owner_client_id: u64,
    tls_config: Arc<rustls::ServerConfig>,
    ca_cert_der: Vec<u8>,
    host_cert_fingerprint_sha256: String,
    human: Credential,
    ai: Credential,
    state: Mutex<SessionState>,
    stop: AtomicBool,
    listener: Mutex<Option<TcpListener>>,
    connections: Mutex<BTreeMap<u64, RemoteSocket>>,
    next_connection_id: AtomicU64,
    limits: ShareLimits,
}

/// A prepared session and the secrets the owner gets exactly once.
pub struct Prepared {
    pub session: Arc<SharingSession>,
    pub info: ShareSessionInfo,
}

/// Bind the listener and build the session's credentials and TLS material.
///
/// Any failure here leaves sharing off: nothing is registered with the engine.
pub fn prepare(
    owner_client_id: u64,
    params: &ShareStartParams,
    devices: Vec<DeviceIdentity>,
    limits: ShareLimits,
) -> Result<Prepared, String> {
    params.bind.validate()?;
    if devices.len() > MAX_SHARED_DEVICES {
        return Err(format!(
            "at most {MAX_SHARED_DEVICES} devices may be shared in one session"
        ));
    }
    let listener = TcpListener::bind(params.bind.socket_addr())
        .map_err(|error| format!("could not bind {}: {error}", params.bind))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("could not configure the listener: {error}"))?;
    let bind = listener
        .local_addr()
        .map_err(|error| format!("could not read the bound address: {error}"))?;

    let mut session_id_bytes = [0u8; 16];
    ring::rand::SystemRandom::new()
        .fill(&mut session_id_bytes)
        .expect("system randomness");
    let session_id = tls::hex(&session_id_bytes);

    let tls = tls::build(&session_id, bind.ip())?;
    let (human, human_credential) = Credential::generate(PairingRole::Human);
    let (ai, ai_credential) = Credential::generate(PairingRole::Ai);
    debug_assert!(!human.matches(&ai_credential));

    let mut allowlist = BTreeMap::new();
    for identity in devices {
        allowlist.insert(
            identity.canonical.clone(),
            AuthorizedDevice {
                identity,
                expired: false,
            },
        );
    }

    let session = Arc::new(SharingSession {
        id: session_id.clone(),
        bind,
        owner_client_id,
        tls_config: tls.server_config,
        ca_cert_der: tls.ca_cert_der,
        host_cert_fingerprint_sha256: tls.host_cert_fingerprint_sha256,
        human,
        ai,
        state: Mutex::new(SessionState {
            devices: allowlist,
            vacated: std::collections::BTreeSet::new(),
            authenticated: 0,
            pending: 0,
        }),
        stop: AtomicBool::new(false),
        listener: Mutex::new(Some(listener)),
        connections: Mutex::new(BTreeMap::new()),
        next_connection_id: AtomicU64::new(1),
        limits,
    });

    let info = ShareSessionInfo {
        session_id,
        bind,
        host: PairedHost {
            address: bind,
            host_name: bind.ip().to_string(),
            ca_cert_der_b64: encode_bytes(&session.ca_cert_der),
            host_cert_fingerprint_sha256: session.host_cert_fingerprint_sha256.clone(),
        },
        human: PairingSecret {
            role: PairingRole::Human,
            credential: human_credential,
        },
        ai: PairingSecret {
            role: PairingRole::Ai,
            credential: ai_credential,
        },
    };
    Ok(Prepared { session, info })
}

impl SharingSession {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn bind(&self) -> SocketAddr {
        self.bind
    }

    pub fn owner_client_id(&self) -> u64 {
        self.owner_client_id
    }

    pub fn tls_config(&self) -> Arc<rustls::ServerConfig> {
        Arc::clone(&self.tls_config)
    }

    pub fn max_pending_writes(&self) -> usize {
        self.limits.max_pending_writes
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// The role of a presented credential, or `None` when it matches neither.
    pub fn credential_role(&self, presented: &str) -> Option<PairingRole> {
        if self.human.matches(presented) {
            Some(self.human.role)
        } else if self.ai.matches(presented) {
            Some(self.ai.role)
        } else {
            None
        }
    }

    /// Authorize a device path for this session and revalidate its identity.
    ///
    /// A path that is not checked, or whose node changed since it was checked
    /// (unplugged, replaced, symlink retargeted), is refused. A changed node is
    /// removed from the allowlist so it has to be checked again explicitly.
    pub fn authorize(&self, requested: &str) -> Result<DeviceIdentity, ErrorBody> {
        let canonical = match std::fs::canonicalize(requested) {
            Ok(canonical) => canonical.to_string_lossy().into_owned(),
            Err(error) => {
                // A checked device may have disappeared; expire its stored
                // entry instead of leaving it usable or listed.
                let mut state = self.state.lock().expect("share state lock");
                if let Some(entry) = state.devices.get_mut(requested) {
                    entry.expired = true;
                }
                let _ = error;
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    "requested device is not available",
                ));
            }
        };
        let mut state = self.state.lock().expect("share state lock");
        let Some(stored) = state.devices.get(&canonical) else {
            return Err(ErrorBody::new(
                ErrorCode::PermissionDenied,
                "device is not shared",
            ));
        };
        if stored.expired {
            return Err(ErrorBody::new(
                ErrorCode::DeviceFailed,
                "device is no longer available; check it again",
            ));
        }
        let stored = stored.identity.clone();
        if !stored.still_valid() {
            if let Some(entry) = state.devices.get_mut(&canonical) {
                entry.expired = true;
            }
            return Err(ErrorBody::new(
                ErrorCode::DeviceFailed,
                "device changed since it was checked; check it again",
            ));
        }
        let current = DeviceIdentity::capture(&canonical).ok();
        match current {
            Some(current) if current == stored => Ok(current),
            _ => {
                if let Some(entry) = state.devices.get_mut(&canonical) {
                    entry.expired = true;
                }
                Err(ErrorBody::new(
                    ErrorCode::DeviceFailed,
                    "device changed since it was checked; check it again",
                ))
            }
        }
    }

    /// Add one detected device to the allowlist (or refresh an expired entry).
    pub fn check(&self, identity: DeviceIdentity) -> Result<(), ErrorBody> {
        let mut state = self.state.lock().expect("share state lock");
        let replacing = state
            .devices
            .get(&identity.canonical)
            .is_some_and(|entry| !entry.expired);
        if !replacing {
            let live = state
                .devices
                .values()
                .filter(|entry| !entry.expired && entry.identity.still_valid())
                .count();
            if live >= MAX_SHARED_DEVICES {
                return Err(ErrorBody::new(
                    ErrorCode::LimitExceeded,
                    format!("at most {MAX_SHARED_DEVICES} devices may be shared"),
                ));
            }
            // Drop stale expired entries for other paths when making room.
            state.devices.retain(|_, entry| !entry.expired);
        }
        state.devices.insert(
            identity.canonical.clone(),
            AuthorizedDevice {
                identity,
                expired: false,
            },
        );
        Ok(())
    }

    /// Remove one device from the allowlist and return it when it was checked.
    pub fn uncheck(&self, requested: &str) -> Option<DeviceIdentity> {
        let mut state = self.state.lock().expect("share state lock");
        // Stored keys are canonical, and callers normally pass one. Try the
        // given key first so a vanished node can still be removed, then its
        // canonical form for aliases.
        if let Some(entry) = state.devices.remove(requested) {
            return Some(entry.identity);
        }
        if let Ok(canonical) = std::fs::canonicalize(requested) {
            let canonical = canonical.to_string_lossy().into_owned();
            if let Some(entry) = state.devices.remove(&canonical) {
                return Some(entry.identity);
            }
        }
        None
    }

    /// Expire an allowlist entry (used when a device closes, fails or is
    /// replaced). Access stops immediately; the path is kept so attached
    /// remote clients still see the `DeviceClosed` event.
    pub fn revoke_path(&self, path: &str) {
        let mut state = self.state.lock().expect("share state lock");
        // The stored key is canonical, but a removed device may no longer
        // resolve, so match the given path directly as well.
        if let Some(entry) = state.devices.get_mut(path) {
            entry.expired = true;
        }
        if let Ok(canonical) = std::fs::canonicalize(path) {
            let canonical = canonical.to_string_lossy().into_owned();
            if let Some(entry) = state.devices.get_mut(&canonical) {
                entry.expired = true;
            }
        }
    }

    /// True when the path is checked and grants access now.
    pub fn allows_path(&self, path: &str) -> bool {
        self.state
            .lock()
            .expect("share state lock")
            .devices
            .get(path)
            .is_some_and(|entry| !entry.expired)
    }

    /// Mark that a shared device just closed, so its `DeviceClosed` event may
    /// reach remote clients. A later open of the path clears it.
    pub fn mark_vacated(&self, path: &str) {
        let mut state = self.state.lock().expect("share state lock");
        // Only a live shared entry may mark a close; an expired entry that a
        // local client later opens and closes must not leak an event.
        if state.devices.get(path).is_some_and(|entry| !entry.expired) {
            state.vacated.insert(path.to_string());
        }
    }

    /// True only for a path whose shared device just closed.
    pub fn is_vacated(&self, path: &str) -> bool {
        self.state
            .lock()
            .expect("share state lock")
            .vacated
            .contains(path)
    }

    /// A device instance opened at this path: later closure events for it are
    /// its own, not the previous instance's.
    pub fn clear_vacated(&self, path: &str) {
        self.state
            .lock()
            .expect("share state lock")
            .vacated
            .remove(path);
    }

    pub fn snapshot(&self) -> SharingSnapshot {
        let mut state = self.state.lock().expect("share state lock");
        // A checked device that is never opened is not otherwise revalidated.
        for entry in state.devices.values_mut() {
            if !entry.expired && !entry.identity.still_valid() {
                entry.expired = true;
            }
        }
        SharingSnapshot {
            active: true,
            session_id: Some(self.id.clone()),
            bind: Some(self.bind),
            checked: state
                .devices
                .values()
                .filter(|entry| !entry.expired)
                .map(|entry| SharedDevice {
                    path: entry.identity.canonical.clone(),
                    identity: entry.identity.describe(),
                })
                .collect(),
            remote_clients: state.authenticated,
            pending_connections: state.pending,
        }
    }

    /// Close the listener and every remote connection. Idempotent.
    pub fn stop(&self) {
        if self.stop.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(listener) = self.listener.lock().expect("listener lock").take() {
            drop(listener);
        }
        let connections = self.connections.lock().expect("connections lock");
        for connection in connections.values() {
            let _ = connection.stream.shutdown(Shutdown::Both);
        }
    }

    fn accept(&self) -> std::io::Result<(TcpStream, SocketAddr)> {
        let guard = self.listener.lock().expect("listener lock");
        match guard.as_ref() {
            Some(listener) => listener.accept(),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "the sharing listener is closed",
            )),
        }
    }

    /// Reserve a connection slot, or refuse when the bounded limits are full.
    fn register_connection(&self, stream: &TcpStream) -> Option<u64> {
        // Clone before reserving a slot: a clone failure must not leak one.
        let cloned = stream.try_clone().ok()?;
        // Keep the connections -> state lock order used by connection_closed.
        let mut connections = self.connections.lock().expect("connections lock");
        let mut state = self.state.lock().expect("share state lock");
        if self.is_stopped() {
            return None;
        }
        if state.authenticated + state.pending >= self.limits.max_connections {
            return None;
        }
        state.pending += 1;
        let id = self.next_connection_id.fetch_add(1, Ordering::SeqCst);
        connections.insert(id, RemoteSocket { stream: cloned });
        Some(id)
    }

    fn mark_authenticated(&self) {
        let mut state = self.state.lock().expect("share state lock");
        state.pending = state.pending.saturating_sub(1);
        state.authenticated += 1;
    }

    /// Release the slot. `authenticated` comes from the connection thread, so
    /// no lock is held across the two counts.
    fn connection_closed(&self, connection_id: u64, authenticated: bool) {
        let removed = self
            .connections
            .lock()
            .expect("connections lock")
            .remove(&connection_id);
        if removed.is_some() {
            let mut state = self.state.lock().expect("share state lock");
            if authenticated {
                state.authenticated = state.authenticated.saturating_sub(1);
            } else {
                state.pending = state.pending.saturating_sub(1);
            }
        }
    }
}

/// Start the accept loop for a session. The thread ends when the session stops.
pub fn spawn_listener(engine: Arc<Engine>, session: Arc<SharingSession>) -> std::io::Result<()> {
    let thread_session = Arc::clone(&session);
    thread::Builder::new()
        .name("serial-nexus-share-listener".into())
        .spawn(move || accept_loop(engine, thread_session))
        .map(|_| ())
        .inspect_err(|_| {
            // No listener thread means no usable sharing: fail closed.
            session.stop();
        })
}

fn accept_loop(engine: Arc<Engine>, session: Arc<SharingSession>) {
    while !session.is_stopped() {
        match session.accept() {
            Ok((stream, _address)) => {
                let _ = stream.set_nonblocking(false);
                let Some(connection_id) = session.register_connection(&stream) else {
                    // At the connection limit: close immediately, saying nothing.
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                };
                let connection_engine = Arc::clone(&engine);
                let connection_session = Arc::clone(&session);
                let spawned = thread::Builder::new()
                    .name("serial-nexus-share-connection".into())
                    .spawn(move || {
                        serve_connection(
                            connection_engine,
                            connection_session,
                            stream,
                            connection_id,
                        )
                    });
                if spawned.is_err() {
                    session.connection_closed(connection_id, false);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                // A closed listener (stop) or a transient accept error: back
                // off briefly, then re-check the stop flag.
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn serve_connection(
    engine: Arc<Engine>,
    session: Arc<SharingSession>,
    tcp: TcpStream,
    connection_id: u64,
) {
    let mut client_id = None;
    let _ = run_connection(&engine, &session, &tcp, &mut client_id);
    // Close the transport before any blocking device cleanup, so stop can
    // always tear this peer down even if unregistering drains a writer.
    let _ = tcp.shutdown(Shutdown::Both);
    let authenticated = client_id.is_some();
    // Release the connection slot before unregistering: unregistering the last
    // attachment may drain a stalled serial writer.
    session.connection_closed(connection_id, authenticated);
    if let Some(client_id) = client_id {
        engine.unregister_client(client_id);
    }
    let _ = tcp.shutdown(Shutdown::Both);
}

fn run_connection(
    engine: &Arc<Engine>,
    session: &Arc<SharingSession>,
    tcp: &TcpStream,
    client_id: &mut Option<u64>,
) -> Result<(), ()> {
    let mut tcp = tcp.try_clone().map_err(|_| ())?;
    let mut connection = rustls::Connection::Server(
        rustls::ServerConnection::new(session.tls_config()).map_err(|_| ())?,
    );
    let handshake_deadline = Instant::now() + session.limits.handshake_timeout;
    tcp.set_write_timeout(Some(Duration::from_millis(200)))
        .map_err(|_| ())?;
    // Keep one decoder across the handshake and the auth read: a client may
    // send its pairing frame in the same TLS flight.
    let mut decoder = FrameDecoder::new();
    tlsio::handshake(&mut connection, &mut tcp, handshake_deadline, &mut decoder)
        .map_err(|_| ())?;

    // First frame: the pairing credential. The role comes from the credential.
    let auth_deadline = Instant::now() + session.limits.auth_timeout;
    let auth_body = loop {
        if let Some(body) = decoder.next_frame().map_err(|_| ())? {
            break body;
        }
        if Instant::now() >= auth_deadline || session.is_stopped() {
            return Err(());
        }
        match tlsio::pump(&mut connection, &mut tcp, &mut decoder) {
            Ok(tlsio::TlsRead::Closed) => return Err(()),
            Ok(_) => {}
            Err(_) => return Err(()),
        }
    };
    let auth: RemoteAuth = serde_json::from_slice(&auth_body).map_err(|_| ())?;
    if auth.protocol != SHARE_PROTOCOL_VERSION {
        return Err(());
    }
    let Some(role) = session.credential_role(&auth.credential) else {
        return Err(());
    };
    if session.is_stopped() {
        return Err(());
    }
    let kind = match role {
        PairingRole::Human => ClientKind::Human,
        PairingRole::Ai => ClientKind::Ai,
    };
    let name = auth
        .client
        .as_ref()
        .and_then(|client| client.name.as_deref())
        .map(|name| name.chars().take(64).collect::<String>())
        .or_else(|| Some("remote".to_string()));

    // Events are droppable for a slow remote peer (it can re-read history);
    // responses and waited-write answers use their own unbounded-by-limit path
    // so they are never dropped.
    let (events_tx, events_rx) = bounded::<ServerMessage>(1024);
    let (responses_tx, responses_rx) = unbounded::<ServerMessage>();
    let id = engine.register_remote_client(events_tx.clone(), Arc::clone(session), kind, name);
    *client_id = Some(id);
    session.mark_authenticated();

    let auth_result = RemoteAuthResult {
        client_id: id,
        protocol: SHARE_PROTOCOL_VERSION,
        core_version: env!("CARGO_PKG_VERSION").to_string(),
        role,
        limits: engine.config().limits(),
        max_frame_bytes: MAX_REMOTE_FRAME_BYTES,
        max_pending_writes: session.max_pending_writes(),
    };
    let frame = encode_frame(&auth_result).map_err(|_| ())?;
    let write_deadline = Instant::now() + session.limits.auth_timeout;
    tlsio::write_frame(&mut connection, &mut tcp, &frame, write_deadline).map_err(|_| ())?;

    let pending_writes = Arc::new(AtomicUsize::new(0));
    loop {
        while let Some(body) = decoder.next_frame().map_err(|_| ())? {
            let message = RemoteRequestMessage::decode(&body).map_err(|_| ())?;
            handle_remote_request(engine, session, id, message, &responses_tx, &pending_writes);
        }
        while let Ok(message) = responses_rx.try_recv() {
            write_server_message(&mut connection, &mut tcp, &message)?;
        }
        while let Ok(message) = events_rx.try_recv() {
            write_server_message(&mut connection, &mut tcp, &message)?;
        }
        if session.is_stopped() {
            break;
        }
        match tlsio::pump(&mut connection, &mut tcp, &mut decoder) {
            Ok(tlsio::TlsRead::Closed) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    Ok(())
}

/// Keep a successful waited-write answer inside one remote frame by trimming
/// whole lines from the oldest end until the serialized response fits. The
/// write outcome (`bytes_written`, `timed_out`) is never changed.
fn fit_write_answer(
    id: u64,
    mut result: serial_nexus_protocol::WriteResult,
) -> serial_nexus_protocol::WriteResult {
    fn fits(id: u64, result: &serial_nexus_protocol::WriteResult) -> bool {
        encode_frame(&ServerMessage::Response {
            id,
            result: ResponseData::Written(result.clone()),
        })
        .is_ok()
    }
    if fits(id, &result) {
        return result;
    }
    let total = result.lines.len();
    let mut low = 0usize;
    let mut high = total;
    while low < high {
        let middle = (low + high).div_ceil(2);
        let mut candidate = result.clone();
        candidate.lines = result.lines[total - middle..].to_vec();
        if fits(id, &candidate) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let original = result.lines.clone();
    result.lines = original[total - low..].to_vec();
    if result.lines.is_empty() && total > 0 {
        result.lines = original[total - 1..].to_vec();
        // A single oversized line is trimmed to the frame by the caller's
        // encoded-size check if needed; dropping it would lose the answer.
        while !result.lines.is_empty() && !fits(id, &result) {
            let last = result.lines.len() - 1;
            if result.lines[last].text.is_empty() {
                break;
            }
            let cut = result.lines[last].text.len() / 2;
            let mut cut = cut;
            while cut > 0 && !result.lines[last].text.is_char_boundary(cut) {
                cut -= 1;
            }
            result.lines[last].text.truncate(cut);
        }
    }
    result.truncated = true;
    result
}

fn fit_read_answer(
    id: u64,
    mut result: serial_nexus_protocol::ReadResult,
) -> serial_nexus_protocol::ReadResult {
    fn fits(id: u64, result: &serial_nexus_protocol::ReadResult) -> bool {
        encode_frame(&ServerMessage::Response {
            id,
            result: ResponseData::Read(result.clone()),
        })
        .is_ok()
    }
    if fits(id, &result) {
        return result;
    }
    let total = result.lines.len();
    let mut low = 0usize;
    let mut high = total;
    while low < high {
        let middle = (low + high).div_ceil(2);
        let mut candidate = result.clone();
        candidate.lines = result.lines[total - middle..].to_vec();
        if fits(id, &candidate) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let original = result.lines.clone();
    result.lines = original[total - low..].to_vec();
    if result.lines.is_empty() && total > 0 {
        // A single oversized line cannot be made to fit by dropping lines;
        // keep the newest one and trim its text below.
        result.lines = original[total - 1..].to_vec();
    }
    result.truncated = true;
    if !result.lines.is_empty() && !fits(id, &result) {
        // One line/tail can exceed the frame by itself; trim its tail.
        let last = result.lines.len() - 1;
        let mut low = 0usize;
        let mut high = result.lines[last].text.len();
        while low < high {
            let middle = (low + high).div_ceil(2);
            let mut cut = middle;
            while cut > 0 && !result.lines[last].text.is_char_boundary(cut) {
                cut -= 1;
            }
            let mut candidate = result.clone();
            candidate.lines[last].text.truncate(cut);
            if fits(id, &candidate) {
                low = middle;
            } else {
                high = middle.saturating_sub(1);
            }
        }
        while low > 0 && !result.lines[last].text.is_char_boundary(low) {
            low -= 1;
        }
        result.lines[last].text.truncate(low);
    }
    result
}

/// Write one bounded frame. A response that cannot fit the remote frame limit
/// becomes a request-level error instead of closing the whole connection.
fn write_server_message(
    connection: &mut rustls::Connection,
    tcp: &mut TcpStream,
    message: &ServerMessage,
) -> Result<(), ()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let frame = match encode_frame(message) {
        Ok(frame) => frame,
        Err(serial_nexus_protocol::ProtocolError::TooLarge { .. }) => {
            let id = match message {
                ServerMessage::Response { id, .. } | ServerMessage::Error { id, .. } => *id,
                ServerMessage::Event { .. } => return Ok(()),
            };
            let fallback = ServerMessage::Error {
                id,
                error: ErrorBody::new(
                    ErrorCode::BadRequest,
                    "response too large for a remote frame; request fewer lines",
                ),
            };
            encode_frame(&fallback).map_err(|_| ())?
        }
        Err(_) => return Err(()),
    };
    tlsio::write_frame(connection, tcp, &frame, deadline).map_err(|_| ())
}

/// Handle one remote request. Writes enqueue in arrival order and, when they
/// wait for output, the wait runs off this loop so later requests are not
/// delayed or reordered behind it.
fn handle_remote_request(
    engine: &Arc<Engine>,
    session: &Arc<SharingSession>,
    client_id: u64,
    message: RemoteRequestMessage,
    events: &crossbeam_channel::Sender<ServerMessage>,
    pending_writes: &Arc<AtomicUsize>,
) {
    let id = message.0.id;
    let request = message.0.request;
    let answer = match request {
        serial_nexus_protocol::Request::Write(params) => {
            // The limit applies only to requests that actually create a
            // waiter; a non-waited write never consumes a wait slot.
            let waits = params.wait.as_ref().is_some_and(|wait| wait.lines > 0);
            if waits && pending_writes.load(Ordering::SeqCst) >= session.max_pending_writes() {
                Err(ErrorBody::new(
                    ErrorCode::LimitExceeded,
                    format!(
                        "at most {} writes may wait at once",
                        session.max_pending_writes()
                    ),
                ))
            } else {
                match engine.handle_remote_write(client_id, params) {
                    Err(error) => Err(error),
                    Ok(WriteStart::Done(result)) => Ok(ResponseData::Written(result)),
                    Ok(WriteStart::Pending(pending)) => {
                        pending_writes.fetch_add(1, Ordering::SeqCst);
                        spawn_write_wait(
                            Arc::clone(engine),
                            pending,
                            id,
                            events.clone(),
                            Arc::clone(pending_writes),
                        );
                        return;
                    }
                }
            }
        }
        other => engine.handle_remote(client_id, other),
    };
    let answer = answer.map(|result| match result {
        ResponseData::Written(result) => ResponseData::Written(fit_write_answer(id, result)),
        ResponseData::Read(result) => ResponseData::Read(fit_read_answer(id, result)),
        other => other,
    });
    let message = match answer {
        Ok(result) => ServerMessage::Response { id, result },
        Err(error) => ServerMessage::Error { id, error },
    };
    let _ = events.send(message);
}

fn spawn_write_wait(
    engine: Arc<Engine>,
    pending: PendingWrite,
    id: u64,
    events: crossbeam_channel::Sender<ServerMessage>,
    pending_writes: Arc<AtomicUsize>,
) {
    let pending_counter = Arc::clone(&pending_writes);
    let failure_events = events.clone();
    let spawned = thread::Builder::new()
        .name("serial-nexus-share-write-wait".into())
        .spawn(move || {
            let answer = engine
                .finish_write(pending)
                .map(ResponseData::Written)
                .map(|result| match result {
                    // Trim a large successful write answer so it always fits a
                    // remote frame; the write itself already happened.
                    ResponseData::Written(written) => {
                        ResponseData::Written(fit_write_answer(id, written))
                    }
                    other => other,
                });
            pending_counter.fetch_sub(1, Ordering::SeqCst);
            let message = match answer {
                Ok(result) => ServerMessage::Response { id, result },
                Err(error) => ServerMessage::Error { id, error },
            };
            let _ = events.send(message);
        });
    if spawned.is_err() {
        pending_writes.fetch_sub(1, Ordering::SeqCst);
        let _ = failure_events.send(ServerMessage::Error {
            id,
            error: ErrorBody::new(
                ErrorCode::DeviceFailed,
                "could not await the write's response lines; the bytes may already have reached the device",
            )
            .with_details(serde_json::json!({ "may_have_reached_device": true })),
        });
    }
}

/// The result type used by tests and the client adapter: a session's snapshot
/// when it is active.
pub fn inactive_snapshot() -> SharingSnapshot {
    SharingSnapshot {
        active: false,
        session_id: None,
        bind: None,
        checked: Vec::new(),
        remote_clients: 0,
        pending_connections: 0,
    }
}
