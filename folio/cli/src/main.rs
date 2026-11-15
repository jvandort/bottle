//! folio-cli: folio's own program, which folio's Python is becoming a front end for, as bottle's
//! Python was for bottle-cli. What it does so far: runners (runner.rs), features (features.rs,
//! routes.rs) and what builds an image of them (images.rs), and folio serve (serve/).
//!
//!     folio-cli runner add NAME --ssh DESTINATION
//!     folio-cli runner ls [--json]
//!     folio-cli runner rm NAME
//!     folio-cli runner ssh NAME [COMMAND]...
//!     folio-cli runner check NAME|DESTINATION [--json]
//!     folio-cli runner detach NAME [--offline]
//!     folio-cli runner attach NAME
//!     folio-cli runner home NAME
//!     folio-cli runner forward NAME THERE=HERE...
//!     folio-cli on [--connect-timeout SECONDS] NAME COMMAND [ARG]...
//!     folio-cli features list|show|canonical|resolve|image|cached ...
//!     folio-cli serve --control PATH --service NAME=PATH... [--restore]
//!     folio-cli hook pre-receive|post-receive   (git's, for a push folio serve takes)
//!
//! It runs bottle's code only by running bottle-cli: here ($BOTTLE_CLI, else bottle-cli on PATH),
//! for the distributions beside it, and on a runner, over ssh. Its state is in folio's home,
//! $FOLIO_HOME, else ~/.folio. Its own failures exit FAILED, saying why on stderr, a line starting
//! `folio-cli: `.

mod advice;
mod auth;
mod bottle_cli;
mod bottles;
mod builds;
mod broker;
mod daemon;
mod features;
mod git;
mod lanes;
mod notes;
mod images;
mod pyjson;
mod repos;
mod routes;
mod runner;
mod runners;
mod serve;
mod services;
mod snapshots;
mod specs;
mod store;
#[cfg(test)]
mod tempdir;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

/// What this program exits when it fails, as bottle-cli does.
const FAILED: u8 = 255;

const USAGE: &str = "usage: folio-cli runner add|ls|rm|ssh|check|detach|attach|home|forward ... \
                     | folio-cli on [--connect-timeout SECONDS] NAME COMMAND [ARG]... \
                     | folio-cli features list|show|canonical|resolve|image|cached ... \
                     | folio-cli serve --control PATH --service NAME=PATH... [--restore]";

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("runner") => runner::main(args),
        Some("on") => runner::on(args),
        Some("features") => features::main(args),
        Some("serve") => serve::main(args),
        Some("hook") => serve::hook::main(args),
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

/// `s` as Python's repr has a string: in single quotes, unless it holds one and no double quote;
/// what folio's messages have always said.
fn repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
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
