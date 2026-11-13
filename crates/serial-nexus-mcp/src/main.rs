use serial_nexus_mcp::Session;
use std::process::ExitCode;

const USAGE: &str = "\
serial-nexus-mcp: an MCP server over stdio for SerialNexus serial devices

The AI host launches it and speaks MCP on stdin/stdout. All local and remote
calls require the foreground serial-nexus-terminal gateway to be running.
MCP does not start serial-nexus-core or dial remote endpoints itself. The
SERIAL_NEXUS_GATEWAY_SOCKET environment variable overrides the per-user socket.

Usage: serial-nexus-mcp [--help] [--version]
";

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("--help" | "-h") => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some("--version" | "-V") => {
            println!("serial-nexus-mcp {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("serial-nexus-mcp: unknown argument {other:?}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    }

    let session = Session::new();
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match serial_nexus_mcp::serve(&session, stdin.lock(), stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("serial-nexus-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}
