//! folio: threads of work in bottles, each one a bottle, a lane for its commits, and Claude. What
//! you type is cli.rs's (clap); a thread is threads.rs's, its bottle bottles.rs's (through
//! engine.rs), what serves the bottles serve/'s.
//!
//! Beside the commands a person types (`folio --help`), it's its own helper, as folio runs it:
//!
//!     folio on [--connect-timeout SECONDS] NAME COMMAND [ARG]...   (bottle-cli on a runner)
//!     folio serve --control PATH --service NAME=PATH... [--restore]   (folio serve itself)
//!     folio hook pre-receive|post-receive   (git's, for a push folio serve takes)
//!
//! It runs bottle's code only by running bottle-cli: here ($BOTTLE_CLI, else bottle-cli on PATH),
//! for the distributions beside it, and on a runner, over ssh. Its state is in folio's home,
//! $FOLIO_HOME, else ~/.folio. Its own failures say why on stderr, a line starting `folio: error: `.

mod advice;
mod asks;
mod auth;
mod bottle_cli;
mod bottles;
mod broker;
mod builds;
mod cli;
mod control;
mod daemon;
mod engine;
mod features;
mod git;
mod hook;
mod images;
mod interrupt;
mod lanes;
mod log;
mod migrate;
mod notes;
mod obsidian;
mod picker;
mod prompt;
mod repos;
mod routes;
mod runner;
mod runners;
mod serve;
mod services;
mod shell;
mod snapshots;
mod specs;
mod store;
#[cfg(test)]
mod tempdir;
mod threads;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

/// What this program exits when it fails, as bottle-cli does.
const FAILED: u8 = 255;

fn main() -> ExitCode {
    let all: Vec<String> = env::args().collect();
    let mut args = all.iter().skip(1).cloned();
    match all.get(1).map(String::as_str) {
        Some("on") => runner::on(args.by_ref().skip(1)),
        // folio serve itself, as folio starts it; `serve start|stop` are folio's commands.
        Some("serve")
            if !matches!(
                all.get(2).map(String::as_str),
                Some("start" | "stop" | "-h" | "--help") | None
            ) =>
        {
            serve::main(args.by_ref().skip(1))
        }
        Some("hook") => hook::main(args.by_ref().skip(1)),
        _ => cli::main(all),
    }
}

fn usage_error(usage: &str) -> ExitCode {
    eprintln!("folio: error: usage: {usage}");
    ExitCode::from(2)
}

/// folio's home: $FOLIO_HOME, else ~/.folio.
fn home() -> PathBuf {
    env::var_os("FOLIO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env::var_os("HOME").unwrap_or_default()).join(".folio"))
}

/// `s` quoted, for a message: as Rust writes a string's Debug, its specials escaped.
fn quoted(s: &str) -> String {
    format!("{s:?}")
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
