use std::path::PathBuf;
use std::time::Duration;

use serialbridge_protocol::{paths, Limits, DEFAULT_BAUD};

/// The core's bounded defaults. Every limit is reported to clients in the
/// `hello` answer, so the MCP tool descriptions and docs can state them.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    pub socket: PathBuf,
    /// Time with no clients before the core exits.
    pub grace: Duration,
    /// Time without a request before an AI's write control expires.
    pub control_idle: Duration,
    pub history_max_lines: usize,
    pub history_max_bytes: usize,
    pub read_default_lines: usize,
    pub read_max_lines: usize,
    pub write_default_timeout: Duration,
    pub write_max_timeout: Duration,
    /// Baud rate used when a new device is opened without one.
    pub default_baud: u32,
}

impl Default for CoreConfig {
    fn default() -> Self {
        Self {
            socket: paths::default_socket_path(),
            grace: Duration::from_secs(30),
            control_idle: Duration::from_secs(300),
            history_max_lines: 2_000,
            history_max_bytes: 256 * 1024,
            read_default_lines: 50,
            read_max_lines: 2_000,
            write_default_timeout: Duration::from_millis(1_000),
            write_max_timeout: Duration::from_secs(30),
            default_baud: DEFAULT_BAUD,
        }
    }
}

impl CoreConfig {
    /// The limits reported to clients.
    pub fn limits(&self) -> Limits {
        Limits {
            read_default_lines: self.read_default_lines,
            read_max_lines: self.read_max_lines,
            write_default_timeout_ms: self.write_default_timeout.as_millis() as u64,
            write_max_timeout_ms: self.write_max_timeout.as_millis() as u64,
            control_idle_ms: self.control_idle.as_millis() as u64,
            history_max_lines: self.history_max_lines,
        }
    }

    /// Build from environment overrides. Used by the binary and by tests.
    ///
    /// - `SERIALBRIDGE_SOCKET`
    /// - `SERIALBRIDGE_GRACE_SECS`
    /// - `SERIALBRIDGE_CONTROL_IDLE_SECS`
    /// - `SERIALBRIDGE_HISTORY_LINES`
    pub fn from_env() -> Result<Self, String> {
        let mut config = Self::default();
        if let Ok(value) = std::env::var("SERIALBRIDGE_GRACE_SECS") {
            config.grace = Duration::from_secs(parse(&value, "SERIALBRIDGE_GRACE_SECS")?);
        }
        if let Ok(value) = std::env::var("SERIALBRIDGE_CONTROL_IDLE_SECS") {
            config.control_idle =
                Duration::from_secs(parse(&value, "SERIALBRIDGE_CONTROL_IDLE_SECS")?);
        }
        if let Ok(value) = std::env::var("SERIALBRIDGE_HISTORY_LINES") {
            config.history_max_lines = parse(&value, "SERIALBRIDGE_HISTORY_LINES")?;
        }
        if let Ok(value) = std::env::var("SERIALBRIDGE_DEFAULT_BAUD") {
            config.default_baud = parse(&value, "SERIALBRIDGE_DEFAULT_BAUD")?;
        }
        config.socket = paths::default_socket_path();
        Ok(config)
    }

    /// Apply command-line flags on top of the environment.
    ///
    /// Flags: `--socket PATH`, `--grace-secs N`, `--control-idle-secs N`,
    /// `--history-lines N`, `--help`, `--version`.
    pub fn from_args<I: IntoIterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut config = Self::from_env()?;
        let mut args = args.into_iter().peekable();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--socket" => {
                    let value = args.next().ok_or("--socket needs a path")?;
                    config.socket = PathBuf::from(value);
                }
                "--grace-secs" => {
                    let value = args.next().ok_or("--grace-secs needs a number")?;
                    config.grace = Duration::from_secs(parse(&value, "--grace-secs")?);
                }
                "--control-idle-secs" => {
                    let value = args.next().ok_or("--control-idle-secs needs a number")?;
                    config.control_idle =
                        Duration::from_secs(parse(&value, "--control-idle-secs")?);
                }
                "--history-lines" => {
                    let value = args.next().ok_or("--history-lines needs a number")?;
                    config.history_max_lines = parse(&value, "--history-lines")?;
                }
                "--default-baud" => {
                    let value = args.next().ok_or("--default-baud needs a number")?;
                    config.default_baud = parse(&value, "--default-baud")?;
                }
                "--help" | "-h" => return Err(USAGE.to_string()),
                "--version" | "-V" => {
                    return Err(format!("serial-core {}", env!("CARGO_PKG_VERSION")))
                }
                other => return Err(format!("unknown argument {other:?}\n\n{USAGE}")),
            }
        }
        Ok(config)
    }
}

fn parse<T: std::str::FromStr>(value: &str, name: &str) -> Result<T, String> {
    value
        .parse()
        .map_err(|_| format!("{name} must be a number, got {value:?}"))
}

pub const USAGE: &str = "\
serial-core: the SerialBridge core, one per local user

Usage: serial-core [OPTIONS]

Options:
  --socket PATH            Unix socket to serve on (default: $SERIALBRIDGE_SOCKET,
                           $XDG_RUNTIME_DIR/serialbridge/core.sock, or a per-user temp dir)
  --grace-secs N           Exit N seconds after the last client leaves (default 30)
  --control-idle-secs N    Release AI write control after N idle seconds (default 300)
  --history-lines N        History lines kept per open device (default 2000)
  --default-baud N         Baud rate when a new device is opened without one (default 115200)
  -h, --help               Show this help
  -V, --version            Show the version

Environment: SERIALBRIDGE_SOCKET, SERIALBRIDGE_GRACE_SECS,
SERIALBRIDGE_CONTROL_IDLE_SECS, SERIALBRIDGE_HISTORY_LINES,
SERIALBRIDGE_DEFAULT_BAUD";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_override_defaults() {
        let config = CoreConfig::from_args([
            "--socket".into(),
            "/tmp/x.sock".into(),
            "--grace-secs".into(),
            "2".into(),
            "--control-idle-secs".into(),
            "3".into(),
            "--history-lines".into(),
            "10".into(),
        ])
        .unwrap();
        assert_eq!(config.socket, PathBuf::from("/tmp/x.sock"));
        assert_eq!(config.grace, Duration::from_secs(2));
        assert_eq!(config.control_idle, Duration::from_secs(3));
        assert_eq!(config.history_max_lines, 10);
    }

    #[test]
    fn bad_flag_value_is_an_error() {
        assert!(CoreConfig::from_args(["--grace-secs".into(), "soon".into()]).is_err());
    }
}
