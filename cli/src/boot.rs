//! `bottle-cli boot`: boots a bottle's VM on libkrun, in this process, until the VM ends; the
//! guest's console is this process's output.
//!
//!     bottle-cli boot SOCKET UPPER [DISK]... [--cpus N] [--mem MIB] [--size GIB]
//!                     [--hostname NAME] [--console FILE]
//!
//! DISKs are the bottle's stage disks, bottom first, as `bottle-cli disk` prints them; UPPER is its
//! own, read-write, made (--size GIB, sparse) if it isn't there. SOCKET is where the agent is
//! reached, for `bottle-cli exec`.
//!
//! The VM's first root is libkrun's, in memory: libkrun's init, the mount points it and the agent
//! need, and the agent, a Linux build of bottle-agent ($BOTTLE_GUEST_AGENT, else `bottle-agent`
//! beside this program), which stacks the disks and moves into them (bottle-agent init). libkrun is
//! $BOTTLE_LIBKRUN, else beside this program, else wherever the dynamic loader finds it, and
//! libkrunfw, the kernel, is found beside it; on macOS this program needs the hypervisor
//! entitlement (cli/hypervisor.entitlements).

use std::env;
use std::ffi::{CString, c_char};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::FAILED;
use crate::krun::{self, Krun, check};

pub const USAGE: &str = "boot SOCKET UPPER [DISK]... [--cpus N] [--mem MIB] [--size GIB] \
                         [--hostname NAME] [--console FILE]";

/// The vsock port the agent listens on.
const AGENT_PORT: u32 = 1024;

/// What the agent runs commands with, before a command's own environment.
const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The mount points libkrun's init and the agent need in the first root, which is read-only.
const MOUNT_POINTS: [&std::ffi::CStr; 4] = [c"dev", c"proc", c"sys", c"run"];

#[derive(Debug, PartialEq, Eq)]
struct Args {
    socket: PathBuf,
    upper: PathBuf,
    disks: Vec<PathBuf>,
    cpus: u8,
    mem_mib: u32,
    size_gib: u64,
    hostname: Option<String>,
    console: Option<PathBuf>,
}

fn number<T: std::str::FromStr>(flag: &str, v: String) -> Result<T, String> {
    v.parse().map_err(|_| format!("{flag} {v}: not a number"))
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut positional = Vec::new();
    let (mut cpus, mut mem_mib, mut size_gib) = (2, 2048, 64);
    let (mut hostname, mut console) = (None, None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "--cpus" => cpus = number("--cpus", value("--cpus")?)?,
            "--mem" => mem_mib = number("--mem", value("--mem")?)?,
            "--size" => size_gib = number("--size", value("--size")?)?,
            "--hostname" => hostname = Some(value("--hostname")?),
            "--console" => console = Some(value("--console")?.into()),
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    let mut positional = positional.into_iter();
    let socket = positional.next().ok_or("no SOCKET")?;
    let upper = positional.next().ok_or("no UPPER")?;
    Ok(Args {
        socket,
        upper,
        disks: positional.collect(),
        cpus,
        mem_mib,
        size_gib,
        hostname,
        console,
    })
}

/// Runs `bottle-cli boot` with `args`, those after `boot`. Returns only if the VM couldn't start:
/// once it has, its end is this process's, with the guest's exit status.
pub fn main(args: impl Iterator<Item = String>) -> ExitCode {
    let args = match parse(args) {
        Ok(args) => args,
        Err(e) => return crate::usage_error(&e),
    };
    let Err(e) = boot(&args);
    eprintln!("bottle-cli: {e}");
    ExitCode::from(FAILED)
}

fn boot(args: &Args) -> io::Result<std::convert::Infallible> {
    let libkrun = krun::locate();
    #[cfg(target_os = "macos")]
    libkrunfw_beside(&libkrun)?;
    let agent = fs::read(guest_agent()?)?;
    // libkrun serves the agent from this memory for as long as the VM runs, which is as long as
    // this process does.
    let agent: &'static [u8] = Box::leak(agent.into_boxed_slice());
    let mke2fs = crate::disk::mke2fs()?;
    bottle_disk::mke2fs::empty(&mke2fs, &args.upper, args.size_gib << 30)
        .map_err(|e| io::Error::other(format!("making {}: {e}", args.upper.display())))?;
    let krun = Krun::load(&libkrun)?;

    let c = |s: &str| CString::new(s).map_err(|_| io::Error::other(format!("{s:?}: a NUL")));
    let path = |p: &Path| -> io::Result<CString> {
        let p =
            fs::canonicalize(p).map_err(|e| io::Error::other(format!("{}: {e}", p.display())))?;
        c(&p.to_string_lossy())
    };
    let _ = fs::remove_file(&args.socket);
    let socket = c(&args.socket.to_string_lossy())?;
    let disks = args
        .disks
        .iter()
        .map(|d| path(d))
        .collect::<io::Result<Vec<_>>>()?;
    let upper = path(&args.upper)?;
    let ids: Vec<CString> = (0..disks.len())
        .map(|i| c(&format!("s{i}")))
        .collect::<io::Result<_>>()?;
    let mut init = vec!["init".to_string()];
    for i in 0..disks.len() {
        init.extend(["--lower".into(), format!("s{i}")]);
    }
    init.extend(["--upper".into(), "rw".into()]);
    if let Some(name) = &args.hostname {
        init.extend(["--hostname".into(), name.clone()]);
    }
    init.extend(["--vsock".into(), AGENT_PORT.to_string()]);
    let init: Vec<CString> = init.iter().map(|a| c(a)).collect::<io::Result<_>>()?;
    let argv = nul_terminated(&init);
    let env = [c(PATH)?];
    let envp = nul_terminated(&env);
    let console = args
        .console
        .as_deref()
        .map(|p| c(&p.to_string_lossy()))
        .transpose()?;

    // SAFETY: libkrun's functions, called as libkrun.h says, with C strings and arrays that live
    // until krun_start_enter, which doesn't return once the VM has started.
    unsafe {
        let level = env::var("BOTTLE_KRUN_LOG")
            .ok()
            .and_then(|l| l.parse().ok())
            .unwrap_or(1);
        (krun.set_log_level)(level);
        let ctx = check("creating libkrun's context", (krun.create_ctx)())? as u32;
        check(
            "setting cpus and memory",
            (krun.set_vm_config)(ctx, args.cpus, args.mem_mib),
        )?;
        // Memory the guest frees goes back to the host, really, on macOS too.
        check(
            "setting free page reporting",
            (krun.set_balloon_free_page_reporting_mode)(
                ctx,
                krun::FREE_PAGE_REPORTING_MODE_REPLACE,
            ),
        )?;
        // vsock for the agent alone: no TSI, which would give the guest sockets of the host's.
        check(
            "disabling the implicit vsock",
            (krun.disable_implicit_vsock)(ctx),
        )?;
        check("adding vsock", (krun.add_vsock)(ctx, 0))?;
        check(
            "adding the agent's port",
            (krun.add_vsock_port2)(ctx, AGENT_PORT, socket.as_ptr(), true),
        )?;
        // The first root: no host directory, read-only, libkrun's init added to it by libkrun.
        check(
            "adding the root",
            (krun.add_virtiofs3)(ctx, krun::ROOT_TAG.as_ptr(), std::ptr::null(), 0, true),
        )?;
        for dir in MOUNT_POINTS {
            check(
                &format!("adding /{}", dir.to_string_lossy()),
                (krun.fs_add_overlay_dir)(ctx, krun::ROOT_TAG.as_ptr(), dir.as_ptr(), 0o040755),
            )?;
        }
        check(
            "adding the agent",
            (krun.fs_add_overlay_file)(
                ctx,
                krun::ROOT_TAG.as_ptr(),
                c"bottle-agent".as_ptr(),
                agent.as_ptr(),
                agent.len(),
                0o100755,
                false,
            ),
        )?;
        for (id, disk) in ids.iter().zip(&disks) {
            check(
                "adding a stage disk",
                (krun.add_disk2)(ctx, id.as_ptr(), disk.as_ptr(), krun::DISK_FORMAT_RAW, true),
            )?;
        }
        check(
            "adding the bottle's disk",
            (krun.add_disk2)(
                ctx,
                c"rw".as_ptr(),
                upper.as_ptr(),
                krun::DISK_FORMAT_RAW,
                false,
            ),
        )?;
        check(
            "setting what the VM runs",
            (krun.set_exec)(ctx, c"/bottle-agent".as_ptr(), argv.as_ptr(), envp.as_ptr()),
        )?;
        if let Some(console) = &console {
            check(
                "setting the console",
                (krun.set_console_output)(ctx, console.as_ptr()),
            )?;
        }
        check("starting the VM", (krun.start_enter)(ctx))?;
    }
    Err(io::Error::other("libkrun returned from starting the VM"))
}

/// Makes sure libkrun finds libkrunfw beside it, when it's there. libkrun loads it by its name
/// alone, which macOS looks for only on DYLD_LIBRARY_PATH: read once, as a program starts, and
/// dropped on the way through protected programs, /bin/bash among them. So this program starts
/// again, with libkrun's directory first on it, unless it's there already.
#[cfg(target_os = "macos")]
fn libkrunfw_beside(libkrun: &Path) -> io::Result<()> {
    use std::os::unix::process::CommandExt;

    let Some(dir) = libkrun.parent().filter(|d| !d.as_os_str().is_empty()) else {
        return Ok(());
    };
    if !dir.join("libkrunfw.5.dylib").exists() {
        return Ok(());
    }
    let current = env::var_os("DYLD_LIBRARY_PATH").unwrap_or_default();
    if env::split_paths(&current).any(|p| p == dir) {
        return Ok(());
    }
    let paths = std::iter::once(dir.to_path_buf()).chain(env::split_paths(&current));
    let path = env::join_paths(paths).map_err(io::Error::other)?;
    let err = std::process::Command::new(env::current_exe()?)
        .args(env::args_os().skip(1))
        .env("DYLD_LIBRARY_PATH", path)
        .exec();
    Err(io::Error::other(format!(
        "starting again for libkrunfw: {err}"
    )))
}

/// Pointers to `strings`, then NULL, as C wants an array of strings.
fn nul_terminated(strings: &[CString]) -> Vec<*const c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

/// The agent the VM runs: $BOTTLE_GUEST_AGENT, else `bottle-agent` beside this program.
fn guest_agent() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("BOTTLE_GUEST_AGENT") {
        return Ok(path.into());
    }
    let beside = env::current_exe()?.with_file_name("bottle-agent");
    if beside.exists() {
        return Ok(beside);
    }
    Err(io::Error::other(format!(
        "no agent for the VM: not at {}, and no $BOTTLE_GUEST_AGENT",
        beside.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Result<Args, String> {
        parse(line.split(' ').map(String::from))
    }

    #[test]
    fn parses_positionals_and_options_in_any_order() {
        let a = args("/s --cpus 4 /rw /d0 --mem 4096 /d1 --hostname b --size 8").unwrap();
        assert_eq!(a.socket, PathBuf::from("/s"));
        assert_eq!(a.upper, PathBuf::from("/rw"));
        assert_eq!(a.disks, [PathBuf::from("/d0"), PathBuf::from("/d1")]);
        assert_eq!((a.cpus, a.mem_mib, a.size_gib), (4, 4096, 8));
        assert_eq!(a.hostname.as_deref(), Some("b"));
        assert_eq!(a.console, None);
    }

    #[test]
    fn refuses_what_it_cant_boot() {
        assert_eq!(args("/s").unwrap_err(), "no UPPER");
        assert_eq!(
            args("/s /rw --cpus x").unwrap_err(),
            "--cpus x: not a number"
        );
        assert_eq!(
            args("/s /rw --cpus 300").unwrap_err(),
            "--cpus 300: not a number"
        );
        assert_eq!(args("/s /rw --nope").unwrap_err(), "unknown option --nope");
    }
}
