use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use serial_nexus_protocol::{
    decode_bytes, ClientKind, ControlReleaseReason, ControlState, DeviceStatus, DeviceSummary,
    ErrorBody, ErrorCode, Event, HelloParams, HelloResult, HistoryLine, Holder, OpenParams,
    OpenResult, ReadParams, ReadResult, Request, ResponseData, Role, ServerMessage,
    ShareDevicesParams, ShareStartParams, Source, WriteParams, WriteResult, MAX_SHARED_DEVICES,
    PROTOCOL_VERSION,
};

use crate::config::CoreConfig;
use crate::device::{self, DeviceEvent, DeviceIdentity};
use crate::enumerate;
use crate::history::History;
use crate::share::{self, ShareLimits, SharingSession};

/// Result of preparing a write: either complete, or waiting off-thread.
pub enum WriteStart {
    Done(WriteResult),
    Pending(PendingWrite),
}

/// A write already enqueued in arrival order, waiting for device response
/// lines. Safe to move to another thread.
pub struct PendingWrite {
    device_id: u64,
    waiter_id: u64,
    rx: Receiver<WaitOutcome>,
    timeout: Duration,
    bytes_written: usize,
}

/// What a waiting write receives from the engine.
pub struct WaitOutcome {
    pub lines: Vec<HistoryLine>,
    pub timed_out: bool,
    pub failed: Option<String>,
}

struct ClientRecord {
    tx: Sender<ServerMessage>,
    kind: ClientKind,
    name: Option<String>,
    hello_done: bool,
    /// Set for remote sharing clients; never for local Unix-socket clients.
    remote: Option<Arc<SharingSession>>,
}

struct Attached {
    kind: ClientKind,
    name: Option<String>,
}

struct Control {
    client_id: u64,
    last_activity: Instant,
}

struct Waiter {
    id: u64,
    client_id: u64,
    after_seq: u64,
    wanted: usize,
    lines: Vec<HistoryLine>,
    tx: Sender<WaitOutcome>,
}

struct Device {
    id: u64,
    path: String,
    baud: u32,
    history: History,
    /// Bytes since the last newline (a prompt or an incomplete line).
    tail: Vec<u8>,
    /// Sequence reserved when the current tail began, for chronological reads.
    tail_seq: Option<u64>,
    clients: BTreeMap<u64, Attached>,
    control: Option<Control>,
    write_tx: Option<Sender<device::WriteCommand>>,
    write_epochs: device::WriteEpochs,
    stop: Arc<AtomicBool>,
    waiters: Vec<Waiter>,
    io: Option<device::IoThreads>,
}

struct State {
    devices: HashMap<u64, Device>,
    next_device_id: u64,
    grace_since: Option<Instant>,
    shutting_down: bool,
}

/// Resolve a device path to one identity: aliases and `..` must name the same
/// open device. Falls back to the original string if resolution fails.
fn identity_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|resolved| resolved.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Worst-case text kept in one response. JSON escapes a control byte as six
/// bytes, so this stays comfortably under `MAX_MESSAGE_BYTES`.
const MAX_RESPONSE_TEXT_BYTES: usize = 96 * 1024;

/// Keep the newest lines that fit the response budget, truncating a single
/// oversized line, and report whether anything was dropped.
fn fit_lines(lines: Vec<HistoryLine>) -> (Vec<HistoryLine>, bool) {
    let mut kept: Vec<HistoryLine> = Vec::new();
    let mut total = 0usize;
    let mut truncated = false;
    for mut line in lines.into_iter().rev() {
        if line.text.len() > MAX_RESPONSE_TEXT_BYTES {
            let mut cut = MAX_RESPONSE_TEXT_BYTES;
            while cut > 0 && !line.text.is_char_boundary(cut) {
                cut -= 1;
            }
            line.text.truncate(cut);
            truncated = true;
        }
        if !kept.is_empty() && total + line.text.len() > MAX_RESPONSE_TEXT_BYTES {
            truncated = true;
            break;
        }
        total += line.text.len();
        kept.push(line);
    }
    kept.reverse();
    (kept, truncated)
}

/// The core's shared engine: owns devices, clients, control and history.
///
/// One instance serves every connection on one socket. All serial I/O threads
/// feed [`DeviceEvent`]s here; one dispatcher thread serializes them, expires
/// idle control and enforces the exit grace period.
pub struct Engine {
    cfg: CoreConfig,
    socket: PathBuf,
    state: Mutex<State>,
    clients: Mutex<HashMap<u64, ClientRecord>>,
    client_count: AtomicUsize,
    next_client_id: AtomicU64,
    next_waiter_id: AtomicU64,
    events_tx: Sender<DeviceEvent>,
    events_rx: Receiver<DeviceEvent>,
    shutdown_tx: Sender<String>,
    shutting_down: AtomicBool,
    sharing: Mutex<Option<Arc<SharingSession>>>,
}

impl Engine {
    pub fn new(cfg: CoreConfig, socket: PathBuf, shutdown_tx: Sender<String>) -> Arc<Self> {
        let (events_tx, events_rx) = unbounded();
        Arc::new(Self {
            cfg,
            socket,
            state: Mutex::new(State {
                devices: HashMap::new(),
                next_device_id: 1,
                grace_since: None,
                shutting_down: false,
            }),
            clients: Mutex::new(HashMap::new()),
            client_count: AtomicUsize::new(0),
            next_client_id: AtomicU64::new(1),
            next_waiter_id: AtomicU64::new(1),
            events_tx,
            events_rx,
            shutdown_tx,
            shutting_down: AtomicBool::new(false),
            sharing: Mutex::new(None),
        })
    }

    /// Start the dispatcher that handles device I/O, timers and shutdown.
    pub fn start(self: &Arc<Self>) {
        let engine = Arc::clone(self);
        std::thread::Builder::new()
            .name("serial-nexus-core-engine".into())
            .spawn(move || engine.dispatch_loop())
            .expect("could not start engine thread");
    }

    pub fn config(&self) -> &CoreConfig {
        &self.cfg
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    fn dispatch_loop(self: Arc<Self>) {
        let ticker = crossbeam_channel::tick(Duration::from_millis(50));
        loop {
            crossbeam_channel::select! {
                recv(self.events_rx) -> message => match message {
                    Ok(event) => self.on_device_event(event),
                    Err(_) => return,
                },
                recv(ticker) -> _ => {
                    if self.tick() {
                        return;
                    }
                }
            }
        }
    }

    fn on_device_event(&self, event: DeviceEvent) {
        match event {
            DeviceEvent::Data { device_id, bytes } => self.on_device_data(device_id, bytes),
            DeviceEvent::Written { .. } => {}
            DeviceEvent::Failed { device_id, reason } => self.on_device_failed(device_id, reason),
        }
    }

    // ---- client lifecycle -------------------------------------------------

    /// Register a connection and return its client id. Every connection gets a
    /// sender for events until it unregisters.
    pub fn register_client(&self, tx: Sender<ServerMessage>) -> u64 {
        let id = self.next_client_id.fetch_add(1, Ordering::SeqCst);
        self.clients.lock().expect("clients lock").insert(
            id,
            ClientRecord {
                tx,
                kind: ClientKind::Other,
                name: None,
                hello_done: false,
                remote: None,
            },
        );
        self.client_count.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut state) = self.state.lock() {
            state.grace_since = None;
        }
        id
    }

    /// Remove a connection: detach it from every device, release control it
    /// held, and close devices it was the last client of.
    pub fn unregister_client(&self, client_id: u64) {
        let session = self.sharing_session();
        // The owning local terminal is the only reason sharing exists: when
        // its connection goes away, close the listener and every remote
        // session before anything else is torn down.
        if self
            .sharing
            .lock()
            .expect("sharing lock")
            .as_ref()
            .is_some_and(|session| session.owner_client_id() == client_id)
        {
            self.stop_sharing();
        }
        self.clients
            .lock()
            .expect("clients lock")
            .remove(&client_id);
        let previous = self.client_count.fetch_sub(1, Ordering::SeqCst);
        let remaining = previous.saturating_sub(1);

        let mut events = Vec::new();
        let mut removed = Vec::new();
        let mut state = self.state.lock().expect("state lock");
        let device_ids: Vec<u64> = state
            .devices
            .iter()
            .filter(|(_, device)| device.clients.contains_key(&client_id))
            .map(|(id, _)| *id)
            .collect();
        for device_id in device_ids {
            let empty = {
                let Some(device) = state.devices.get_mut(&device_id) else {
                    continue;
                };
                if let Some(attached) = device.clients.remove(&client_id) {
                    events.push(Event::ClientLeft {
                        device_id,
                        client: Holder {
                            client_id,
                            name: attached.name,
                            kind: attached.kind,
                        },
                    });
                }
                if device
                    .control
                    .as_ref()
                    .is_some_and(|c| c.client_id == client_id)
                {
                    if let Some(holder) = take_control(device) {
                        events.push(Event::ControlReleased {
                            device_id,
                            holder,
                            reason: ControlReleaseReason::Disconnected,
                        });
                    }
                }
                // This client can no longer receive a waited write's answer;
                // release its waiters instead of leaving them until timeout.
                let mut kept = Vec::new();
                for waiter in std::mem::take(&mut device.waiters) {
                    if waiter.client_id == client_id {
                        let _ = waiter.tx.send(WaitOutcome {
                            lines: waiter.lines,
                            timed_out: false,
                            failed: Some("client disconnected".into()),
                        });
                    } else {
                        kept.push(waiter);
                    }
                }
                device.waiters = kept;
                device.clients.is_empty()
            };
            if empty {
                if let Some(device) = take_device_locked(
                    &mut state,
                    device_id,
                    "last client disconnected",
                    &mut events,
                    session.as_deref(),
                ) {
                    removed.push(device);
                }
            }
        }
        if remaining == 0 {
            state.grace_since = Some(Instant::now());
        }
        drop(state);
        // Release the serial handles before anyone is told the device closed.
        join_devices(removed);
        self.emit(events);
    }

    // ---- requests ---------------------------------------------------------

    pub fn handle(
        self: &Arc<Self>,
        client_id: u64,
        request: Request,
    ) -> Result<ResponseData, ErrorBody> {
        if self.is_shutting_down() && !matches!(request, Request::Ping) {
            return Err(ErrorBody::new(
                ErrorCode::ShuttingDown,
                "core is shutting down",
            ));
        }
        let hello_done = {
            let clients = self.clients.lock().expect("clients lock");
            clients
                .get(&client_id)
                .map(|record| record.hello_done)
                .unwrap_or(false)
        };
        if !hello_done && !matches!(request, Request::Hello(_) | Request::Ping) {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "hello must be the first request on a connection",
            ));
        }
        match request {
            Request::Hello(params) => self.hello(client_id, params),
            Request::Ping => Ok(ResponseData::Pong),
            Request::ListDevices => Ok(ResponseData::Devices(self.list_devices())),
            Request::Open(params) => self.open(client_id, params),
            Request::Close(device) => self.close(client_id, device.device_id),
            Request::Read(params) => self.read(client_id, params),
            Request::Write(params) => self.write(client_id, params),
            Request::Status(device) => self.status(client_id, device.device_id),
            Request::ShareStart(params) => self.start_sharing(client_id, params),
            Request::ShareStop => self.stop_sharing_from_owner(client_id),
            Request::ShareCheck(params) => self.check_shared_devices(client_id, params),
            Request::ShareUncheck(params) => self.uncheck_shared_devices(client_id, params),
            Request::ShareStatus => self.sharing_status(client_id),
        }
    }

    fn hello(&self, client_id: u64, params: HelloParams) -> Result<ResponseData, ErrorBody> {
        if params.protocol != PROTOCOL_VERSION {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                format!(
                    "core speaks protocol {PROTOCOL_VERSION}, client speaks {}",
                    params.protocol
                ),
            ));
        }
        {
            let mut clients = self.clients.lock().expect("clients lock");
            let record = clients
                .get_mut(&client_id)
                .ok_or_else(|| ErrorBody::new(ErrorCode::Internal, "unknown client"))?;
            record.kind = params.client.kind;
            record.name = params.client.name;
            record.hello_done = true;
        }
        Ok(ResponseData::Hello(HelloResult {
            client_id,
            protocol: PROTOCOL_VERSION,
            core_version: env!("CARGO_PKG_VERSION").to_string(),
            socket: self.socket.display().to_string(),
            limits: self.cfg.limits(),
        }))
    }

    fn list_devices(&self) -> serial_nexus_protocol::DevicesResult {
        let ports = enumerate::list_ports();
        let state = self.state.lock().expect("state lock");
        let mut devices: Vec<DeviceSummary> = Vec::new();
        let mut seen = HashSet::new();
        for port in ports {
            seen.insert(port.path.clone());
            match state
                .devices
                .values()
                .find(|device| device.path == port.path)
            {
                Some(device) => {
                    let mut entry = summary(device);
                    entry.description = port.description;
                    devices.push(entry);
                }
                None => devices.push(DeviceSummary {
                    path: port.path,
                    description: port.description,
                    open: false,
                    device_id: None,
                    baud: None,
                    client_count: 0,
                    control_holder: None,
                }),
            }
        }
        for device in state.devices.values() {
            if !seen.contains(&device.path) {
                devices.push(summary(device));
            }
        }
        devices.sort_by(|a, b| a.path.cmp(&b.path));
        serial_nexus_protocol::DevicesResult { devices }
    }

    fn open(&self, client_id: u64, params: OpenParams) -> Result<ResponseData, ErrorBody> {
        self.open_with_identity(client_id, params, None, None)
    }

    fn open_with_identity(
        &self,
        client_id: u64,
        params: OpenParams,
        expected: Option<&DeviceIdentity>,
        session: Option<&Arc<SharingSession>>,
    ) -> Result<ResponseData, ErrorBody> {
        let (kind, name) = self.identity(client_id);
        // Resolve aliases (symlinks, `..`) so two paths to one device join the
        // same open device instead of colliding at the OS level.
        let port = identity_path(&params.port);
        let mut state = self.state.lock().expect("state lock");
        let mut events = Vec::new();

        let existing_id = state
            .devices
            .values()
            .find(|device| device.path == port)
            .map(|device| device.id);

        let device_id = match existing_id {
            Some(device_id) => {
                let device = state.devices.get_mut(&device_id).expect("device exists");
                if let Some(requested) = params.baud {
                    if requested != device.baud {
                        return Err(ErrorBody::new(
                            ErrorCode::BaudConflict,
                            format!(
                                "{} is already open at {} baud, not {requested}",
                                device.path, device.baud
                            ),
                        )
                        .with_details(serde_json::json!({ "active_baud": device.baud })));
                    }
                }
                attach(
                    device,
                    client_id,
                    kind,
                    name,
                    params.request_control,
                    &mut events,
                );
                device_id
            }
            None => {
                let baud = params.baud.unwrap_or(self.cfg.default_baud);
                if baud == 0 || baud > 10_000_000 {
                    return Err(ErrorBody::new(
                        ErrorCode::BadRequest,
                        format!("baud rate {baud} is out of range"),
                    ));
                }
                let read_port =
                    device::open_serial_checked(&port, baud, expected).map_err(|error| {
                        let code = match error.kind() {
                            std::io::ErrorKind::NotFound => ErrorCode::NotFound,
                            std::io::ErrorKind::PermissionDenied => ErrorCode::PermissionDenied,
                            _ => ErrorCode::DeviceFailed,
                        };
                        ErrorBody::new(code, format!("could not open {port}: {error}"))
                    })?;
                // Re-check that the authorization still exists after the open,
                // while the engine state lock keeps a concurrent close/revoke
                // from racing this device into service.
                if let (Some(session), Some(expected)) = (session, expected) {
                    let still_shared = session
                        .authorize(&port)
                        .map(|current| current == *expected)
                        .unwrap_or(false);
                    if !still_shared {
                        return Err(ErrorBody::new(
                            ErrorCode::DeviceFailed,
                            format!("{port} is no longer shared; check it again"),
                        ));
                    }
                }
                let write_port = read_port.try_clone().map_err(|error| {
                    ErrorBody::new(
                        ErrorCode::Internal,
                        format!("could not clone {port}: {error}"),
                    )
                })?;
                let device_id = state.next_device_id;
                state.next_device_id += 1;
                // Bounded so a slow device cannot accumulate unbounded queued
                // write bytes from any client.
                let (write_tx, write_rx) = crossbeam_channel::bounded(64);
                let write_epochs: device::WriteEpochs = Arc::new(Mutex::new(HashMap::new()));
                let stop = Arc::new(AtomicBool::new(false));
                let io = device::spawn_io(
                    device_id,
                    &port,
                    read_port,
                    write_port,
                    write_rx,
                    Arc::clone(&write_epochs),
                    Arc::clone(&stop),
                    self.events_tx.clone(),
                )
                .map_err(|error| {
                    ErrorBody::new(
                        ErrorCode::Internal,
                        format!("could not start I/O for {port}: {error}"),
                    )
                })?;
                let device = Device {
                    id: device_id,
                    path: port,
                    baud,
                    history: History::new(self.cfg.history_max_lines, self.cfg.history_max_bytes),
                    tail: Vec::new(),
                    tail_seq: None,
                    clients: BTreeMap::new(),
                    control: None,
                    write_tx: Some(write_tx),
                    write_epochs,
                    stop,
                    waiters: Vec::new(),
                    io: Some(io),
                };
                state.devices.insert(device_id, device);
                let device = state.devices.get_mut(&device_id).expect("just inserted");
                attach(
                    device,
                    client_id,
                    kind,
                    name,
                    params.request_control,
                    &mut events,
                );
                device_id
            }
        };

        let device = state.devices.get(&device_id).expect("device exists");
        let role = match kind {
            ClientKind::Human => Role::Human,
            _ if device
                .control
                .as_ref()
                .is_some_and(|c| c.client_id == client_id) =>
            {
                Role::Controller
            }
            _ => Role::Reader,
        };
        let result = OpenResult {
            device_id,
            port: device.path.clone(),
            baud: device.baud,
            role,
            control: self.control_state(device),
        };
        drop(state);
        self.emit(events);
        Ok(ResponseData::Opened(result))
    }

    fn close(&self, client_id: u64, device_id: u64) -> Result<ResponseData, ErrorBody> {
        let (kind, name) = self.identity(client_id);
        let session = self.sharing_session();
        let mut state = self.state.lock().expect("state lock");
        let mut events = Vec::new();
        let mut removed = None;
        let empty = {
            let Some(device) = state.devices.get_mut(&device_id) else {
                return Err(ErrorBody::new(
                    ErrorCode::NotFound,
                    format!("device {device_id} is not open"),
                ));
            };
            if !device.clients.contains_key(&client_id) {
                return Err(ErrorBody::new(
                    ErrorCode::NotAttached,
                    format!("client {client_id} is not attached to device {device_id}"),
                ));
            }
            device.clients.remove(&client_id);
            events.push(Event::ClientLeft {
                device_id,
                client: Holder {
                    client_id,
                    name,
                    kind,
                },
            });
            if device
                .control
                .as_ref()
                .is_some_and(|control| control.client_id == client_id)
            {
                if let Some(holder) = take_control(device) {
                    events.push(Event::ControlReleased {
                        device_id,
                        holder,
                        reason: ControlReleaseReason::Disconnected,
                    });
                }
            }
            device.clients.is_empty()
        };
        if empty {
            removed = take_device_locked(
                &mut state,
                device_id,
                "last client disconnected",
                &mut events,
                session.as_deref(),
            );
        }
        drop(state);
        if let Some(device) = removed {
            join_devices(vec![device]);
        }
        self.emit(events);
        Ok(ResponseData::Closed)
    }

    fn read(&self, client_id: u64, params: ReadParams) -> Result<ResponseData, ErrorBody> {
        let mut state = self.state.lock().expect("state lock");
        let device = state.devices.get_mut(&params.device_id).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("device {} is not open", params.device_id),
            )
        })?;
        if !device.clients.contains_key(&client_id) {
            return Err(ErrorBody::new(
                ErrorCode::NotAttached,
                "open the device before reading from it",
            ));
        }
        let wanted = params
            .lines
            .unwrap_or(self.cfg.read_default_lines)
            .clamp(1, self.cfg.read_max_lines);
        refresh_control(device, client_id);
        // Merge the open tail into the chronological order of complete lines
        // and keep the newest `wanted` entries. An old prompt must not
        // displace newer writes just because it is still unfinished.
        let mut entries: Vec<(u64, HistoryLine)> = device
            .history
            .recent(wanted)
            .iter()
            .map(|line| (line.seq, line.to_wire()))
            .collect();
        if let (false, Some(seq)) = (device.tail.is_empty(), device.tail_seq) {
            entries.push((
                seq,
                HistoryLine {
                    source: Source::Device,
                    text: String::from_utf8_lossy(&device.tail).into_owned(),
                    partial: true,
                },
            ));
        }
        entries.sort_by_key(|(seq, _)| *seq);
        let lines: Vec<HistoryLine> = entries
            .into_iter()
            .map(|(_, line)| line)
            .rev()
            .take(wanted)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let (lines, truncated) = fit_lines(lines);
        Ok(ResponseData::Read(ReadResult {
            lines,
            dropped_lines: device.history.dropped(),
            truncated,
        }))
    }

    fn write(&self, client_id: u64, params: WriteParams) -> Result<ResponseData, ErrorBody> {
        match self.begin_write(client_id, params)? {
            WriteStart::Done(result) => Ok(ResponseData::Written(result)),
            WriteStart::Pending(pending) => Ok(ResponseData::Written(self.finish_write(pending)?)),
        }
    }

    /// Validate and enqueue a write in request-arrival order. When the caller
    /// asked to wait for response lines, the wait is returned unresolved so a
    /// server can offload it without delaying later writes.
    pub fn begin_write(
        &self,
        client_id: u64,
        params: WriteParams,
    ) -> Result<WriteStart, ErrorBody> {
        let (kind, _name) = self.identity(client_id);
        let data = decode_bytes(&params.data_b64)
            .map_err(|error| ErrorBody::new(ErrorCode::BadRequest, error.to_string()))?;
        let bytes_written = data.len();

        let mut state = self.state.lock().expect("state lock");
        let device = state.devices.get_mut(&params.device_id).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("device {} is not open", params.device_id),
            )
        })?;
        if !device.clients.contains_key(&client_id) {
            return Err(ErrorBody::new(
                ErrorCode::NotAttached,
                "open the device before writing to it",
            ));
        }
        let source = match kind {
            ClientKind::Ai => Source::Ai,
            ClientKind::Human => Source::Human,
            ClientKind::Other => {
                return Err(ErrorBody::new(
                    ErrorCode::PermissionDenied,
                    "only AI and human clients may write to a serial device",
                ))
            }
        };
        if kind == ClientKind::Ai
            && device
                .control
                .as_ref()
                .is_none_or(|control| control.client_id != client_id)
        {
            let holder = device
                .control
                .as_ref()
                .map(|control| holder_of(device, control.client_id).label())
                .unwrap_or_else(|| "nobody".to_string());
            return Err(ErrorBody::new(
                ErrorCode::ControlHeld,
                format!(
                    "write control of {} is held by {holder}; call open with request_control",
                    device.path
                ),
            ));
        }
        refresh_control(device, client_id);
        let epoch = device
            .write_epochs
            .lock()
            .expect("write epochs lock")
            .get(&client_id)
            .copied()
            .unwrap_or(0);
        record_written_lines(device, source, &data);
        device
            .write_tx
            .as_ref()
            .ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::DeviceClosed,
                    format!("device {} was closed", params.device_id),
                )
            })?
            .try_send(device::WriteCommand {
                device_id: params.device_id,
                client_id,
                epoch,
                bytes: data.clone(),
            })
            .map_err(|error| match error {
                crossbeam_channel::TrySendError::Full(_) => ErrorBody::new(
                    ErrorCode::LimitExceeded,
                    format!(
                        "device {} has too much queued work; retry shortly",
                        params.device_id
                    ),
                ),
                crossbeam_channel::TrySendError::Disconnected(_) => ErrorBody::new(
                    ErrorCode::DeviceClosed,
                    format!("device {} was closed", params.device_id),
                ),
            })?;

        let pending = params.wait.and_then(|spec| {
            if spec.lines == 0 {
                return None;
            }
            let waiter_id = self.next_waiter_id.fetch_add(1, Ordering::SeqCst);
            let wanted = spec.lines.min(self.cfg.read_max_lines);
            let timeout = Duration::from_millis(
                spec.timeout_ms
                    .unwrap_or(self.cfg.write_default_timeout.as_millis() as u64)
                    .min(self.cfg.write_max_timeout.as_millis() as u64),
            );
            let (tx, rx) = crossbeam_channel::bounded(1);
            let after_seq = device.history.next_seq();
            device.waiters.push(Waiter {
                id: waiter_id,
                client_id,
                after_seq,
                wanted,
                lines: Vec::new(),
                tx,
            });
            Some((waiter_id, rx, timeout, after_seq))
        });
        let device_id = params.device_id;
        let event = Event::write(device_id, source, &data);
        drop(state);
        self.emit(vec![event]);

        match pending {
            None => Ok(WriteStart::Done(WriteResult {
                bytes_written,
                lines: Vec::new(),
                timed_out: false,
                truncated: false,
            })),
            Some((waiter_id, rx, timeout, _after_seq)) => Ok(WriteStart::Pending(PendingWrite {
                device_id,
                waiter_id,
                rx,
                timeout,
                bytes_written,
            })),
        }
    }

    /// Wait for the response lines of a write prepared by [`Self::begin_write`].
    pub fn finish_write(&self, pending: PendingWrite) -> Result<WriteResult, ErrorBody> {
        let PendingWrite {
            device_id,
            waiter_id,
            rx,
            timeout,
            bytes_written,
        } = pending;
        match rx.recv_timeout(timeout + Duration::from_millis(50)) {
            Ok(outcome) if outcome.failed.is_none() => {
                let (lines, truncated) = fit_lines(outcome.lines);
                Ok(WriteResult {
                    bytes_written,
                    lines,
                    timed_out: outcome.timed_out,
                    truncated,
                })
            }
            Ok(outcome) => Err(ErrorBody::new(
                ErrorCode::DeviceFailed,
                format!(
                    "device {device_id} failed during the write: {}; the bytes may already have reached the device and are not replayed",
                    outcome.failed.unwrap_or_default()
                ),
            )
            .with_details(serde_json::json!({ "may_have_reached_device": true }))),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // A waiter may have completed in the gap before the timeout;
                // keep its lines rather than losing them.
                let mut lines = match rx.try_recv() {
                    Ok(outcome) => outcome.lines,
                    Err(_) => self.remove_waiter(device_id, waiter_id),
                };
                if let Some(tail) = self.snapshot_tail(device_id) {
                    lines.push(HistoryLine {
                        source: Source::Device,
                        text: tail,
                        partial: true,
                    });
                }
                let (lines, truncated) = fit_lines(lines);
                Ok(WriteResult {
                    bytes_written,
                    lines,
                    timed_out: true,
                    truncated,
                })
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                let (lines, truncated) = fit_lines(self.remove_waiter(device_id, waiter_id));
                Ok(WriteResult {
                    bytes_written,
                    lines,
                    timed_out: true,
                    truncated,
                })
            }
        }
    }

    fn status(&self, client_id: u64, device_id: u64) -> Result<ResponseData, ErrorBody> {
        let mut state = self.state.lock().expect("state lock");
        let device = state.devices.get_mut(&device_id).ok_or_else(|| {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("device {device_id} is not open"),
            )
        })?;
        if !device.clients.contains_key(&client_id) {
            return Err(ErrorBody::new(
                ErrorCode::NotAttached,
                "open the device first",
            ));
        }
        refresh_control(device, client_id);
        Ok(ResponseData::Status(DeviceStatus {
            device_id,
            path: device.path.clone(),
            baud: device.baud,
            client_count: device.clients.len(),
            control: self.control_state(device),
            history_lines: device.history.len(),
            dropped_lines: device.history.dropped(),
        }))
    }

    // ---- device events ----------------------------------------------------

    fn on_device_data(&self, device_id: u64, bytes: Vec<u8>) {
        let mut completed: Vec<(Sender<WaitOutcome>, WaitOutcome)> = Vec::new();
        let event = Event::device_output(device_id, &bytes);
        {
            let mut state = self.state.lock().expect("state lock");
            let Some(device) = state.devices.get_mut(&device_id) else {
                return;
            };
            // Parse the continuous stream: the previous partial line is the
            // head of this chunk, so chunk boundaries never corrupt a line.
            let mut data = std::mem::take(&mut device.tail);
            data.extend_from_slice(&bytes);
            let mut start = 0usize;
            for index in 0..data.len() {
                if data[index] != b'\n' {
                    continue;
                }
                let mut line = data[start..index].to_vec();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                start = index + 1;
                let wire = HistoryLine {
                    source: Source::Device,
                    text: String::from_utf8_lossy(&line).into_owned(),
                    partial: false,
                };
                let seq = device.history.push(Source::Device, line, false);
                for waiter in &mut device.waiters {
                    if seq >= waiter.after_seq {
                        waiter.lines.push(wire.clone());
                    }
                }
            }
            let leftover = &data[start..];
            if leftover.is_empty() {
                device.tail.clear();
                device.tail_seq = None;
            } else {
                if device.tail_seq.is_none() {
                    device.tail_seq = Some(device.history.reserve_seq());
                }
                if leftover.len() > self.cfg.history_max_bytes {
                    let tail = leftover.to_vec();
                    device.history.push(Source::Device, tail, true);
                    device.tail.clear();
                    device.tail_seq = None;
                } else {
                    device.tail = leftover.to_vec();
                }
            }

            let tail_line = match (device.tail.is_empty(), device.tail_seq) {
                (false, Some(seq)) => Some((
                    seq,
                    HistoryLine {
                        source: Source::Device,
                        text: String::from_utf8_lossy(&device.tail).into_owned(),
                        partial: true,
                    },
                )),
                _ => None,
            };
            let mut index = device.waiters.len();
            while index > 0 {
                index -= 1;
                if device.waiters[index].lines.len() >= device.waiters[index].wanted {
                    let mut waiter = device.waiters.remove(index);
                    // "What arrived": include a partial tail only when it
                    // started after this write, not an older prompt.
                    if let Some((seq, line)) = &tail_line {
                        if *seq >= waiter.after_seq {
                            waiter.lines.push(line.clone());
                        }
                    }
                    completed.push((
                        waiter.tx,
                        WaitOutcome {
                            lines: waiter.lines,
                            timed_out: false,
                            failed: None,
                        },
                    ));
                }
            }
        }
        for (tx, outcome) in completed {
            let _ = tx.send(outcome);
        }
        self.emit(vec![event]);
    }

    fn on_device_failed(&self, device_id: u64, reason: String) {
        let session = self.sharing_session();
        let mut events = Vec::new();
        let reason_text = format!("device failed: {reason}");
        let removed = {
            let mut state = self.state.lock().expect("state lock");
            take_device_locked(
                &mut state,
                device_id,
                &reason_text,
                &mut events,
                session.as_deref(),
            )
        };
        if let Some(device) = removed {
            join_devices(vec![device]);
        }
        // Tell clients (including remote ones) first: the allowlist still
        // names the path, so a shared device's failure is reported rather than
        // silently disappearing.
        self.emit(events);
    }

    fn remove_waiter(&self, device_id: u64, waiter_id: u64) -> Vec<HistoryLine> {
        let mut state = self.state.lock().expect("state lock");
        let Some(device) = state.devices.get_mut(&device_id) else {
            return Vec::new();
        };
        if let Some(position) = device
            .waiters
            .iter()
            .position(|waiter| waiter.id == waiter_id)
        {
            return device.waiters.remove(position).lines;
        }
        Vec::new()
    }

    /// Copy the current partial tail without consuming it. A timed-out send
    /// returns it (marked partial), but must not remove it from shared history.
    fn snapshot_tail(&self, device_id: u64) -> Option<String> {
        let state = self.state.lock().expect("state lock");
        let device = state.devices.get(&device_id)?;
        if device.tail.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&device.tail).into_owned())
    }

    // ---- timers and shutdown ---------------------------------------------

    fn tick(&self) -> bool {
        let now = Instant::now();
        let mut events = Vec::new();
        let graceful_exit;
        {
            let mut state = self.state.lock().expect("state lock");
            if state.shutting_down {
                return true;
            }
            let expire: Vec<u64> = state
                .devices
                .iter()
                .filter(|(_, device)| {
                    device.control.as_ref().is_some_and(|control| {
                        now.duration_since(control.last_activity) >= self.cfg.control_idle
                    })
                })
                .map(|(id, _)| *id)
                .collect();
            for device_id in expire {
                if let Some(device) = state.devices.get_mut(&device_id) {
                    if let Some(holder) = take_control(device) {
                        events.push(Event::ControlReleased {
                            device_id,
                            holder,
                            reason: ControlReleaseReason::Idle,
                        });
                    }
                }
            }
            if self.client_count.load(Ordering::SeqCst) == 0 {
                match state.grace_since {
                    None => {
                        state.grace_since = Some(now);
                        graceful_exit = false;
                    }
                    Some(since) if now.duration_since(since) >= self.cfg.grace => {
                        state.shutting_down = true;
                        graceful_exit = true;
                    }
                    Some(_) => graceful_exit = false,
                }
            } else {
                state.grace_since = None;
                graceful_exit = false;
            }
        }
        self.emit(events);
        if graceful_exit {
            self.shutting_down.store(true, Ordering::SeqCst);
            let _ = self.shutdown_tx.send("no clients left".to_string());
            return true;
        }
        false
    }

    /// Close every device and tell clients the core is going away.
    pub fn shutdown_all(&self, reason: &str) {
        let session = self.sharing_session();
        self.shutting_down.store(true, Ordering::SeqCst);
        // Close the sharing listener first: no new remote work can start, and
        // every remote session is disconnected before devices close.
        self.stop_sharing();
        let mut events = Vec::new();
        let mut removed = Vec::new();
        {
            let mut state = self.state.lock().expect("state lock");
            let ids: Vec<u64> = state.devices.keys().copied().collect();
            for device_id in ids {
                if let Some(device) = take_device_locked(
                    &mut state,
                    device_id,
                    reason,
                    &mut events,
                    session.as_deref(),
                ) {
                    removed.push(device);
                }
            }
        }
        join_devices(removed);
        events.push(Event::CoreShutdown {
            reason: reason.to_string(),
        });
        self.emit(events);
    }

    // ---- LAN sharing ------------------------------------------------------

    fn share_limits(&self) -> ShareLimits {
        ShareLimits {
            handshake_timeout: self.cfg.share_handshake_timeout,
            auth_timeout: self.cfg.share_auth_timeout,
            max_connections: self.cfg.share_max_connections,
            max_pending_writes: self.cfg.share_max_pending_writes,
        }
    }

    pub fn sharing_session(&self) -> Option<Arc<SharingSession>> {
        self.sharing.lock().expect("sharing lock").clone()
    }

    /// The local owner of the active session, or an error that never reveals
    /// session details.
    fn owner_session(&self, client_id: u64) -> Result<Arc<SharingSession>, ErrorBody> {
        let session = self
            .sharing_session()
            .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "sharing is not active"))?;
        if session.owner_client_id() != client_id {
            return Err(ErrorBody::new(
                ErrorCode::PermissionDenied,
                "only the local terminal that started sharing may control it",
            ));
        }
        Ok(session)
    }

    /// Capture a detected device's identity. Checking never opens the device.
    fn detected_device(&self, path: &str) -> Result<DeviceIdentity, ErrorBody> {
        let identity = DeviceIdentity::capture(path).map_err(|error| {
            ErrorBody::new(
                ErrorCode::NotFound,
                format!("{path} is not a usable device: {error}"),
            )
        })?;
        let detected = enumerate::list_ports()
            .iter()
            .any(|port| port.path == identity.canonical)
            || self
                .state
                .lock()
                .expect("state lock")
                .devices
                .values()
                .any(|device| device.path == identity.canonical);
        if !detected {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("{} is not a detected serial device", identity.canonical),
            ));
        }
        Ok(identity)
    }

    fn start_sharing(
        self: &Arc<Self>,
        client_id: u64,
        params: ShareStartParams,
    ) -> Result<ResponseData, ErrorBody> {
        let (kind, _) = self.identity(client_id);
        if kind != ClientKind::Human {
            return Err(ErrorBody::new(
                ErrorCode::PermissionDenied,
                "only a local human terminal may enable sharing",
            ));
        }
        let mut guard = self.sharing.lock().expect("sharing lock");
        if guard.is_some() {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "sharing is already active; stop it before starting again",
            ));
        }
        params
            .bind
            .validate()
            .map_err(|message| ErrorBody::new(ErrorCode::BadRequest, message))?;
        if params.devices.len() > MAX_SHARED_DEVICES {
            return Err(ErrorBody::new(
                ErrorCode::LimitExceeded,
                format!("at most {MAX_SHARED_DEVICES} devices may be shared"),
            ));
        }
        let mut identities = Vec::new();
        let mut seen = HashSet::new();
        for path in &params.devices {
            let identity = self.detected_device(path)?;
            if seen.insert(identity.canonical.clone()) {
                identities.push(identity);
            }
        }
        // Binding is synchronous: a failure here leaves sharing off.
        let prepared = share::prepare(client_id, &params, identities, self.share_limits())
            .map_err(|message| ErrorBody::new(ErrorCode::DeviceFailed, message))?;
        *guard = Some(Arc::clone(&prepared.session));
        if let Err(error) = share::spawn_listener(Arc::clone(self), Arc::clone(&prepared.session)) {
            guard.take();
            prepared.session.stop();
            return Err(ErrorBody::new(
                ErrorCode::Internal,
                format!("could not start the sharing listener: {error}"),
            ));
        }
        drop(guard);
        Ok(ResponseData::ShareStarted(prepared.info))
    }

    /// Stop sharing: close the listener, disconnect every remote client and
    /// revoke the session's credentials.
    pub fn stop_sharing(&self) {
        let session = self.sharing.lock().expect("sharing lock").take();
        if let Some(session) = session {
            session.stop();
        }
    }

    fn stop_sharing_from_owner(&self, client_id: u64) -> Result<ResponseData, ErrorBody> {
        self.owner_session(client_id)?;
        self.stop_sharing();
        Ok(ResponseData::ShareStopped(
            serial_nexus_protocol::ShareStopped { was_active: true },
        ))
    }

    fn check_shared_devices(
        &self,
        client_id: u64,
        params: ShareDevicesParams,
    ) -> Result<ResponseData, ErrorBody> {
        let session = self.owner_session(client_id)?;
        if session.is_stopped() {
            return Err(ErrorBody::new(
                ErrorCode::ShuttingDown,
                "sharing is stopping",
            ));
        }
        for path in &params.devices {
            let identity = self.detected_device(path)?;
            session.check(identity)?;
        }
        Ok(ResponseData::Sharing(session.snapshot()))
    }

    fn uncheck_shared_devices(
        &self,
        client_id: u64,
        params: ShareDevicesParams,
    ) -> Result<ResponseData, ErrorBody> {
        let session = self.owner_session(client_id)?;
        for path in &params.devices {
            if let Some(identity) = session.uncheck(path) {
                let device_ids: Vec<u64> = self
                    .state
                    .lock()
                    .expect("state lock")
                    .devices
                    .iter()
                    .filter(|(_, device)| device.path == identity.canonical)
                    .map(|(id, _)| *id)
                    .collect();
                // Revoke immediately: detach remote clients and fail their
                // pending work. Local clients and other devices are untouched.
                self.detach_remote_clients(&device_ids, "device was unshared");
            }
        }
        Ok(ResponseData::Sharing(session.snapshot()))
    }

    fn sharing_status(&self, client_id: u64) -> Result<ResponseData, ErrorBody> {
        match self.sharing_session() {
            Some(session) if session.owner_client_id() == client_id => {
                Ok(ResponseData::Sharing(session.snapshot()))
            }
            Some(_) => Err(ErrorBody::new(
                ErrorCode::PermissionDenied,
                "only the local terminal that started sharing may control it",
            )),
            None => Ok(ResponseData::Sharing(share::inactive_snapshot())),
        }
    }

    /// Register an authenticated remote connection. The role came from the
    /// credential; it is never read from the request.
    pub fn register_remote_client(
        &self,
        tx: Sender<ServerMessage>,
        session: Arc<SharingSession>,
        kind: ClientKind,
        name: Option<String>,
    ) -> u64 {
        let id = self.next_client_id.fetch_add(1, Ordering::SeqCst);
        self.clients.lock().expect("clients lock").insert(
            id,
            ClientRecord {
                tx,
                kind,
                name,
                hello_done: true,
                remote: Some(session),
            },
        );
        self.client_count.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut state) = self.state.lock() {
            state.grace_since = None;
        }
        id
    }

    fn remote_scope(&self, client_id: u64) -> Option<Arc<SharingSession>> {
        self.clients
            .lock()
            .expect("clients lock")
            .get(&client_id)
            .and_then(|record| record.remote.clone())
    }

    /// Every remote operation goes through here: ACL, role and lifecycle are
    /// enforced on the request, not on a UI.
    pub fn handle_remote(
        self: &Arc<Self>,
        client_id: u64,
        request: Request,
    ) -> Result<ResponseData, ErrorBody> {
        let Some(session) = self.remote_scope(client_id) else {
            return Err(ErrorBody::new(
                ErrorCode::PermissionDenied,
                "not an authenticated remote client",
            ));
        };
        if session.is_stopped() {
            return Err(ErrorBody::new(ErrorCode::ShuttingDown, "sharing stopped"));
        }
        if self.is_shutting_down() && !matches!(request, Request::Ping) {
            return Err(ErrorBody::new(
                ErrorCode::ShuttingDown,
                "core is shutting down",
            ));
        }
        match request {
            Request::Ping => Ok(ResponseData::Pong),
            Request::ListDevices => {
                let mut devices = self.list_devices();
                devices
                    .devices
                    .retain(|device| session.allows_path(&device.path));
                Ok(ResponseData::Devices(devices))
            }
            Request::Open(params) => {
                let identity = session.authorize(&params.port)?;
                self.open_with_identity(
                    client_id,
                    OpenParams {
                        port: identity.canonical.clone(),
                        ..params
                    },
                    Some(&identity),
                    Some(&session),
                )
            }
            Request::Close(device) => {
                self.authorize_remote_device(&session, device.device_id)?;
                self.handle(client_id, Request::Close(device))
            }
            Request::Read(params) => {
                self.authorize_remote_device(&session, params.device_id)?;
                self.handle(client_id, Request::Read(params))
            }
            Request::Status(device) => {
                self.authorize_remote_device(&session, device.device_id)?;
                self.handle(client_id, Request::Status(device))
            }
            Request::Write(_) => Err(ErrorBody::new(
                ErrorCode::Internal,
                "writes are authorized separately",
            )),
            Request::Hello(_) => Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "the connection is already authenticated",
            )),
            Request::ShareStart(_)
            | Request::ShareStop
            | Request::ShareCheck(_)
            | Request::ShareUncheck(_)
            | Request::ShareStatus => Err(ErrorBody::new(
                ErrorCode::PermissionDenied,
                "sharing is controlled only by the local owner",
            )),
        }
    }

    /// Validate a remote write's device before it is queued.
    pub fn handle_remote_write(
        &self,
        client_id: u64,
        params: WriteParams,
    ) -> Result<WriteStart, ErrorBody> {
        let Some(session) = self.remote_scope(client_id) else {
            return Err(ErrorBody::new(
                ErrorCode::PermissionDenied,
                "not an authenticated remote client",
            ));
        };
        if session.is_stopped() {
            return Err(ErrorBody::new(ErrorCode::ShuttingDown, "sharing stopped"));
        }
        self.authorize_remote_device(&session, params.device_id)?;
        self.begin_write(client_id, params)
    }

    fn authorize_remote_device(
        &self,
        session: &SharingSession,
        device_id: u64,
    ) -> Result<(), ErrorBody> {
        let path = self
            .state
            .lock()
            .expect("state lock")
            .devices
            .get(&device_id)
            .map(|device| device.path.clone());
        let Some(path) = path else {
            return Err(ErrorBody::new(
                ErrorCode::NotFound,
                format!("device {device_id} is not open"),
            ));
        };
        match session.authorize(&path) {
            Ok(_) => Ok(()),
            Err(error) => {
                // A changed device revokes access and fails remote work now.
                self.detach_remote_clients(&[device_id], "device is no longer shared");
                Err(error)
            }
        }
    }

    /// Detach remote clients from these devices and fail their pending waits,
    /// leaving local clients and other shared devices alone.
    fn detach_remote_clients(&self, device_ids: &[u64], reason: &str) {
        let session = self.sharing_session();
        let remote_ids: HashSet<u64> = self
            .clients
            .lock()
            .expect("clients lock")
            .iter()
            .filter(|(_, record)| record.remote.is_some())
            .map(|(id, _)| *id)
            .collect();
        if device_ids.is_empty() || remote_ids.is_empty() {
            return;
        }
        let mut events = Vec::new();
        let mut removed = Vec::new();
        {
            let mut state = self.state.lock().expect("state lock");
            for device_id in device_ids {
                let Some(device) = state.devices.get_mut(device_id) else {
                    continue;
                };
                let detach: Vec<u64> = device
                    .clients
                    .keys()
                    .filter(|client| remote_ids.contains(client))
                    .copied()
                    .collect();
                for client in detach {
                    if let Some(attached) = device.clients.remove(&client) {
                        events.push(Event::ClientLeft {
                            device_id: *device_id,
                            client: Holder {
                                client_id: client,
                                name: attached.name,
                                kind: attached.kind,
                            },
                        });
                    }
                    // Drop this remote client's queued, not-yet-started writes.
                    // The write already in the writer finishes (no rollback).
                    *device
                        .write_epochs
                        .lock()
                        .expect("write epochs lock")
                        .entry(client)
                        .or_insert(0) += 1;
                    if device
                        .control
                        .as_ref()
                        .is_some_and(|control| control.client_id == client)
                    {
                        if let Some(holder) = take_control(device) {
                            events.push(Event::ControlReleased {
                                device_id: *device_id,
                                holder,
                                reason: ControlReleaseReason::Disconnected,
                            });
                        }
                    }
                    let mut kept = Vec::new();
                    for waiter in std::mem::take(&mut device.waiters) {
                        if waiter.client_id == client {
                            let _ = waiter.tx.send(WaitOutcome {
                                lines: waiter.lines,
                                timed_out: false,
                                failed: Some(format!("device is no longer shared: {reason}")),
                            });
                        } else {
                            kept.push(waiter);
                        }
                    }
                    device.waiters = kept;
                }
                if device.clients.is_empty() {
                    if let Some(device) = take_device_locked(
                        &mut state,
                        *device_id,
                        reason,
                        &mut events,
                        session.as_deref(),
                    ) {
                        removed.push(device);
                    }
                }
            }
        }
        join_devices(removed);
        self.emit(events);
    }

    // ---- helpers ----------------------------------------------------------

    fn identity(&self, client_id: u64) -> (ClientKind, Option<String>) {
        let clients = self.clients.lock().expect("clients lock");
        match clients.get(&client_id) {
            Some(record) => (record.kind, record.name.clone()),
            None => (ClientKind::Other, None),
        }
    }

    fn control_state(&self, device: &Device) -> ControlState {
        match &device.control {
            Some(control) => {
                let elapsed = Instant::now().duration_since(control.last_activity);
                let remaining = self.cfg.control_idle.saturating_sub(elapsed);
                ControlState {
                    holder: Some(holder_of(device, control.client_id)),
                    expires_in_ms: Some(remaining.as_millis() as u64),
                }
            }
            None => ControlState {
                holder: None,
                expires_in_ms: None,
            },
        }
    }

    fn emit(&self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }
        // Copy senders out so the event path never holds the client lock while
        // looking up a device or a sharing allowlist.
        let clients: Vec<(Sender<ServerMessage>, Option<Arc<SharingSession>>)> = self
            .clients
            .lock()
            .expect("clients lock")
            .values()
            .map(|record| (record.tx.clone(), record.remote.clone()))
            .collect();
        for event in events {
            let message = ServerMessage::Event {
                event: event.clone(),
            };
            for (tx, remote) in &clients {
                match remote {
                    Some(session) => {
                        if !self.remote_may_see(session, &event) {
                            continue;
                        }
                        // A slow remote peer loses events (it can re-read
                        // history); it must never grow core memory or block
                        // local clients.
                        let _ = tx.try_send(message.clone());
                    }
                    None => {
                        let _ = tx.send(message.clone());
                    }
                }
            }
        }
    }

    /// Remote clients see events only for checked devices. A closed device is
    /// matched by its path, because its engine entry is already gone.
    fn remote_may_see(&self, session: &SharingSession, event: &Event) -> bool {
        if let Event::DeviceClosed { path, .. } = event {
            return session.names_path(path);
        }
        match event.device_id() {
            Some(device_id) => {
                let path = self
                    .state
                    .lock()
                    .expect("state lock")
                    .devices
                    .get(&device_id)
                    .map(|device| device.path.clone());
                path.is_some_and(|path| session.allows_path(&path))
            }
            None => true,
        }
    }
}

fn attach(
    device: &mut Device,
    client_id: u64,
    kind: ClientKind,
    name: Option<String>,
    request_control: bool,
    events: &mut Vec<Event>,
) {
    let mut granted = false;
    let mut denied = None;

    if request_control && kind == ClientKind::Ai {
        match device.control.as_ref().map(|control| control.client_id) {
            Some(holder_id) if holder_id == client_id => {
                if let Some(control) = device.control.as_mut() {
                    control.last_activity = Instant::now();
                }
                granted = true;
            }
            Some(holder_id) => denied = Some(holder_of(device, holder_id)),
            None => {
                device.control = Some(Control {
                    client_id,
                    last_activity: Instant::now(),
                });
                granted = true;
            }
        }
    }
    if let Some(control) = device.control.as_mut() {
        if control.client_id == client_id {
            control.last_activity = Instant::now();
        }
    }
    device.clients.insert(
        client_id,
        Attached {
            kind,
            name: name.clone(),
        },
    );
    let controller = device
        .control
        .as_ref()
        .is_some_and(|control| control.client_id == client_id);
    events.push(Event::ClientJoined {
        device_id: device.id,
        client: Holder {
            client_id,
            name: name.clone(),
            kind,
        },
        role: if kind == ClientKind::Human {
            Role::Human
        } else if controller {
            Role::Controller
        } else {
            Role::Reader
        },
    });
    if granted {
        events.push(Event::ControlGranted {
            device_id: device.id,
            holder: holder_of(device, client_id),
        });
    } else if let Some(holder) = denied {
        events.push(Event::ControlDenied {
            device_id: device.id,
            requester: Holder {
                client_id,
                name,
                kind,
            },
            holder,
        });
    }
}

fn refresh_control(device: &mut Device, client_id: u64) {
    if let Some(control) = device.control.as_mut() {
        if control.client_id == client_id {
            control.last_activity = Instant::now();
        }
    }
}

fn take_control(device: &mut Device) -> Option<Holder> {
    let control = device.control.take()?;
    Some(holder_of(device, control.client_id))
}

fn holder_of(device: &Device, client_id: u64) -> Holder {
    match device.clients.get(&client_id) {
        Some(attached) => Holder {
            client_id,
            name: attached.name.clone(),
            kind: attached.kind,
        },
        None => Holder {
            client_id,
            name: None,
            kind: ClientKind::Other,
        },
    }
}

fn summary(device: &Device) -> DeviceSummary {
    DeviceSummary {
        path: device.path.clone(),
        description: None,
        open: true,
        device_id: Some(device.id),
        baud: Some(device.baud),
        client_count: device.clients.len(),
        control_holder: device
            .control
            .as_ref()
            .map(|control| holder_of(device, control.client_id)),
    }
}

fn take_device_locked(
    state: &mut State,
    device_id: u64,
    reason: &str,
    events: &mut Vec<Event>,
    session: Option<&SharingSession>,
) -> Option<Device> {
    let mut device = state.devices.remove(&device_id)?;
    // A closed device can no longer prove its identity (a replacement can
    // reuse its path), so its authorization ends here and must be rechecked.
    if let Some(session) = session {
        session.revoke_path(&device.path);
    }
    device.stop.store(true, Ordering::SeqCst);
    // End the writer thread. The caller joins both I/O threads after the
    // engine lock is released, so a closed device's serial file is released.
    device.write_tx = None;
    let control_holder = device.control.take().map(|control| Holder {
        client_id: control.client_id,
        name: device
            .clients
            .get(&control.client_id)
            .and_then(|attached| attached.name.clone()),
        kind: device
            .clients
            .get(&control.client_id)
            .map(|attached| attached.kind)
            .unwrap_or(ClientKind::Other),
    });
    // Dropping the write sender ends the writer thread.
    for waiter in std::mem::take(&mut device.waiters) {
        let _ = waiter.tx.send(WaitOutcome {
            lines: waiter.lines,
            timed_out: false,
            failed: Some(reason.to_string()),
        });
    }
    if let Some(holder) = control_holder {
        events.push(Event::ControlReleased {
            device_id,
            holder,
            reason: ControlReleaseReason::DeviceClosed,
        });
    }
    events.push(Event::DeviceClosed {
        device_id,
        path: device.path.clone(),
        reason: reason.to_string(),
    });
    Some(device)
}

/// Join I/O threads outside the engine lock.
fn join_devices(devices: Vec<Device>) {
    for mut device in devices {
        device.write_tx = None;
        device.stop.store(true, Ordering::SeqCst);
        if let Some(io) = device.io.take() {
            io.join();
        }
    }
}

fn record_written_lines(device: &mut Device, source: Source, data: &[u8]) {
    let mut start = 0usize;
    for index in 0..data.len() {
        if data[index] != b'\n' {
            continue;
        }
        let mut line = data[start..index].to_vec();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        device.history.push(source, line, false);
        start = index + 1;
    }
    let rest = &data[start..];
    if !rest.is_empty() {
        let mut line = rest.to_vec();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if !line.is_empty() {
            device.history.push(source, line, true);
        }
    }
}
