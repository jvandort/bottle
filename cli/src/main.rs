//! bottle-cli: machines, libkrun VMs, kept, started and stopped by name, here and on runners
//! (machine.rs, runner.rs, remote.rs), and the images they boot (build.rs). `boot`, a machine's
//! VM, and `forward`, a runner's machine's forwards, are its own processes.

use std::process::ExitCode;

mod boot;
mod build;
mod copy;
mod exec;
mod machine;
mod remote;
mod runner;
mod upkeep;

/// A failure of bottle-cli's own, not a command's: as ssh has it.
const FAILED: u8 = 255;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some(
            command @ ("create" | "start" | "stop" | "status" | "shell" | "configure" | "rename"
            | "delete" | "ls"),
        ) => machine::main(command, args.collect()),
        Some("exec") => {
            let args: Vec<String> = args.collect();
            match args.split_first() {
                // On an agent's socket: what `exec NAME` comes to.
                Some((flag, rest)) if flag == "--socket" => exec::main(rest.iter().cloned()),
                _ => machine::main("exec", args),
            }
        }
        Some("cp") => copy::main(args),
        Some("info") => machine::info(args),
        Some("image") => build::image(args),
        Some("images") => build::images(args),
        Some("gc") => build::gc(args),
        Some("runner") => runner::main(args),
        Some("on") => runner::on(args),
        // Its own processes: a machine's VM, and a runner's machine's forwards.
        Some("boot") => boot::main(args),
        Some("forward") => remote::forward(args),
        Some(other) => usage_error(&format!("no command {other}")),
        None => usage_error("no command"),
    }
}

/// Says what's wrong with the command line of one of its own processes (`boot`, `exec --socket`),
/// and how that goes.
fn own_usage_error(problem: &str, usage: &str) -> ExitCode {
    eprintln!("bottle-cli: {problem}");
    eprintln!("usage: bottle-cli {usage}");
    ExitCode::from(FAILED)
}

/// Says what's wrong with the command line, and how it goes.
fn usage_error(problem: &str) -> ExitCode {
    eprintln!("bottle-cli: {problem}");
    eprintln!("usage: bottle-cli {}", machine::USAGE);
    eprintln!("   or: bottle-cli {}", build::USAGE);
    eprintln!("   or: bottle-cli {}", runner::USAGE);
    ExitCode::from(FAILED)
}

/// The mke2fs bottle runs: $BOTTLE_MKE2FS, else the one beside this program.
fn mke2fs() -> std::io::Result<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("BOTTLE_MKE2FS") {
        return Ok(path.into());
    }
    let beside = std::env::current_exe()?.with_file_name("mke2fs");
    if beside.exists() {
        return Ok(beside);
    }
    Err(std::io::Error::other(format!(
        "no mke2fs: not at {}, and no $BOTTLE_MKE2FS",
        beside.display()
    )))
}
