//! Deterministic tests for the two test seams:
//!
//! - an injected port enumeration that lets a **detected device be shared and
//!   opened remotely without ever being opened on the host first**, and
//! - an injected serial I/O implementation that writes a known prefix and then
//!   fails, proving uncertainty reporting and no automatic replay.
//!
//! The seams only replace *discovery* and *I/O*; every authorization check
//! (real node capture, identity comparison, allowlist, role, open-time
//! revalidation) still runs exactly as in production. The hooks are
//! process-wide, so these tests hold a lock while installing them.

mod support;

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serial_nexus_client::{ClientInfo, RemoteClient};
use serial_nexus_core::device::install_serial_factory;
use serial_nexus_core::enumerate::{install_port_lister, PortInfo};
use serial_nexus_protocol::{ShareBind, WaitSpec};
use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};
use support::{connect, wait_until, SimulatedDevice};
use tempfile::TempDir;

fn seam_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct Core {
    _dir: TempDir,
    socket: PathBuf,
}

fn core() -> Core {
    let directory = TempDir::new().expect("tempdir");
    let socket = directory.path().join("core.sock");
    let config = serial_nexus_core::CoreConfig {
        socket: socket.clone(),
        grace: Duration::from_secs(30),
        control_idle: Duration::from_secs(300),
        ..serial_nexus_core::CoreConfig::default()
    };
    std::thread::spawn(move || {
        if let Err(error) = serial_nexus_core::server::run(config) {
            eprintln!("seam test core failed: {error}");
        }
    });
    wait_until(Duration::from_secs(5), || {
        std::os::unix::net::UnixStream::connect(&socket).is_ok()
    });
    Core {
        _dir: directory,
        socket,
    }
}

fn bind() -> ShareBind {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("probe");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    ShareBind::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

#[test]
fn an_enumerated_device_can_be_shared_and_opened_remotely_without_a_host_open() {
    let _guard = seam_lock().lock().expect("seam lock");
    let core = core();
    let mut device = SimulatedDevice::new();
    let path = device.path.clone();

    // Enumerate the real PTY as a detected device. No local client opens it.
    let lister: Arc<serial_nexus_core::enumerate::PortLister> = Arc::new(move || {
        vec![PortInfo {
            path: path.clone(),
            description: Some("injected enumerated board".into()),
        }]
    });
    install_port_lister(Some(lister));

    let owner = connect(&core.socket, ClientInfo::human("owner"));
    let before = owner.list_devices().expect("local list");
    let entry = before
        .devices
        .iter()
        .find(|entry| entry.path == device.path)
        .expect("the injected device is detected");
    assert!(!entry.open, "nothing may open it before sharing starts");

    let info = owner
        .start_sharing(bind(), vec![device.path.clone()])
        .expect("share a detected but unopened device");
    // Still not open on the host: checking never opens the device.
    let status = owner.list_devices().expect("local list");
    assert!(
        !status
            .devices
            .iter()
            .find(|entry| entry.path == device.path)
            .expect("detected")
            .open,
        "the host must not open the checked device"
    );

    let remote = RemoteClient::connect(
        &info.host,
        &info.human,
        Some("seam-remote".into()),
        Duration::from_secs(5),
    )
    .expect("pair");
    let listed = remote.list_devices().expect("remote list");
    assert_eq!(listed.devices.len(), 1);
    assert_eq!(listed.devices[0].path, device.path);

    let opened = remote
        .open(&device.path, Some(115_200), false)
        .expect("remote opens the allowed enumerated device");
    remote
        .write_bytes(opened.device_id, b"remote-first", None)
        .expect("remote write");
    device.read_until(b"remote-first", Duration::from_secs(2));

    // The real device identity still governs the allowlist.
    let denied = {
        let other = SimulatedDevice::new();
        remote.open(&other.path, None, false).is_err()
    };
    assert!(denied, "an unshared device must not open");

    owner
        .uncheck_shared_devices(vec![device.path.clone()])
        .expect("uncheck");
    assert!(remote.read(opened.device_id, Some(5)).is_err());
    install_port_lister(None);
}

struct FakeState {
    incoming: Mutex<VecDeque<u8>>,
    fail_reads: AtomicBool,
    block_writes: AtomicBool,
    written: Mutex<Vec<u8>>,
    /// Every buffer passed to `write`, including calls that then fail, so a
    /// replay attempt is visible even when it writes nothing.
    attempts: Mutex<Vec<Vec<u8>>>,
    writes: AtomicUsize,
    fail_after: usize,
    baud: u32,
}

struct FakePort {
    state: Arc<FakeState>,
}

impl FakePort {
    fn boxed(state: Arc<FakeState>) -> Box<dyn SerialPort> {
        Box::new(FakePort { state })
    }
}

impl Read for FakePort {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.state.fail_reads.load(Ordering::SeqCst) {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        let mut incoming = self.state.incoming.lock().expect("incoming lock");
        if incoming.is_empty() {
            drop(incoming);
            std::thread::sleep(Duration::from_millis(10));
            return Err(io::Error::from(io::ErrorKind::TimedOut));
        }
        let take = buffer.len().min(incoming.len());
        for slot in &mut buffer[..take] {
            *slot = incoming.pop_front().expect("queued byte");
        }
        Ok(take)
    }
}

impl Write for FakePort {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        while self.state.block_writes.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }
        self.state
            .attempts
            .lock()
            .expect("attempts lock")
            .push(buffer.to_vec());
        let mut written = self.state.written.lock().expect("written lock");
        let room = self.state.fail_after.saturating_sub(written.len());
        if room == 0 {
            return Err(io::Error::other("injected serial write failure"));
        }
        let take = room.min(buffer.len());
        written.extend_from_slice(&buffer[..take]);
        self.state.writes.fetch_add(1, Ordering::SeqCst);
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SerialPort for FakePort {
    fn name(&self) -> Option<String> {
        Some("injected-fake-serial".into())
    }
    fn baud_rate(&self) -> serialport::Result<u32> {
        Ok(self.state.baud)
    }
    fn data_bits(&self) -> serialport::Result<DataBits> {
        Ok(DataBits::Eight)
    }
    fn flow_control(&self) -> serialport::Result<FlowControl> {
        Ok(FlowControl::None)
    }
    fn parity(&self) -> serialport::Result<Parity> {
        Ok(Parity::None)
    }
    fn stop_bits(&self) -> serialport::Result<StopBits> {
        Ok(StopBits::One)
    }
    fn timeout(&self) -> Duration {
        Duration::from_millis(50)
    }
    fn set_baud_rate(&mut self, _baud: u32) -> serialport::Result<()> {
        Ok(())
    }
    fn set_data_bits(&mut self, _bits: DataBits) -> serialport::Result<()> {
        Ok(())
    }
    fn set_flow_control(&mut self, _flow: FlowControl) -> serialport::Result<()> {
        Ok(())
    }
    fn set_parity(&mut self, _parity: Parity) -> serialport::Result<()> {
        Ok(())
    }
    fn set_stop_bits(&mut self, _bits: StopBits) -> serialport::Result<()> {
        Ok(())
    }
    fn set_timeout(&mut self, _timeout: Duration) -> serialport::Result<()> {
        Ok(())
    }
    fn write_request_to_send(&mut self, _level: bool) -> serialport::Result<()> {
        Ok(())
    }
    fn write_data_terminal_ready(&mut self, _level: bool) -> serialport::Result<()> {
        Ok(())
    }
    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        Ok(false)
    }
    fn bytes_to_read(&self) -> serialport::Result<u32> {
        Ok(0)
    }
    fn bytes_to_write(&self) -> serialport::Result<u32> {
        Ok(0)
    }
    fn clear(&self, _buffer: ClearBuffer) -> serialport::Result<()> {
        Ok(())
    }
    fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
        Ok(FakePort::boxed(Arc::clone(&self.state)))
    }
    fn set_break(&self) -> serialport::Result<()> {
        Ok(())
    }
    fn clear_break(&self) -> serialport::Result<()> {
        Ok(())
    }
}

#[test]
fn a_partial_serial_write_reports_uncertainty_and_is_never_replayed() {
    let _guard = seam_lock().lock().expect("seam lock");
    let core = core();
    let device = SimulatedDevice::new();
    let owner = connect(&core.socket, ClientInfo::human("owner"));
    let state = Arc::new(FakeState {
        incoming: Mutex::new(VecDeque::new()),
        fail_reads: AtomicBool::new(false),
        block_writes: AtomicBool::new(false),
        written: Mutex::new(Vec::new()),
        attempts: Mutex::new(Vec::new()),
        writes: AtomicUsize::new(0),
        fail_after: 4,
        baud: 115_200,
    });
    let factory_state = Arc::clone(&state);
    let factory: Arc<serial_nexus_core::device::SerialFactory> = Arc::new(move |_path, baud| {
        let _ = baud;
        Ok(FakePort::boxed(Arc::clone(&factory_state)))
    });
    install_serial_factory(Some(factory));

    // Opening validates the real PTY identity, then uses the injected I/O.
    owner
        .open(&device.path, Some(115_200), false)
        .expect("open real device through the injected I/O");

    let info = owner
        .start_sharing(bind(), vec![device.path.clone()])
        .expect("share");
    let remote = RemoteClient::connect(
        &info.host,
        &info.human,
        Some("seam-remote".into()),
        Duration::from_secs(5),
    )
    .expect("pair");
    let opened = remote
        .open(&device.path, Some(115_200), false)
        .expect("remote open");

    let payload = b"PARTIAL-BYTES";
    let error = remote
        .write_bytes(
            opened.device_id,
            payload,
            Some(WaitSpec {
                lines: 1,
                timeout_ms: Some(3_000),
            }),
        )
        .expect_err("the injected partial write must fail");
    assert!(error.is_write_uncertain(), "{error:?}");
    assert!(
        error.to_string().contains("may already have reached"),
        "the failure must report uncertainty: {error}"
    );

    // Exactly the prefix left the client once; nothing was replayed. The
    // attempts list proves no second write attempt happened either.
    let written = state.written.lock().expect("written lock").clone();
    assert_eq!(written, b"PART", "written bytes: {written:?}");
    assert_eq!(state.writes.load(Ordering::SeqCst), 1);
    let attempts = state.attempts.lock().expect("attempts lock").clone();
    assert_eq!(
        attempts,
        vec![payload.to_vec(), payload[4..].to_vec()],
        "write_all's remainder call is the only other attempt"
    );

    // A non-waited write cannot claim success after the device failed.
    let late = remote.write_bytes(opened.device_id, b"late", None);
    assert!(late.is_err(), "a failed device must not accept more writes");
    let attempts_after = state.attempts.lock().expect("attempts lock").clone();
    assert_eq!(
        attempts_after, attempts,
        "a refused write must not reach the serial writer"
    );
    install_serial_factory(None);
}

#[test]
fn a_remote_write_cannot_be_admitted_after_stop_begins() {
    // A concurrent stop and remote write must not let the write execute after
    // stop returns. The engine holds its state lock while the serial factory
    // below sleeps, which widens the stop/enqueue window deterministically.
    let _guard = seam_lock().lock().expect("seam lock");

    struct SlowPort {
        writes: Arc<Mutex<Vec<u8>>>,
        baud: u32,
    }
    impl std::io::Read for SlowPort {
        fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(5));
            Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
        }
    }
    impl std::io::Write for SlowPort {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.writes
                .lock()
                .expect("writes lock")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl SerialPort for SlowPort {
        fn name(&self) -> Option<String> {
            Some("slow".into())
        }
        fn baud_rate(&self) -> serialport::Result<u32> {
            Ok(self.baud)
        }
        fn data_bits(&self) -> serialport::Result<DataBits> {
            Ok(DataBits::Eight)
        }
        fn flow_control(&self) -> serialport::Result<FlowControl> {
            Ok(FlowControl::None)
        }
        fn parity(&self) -> serialport::Result<Parity> {
            Ok(Parity::None)
        }
        fn stop_bits(&self) -> serialport::Result<StopBits> {
            Ok(StopBits::One)
        }
        fn timeout(&self) -> Duration {
            Duration::from_millis(50)
        }
        fn set_baud_rate(&mut self, _baud: u32) -> serialport::Result<()> {
            Ok(())
        }
        fn set_data_bits(&mut self, _bits: DataBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_flow_control(&mut self, _flow: FlowControl) -> serialport::Result<()> {
            Ok(())
        }
        fn set_parity(&mut self, _parity: Parity) -> serialport::Result<()> {
            Ok(())
        }
        fn set_stop_bits(&mut self, _bits: StopBits) -> serialport::Result<()> {
            Ok(())
        }
        fn set_timeout(&mut self, _timeout: Duration) -> serialport::Result<()> {
            Ok(())
        }
        fn write_request_to_send(&mut self, _level: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn write_data_terminal_ready(&mut self, _level: bool) -> serialport::Result<()> {
            Ok(())
        }
        fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
            Ok(false)
        }
        fn bytes_to_read(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn bytes_to_write(&self) -> serialport::Result<u32> {
            Ok(0)
        }
        fn clear(&self, _buffer: ClearBuffer) -> serialport::Result<()> {
            Ok(())
        }
        fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>> {
            Ok(Box::new(SlowPort {
                writes: Arc::clone(&self.writes),
                baud: self.baud,
            }))
        }
        fn set_break(&self) -> serialport::Result<()> {
            Ok(())
        }
        fn clear_break(&self) -> serialport::Result<()> {
            Ok(())
        }
    }

    let core = core();
    let owner = connect(&core.socket, ClientInfo::human("owner"));
    let shared = SimulatedDevice::new();
    let blocker = SimulatedDevice::new();
    let writes: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writes_for_factory = Arc::clone(&writes);
    let block_path = blocker.path.clone();
    let factory: Arc<serial_nexus_core::device::SerialFactory> = Arc::new(move |path, baud| {
        if path == block_path {
            // Open holds the engine state lock; sleeping here widens the
            // window between stop's epoch bump and its session teardown.
            std::thread::sleep(Duration::from_millis(400));
        }
        Ok(Box::new(SlowPort {
            writes: Arc::clone(&writes_for_factory),
            baud,
        }))
    });
    install_serial_factory(Some(factory));

    owner
        .open(&shared.path, Some(115_200), false)
        .expect("open shared");
    let info = owner
        .start_sharing(bind(), vec![shared.path.clone()])
        .expect("start");
    let human = RemoteClient::connect(
        &info.host,
        &info.human,
        Some("race-remote".into()),
        Duration::from_secs(5),
    )
    .expect("pair");
    let opened = human.open(&shared.path, None, false).expect("open");

    // Hold the engine state lock by opening the blocker device.
    let blocker_client = connect(&core.socket, ClientInfo::human("blocker"));
    let blocker_path = blocker.path.clone();
    let opening = std::thread::spawn(move || {
        blocker_client
            .open(&blocker_path, Some(115_200), false)
            .ok();
        blocker_client
    });
    std::thread::sleep(Duration::from_millis(60));

    let stop_thread = std::thread::spawn(move || owner.stop_sharing());
    std::thread::sleep(Duration::from_millis(60));
    let result = human.write_bytes(opened.device_id, b"AFTER-REVOKE", None);
    let stop_result = stop_thread.join().expect("stop thread");
    let _blocker_client = opening.join().expect("opening thread");
    assert!(
        stop_result.is_ok(),
        "stop itself must succeed: {stop_result:?}"
    );
    assert!(
        result.is_err(),
        "a write admitted after stop began must be refused"
    );
    assert!(
        !String::from_utf8_lossy(&writes.lock().expect("writes lock")).contains("AFTER-REVOKE"),
        "a revoked remote write reached the device"
    );
    install_serial_factory(None);
}

#[test]
fn discovery_and_events_revalidate_a_stale_shared_identity() {
    let _guard = seam_lock().lock().expect("seam lock");
    let core = core();
    let directory = TempDir::new().expect("tempdir");
    let path = directory.path().join("device-node");
    std::fs::write(&path, b"").expect("node");
    let path_text = path.to_string_lossy().into_owned();
    let lister_path = path_text.clone();
    let lister: Arc<serial_nexus_core::enumerate::PortLister> = Arc::new(move || {
        vec![PortInfo {
            path: lister_path.clone(),
            description: Some("injected file node".into()),
        }]
    });
    install_port_lister(Some(lister));
    let state = Arc::new(FakeState {
        incoming: Mutex::new(VecDeque::new()),
        fail_reads: AtomicBool::new(false),
        block_writes: AtomicBool::new(false),
        written: Mutex::new(Vec::new()),
        attempts: Mutex::new(Vec::new()),
        writes: AtomicUsize::new(0),
        fail_after: usize::MAX,
        baud: 115_200,
    });
    let factory_state = Arc::clone(&state);
    let factory: Arc<serial_nexus_core::device::SerialFactory> = Arc::new(move |_path, baud| {
        let _ = baud;
        Ok(FakePort::boxed(Arc::clone(&factory_state)))
    });
    install_serial_factory(Some(factory));

    let owner = connect(&core.socket, ClientInfo::human("owner"));
    let info = owner
        .start_sharing(bind(), vec![path_text.clone()])
        .expect("share injected node");
    let remote = RemoteClient::connect(
        &info.host,
        &info.human,
        Some("stale-identity".into()),
        Duration::from_secs(5),
    )
    .expect("pair");
    assert!(remote
        .list_devices()
        .expect("list")
        .devices
        .iter()
        .any(|entry| entry.path == path_text));
    let opened = remote
        .open(&path_text, Some(115_200), false)
        .expect("open injected node");
    let events = remote.subscribe();
    let waiting = remote.clone();
    let wait_id = opened.device_id;
    let waiting = std::thread::spawn(move || {
        waiting.write_bytes(
            wait_id,
            b"WAIT-FOR-EXPIRY\n",
            Some(serial_nexus_protocol::WaitSpec {
                lines: 1,
                timeout_ms: Some(3_000),
            }),
        )
    });
    std::thread::sleep(Duration::from_millis(100));

    // Replace the pinned node. Discovery and event delivery must notice.
    std::fs::remove_file(&path).expect("unlink");
    std::fs::write(&path, b"replacement").expect("replacement");
    assert!(
        !remote
            .list_devices()
            .expect("list after replacement")
            .devices
            .iter()
            .any(|entry| entry.path == path_text),
        "discovery must expire a stale identity"
    );
    let started = Instant::now();
    let wait_result = waiting.join().expect("wait thread");
    assert!(
        wait_result.is_err(),
        "expiry must cancel the pending waiter"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "the waiter was not cancelled by expiry: {wait_result:?}"
    );
    state
        .incoming
        .lock()
        .expect("incoming lock")
        .extend(b"POST-REPLACEMENT\n");
    std::thread::sleep(Duration::from_millis(250));
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(event, serial_nexus_protocol::Event::DeviceOutput { .. }),
            "event delivery must not expose data after identity expiry: {event:?}"
        );
    }
    assert!(remote.write_bytes(opened.device_id, b"late", None).is_err());
    install_serial_factory(None);
    install_port_lister(None);
}

#[test]
fn a_failed_devices_stalled_writer_does_not_block_healthy_output() {
    use std::collections::HashMap;

    let _guard = seam_lock().lock().expect("seam lock");
    let core = core();
    let first = SimulatedDevice::new();
    let second = SimulatedDevice::new();
    let paths = vec![first.path.clone(), second.path.clone()];
    let lister_paths = paths.clone();
    let lister: Arc<serial_nexus_core::enumerate::PortLister> = Arc::new(move || {
        lister_paths
            .iter()
            .map(|path| PortInfo {
                path: path.clone(),
                description: Some("injected isolation node".into()),
            })
            .collect()
    });
    install_port_lister(Some(lister));

    let make_state = || {
        Arc::new(FakeState {
            incoming: Mutex::new(VecDeque::new()),
            fail_reads: AtomicBool::new(false),
            block_writes: AtomicBool::new(false),
            written: Mutex::new(Vec::new()),
            attempts: Mutex::new(Vec::new()),
            writes: AtomicUsize::new(0),
            fail_after: usize::MAX,
            baud: 115_200,
        })
    };
    let first_state = make_state();
    let second_state = make_state();
    let mut states: HashMap<String, Arc<FakeState>> = HashMap::new();
    states.insert(first.path.clone(), Arc::clone(&first_state));
    states.insert(second.path.clone(), Arc::clone(&second_state));
    let factory: Arc<serial_nexus_core::device::SerialFactory> = Arc::new(move |path, baud| {
        let _ = baud;
        let state = states.get(path).expect("known test device");
        Ok(FakePort::boxed(Arc::clone(state)))
    });
    install_serial_factory(Some(factory));

    let owner = connect(&core.socket, ClientInfo::human("owner"));
    let info = owner
        .start_sharing(bind(), paths.clone())
        .expect("share two devices");
    let remote = RemoteClient::connect(
        &info.host,
        &info.human,
        Some("isolation".into()),
        Duration::from_secs(5),
    )
    .expect("pair");
    let first_open = remote
        .open(&first.path, Some(115_200), false)
        .expect("first");
    let second_open = remote
        .open(&second.path, Some(115_200), false)
        .expect("second");

    second_state
        .incoming
        .lock()
        .expect("incoming lock")
        .extend(b"BEFORE\n");
    wait_until(Duration::from_secs(2), || {
        remote
            .read(second_open.device_id, Some(10))
            .map(|read| read.lines.iter().any(|line| line.text.contains("BEFORE")))
            .unwrap_or(false)
    });

    // Stall the first writer, then fail its reader. The dispatcher must not
    // wait for that writer before processing the healthy device.
    first_state.block_writes.store(true, Ordering::SeqCst);
    remote
        .write_bytes(first_open.device_id, b"stalled", None)
        .expect("queue stalled write");
    first_state.fail_reads.store(true, Ordering::SeqCst);
    // Wait until the failed device has been removed; the dispatcher may still
    // be joining its stalled writer.
    wait_until(Duration::from_secs(2), || {
        owner
            .list_devices()
            .map(|devices| {
                devices
                    .devices
                    .iter()
                    .all(|device| !(device.path == first.path && device.open))
            })
            .unwrap_or(false)
    });
    second_state
        .incoming
        .lock()
        .expect("incoming lock")
        .extend(b"AFTER\n");
    wait_until(Duration::from_secs(3), || {
        remote
            .read(second_open.device_id, Some(10))
            .map(|read| read.lines.iter().any(|line| line.text.contains("AFTER")))
            .unwrap_or(false)
    });
    first_state.block_writes.store(false, Ordering::SeqCst);
    install_serial_factory(None);
    install_port_lister(None);
}
