//! folio-cli: folio's own program, which folio's Python is becoming a front end for, as bottle's
//! Python was for bottle-cli. What it does so far: runners (runner.rs).
//!
//!     folio-cli runner add NAME --ssh DESTINATION
//!     folio-cli runner ls [--json]
//!     folio-cli runner rm NAME
//!     folio-cli runner ssh NAME [COMMAND]...
//!     folio-cli runner check NAME|DESTINATION [--json]
//!     folio-cli runner detach NAME [--offline]
//!     folio-cli runner attach NAME
//!     folio-cli runner forward NAME THERE=HERE...
//!     folio-cli on NAME COMMAND [ARG]...
//!
//! It runs bottle's code only by running bottle-cli: here ($BOTTLE_CLI, else bottle-cli on PATH),
//! for the distributions beside it, and on a runner, over ssh. Its state is in folio's home,
//! $FOLIO_HOME, else ~/.folio. Its own failures exit FAILED, saying why on stderr, a line starting
//! `folio-cli: `.

mod runner;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

/// What this program exits when it fails, as bottle-cli does.
const FAILED: u8 = 255;

const USAGE: &str = "usage: folio-cli runner add|ls|rm|ssh|check|detach|attach|forward ... \
                     | folio-cli on NAME COMMAND [ARG]...";

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("runner") => runner::main(args),
        Some("on") => runner::on(args),
        _ => usage_error(USAGE),
    }
}

fn usage_error(usage: &str) -> ExitCode {
    eprintln!("folio-cli: {usage}");
    ExitCode::from(2)
}

/// folio's home: $FOLIO_HOME, else ~/.folio.
fn home() -> PathBuf {
    env::var_os("FOLIO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".folio"))
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
