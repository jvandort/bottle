//! Stand-ins for the programs folio runs, for folio's tests: bottle-cli, ssh and macOS's
//! `security`. One program, which is the one it's run as: a test links it under each name.

use std::process::ExitCode;

mod bottle_cli;
mod security;
mod ssh;

fn main() -> ExitCode {
    let mut args = std::env::args();
    let program = args.next().unwrap_or_default();
    let name = std::path::Path::new(&program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let args: Vec<String> = args.collect();
    match name.as_str() {
        "bottle-cli" => bottle_cli::main(args),
        "ssh" => ssh::main(args),
        "security" => security::main(args),
        _ => {
            eprintln!("folio-standin: run as bottle-cli, ssh or security, not {name}");
            ExitCode::from(2)
        }
    }
}
