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

use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serial_nexus_client::{ClientInfo, ErrorCode, RemoteClient};
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
    written: Mutex<Vec<u8>>,
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
    fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
        std::thread::sleep(Duration::from_millis(10));
        Err(io::Error::from(io::ErrorKind::TimedOut))
    }
}

impl Write for FakePort {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
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
        written: Mutex::new(Vec::new()),
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
    assert_eq!(error.code(), Some(ErrorCode::DeviceFailed), "{error:?}");
    assert!(
        error.to_string().contains("may already have reached"),
        "the failure must report uncertainty: {error}"
    );

    // Exactly the prefix left the client once; nothing was replayed.
    let written = state.written.lock().expect("written lock").clone();
    assert_eq!(written, b"PART", "written bytes: {written:?}");
    assert_eq!(state.writes.load(Ordering::SeqCst), 1);

    // A non-waited write cannot claim success after the device failed.
    let late = remote.write_bytes(opened.device_id, b"late", None);
    assert!(late.is_err(), "a failed device must not accept more writes");
    install_serial_factory(None);
}
