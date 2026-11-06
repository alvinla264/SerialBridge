//! Finding and starting `serial-core`.
//!
//! The seam exists so packaging can put the core anywhere: callers may name an
//! explicit executable path, set `SERIALBRIDGE_CORE`, or let the client look
//! next to itself and on `PATH`.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::ClientError;

/// Environment variable naming an explicit `serial-core` executable.
pub const CORE_ENV: &str = "SERIALBRIDGE_CORE";

/// Executable name looked for next to the client and on `PATH`.
pub const CORE_PROGRAM: &str = "serial-core";

/// Pick the core executable to run.
///
/// Precedence: `explicit`, then `$SERIALBRIDGE_CORE`, then a sibling of the
/// current executable (how `cargo build` lays out a workspace), then `PATH`.
pub fn find_core_binary(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os(CORE_ENV) {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let sibling = dir.join(CORE_PROGRAM);
            if sibling.is_file() {
                return Some(sibling);
            }
        }
    }
    find_on_path(CORE_PROGRAM)
}

fn find_on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Start the core detached and wait until its socket answers.
///
/// `socket` is passed as `--socket`, so the started core is the one this
/// client will talk to. The child's stdio is discarded, it runs in its own
/// session (Ctrl-C in the launching client does not kill the shared core),
/// and a reaper thread collects it, so the caller keeps no `Child` and
/// creates no zombie. Returns the new core's process id.
pub fn spawn_core(
    binary: &Path,
    socket: &Path,
    start_timeout: Duration,
) -> Result<u32, ClientError> {
    let mut command = Command::new(binary);
    command
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Keep the core in the same environment (SERIALBRIDGE_* overrides included).
    for (key, value) in std::env::vars_os() {
        command.env(key, value);
    }
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    // Detach from the launcher's session and process group. The core outlives
    // the client that happened to start it; a terminal Ctrl-C must not take
    // down other clients' devices. Failure here is not fatal.
    // SAFETY: setsid is async-signal-safe and only touches the forked child.
    unsafe {
        command.pre_exec(|| {
            let _ = libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|error| {
        ClientError::CoreStart(format!("could not run {}: {error}", binary.display()))
    })?;
    let pid = child.id();
    std::thread::Builder::new()
        .name("serial-core-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .ok();

    let deadline = Instant::now() + start_timeout;
    loop {
        if socket.is_socket() && std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return Ok(pid);
        }
        if Instant::now() >= deadline {
            return Err(ClientError::CoreStart(format!(
                "no socket appeared at {} within {:?} (started {})",
                socket.display(),
                start_timeout,
                binary.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

trait SocketPath {
    fn is_socket(&self) -> bool;
}

impl SocketPath for Path {
    fn is_socket(&self) -> bool {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(self)
            .map(|meta| meta.file_type().is_socket())
            .unwrap_or(false)
    }
}
