use std::process::ExitCode;

use serial_core::config::{CoreConfig, USAGE};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("serial-core {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let config = match CoreConfig::from_args(args) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("serial-core: {message}");
            return ExitCode::from(2);
        }
    };
    match serial_core::server::run(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("serial-core: {error}");
            ExitCode::FAILURE
        }
    }
}
