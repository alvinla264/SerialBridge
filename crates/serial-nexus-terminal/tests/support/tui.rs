//! The real terminal binary in a pseudo-terminal, driven and read like a
//! human's keyboard and screen.
//!
//! `TuiSession` gives the child a PTY as its controlling terminal, so raw
//! mode, mouse capture, signal-driven resize and the alternate screen all take
//! their real path. A small terminal emulator (`vt100`) turns what the child
//! wrote back into the screen a human would see, so a test asserts on the
//! drawn interface rather than on the app's state.

#![allow(dead_code)]

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::pty::{openpty, Winsize};
use vt100::Parser;

/// How long a test waits for the screen to show an effect.
pub const STEP: Duration = Duration::from_secs(5);

pub const KEY_ENTER: &[u8] = b"\r";
pub const KEY_ESC: &[u8] = b"\x1b";
pub const KEY_TAB: &[u8] = b"\t";
pub const KEY_UP: &[u8] = b"\x1b[A";
pub const KEY_DOWN: &[u8] = b"\x1b[B";
pub const KEY_PAGE_UP: &[u8] = b"\x1b[5~";
pub const KEY_CTRL_Q: &[u8] = &[0x11];
pub const KEY_CTRL_S: &[u8] = &[0x13];
/// What a terminal without the enhanced keyboard protocol sends for `Ctrl+]`.
pub const KEY_CTRL_BRACKET: &[u8] = &[0x1d];

/// A running `serial-nexus-terminal` on a PTY, with its screen.
pub struct TuiSession {
    master: File,
    child: Child,
    parser: Arc<Mutex<Parser>>,
    reader: Option<std::thread::JoinHandle<()>>,
    rows: u16,
    cols: u16,
}

impl TuiSession {
    /// Start the real binary against `socket`, with `xdg` as its home so the
    /// settings file stays inside the test.
    pub fn spawn(socket: &Path, xdg: &Path, rows: u16, cols: u16) -> Self {
        let pty = openpty(
            Some(&Winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .expect("openpty");
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);

        let mut command = Command::new(env!("CARGO_BIN_EXE_serial-nexus-terminal"));
        command
            .env("SERIAL_NEXUS_SOCKET", socket)
            .env("XDG_CONFIG_HOME", xdg)
            .env("HOME", xdg)
            .env("TERM", "xterm-256color")
            .env_remove("SERIAL_NEXUS_CORE")
            .stdin(Stdio::from(slave.try_clone().expect("clone slave")))
            .stdout(Stdio::from(slave.try_clone().expect("clone slave")))
            .stderr(Stdio::from(slave.try_clone().expect("clone slave")));
        // A new session with the slave as its controlling terminal: the
        // kernel then sends SIGWINCH when the window size changes, which is
        // how crossterm learns about a resize.
        unsafe {
            command.pre_exec(|| {
                nix::unistd::setsid().map_err(io::Error::from)?;
                if nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("start serial-nexus-terminal");
        drop(slave);

        let parser = Arc::new(Mutex::new(Parser::new(rows, cols, 0)));
        let mut reads = master.try_clone().expect("clone master");
        let screen = Arc::clone(&parser);
        let reader = std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            loop {
                match reads.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => screen.lock().expect("screen lock").process(&buffer[..read]),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });

        Self {
            master,
            child,
            parser,
            reader: Some(reader),
            rows,
            cols,
        }
    }

    pub fn rows(&self) -> u16 {
        self.rows
    }

    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Type raw bytes, exactly as a terminal would send them.
    pub fn send(&mut self, bytes: &[u8]) {
        self.master.write_all(bytes).expect("write to the TUI");
        self.master.flush().expect("flush");
    }

    pub fn send_text(&mut self, text: &str) {
        self.send(text.as_bytes());
    }

    /// A left click at a zero-based screen cell, as SGR mouse reporting.
    pub fn click(&mut self, column: u16, row: u16) {
        self.send(
            format!(
                "\x1b[<0;{};{}M\x1b[<0;{};{}m",
                column + 1,
                row + 1,
                column + 1,
                row + 1
            )
            .as_bytes(),
        );
    }

    /// Two clicks on the same cell, inside the app's double-click window.
    pub fn double_click(&mut self, column: u16, row: u16) {
        self.click(column, row);
        std::thread::sleep(Duration::from_millis(40));
        self.click(column, row);
    }

    /// Change the window size. The kernel signals SIGWINCH to the child and
    /// the emulator follows the same size.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let size = Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let result =
            unsafe { nix::libc::ioctl(self.master.as_raw_fd(), nix::libc::TIOCSWINSZ, &size) };
        assert_eq!(result, 0, "set window size");
        self.rows = rows;
        self.cols = cols;
        self.parser
            .lock()
            .expect("screen lock")
            .set_size(rows, cols);
    }

    /// Everything the screen shows, one line per row.
    pub fn text(&self) -> String {
        self.parser.lock().expect("screen lock").screen().contents()
    }

    pub fn row(&self, row: u16) -> String {
        self.text()
            .lines()
            .nth(row as usize)
            .unwrap_or_default()
            .trim_end()
            .to_string()
    }

    /// The cell a character sits at, as `(column, row)`.
    pub fn find(&self, needle: &str) -> Option<(u16, u16)> {
        use unicode_width::UnicodeWidthStr;

        let parser = self.parser.lock().expect("screen lock");
        for (row, line) in parser.screen().rows(0, self.cols).enumerate() {
            if let Some(byte) = line.find(needle) {
                let column = line[..byte].width() as u16;
                return Some((column, row as u16));
            }
        }
        None
    }

    pub fn wait_for_text(&mut self, needle: &str) {
        self.wait_until(&format!("screen to show {needle:?}"), |text| {
            text.contains(needle)
        });
    }

    /// Wait for a condition on the whole screen, then keep going.
    pub fn wait_until(&mut self, what: &str, mut ready: impl FnMut(&str) -> bool) {
        let deadline = Instant::now() + STEP;
        loop {
            let text = self.text();
            if ready(&text) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}\n--- screen ---\n{text}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Quit the terminal the way a human would: make sure the sidebar has the
    /// keys, then `Ctrl+Q`. The process exit is part of the check.
    pub fn quit(mut self) {
        // Ctrl+] twice returns the focus to the sidebar whatever pane has it.
        self.send(KEY_CTRL_BRACKET);
        self.send(KEY_CTRL_BRACKET);
        self.send(KEY_CTRL_Q);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.child.try_wait().expect("try_wait").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the terminal did not quit on Ctrl+Q");
    }
}

impl Drop for TuiSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}
