//! Finding and starting `serial-core`.
//!
//! The seam exists so packaging can put the core anywhere: callers may name an
//! explicit executable path, set `SERIALBRIDGE_CORE`, build the client with
//! `SERIALBRIDGE_CORE_DEFAULT` (Nix packaging does this), or let the client
//! look next to itself and on `PATH`.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::ClientError;

/// Environment variable naming an explicit `serial-core` executable.
pub const CORE_ENV: &str = "SERIALBRIDGE_CORE";

/// Executable name looked for next to the client and on `PATH`.
pub const CORE_PROGRAM: &str = "serial-core";

/// The core path packaging compiled in, if any.
///
/// A build sets `SERIALBRIDGE_CORE_DEFAULT` for the compiler; Nix packaging
/// points it at the `serial-core` store path. `option_env!` is read at build
/// time, so a normal `cargo build` has no compiled default.
const COMPILED_CORE: Option<&str> = option_env!("SERIALBRIDGE_CORE_DEFAULT");

/// Pick the core executable to run.
///
/// Precedence: `explicit`, `$SERIALBRIDGE_CORE`, the path compiled in by
/// packaging, a sibling of the current executable (how `cargo build` lays out a
/// workspace), then `PATH`.
pub fn find_core_binary(explicit: Option<&Path>) -> Option<PathBuf> {
    choose_core(
        explicit.map(PathBuf::from),
        non_empty(std::env::var_os(CORE_ENV)).map(PathBuf::from),
        COMPILED_CORE.map(PathBuf::from),
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf)),
        std::env::var_os("PATH"),
    )
}

/// The pure form of [`find_core_binary`]: every input is explicit so the
/// precedence can be tested without touching the environment.
fn choose_core(
    explicit: Option<PathBuf>,
    from_env: Option<PathBuf>,
    compiled: Option<PathBuf>,
    exe_dir: Option<PathBuf>,
    path: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path);
    }
    if let Some(path) = from_env {
        return Some(path);
    }
    if let Some(path) = compiled {
        return Some(path);
    }
    if let Some(dir) = exe_dir {
        let sibling = dir.join(CORE_PROGRAM);
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    find_on_path(CORE_PROGRAM, path.as_deref())
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

fn find_on_path(program: &str, path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bin(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, b"#!/bin/sh\n").expect("write fake program");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make executable");
        path
    }

    #[test]
    fn explicit_then_env_then_compiled_then_sibling_then_path() {
        let directory = tempfile::TempDir::new().expect("tempdir");
        let exe_dir = directory.path().join("exe");
        let path_dir = directory.path().join("path");
        std::fs::create_dir_all(&exe_dir).expect("exe dir");
        std::fs::create_dir_all(&path_dir).expect("path dir");
        let sibling = bin(&exe_dir, CORE_PROGRAM);
        let on_path = bin(&path_dir, CORE_PROGRAM);
        let compiled = PathBuf::from("/nix/store/core-packaged/bin/serial-core");
        let env = PathBuf::from("/opt/serial/bin/serial-core");
        let explicit = PathBuf::from("/explicit/serial-core");
        let path_var = || Some(OsString::from(path_dir.clone()));

        // Every layer in turn, each one answering before the next is reached.
        assert_eq!(
            choose_core(
                Some(explicit.clone()),
                Some(env.clone()),
                Some(compiled.clone()),
                Some(exe_dir.clone()),
                path_var()
            ),
            Some(explicit)
        );
        assert_eq!(
            choose_core(
                None,
                Some(env.clone()),
                Some(compiled.clone()),
                Some(exe_dir.clone()),
                path_var()
            ),
            Some(env)
        );
        // Packaging's core beats a stale sibling and PATH.
        assert_eq!(
            choose_core(
                None,
                None,
                Some(compiled.clone()),
                Some(exe_dir.clone()),
                path_var()
            ),
            Some(compiled)
        );
        // Without a compiled default, a cargo build's sibling is used.
        assert_eq!(
            choose_core(None, None, None, Some(exe_dir.clone()), path_var()),
            Some(sibling)
        );
        // No sibling: PATH.
        let empty_dir = directory.path().join("empty");
        std::fs::create_dir_all(&empty_dir).expect("empty dir");
        assert_eq!(
            choose_core(None, None, None, Some(empty_dir), path_var()),
            Some(on_path)
        );
        // Nothing at all.
        assert_eq!(
            choose_core(None, None, None, None, Some(OsString::from("/nonexistent"))),
            None
        );
    }

    #[test]
    fn empty_env_is_ignored() {
        let directory = tempfile::TempDir::new().expect("tempdir");
        let sibling = bin(directory.path(), CORE_PROGRAM);
        // An empty variable must not win the way `PathBuf::from("")` would.
        assert_eq!(non_empty(Some(OsString::new())), None);
        assert_eq!(
            choose_core(None, None, None, Some(directory.path().to_path_buf()), None),
            Some(sibling)
        );
    }

    #[test]
    fn missing_compiled_core_is_still_returned_for_a_clear_error() {
        // Packaging's path is authoritative: if it is gone, the start attempt
        // reports the path rather than silently starting a different core.
        let compiled = PathBuf::from("/nix/store/gone/bin/serial-core");
        assert_eq!(
            choose_core(
                None,
                None,
                Some(compiled.clone()),
                None,
                Some(OsString::from("/nonexistent"))
            ),
            Some(compiled)
        );
    }
}
