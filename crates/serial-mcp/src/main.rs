use serial_mcp::Session;
use std::process::ExitCode;

const USAGE: &str = "\
serial-mcp: an MCP server over stdio for SerialBridge serial devices

The AI host launches it and speaks MCP on stdin/stdout. It connects to the
local serial-core (starting it when needed); the socket comes from
SERIALBRIDGE_SOCKET like every other SerialBridge client.

Usage: serial-mcp [--help] [--version]
";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("--help" | "-h") => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("--version" | "-V") => {
            println!("serial-mcp {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("serial-mcp: unknown argument {other:?}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    }

    let session = Session::new();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match serial_mcp::serve(&session, stdin.lock(), stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("serial-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}
