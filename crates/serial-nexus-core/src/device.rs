use std::collections::HashMap;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use serial_nexus_protocol::Source;
use serialport::{DataBits, FlowControl, Parity, StopBits};

/// Events a device's I/O threads send to the engine.
#[derive(Debug)]
pub enum DeviceEvent {
    /// Bytes arrived from the device.
    Data { device_id: u64, bytes: Vec<u8> },
    /// A write completed (used only for diagnostics; history is recorded when
    /// the write is accepted, so ordering is deterministic).
    Written { device_id: u64, bytes: usize },
    /// The device failed or was unplugged. The engine closes it.
    Failed { device_id: u64, reason: String },
}

/// One concrete device node, captured as the inode the kernel opened.
///
/// A path alone is not an identity: a device can be unplugged and another one
/// can reappear at the same path. Sharing stores this fingerprint when the
/// owner checks a device and compares it again at authorization and open time.
#[derive(Clone)]
pub struct DeviceIdentity {
    /// Canonical path used for every later lookup.
    pub canonical: String,
    dev: u64,
    ino: u64,
    rdev: u64,
    /// Link count when captured. A node that has been unlinked (`nlink == 0`)
    /// is no longer the device at its path, even if the numbers are recycled.
    links: u64,
    /// An `O_PATH` descriptor opened when the identity was captured. Holding
    /// it pins the device node, so the kernel cannot hand the same inode to a
    /// replacement at the same path while this authorization exists.
    holder: Arc<OwnedFd>,
}

impl std::fmt::Debug for DeviceIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeviceIdentity")
            .field("canonical", &self.canonical)
            .field("dev", &self.dev)
            .field("ino", &self.ino)
            .field("rdev", &self.rdev)
            .field("links", &self.links)
            .finish_non_exhaustive()
    }
}

// The descriptor is not part of the identity value: two captures of the same
// live node compare equal.
impl PartialEq for DeviceIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical && self.same_node(other)
    }
}

impl Eq for DeviceIdentity {}

impl DeviceIdentity {
    /// Capture the current device node behind `path` (following symlinks).
    ///
    /// An `O_PATH` open succeeds without read/write permission and without
    /// initializing the device; its only purpose is pinning the node.
    pub fn capture(path: &str) -> io::Result<Self> {
        let canonical = std::fs::canonicalize(path)?;
        let raw = nix::fcntl::open(
            &canonical,
            nix::fcntl::OFlag::O_PATH | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        // SAFETY: `open` just returned this descriptor and nothing else owns it.
        let holder = unsafe { OwnedFd::from_raw_fd(raw) };
        let stat = nix::sys::stat::fstat(holder.as_raw_fd())
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        Ok(Self::from_stat(
            canonical.to_string_lossy().into_owned(),
            &stat,
            holder,
        ))
    }

    fn from_stat(canonical: String, stat: &nix::sys::stat::FileStat, holder: OwnedFd) -> Self {
        Self {
            canonical,
            dev: stat.st_dev,
            ino: stat.st_ino,
            rdev: stat.st_rdev,
            links: stat.st_nlink,
            holder: Arc::new(holder),
        }
    }

    fn same_node(&self, other: &Self) -> bool {
        self.dev == other.dev
            && self.ino == other.ino
            && self.rdev == other.rdev
            && self.links == other.links
    }

    /// True when the node behind the canonical path is still the same one.
    pub fn still_valid(&self) -> bool {
        match nix::sys::stat::fstat(self.holder.as_raw_fd()) {
            Ok(stat)
                if self.dev == stat.st_dev
                    && self.ino == stat.st_ino
                    && self.rdev == stat.st_rdev
                    && stat.st_nlink == self.links
                    && stat.st_nlink > 0 => {}
            _ => return false,
        }
        match Self::capture(&self.canonical) {
            Ok(current) => self.same_node(&current),
            Err(_) => false,
        }
    }

    /// An opaque label for the wire; never contains a path from outside the
    /// session's own allowlist.
    pub fn describe(&self) -> String {
        format!("dev={}:{} ino={}", self.dev, self.rdev, self.ino)
    }

    /// Compare the identity of an already-open descriptor with this one.
    fn same_open_file(&self, fd: std::os::unix::io::RawFd) -> io::Result<bool> {
        let stat = nix::sys::stat::fstat(fd)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        Ok(self.dev == stat.st_dev && self.ino == stat.st_ino && self.rdev == stat.st_rdev)
    }
}

/// One accepted write, tagged so a revoked remote client's queued work can be
/// dropped by the writer before it reaches the device.
#[derive(Debug)]
pub struct WriteCommand {
    pub device_id: u64,
    pub client_id: u64,
    /// The client's write generation for this device. A revocation bumps it,
    /// which invalidates every command queued with the old value.
    pub epoch: u64,
    pub bytes: Vec<u8>,
}

/// Per-device write generations, keyed by client id.
pub type WriteEpochs = Arc<Mutex<HashMap<u64, u64>>>;

/// Open a serial port with the first version's fixed framing: 8N1, no flow
/// control, configurable baud, short read timeout so threads can stop.
pub fn open_serial(path: &str, baud: u32) -> io::Result<Box<dyn serialport::SerialPort>> {
    open_serial_checked(path, baud, None)
}

/// Open a serial port and, when `expected` is given, refuse the open unless the
/// descriptor is that exact device node. The identity is checked on the open
/// file descriptor, so swapping the path between the check and the open cannot
/// hand a different device the authorization.
pub fn open_serial_checked(
    path: &str,
    baud: u32,
    expected: Option<&DeviceIdentity>,
) -> io::Result<Box<dyn serialport::SerialPort>> {
    let builder = serialport::new(path, baud)
        .data_bits(DataBits::Eight)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .flow_control(FlowControl::None)
        .timeout(Duration::from_millis(50));
    let port = serialport::TTYPort::open(&builder)?;
    if let Some(expected) = expected {
        let same = expected.same_open_file(port.as_raw_fd())?;
        // Also require the canonical path to still name this node, so a
        // retargeted symlink cannot keep an authorization alive.
        let path_still_same = expected.still_valid();
        if !same || !path_still_same {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path} changed since it was checked; check it again"),
            ));
        }
    }
    Ok(Box::new(port))
}

/// Handles for a device's I/O threads, joined when the device closes so the
/// serial file is released before a reopen is accepted.
pub struct IoThreads {
    reader: thread::JoinHandle<()>,
    writer: thread::JoinHandle<()>,
}

impl IoThreads {
    /// Wait for both threads. The caller must drop the write sender first, so
    /// the writer thread can finish.
    pub fn join(self) {
        let _ = self.writer.join();
        let _ = self.reader.join();
    }
}

/// Run the reader and writer threads for one open device.
///
/// Reads are `O(50ms)` blocking so a stop flag is noticed promptly. Writes are
/// taken in channel order, so a complete write never interleaves with another
/// write from a different client.
#[allow(clippy::too_many_arguments)]
pub fn spawn_io(
    device_id: u64,
    path: &str,
    mut read_port: Box<dyn serialport::SerialPort>,
    mut write_port: Box<dyn serialport::SerialPort>,
    write_rx: Receiver<WriteCommand>,
    write_epochs: WriteEpochs,
    stop: Arc<AtomicBool>,
    events: Sender<DeviceEvent>,
) -> io::Result<IoThreads> {
    let name = path.to_string();
    let read_stop = Arc::clone(&stop);
    let read_events = events.clone();
    let reader = thread::Builder::new()
        .name(format!("serial-read-{device_id}"))
        .spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                if read_stop.load(Ordering::SeqCst) {
                    return;
                }
                match read_port.read(&mut buffer) {
                    Ok(0) => {
                        let _ = read_events.send(DeviceEvent::Failed {
                            device_id,
                            reason: "device closed".into(),
                        });
                        return;
                    }
                    Ok(read) => {
                        if read_events
                            .send(DeviceEvent::Data {
                                device_id,
                                bytes: buffer[..read].to_vec(),
                            })
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) if is_transient(&error) => continue,
                    Err(error) => {
                        let _ = read_events.send(DeviceEvent::Failed {
                            device_id,
                            reason: error.to_string(),
                        });
                        return;
                    }
                }
            }
        })
        .map_err(io::Error::other)?;

    let writer = thread::Builder::new()
        .name(format!("serial-write-{device_id}"))
        .spawn(move || {
            // Drain every accepted write before exiting: a graceful close must
            // not silently drop bytes that already have a successful answer.
            // (On a device failure the write below reports the error.)
            while let Ok(command) = write_rx.recv() {
                // A revoked remote client's queued work is dropped before it
                // touches the device; the write already in progress finishes.
                let current = write_epochs
                    .lock()
                    .expect("write epochs lock")
                    .get(&command.client_id)
                    .copied()
                    .unwrap_or(0);
                if current != command.epoch {
                    continue;
                }
                let bytes = command.bytes;
                let wrote = bytes.len();
                if let Err(error) = write_port.write_all(&bytes) {
                    let _ = events.send(DeviceEvent::Failed {
                        device_id,
                        reason: format!("write to {name} failed: {error}"),
                    });
                    return;
                }
                let _ = events.send(DeviceEvent::Written {
                    device_id,
                    bytes: wrote,
                });
            }
        })
        .map_err(io::Error::other)?;

    Ok(IoThreads { reader, writer })
}

fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

/// The source label a write should carry in history.
pub fn write_source(ai: bool) -> Source {
    if ai {
        Source::Ai
    } else {
        Source::Human
    }
}
