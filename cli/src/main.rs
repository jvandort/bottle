//! bottle-cli: the host's side of bottles that bottle runs on libkrun itself. For now, `exec`,
//! `boot`, the VM's launcher, `build` and `import`, which make images' stages in VMs, and
//! `machine`, bottles' VMs kept, started and stopped.

use std::process::ExitCode;

mod boot;
mod build;
mod ca;
mod exec;
mod machine;
mod upkeep;

/// A failure of bottle-cli's own, not a command's: as ssh has it.
const FAILED: u8 = 255;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("boot") => boot::main(args),
        Some("build") => build::build(args),
        Some("import") => build::import(args),
        Some("pull") => build::pull(args),
        Some("gc") => build::gc(args),
        Some("image") => build::image(args),
        Some("images") => build::images(args),
        Some("machine") => machine::main(args),
        Some("ca") => ca::main(args),
        Some("exec") => {
            let args: Vec<String> = args.collect();
            match args.split_first() {
                // On an agent's socket: what `machine exec` and `exec NAME` come to.
                Some((flag, rest)) if flag == "--socket" => exec::main(rest.iter().cloned()),
                _ => machine::top("exec", args),
            }
        }
        Some(command @ ("ls" | "status" | "start" | "stop" | "shell")) => {
            machine::top(command, args.collect())
        }
        Some(other) => usage_error(&format!("no command {other}")),
        None => usage_error("no command"),
    }
}

/// Says what's wrong with the command line, and how it goes.
fn usage_error(problem: &str) -> ExitCode {
    eprintln!("bottle-cli: {problem}");
    eprintln!("usage: bottle-cli {}", machine::TOP_USAGE);
    eprintln!("   or: bottle-cli {}", exec::USAGE);
    eprintln!("   or: bottle-cli {}", boot::USAGE);
    eprintln!("   or: bottle-cli {}", ca::USAGE);
    eprintln!("   or: bottle-cli {}", build::BUILD_USAGE);
    eprintln!("   or: bottle-cli {}", build::IMPORT_USAGE);
    eprintln!("   or: bottle-cli {}", build::PULL_USAGE);
    eprintln!("   or: bottle-cli {}", build::GC_USAGE);
    eprintln!("   or: bottle-cli {}", build::IMAGE_USAGE);
    eprintln!("   or: bottle-cli {}", build::IMAGES_USAGE);
    eprintln!("   or: bottle-cli {}", machine::USAGE);
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
