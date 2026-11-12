//! bottle-agent: runs commands for the host, listening on a vsock port in a VM (--vsock PORT), or
//! on a unix socket (--unix PATH) to try it out of one. As `bottle-agent init` it first stacks the
//! bottle's disks into its root and moves into it (see init.rs). As `bottle-agent unpack`, run
//! through it, it extracts a tar into the bottle (see unpack.rs).
//!
//! In a VM it's the init the kernel starts, as /init.krun (where libkrun's own init would be):
//! then it mounts what an init mounts before anything, and powers the VM off when it's done,
//! rather than exit, which would panic the kernel.

use std::process::ExitCode;

use bottle_agent::init;
use bottle_agent::reaper::Reaper;
use bottle_agent::socket::Listener;

fn main() -> ExitCode {
    let booted = std::env::args()
        .next()
        .is_some_and(|a| a.ends_with("init.krun"));
    if booted {
        bottle_agent::BOOTED.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = init::early() {
            eprintln!("bottle-agent: {e}");
            bottle_agent::end(1);
        }
    }
    let code = run();
    if booted {
        bottle_agent::end(1);
    }
    code
}

fn run() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("unpack") {
        return match bottle_agent::unpack::main(&args[1..]) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("bottle-agent unpack: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let mut upper = None;
    if args.first().map(String::as_str) == Some("init") {
        args.remove(0);
        let stack = match init::parse(&mut args) {
            Ok(stack) => stack,
            Err(e) => {
                eprintln!("bottle-agent init: {e}");
                return usage();
            }
        };
        match init::stack(&stack) {
            Ok(disk) => upper = Some(disk),
            Err(e) => {
                eprintln!("bottle-agent init: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    let listener = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["--vsock", port] => match port.parse() {
            Ok(port) => Listener::vsock(port),
            Err(_) => return usage(),
        },
        ["--unix", path] => Listener::unix(path.as_ref()),
        _ => return usage(),
    };
    let listener = match listener {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("bottle-agent: listening: {e}");
            return ExitCode::FAILURE;
        }
    };
    // Not PID 1 (out of a VM): orphans of the commands run are still this process's to reap.
    // SAFETY: getpid and prctl have no preconditions.
    unsafe {
        if libc::getpid() != 1 {
            libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1);
        }
    }
    match bottle_agent::serve(listener, Reaper::start(), upper) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("bottle-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage: bottle-agent [init [--lower DISK]... --upper DISK [--hostname NAME] [--config FILE]]"
    );
    eprintln!("                    --vsock PORT | --unix PATH");
    eprintln!("       bottle-agent unpack DIR [--oci]");
    ExitCode::from(2)
}
