//! bottle-cli: the host's side of bottles that bottle runs on libkrun itself. For now, `exec` and
//! `disk`; the VM's launcher and the rest of its lifecycle are to come.

use std::process::ExitCode;

mod disk;
mod exec;

/// A failure of bottle-cli's own, not a command's: as ssh has it.
const FAILED: u8 = 255;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("disk") => disk::main(args),
        Some("exec") => exec::main(args),
        Some(other) => usage_error(&format!("no command {other}")),
        None => usage_error("no command"),
    }
}

/// Says what's wrong with the command line, and how it goes.
fn usage_error(problem: &str) -> ExitCode {
    eprintln!("bottle-cli: {problem}");
    eprintln!("usage: bottle-cli {}", disk::USAGE);
    eprintln!("       bottle-cli {}", exec::USAGE);
    ExitCode::from(FAILED)
}
