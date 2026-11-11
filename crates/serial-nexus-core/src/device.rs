use std::fs::File;
use std::io;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use serial_nexus_protocol::Source;
use serialport::{DataBits, FlowControl, Parity, StopBits};

/// A serial port opened and validated by this crate.
///
/// `serialport` is kept for the `SerialPort` interface, but opening and
/// configuration are done here so the descriptor's identity is known before
/// termios is touched.
struct ExclusiveToken {
    // Owns a duplicate descriptor, so cleanup never targets a closed or
    // reused number after the original handle goes away.
    file: File,
}

impl Drop for ExclusiveToken {
    fn drop(&mut self) {
        // Only the last serial handle (original or clone) drops this token.
        // SAFETY: TIOCNXCL takes no argument for this valid descriptor.
        let _ = unsafe {
            libc::ioctl(self.file.as_raw_fd(), 0x540D /* TIOCNXCL */)
        };
    }
}

struct PosixSerial {
    // Declared before `file` so the last handle clears TIOCEXCL while the
    // descriptor is still open.
    exclusive: Arc<ExclusiveToken>,
    file: Arc<File>,
    baud: u32,
    timeout: Duration,
}

impl PosixSerial {
    fn wait(&self, events: nix::poll::PollFlags, timeout: Duration) -> io::Result<()> {
        use nix::poll::{poll, PollFd, PollTimeout};
        // SAFETY: the descriptor is owned by `self` for this call.
        let fd = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(self.file.as_raw_fd()) };
        let mut fds = [PollFd::new(fd, events)];
        let millis = timeout.as_millis().min(u16::MAX as u128) as u16;
        match poll(&mut fds, PollTimeout::from(millis)) {
            Ok(0) => Err(io::Error::from(io::ErrorKind::TimedOut)),
            Ok(_) => Ok(()),
            Err(nix::errno::Errno::EINTR) => Ok(()),
            Err(error) => Err(io::Error::from_raw_os_error(error as i32)),
        }
    }
}

fn errno_to_serial(error: nix::errno::Errno) -> serialport::Error {
    serialport::Error::new(
        serialport::ErrorKind::Io(io::Error::from_raw_os_error(error as i32).kind()),
        error.to_string(),
    )
}

fn to_serial_error(error: io::Error) -> serialport::Error {
    serialport::Error::new(serialport::ErrorKind::Io(error.kind()), error.to_string())
}

fn set_modem_bit(
    fd: std::os::unix::io::RawFd,
    bit: libc::c_int,
    level: bool,
) -> serialport::Result<()> {
    let request = if level {
        libc::TIOCMBIS
    } else {
        libc::TIOCMBIC
    };
    // SAFETY: TIOCMBIS/TIOCMBIC read one int for this valid descriptor.
    if unsafe { libc::ioctl(fd, request, &bit) } != 0 {
        return Err(to_serial_error(io::Error::last_os_error()));
    }
    Ok(())
}

fn read_modem_bit(fd: std::os::unix::io::RawFd, bit: libc::c_int) -> serialport::Result<bool> {
    let mut bits: libc::c_int = 0;
    // SAFETY: TIOCMGET writes one int for this valid descriptor.
    if unsafe { libc::ioctl(fd, libc::TIOCMGET, &mut bits) } != 0 {
        return Err(to_serial_error(io::Error::last_os_error()));
    }
    Ok(bits & bit != 0)
}

impl PosixSerial {
    fn control_flags(&self) -> serialport::Result<nix::sys::termios::ControlFlags> {
        let settings = nix::sys::termios::tcgetattr(&self.file).map_err(errno_to_serial)?;
        Ok(settings.control_flags)
    }
}

impl io::Read for PosixSerial {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.wait(nix::poll::PollFlags::POLLIN, self.timeout)?;
        let mut file = &*self.file;
        match file.read(buffer) {
            Ok(read) => Ok(read),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Err(io::Error::from(io::ErrorKind::TimedOut))
            }
            Err(error) => Err(error),
        }
    }
}

impl io::Write for PosixSerial {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
        // The writer thread owns this call: wait for room instead of failing a
        // slow device, matching the previous blocking serial behaviour.
        loop {
            // SAFETY: the descriptor is owned by `self` for this call.
            let fd = unsafe { std::os::unix::io::BorrowedFd::borrow_raw(self.file.as_raw_fd()) };
            let mut fds = [PollFd::new(fd, PollFlags::POLLOUT)];
            match poll(&mut fds, PollTimeout::from(50u16)) {
                Ok(0) => continue,
                Ok(_) => {
                    let flags = fds[0].revents().unwrap_or(PollFlags::empty());
                    if flags
                        .intersects(PollFlags::POLLERR | PollFlags::POLLHUP | PollFlags::POLLNVAL)
                        && !flags.contains(PollFlags::POLLOUT)
                    {
                        return Err(io::Error::from(io::ErrorKind::BrokenPipe));
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(error) => return Err(io::Error::from_raw_os_error(error as i32)),
            }
            let mut file = &*self.file;
            match file.write(buffer) {
                Ok(written) => return Ok(written),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl serialport::SerialPort for PosixSerial {
    fn name(&self) -> Option<String> {
        None
    }
    fn baud_rate(&self) -> serialport::Result<u32> {
        Ok(self.baud)
    }
    fn data_bits(&self) -> serialport::Result<DataBits> {
        use nix::sys::termios::ControlFlags;
        let size = self.control_flags()? & ControlFlags::CSIZE;
        if size == ControlFlags::CS6 {
            Ok(DataBits::Six)
        } else if size == ControlFlags::CS7 {
            Ok(DataBits::Seven)
        } else if size == ControlFlags::CS8 {
            Ok(DataBits::Eight)
        } else {
            Ok(DataBits::Five)
        }
    }
    fn flow_control(&self) -> serialport::Result<FlowControl> {
        use nix::sys::termios::{ControlFlags, InputFlags};
        let settings = nix::sys::termios::tcgetattr(&self.file).map_err(errno_to_serial)?;
        if settings.control_flags.contains(ControlFlags::CRTSCTS) {
            Ok(FlowControl::Hardware)
        } else if settings
            .input_flags
            .intersects(InputFlags::IXON | InputFlags::IXOFF)
        {
            Ok(FlowControl::Software)
        } else {
            Ok(FlowControl::None)
        }
    }
    fn parity(&self) -> serialport::Result<Parity> {
        use nix::sys::termios::ControlFlags;
        let flags = self.control_flags()?;
        if !flags.contains(ControlFlags::PARENB) {
            Ok(Parity::None)
        } else if flags.contains(ControlFlags::PARODD) {
            Ok(Parity::Odd)
        } else {
            Ok(Parity::Even)
        }
    }
    fn stop_bits(&self) -> serialport::Result<StopBits> {
        use nix::sys::termios::ControlFlags;
        if self.control_flags()?.contains(ControlFlags::CSTOPB) {
            Ok(StopBits::Two)
        } else {
            Ok(StopBits::One)
        }
    }
    fn timeout(&self) -> Duration {
        self.timeout
    }
    fn set_baud_rate(&mut self, baud: u32) -> serialport::Result<()> {
        set_speed(&self.file, baud).map_err(|error| {
            serialport::Error::new(
                serialport::ErrorKind::Io(io::ErrorKind::Other),
                error.to_string(),
            )
        })?;
        self.baud = baud;
        Ok(())
    }
    fn set_data_bits(&mut self, bits: DataBits) -> serialport::Result<()> {
        use nix::sys::termios::{tcgetattr, tcsetattr, ControlFlags, SetArg};
        let mut settings = tcgetattr(&self.file).map_err(errno_to_serial)?;
        settings.control_flags.remove(ControlFlags::CSIZE);
        settings.control_flags.insert(match bits {
            DataBits::Five => ControlFlags::CS5,
            DataBits::Six => ControlFlags::CS6,
            DataBits::Seven => ControlFlags::CS7,
            DataBits::Eight => ControlFlags::CS8,
        });
        tcsetattr(&self.file, SetArg::TCSANOW, &settings).map_err(errno_to_serial)
    }
    fn set_flow_control(&mut self, flow: FlowControl) -> serialport::Result<()> {
        use nix::sys::termios::{tcgetattr, tcsetattr, ControlFlags, InputFlags, SetArg};
        let mut settings = tcgetattr(&self.file).map_err(errno_to_serial)?;
        settings.control_flags.remove(ControlFlags::CRTSCTS);
        settings
            .input_flags
            .remove(InputFlags::IXON | InputFlags::IXOFF);
        match flow {
            FlowControl::None => {}
            FlowControl::Hardware => {
                settings.control_flags.insert(ControlFlags::CRTSCTS);
            }
            FlowControl::Software => {
                settings
                    .input_flags
                    .insert(InputFlags::IXON | InputFlags::IXOFF);
            }
        }
        tcsetattr(&self.file, SetArg::TCSANOW, &settings).map_err(errno_to_serial)
    }
    fn set_parity(&mut self, parity: Parity) -> serialport::Result<()> {
        use nix::sys::termios::{tcgetattr, tcsetattr, ControlFlags, SetArg};
        let mut settings = tcgetattr(&self.file).map_err(errno_to_serial)?;
        settings
            .control_flags
            .remove(ControlFlags::PARENB | ControlFlags::PARODD);
        match parity {
            Parity::None => {}
            Parity::Odd => {
                settings
                    .control_flags
                    .insert(ControlFlags::PARENB | ControlFlags::PARODD);
            }
            Parity::Even => {
                settings.control_flags.insert(ControlFlags::PARENB);
            }
        }
        tcsetattr(&self.file, SetArg::TCSANOW, &settings).map_err(errno_to_serial)
    }
    fn set_stop_bits(&mut self, bits: StopBits) -> serialport::Result<()> {
        use nix::sys::termios::{tcgetattr, tcsetattr, ControlFlags, SetArg};
        let mut settings = tcgetattr(&self.file).map_err(errno_to_serial)?;
        settings.control_flags.remove(ControlFlags::CSTOPB);
        if bits == StopBits::Two {
            settings.control_flags.insert(ControlFlags::CSTOPB);
        }
        tcsetattr(&self.file, SetArg::TCSANOW, &settings).map_err(errno_to_serial)
    }
    fn set_timeout(&mut self, timeout: Duration) -> serialport::Result<()> {
        self.timeout = timeout;
        Ok(())
    }
    fn write_request_to_send(&mut self, level: bool) -> serialport::Result<()> {
        set_modem_bit(self.file.as_raw_fd(), libc::TIOCM_RTS, level)
    }
    fn write_data_terminal_ready(&mut self, level: bool) -> serialport::Result<()> {
        set_modem_bit(self.file.as_raw_fd(), libc::TIOCM_DTR, level)
    }
    fn read_clear_to_send(&mut self) -> serialport::Result<bool> {
        read_modem_bit(self.file.as_raw_fd(), libc::TIOCM_CTS)
    }
    fn read_data_set_ready(&mut self) -> serialport::Result<bool> {
        read_modem_bit(self.file.as_raw_fd(), libc::TIOCM_DSR)
    }
    fn read_ring_indicator(&mut self) -> serialport::Result<bool> {
        read_modem_bit(self.file.as_raw_fd(), libc::TIOCM_RI)
    }
    fn read_carrier_detect(&mut self) -> serialport::Result<bool> {
        read_modem_bit(self.file.as_raw_fd(), libc::TIOCM_CD)
    }
    fn bytes_to_read(&self) -> serialport::Result<u32> {
        let mut available: libc::c_int = 0;
        // SAFETY: FIONREAD writes one int for this valid descriptor.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::FIONREAD, &mut available) } != 0 {
            return Err(to_serial_error(io::Error::last_os_error()));
        }
        Ok(available.max(0) as u32)
    }
    fn bytes_to_write(&self) -> serialport::Result<u32> {
        let mut queued: libc::c_int = 0;
        // SAFETY: TIOCOUTQ writes one int for this valid descriptor.
        if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCOUTQ, &mut queued) } != 0 {
            return Err(to_serial_error(io::Error::last_os_error()));
        }
        Ok(queued.max(0) as u32)
    }
    fn clear(&self, buffer: serialport::ClearBuffer) -> serialport::Result<()> {
        use serialport::ClearBuffer;
        let what = match buffer {
            ClearBuffer::Input => libc::TCIFLUSH,
            ClearBuffer::Output => libc::TCOFLUSH,
            ClearBuffer::All => libc::TCIOFLUSH,
        };
        // SAFETY: tcflush on this valid descriptor.
        if unsafe { libc::tcflush(self.file.as_raw_fd(), what) } != 0 {
            return Err(to_serial_error(io::Error::last_os_error()));
        }
        Ok(())
    }
    fn try_clone(&self) -> serialport::Result<Box<dyn serialport::SerialPort>> {
        let fd = nix::unistd::dup(self.file.as_raw_fd()).map_err(|error| {
            serialport::Error::new(
                serialport::ErrorKind::Io(io::ErrorKind::Other),
                error.to_string(),
            )
        })?;
        // SAFETY: `dup` returned a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        Ok(Box::new(PosixSerial {
            file: Arc::new(file),
            exclusive: Arc::clone(&self.exclusive),
            baud: self.baud,
            timeout: self.timeout,
        }))
    }
    fn set_break(&self) -> serialport::Result<()> {
        // SAFETY: TIOCSBRK takes no argument for this valid descriptor.
        if unsafe {
            libc::ioctl(self.file.as_raw_fd(), 0x5427 /* TIOCSBRK */)
        } != 0
        {
            return Err(to_serial_error(io::Error::last_os_error()));
        }
        Ok(())
    }
    fn clear_break(&self) -> serialport::Result<()> {
        // SAFETY: TIOCCBRK takes no argument for this valid descriptor.
        if unsafe {
            libc::ioctl(self.file.as_raw_fd(), 0x5428 /* TIOCCBRK */)
        } != 0
        {
            return Err(to_serial_error(io::Error::last_os_error()));
        }
        Ok(())
    }
}

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

/// One accepted write, tagged so revoked remote work can be dropped by the
/// writer before it reaches the device.
#[derive(Debug)]
pub struct WriteCommand {
    pub device_id: u64,
    /// True for a remote client. Local work is never revoked.
    pub remote: bool,
    /// The device's remote-write generation when the write was accepted. Any
    /// revocation bumps it, invalidating every remote command already queued
    /// (including commands from clients that have since detached).
    pub epoch: u64,
    pub bytes: Vec<u8>,
}

/// The remote-write generation for one device.
pub type WriteEpoch = Arc<AtomicU64>;

/// An opened serial port plus the device-node identity it was opened as.
pub struct OpenedSerial {
    pub port: Box<dyn serialport::SerialPort>,
    pub identity: DeviceIdentity,
}

/// The kernel baud rates this core supports.
fn baud_rate(baud: u32) -> Option<nix::sys::termios::BaudRate> {
    use nix::sys::termios::BaudRate;
    Some(match baud {
        50 => BaudRate::B50,
        75 => BaudRate::B75,
        110 => BaudRate::B110,
        134 => BaudRate::B134,
        150 => BaudRate::B150,
        200 => BaudRate::B200,
        300 => BaudRate::B300,
        600 => BaudRate::B600,
        1_200 => BaudRate::B1200,
        1_800 => BaudRate::B1800,
        2_400 => BaudRate::B2400,
        4_800 => BaudRate::B4800,
        9_600 => BaudRate::B9600,
        19_200 => BaudRate::B19200,
        38_400 => BaudRate::B38400,
        57_600 => BaudRate::B57600,
        115_200 => BaudRate::B115200,
        230_400 => BaudRate::B230400,
        460_800 => BaudRate::B460800,
        500_000 => BaudRate::B500000,
        576_000 => BaudRate::B576000,
        921_600 => BaudRate::B921600,
        1_000_000 => BaudRate::B1000000,
        1_152_000 => BaudRate::B1152000,
        1_500_000 => BaudRate::B1500000,
        2_000_000 => BaudRate::B2000000,
        2_500_000 => BaudRate::B2500000,
        3_000_000 => BaudRate::B3000000,
        3_500_000 => BaudRate::B3500000,
        4_000_000 => BaudRate::B4000000,
        _ => return None,
    })
}

/// Apply the fixed 8N1 framing to an already-open, already-validated fd.
fn configure_termios(file: &File, baud: u32) -> io::Result<()> {
    use nix::sys::termios::{
        cfmakeraw, cfsetispeed, cfsetospeed, tcgetattr, tcsetattr, ControlFlags, InputFlags, SetArg,
    };
    let mut settings =
        tcgetattr(file).map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    cfmakeraw(&mut settings);
    // cfmakeraw does not clear all framing/flow bits inherited from another
    // application, so normalize them explicitly to 8N1 with no flow control.
    settings.control_flags.remove(
        ControlFlags::CSTOPB | ControlFlags::PARENB | ControlFlags::PARODD | ControlFlags::CRTSCTS,
    );
    settings
        .input_flags
        .remove(InputFlags::IXON | InputFlags::IXOFF | InputFlags::IXANY);
    settings
        .control_flags
        .insert(ControlFlags::CLOCAL | ControlFlags::CREAD | ControlFlags::CS8);
    if let Some(rate) = baud_rate(baud) {
        cfsetispeed(&mut settings, rate)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        cfsetospeed(&mut settings, rate)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    } else {
        // tcsetattr cannot express an arbitrary speed; apply the flags with a
        // table rate, then set the real speed through termios2 below.
        cfsetispeed(&mut settings, nix::sys::termios::BaudRate::B9600)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        cfsetospeed(&mut settings, nix::sys::termios::BaudRate::B9600)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    }
    tcsetattr(file, SetArg::TCSANOW, &settings)
        .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    if baud_rate(baud).is_none() {
        set_arbitrary_baud(file, baud)?;
    }
    Ok(())
}

/// Linux `termios2` supports arbitrary baud rates through `BOTHER`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Termios2 {
    c_iflag: u32,
    c_oflag: u32,
    c_cflag: u32,
    c_lflag: u32,
    c_line: u8,
    c_cc: [u8; 19],
    c_ispeed: u32,
    c_ospeed: u32,
}

fn set_speed(file: &File, baud: u32) -> io::Result<()> {
    use nix::sys::termios::{cfsetispeed, cfsetospeed, tcgetattr, tcsetattr, SetArg};
    if let Some(rate) = baud_rate(baud) {
        let mut settings =
            tcgetattr(file).map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        cfsetispeed(&mut settings, rate)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        cfsetospeed(&mut settings, rate)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        tcsetattr(file, SetArg::TCSANOW, &settings)
            .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
        return Ok(());
    }
    set_arbitrary_baud(file, baud)
}

fn set_arbitrary_baud(file: &File, baud: u32) -> io::Result<()> {
    const TCGETS2: libc::c_ulong = 0x802C_542A;
    const TCSETS2: libc::c_ulong = 0x402C_542B;
    const BOTHER: u32 = 0o010000;
    const CBAUD: u32 = 0o010017;
    let fd = file.as_raw_fd();
    let mut termios = Termios2 {
        c_iflag: 0,
        c_oflag: 0,
        c_cflag: 0,
        c_lflag: 0,
        c_line: 0,
        c_cc: [0; 19],
        c_ispeed: 0,
        c_ospeed: 0,
    };
    // SAFETY: TCGETS2/TCSETS2 read and write a `termios2` for this descriptor.
    if unsafe { libc::ioctl(fd, TCGETS2, &mut termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    termios.c_cflag = (termios.c_cflag & !CBAUD) | BOTHER;
    termios.c_ispeed = baud;
    termios.c_ospeed = baud;
    // SAFETY: same struct, valid for this descriptor.
    if unsafe { libc::ioctl(fd, TCSETS2, &termios) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Open a serial port with the first version's fixed framing: 8N1, no flow
/// control, configurable baud, short read timeout so threads can stop.
pub fn open_serial(path: &str, baud: u32) -> io::Result<OpenedSerial> {
    open_serial_checked(path, baud, None)
}

/// Test-only serial I/O seam. The real device node is still opened and its
/// identity validated first; only the I/O implementation is replaced.
#[doc(hidden)]
pub type SerialFactory =
    dyn Fn(&str, u32) -> io::Result<Box<dyn serialport::SerialPort>> + Send + Sync;

static SERIAL_FACTORY: Mutex<Option<Arc<SerialFactory>>> = Mutex::new(None);

/// Install or clear the process-wide test serial factory.
#[doc(hidden)]
pub fn install_serial_factory(factory: Option<Arc<SerialFactory>>) {
    *SERIAL_FACTORY.lock().expect("serial factory lock") = factory;
}

/// Open a serial port and, when `expected` is given, refuse the open unless the
/// descriptor is that exact device node. The identity is checked on the open
/// file descriptor, so swapping the path between the check and the open cannot
/// hand a different device the authorization.
pub fn open_serial_checked(
    path: &str,
    baud: u32,
    expected: Option<&DeviceIdentity>,
) -> io::Result<OpenedSerial> {
    // Validate the path's node *before* opening/configuring it, so a rejected
    // replacement is not even reconfigured.
    let current = DeviceIdentity::capture(path)?;
    if let Some(expected) = expected {
        if current != *expected || !current.still_valid() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path} changed since it was checked; check it again"),
            ));
        }
    }
    let raw = nix::fcntl::open(
        path,
        nix::fcntl::OFlag::O_RDWR
            | nix::fcntl::OFlag::O_NOCTTY
            | nix::fcntl::OFlag::O_NONBLOCK
            | nix::fcntl::OFlag::O_CLOEXEC,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
    // SAFETY: `open` just returned this descriptor and nothing else owns it.
    let file = unsafe { File::from_raw_fd(raw) };
    // Validate the descriptor that was actually opened, before configuring it.
    if let Some(expected) = expected {
        if current.canonical != expected.canonical || !current.same_open_file(file.as_raw_fd())? {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{path} changed since it was checked; check it again"),
            ));
        }
    }
    // Take ownership before changing settings: a failed acquisition must not
    // alter another application's termios. flock serializes core instances;
    // TIOCEXCL refuses later opens. A failed configure below clears it.
    // SAFETY: flock on a valid descriptor we own.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: TIOCEXCL takes no argument for this valid descriptor.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCEXCL) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if let Err(error) = configure_termios(&file, baud) {
        // SAFETY: clear the exclusive flag we just set before closing.
        let _ = unsafe {
            libc::ioctl(file.as_raw_fd(), 0x540D /* TIOCNXCL */)
        };
        return Err(error);
    }
    let port = PosixSerial {
        exclusive: Arc::new(ExclusiveToken {
            file: file.try_clone()?,
        }),
        file: Arc::new(file),
        baud,
        timeout: Duration::from_millis(50),
    };
    if let Some(factory) = SERIAL_FACTORY.lock().expect("serial factory lock").clone() {
        drop(port);
        let port = factory(path, baud)?;
        return Ok(OpenedSerial {
            port,
            identity: current,
        });
    }
    Ok(OpenedSerial {
        port: Box::new(port),
        identity: current,
    })
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
    write_epoch: WriteEpoch,
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
                // Revoked remote work is dropped before it touches the device;
                // the write already in progress finishes.
                if command.remote && write_epoch.load(Ordering::SeqCst) != command.epoch {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;

    #[test]
    fn opening_normalizes_framing_and_flow_control() {
        use nix::pty::openpty;
        use nix::sys::termios::{tcgetattr, tcsetattr, ControlFlags, SetArg};

        let pty = openpty(None, None).expect("pty");
        let slave: std::fs::File = pty.slave.into();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .expect("path")
            .to_string_lossy()
            .into_owned();
        let mut settings = tcgetattr(&slave).expect("termios");
        settings
            .control_flags
            .insert(ControlFlags::CSTOPB | ControlFlags::CRTSCTS);
        tcsetattr(&slave, SetArg::TCSANOW, &settings).expect("seed");

        let _port = open_serial(&path, 115_200).expect("open");
        let after = tcgetattr(&slave).expect("termios after");
        assert!(!after.control_flags.contains(ControlFlags::CSTOPB));
        assert!(!after.control_flags.contains(ControlFlags::CRTSCTS));
        assert!(after.control_flags.contains(ControlFlags::CS8));
    }

    #[test]
    fn opening_keeps_exclusive_ownership() {
        use nix::pty::openpty;
        let pty = openpty(None, None).expect("pty");
        let slave: std::fs::File = pty.slave.into();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .expect("path")
            .to_string_lossy()
            .into_owned();
        let _port = open_serial(&path, 115_200).expect("open");
        let second = nix::fcntl::open(
            path.as_str(),
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY,
            nix::sys::stat::Mode::empty(),
        );
        assert!(second.is_err(), "a second open must be refused");
    }

    #[test]
    fn dropping_one_clone_keeps_exclusive_mode() {
        use nix::pty::openpty;
        let pty = openpty(None, None).expect("pty");
        let slave: std::fs::File = pty.slave.into();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .expect("path")
            .to_string_lossy()
            .into_owned();
        let port = open_serial(&path, 115_200).expect("open");
        let clone = port.port.try_clone().expect("clone");
        drop(clone);
        let second = nix::fcntl::open(
            path.as_str(),
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY,
            nix::sys::stat::Mode::empty(),
        );
        assert!(second.is_err(), "a live handle must stay exclusive");
        drop(port);
        open_serial(&path, 115_200).expect("reopen after all handles drop");

        // Reverse drop order: the original goes first and the clone's token
        // must still own a valid descriptor for cleanup.
        let original = open_serial(&path, 115_200).expect("reopen");
        let clone = original.port.try_clone().expect("clone");
        drop(original);
        let second = nix::fcntl::open(
            path.as_str(),
            nix::fcntl::OFlag::O_RDWR | nix::fcntl::OFlag::O_NOCTTY,
            nix::sys::stat::Mode::empty(),
        );
        assert!(second.is_err(), "the clone must remain exclusive");
        drop(clone);
        open_serial(&path, 115_200).expect("reopen after reverse drop order");
        drop(slave);
    }

    #[test]
    fn failed_opening_leaves_other_settings_and_exclusivity_untouched() {
        use nix::pty::openpty;
        use nix::sys::termios::{
            cfgetispeed, cfsetispeed, cfsetospeed, tcgetattr, tcsetattr, SetArg,
        };
        let pty = openpty(None, None).expect("pty");
        let slave: std::fs::File = pty.slave.into();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .expect("path")
            .to_string_lossy()
            .into_owned();
        let mut settings = tcgetattr(&slave).expect("termios");
        cfsetispeed(&mut settings, nix::sys::termios::BaudRate::B9600).expect("speed");
        cfsetospeed(&mut settings, nix::sys::termios::BaudRate::B9600).expect("speed");
        tcsetattr(&slave, SetArg::TCSANOW, &settings).expect("seed");
        // SAFETY: flock on a descriptor the test owns.
        assert_eq!(
            unsafe { libc::flock(slave.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0
        );
        assert!(
            open_serial(&path, 115_200).is_err(),
            "locked port must fail"
        );
        let after = tcgetattr(&slave).expect("termios after");
        assert_eq!(cfgetispeed(&after), nix::sys::termios::BaudRate::B9600);
        // SAFETY: release the lock before reopening.
        assert_eq!(unsafe { libc::flock(slave.as_raw_fd(), libc::LOCK_UN) }, 0);
        open_serial(&path, 115_200).expect("open after lock release");
    }

    #[test]
    fn framing_setters_are_reflected_by_getters() {
        use nix::pty::openpty;
        use serialport::StopBits;
        let pty = openpty(None, None).expect("pty");
        let slave: std::fs::File = pty.slave.into();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .expect("path")
            .to_string_lossy()
            .into_owned();
        let mut opened = open_serial(&path, 115_200).expect("open");
        let port = &mut opened.port;
        port.set_stop_bits(StopBits::Two).expect("stop bits");
        assert_eq!(port.stop_bits().expect("stop getter"), StopBits::Two);
        assert_eq!(
            port.data_bits().expect("bits getter"),
            serialport::DataBits::Eight
        );
        // Changing speed must not silently reset framing.
        port.set_baud_rate(9_600).expect("baud");
        assert_eq!(port.stop_bits().expect("stop getter"), StopBits::Two);

        // Break start/stop reach the device instead of being a pulse/no-op.
        port.set_break().expect("start break");
        port.clear_break().expect("clear break");
    }

    #[test]
    fn reopening_after_drop_clears_exclusive_mode() {
        use nix::pty::openpty;
        let pty = openpty(None, None).expect("pty");
        let slave: std::fs::File = pty.slave.into();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .expect("path")
            .to_string_lossy()
            .into_owned();
        drop(open_serial(&path, 115_200).expect("first open"));
        open_serial(&path, 115_200).expect("reopen after drop");
        drop(slave);
    }

    #[test]
    fn opening_supports_arbitrary_baud_rates() {
        use nix::pty::openpty;
        let pty = openpty(None, None).expect("pty");
        let slave: std::fs::File = pty.slave.into();
        let path = std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd()))
            .expect("path")
            .to_string_lossy()
            .into_owned();
        let _port = open_serial(&path, 123_456).expect("open arbitrary baud");
        let mut termios = Termios2 {
            c_iflag: 0,
            c_oflag: 0,
            c_cflag: 0,
            c_lflag: 0,
            c_line: 0,
            c_cc: [0; 19],
            c_ispeed: 0,
            c_ospeed: 0,
        };
        const TCGETS2: libc::c_ulong = 0x802C_542A;
        // SAFETY: TCGETS2 fills a termios2 for this valid descriptor.
        assert_eq!(
            unsafe { libc::ioctl(slave.as_raw_fd(), TCGETS2, &mut termios) },
            0
        );
        assert_eq!(termios.c_ospeed, 123_456);
    }

    #[test]
    fn a_mismatched_expected_identity_is_rejected_before_configuration() {
        use nix::pty::openpty;
        use nix::sys::termios::{cfgetispeed, tcgetattr, tcsetattr, BaudRate, SetArg};

        let target = openpty(None, None).expect("target pty");
        let mismatch = openpty(None, None).expect("other pty");
        let target_slave: std::fs::File = target.slave.into();
        let target_path = std::fs::read_link(format!("/proc/self/fd/{}", target_slave.as_raw_fd()))
            .expect("pty path")
            .to_string_lossy()
            .into_owned();
        let other: std::fs::File = mismatch.slave.into();
        let other_path = std::fs::read_link(format!("/proc/self/fd/{}", other.as_raw_fd()))
            .expect("pty path")
            .to_string_lossy()
            .into_owned();
        let other_identity = DeviceIdentity::capture(&other_path).expect("identity");

        // Set a distinctive baud on the target, then ask for a different one
        // with an identity that does not match: the open must be refused
        // before TTYPort::open configures the target.
        let mut termios = tcgetattr(&target_slave).expect("termios");
        nix::sys::termios::cfsetispeed(&mut termios, BaudRate::B38400).expect("ispeed");
        nix::sys::termios::cfsetospeed(&mut termios, BaudRate::B38400).expect("ospeed");
        tcsetattr(&target_slave, SetArg::TCSANOW, &termios).expect("set");

        let error = match open_serial_checked(&target_path, 115_200, Some(&other_identity)) {
            Ok(_) => panic!("mismatched identity must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        let after = tcgetattr(&target_slave).expect("termios after");
        assert_eq!(
            cfgetispeed(&after),
            BaudRate::B38400,
            "a rejected open must not reconfigure the device"
        );
    }
}
