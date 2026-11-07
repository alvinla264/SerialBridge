//! Test helpers: a simulated serial device (a PTY pair), a core on a temp
//! socket, and small polling utilities. No physical hardware is involved.

#![allow(dead_code)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::pty::openpty;
use serial_nexus_client::{Client, ClientInfo};
use serial_nexus_protocol::Event;
use tempfile::TempDir;

/// A PTY pair: the core opens the slave path as a serial port, the test drives
/// the master side.
pub struct SimulatedDevice {
    pub path: String,
    master: File,
    // Keep the slave open so the PTY stays alive for the whole test.
    _slave: File,
}

impl SimulatedDevice {
    pub fn new() -> Self {
        let pty = openpty(None, None).expect("openpty");
        let master: File = pty.master.into();
        let slave: File = pty.slave.into();
        let link =
            std::fs::read_link(format!("/proc/self/fd/{}", slave.as_raw_fd())).expect("slave path");
        fcntl(master.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).expect("nonblocking");
        Self {
            path: link.to_string_lossy().into_owned(),
            master,
            _slave: slave,
        }
    }

    /// Bytes the core wrote to the device.
    pub fn write(&mut self, data: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut remaining = data;
        while !remaining.is_empty() {
            match self.master.write(remaining) {
                Ok(written) => remaining = &remaining[written..],
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        panic!("master write timed out");
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("master write failed: {error}"),
            }
        }
        self.master.flush().ok();
    }

    /// Read exactly `count` bytes from the master side, else panic.
    pub fn read_n(&mut self, count: usize, timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut got = Vec::new();
        loop {
            let mut buffer = [0u8; 512];
            match self.master.read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    got.extend_from_slice(&buffer[..read]);
                    if got.len() >= count {
                        got.truncate(count);
                        return got;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("master read failed: {error} (got {} bytes)", got.len()),
            }
            if Instant::now() >= deadline {
                panic!("did not read {count} bytes; got {} bytes", got.len());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Read the master side until `needle` appears, else panic after timeout.
    pub fn read_until(&mut self, needle: &[u8], timeout: Duration) -> Vec<u8> {
        let deadline = Instant::now() + timeout;
        let mut got = Vec::new();
        loop {
            let mut buffer = [0u8; 512];
            match self.master.read(&mut buffer) {
                Ok(0) => {}
                Ok(read) => {
                    got.extend_from_slice(&buffer[..read]);
                    if contains(&got, needle) {
                        return got;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("master read failed: {error} (got {got:?})"),
            }
            if Instant::now() >= deadline {
                panic!("did not see {needle:?}; got {got:?}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Start a core in a background thread on a fresh temp socket.
pub fn start_core(
    grace: Duration,
    control_idle: Duration,
    history_max_lines: usize,
) -> (TempDir, PathBuf) {
    let directory = TempDir::new().expect("tempdir");
    let socket = directory.path().join("core.sock");
    let config = serial_nexus_core::CoreConfig {
        socket: socket.clone(),
        grace,
        control_idle,
        history_max_lines,
        history_max_bytes: 256 * 1024,
        ..serial_nexus_core::CoreConfig::default()
    };
    std::thread::spawn(move || {
        if let Err(error) = serial_nexus_core::server::run(config) {
            eprintln!("test core failed: {error}");
        }
    });
    wait_until(Duration::from_secs(5), || {
        std::os::unix::net::UnixStream::connect(&socket).is_ok()
    });
    (directory, socket)
}

pub fn connect(socket: &PathBuf, client: ClientInfo) -> Client {
    Client::connect_to_socket(socket, client).expect("client connects")
}

pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not met within {timeout:?}");
}

pub fn wait_event(
    events: &Receiver<Event>,
    timeout: Duration,
    mut matches: impl FnMut(&Event) -> bool,
) -> Event {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            panic!("no matching event within {timeout:?}");
        }
        match events.recv_timeout(left) {
            Ok(event) if matches(&event) => return event,
            Ok(_) => continue,
            Err(_) => panic!("no matching event within {timeout:?}"),
        }
    }
}

/// Read repeatedly until the recent history contains `needle`.
pub fn read_until_contains(
    client: &Client,
    device_id: u64,
    needle: &str,
    timeout: Duration,
) -> serial_nexus_protocol::ReadResult {
    let deadline = Instant::now() + timeout;
    loop {
        let result = client.read(device_id, Some(500)).expect("read");
        if result.lines.iter().any(|line| line.text.contains(needle)) {
            return result;
        }
        if Instant::now() >= deadline {
            panic!("history never contained {needle:?}: {:?}", result.lines);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
