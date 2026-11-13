//! bottle-cli: this machine's machines, libkrun VMs, kept, started and stopped by name
//! (machine.rs), and the images they boot (build.rs). `boot`, a machine's VM, is its own process.
//! Another machine's are its bottle-cli's, there: whoever reaches it runs that (folio does, over
//! ssh). Its state is in $BOTTLE_HOME, else ~/.bottle (home).

use std::process::ExitCode;

mod boot;
mod build;
mod copy;
mod exec;
mod machine;
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
        // Its own process: a machine's VM.
        Some("boot") => boot::main(args),
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
    ExitCode::from(FAILED)
}

/// $BOTTLE_HOME, else ~/.bottle.
pub fn home() -> std::path::PathBuf {
    std::env::var_os("BOTTLE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".bottle")
        })
}

/// `tar`, to pack an archive for a Linux machine: on a Mac, bsdtar without the extended attributes
/// and Mac metadata it adds (com.apple.provenance, say), which GNU tar there warns of, file by file.
fn packing_tar() -> std::process::Command {
    let mut tar = std::process::Command::new("tar");
    if cfg!(target_os = "macos") {
        tar.args(["--no-xattrs", "--no-mac-metadata"])
            .env("COPYFILE_DISABLE", "1");
    }
    tar
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
