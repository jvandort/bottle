//! `bottle-cli boot`: boots a bottle's VM on libkrun, in this process, until the VM ends; the
//! guest's console is this process's output.
//!
//!     bottle-cli boot SOCKET UPPER [DISK]... [--cpus N] [--mem MIB] [--size GIB]
//!                     [--hostname NAME] [--console FILE] [--env NAME=VALUE]...
//!                     [--mount SOURCE:TARGET]... [--decider ADDRESS:PORT | --net-direct]
//!
//! DISKs are the bottle's stage disks, bottom first, as `bottle-cli disk` prints them; UPPER is its
//! own, read-write, made (--size GIB, sparse) if it isn't there. SOCKET is where the agent is
//! reached, for `bottle-cli exec`. Everything run in it gets --env's variables; each --mount is a
//! host directory, read-only at TARGET (virtio-fs). With --decider, the bottle has a network,
//! every connection it makes asked of bottled's decider there, as the bottle whose token is
//! $BOTTLE_DECIDER_TOKEN; with --net-direct, every connection allowed; without either, none.
//!
//! The VM's first root is libkrun's, in memory: libkrun's init, the mount points it and the agent
//! need, and the agent, a Linux build of bottle-agent ($BOTTLE_GUEST_AGENT, else the one built into
//! this program, else `bottle-agent` beside it), which stacks the disks and moves into them (bottle-agent init). libkrun is
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
                         [--hostname NAME] [--console FILE] [--env NAME=VALUE]... \
                         [--mount SOURCE:TARGET]... [--decider ADDRESS:PORT | --net-direct]";

/// The vsock port the agent listens on.
const AGENT_PORT: u32 = 1024;

/// What the agent runs commands with, before a command's own environment.
const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The bottle's environment and mounts, for the agent, in the first root.
const CONFIG: &std::ffi::CStr = c"bottle-init.json";

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
    env: Vec<(String, String)>,
    /// Host directories, read-only in the bottle at their targets.
    mounts: Vec<(PathBuf, String)>,
    net: Net,
}

/// The bottle's network: every connection asked of a decider (bottled's), every connection
/// allowed (for trying a machine out without bottled), or none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Net {
    Decider(std::net::SocketAddr),
    Direct,
    None,
}

impl Net {
    /// The network options of `boot`, `machine start` and `build`, taken from `arg` and what
    /// follows it, if they're one: Some(Ok(())) if taken.
    pub fn take(
        &mut self,
        arg: &str,
        mut next: impl FnMut() -> Option<String>,
    ) -> Option<Result<(), String>> {
        match arg {
            "--decider" => Some(
                next()
                    .ok_or("--decider needs ADDRESS:PORT".to_string())
                    .and_then(|a| {
                        a.parse()
                            .map_err(|_| format!("--decider {a}: not ADDRESS:PORT"))
                    })
                    .map(|a| *self = Net::Decider(a)),
            ),
            "--net-direct" => {
                *self = Net::Direct;
                Some(Ok(()))
            }
            _ => None,
        }
    }

    /// As options again, for a `boot` to come.
    pub fn args(&self) -> Vec<String> {
        match self {
            Net::Decider(a) => vec!["--decider".into(), a.to_string()],
            Net::Direct => vec!["--net-direct".into()],
            Net::None => Vec::new(),
        }
    }
}

fn number<T: std::str::FromStr>(flag: &str, v: String) -> Result<T, String> {
    v.parse().map_err(|_| format!("{flag} {v}: not a number"))
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut positional = Vec::new();
    let (mut cpus, mut mem_mib, mut size_gib) = (2, 2048, 64);
    let (mut hostname, mut console) = (None, None);
    let (mut env, mut mounts) = (Vec::new(), Vec::new());
    let mut net = Net::None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if let Some(taken) = net.take(&arg, || args.next()) {
            taken?;
            continue;
        }
        let mut value = |flag: &str| args.next().ok_or(format!("{flag} needs a value"));
        match arg.as_str() {
            "--cpus" => cpus = number("--cpus", value("--cpus")?)?,
            "--mem" => mem_mib = number("--mem", value("--mem")?)?,
            "--size" => size_gib = number("--size", value("--size")?)?,
            "--hostname" => hostname = Some(value("--hostname")?),
            "--console" => console = Some(value("--console")?.into()),
            "--env" => {
                let pair = value("--env")?;
                let (name, val) = pair
                    .split_once('=')
                    .ok_or(format!("--env {pair}: not NAME=VALUE"))?;
                env.push((name.into(), val.into()));
            }
            "--mount" => {
                let pair = value("--mount")?;
                let (source, target) = pair
                    .rsplit_once(':')
                    .ok_or(format!("--mount {pair}: not SOURCE:TARGET"))?;
                mounts.push((source.into(), target.into()));
            }
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
        env,
        mounts,
        net,
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
    let agent = guest_agent()?;
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
    // The bottle's network: served here, on a unix socket libkrun carries the agent's vsock
    // connections to (see bottle-agent-client's net).
    let net_socket = match &args.net {
        Net::None => None,
        net => {
            let decider = match net {
                Net::Decider(address) => Some(decider(*address)?),
                _ => None,
            };
            let path = net_socket_beside(&args.socket);
            let _ = fs::remove_file(&path);
            let listener = std::os::unix::net::UnixListener::bind(&path)
                .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
            std::thread::spawn(move || bottle_agent_client::net::serve(listener, decider));
            Some(c(&path.to_string_lossy())?)
        }
    };
    // The bottle's environment, mounts and network, as a file in the first root (see
    // bottle-agent init).
    let mut config = serde_json::json!({
        "env": args.env,
        "mounts": args.mounts.iter().enumerate()
            .map(|(i, (_, target))| serde_json::json!({"source": format!("m{i}"), "target": target}))
            .collect::<Vec<_>>(),
    });
    if net_socket.is_some() {
        config["net"] = serde_json::json!({"address": ADDRESS, "host": "vsock"});
    }
    let config: &'static [u8] = Box::leak(config.to_string().into_bytes().into_boxed_slice());
    init.extend(["--config".into(), format!("/{}", CONFIG.to_string_lossy())]);
    init.extend(["--vsock".into(), AGENT_PORT.to_string()]);
    let mounts = args
        .mounts
        .iter()
        .enumerate()
        .map(|(i, (source, _))| Ok((c(&format!("m{i}"))?, path(source)?)))
        .collect::<io::Result<Vec<_>>>()?;
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
        if let Some(net_socket) = &net_socket {
            // The guest connects; libkrun connects here for it.
            check(
                "adding the network's port",
                (krun.add_vsock_port2)(
                    ctx,
                    bottle_agent_client::net::PORT,
                    net_socket.as_ptr(),
                    false,
                ),
            )?;
        }
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
        check(
            "adding the bottle's config",
            (krun.fs_add_overlay_file)(
                ctx,
                krun::ROOT_TAG.as_ptr(),
                CONFIG.as_ptr(),
                config.as_ptr(),
                config.len(),
                0o100644,
                false,
            ),
        )?;
        for (tag, source) in &mounts {
            check(
                "adding a mount",
                (krun.add_virtiofs3)(ctx, tag.as_ptr(), source.as_ptr(), 0, true),
            )?;
        }
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

/// The bottle's address, on an interface of its own with nothing behind it: what leaves it is the
/// agent's to carry (see bottle-agent's net). smolvm's, as the bottle had it.
const ADDRESS: &str = "100.96.0.2/30";

/// The unix socket the bottle's network is served on: beside the agent's, unless that makes too
/// long a path for one (104 bytes, on macOS).
fn net_socket_beside(socket: &Path) -> PathBuf {
    let mut beside = socket.as_os_str().to_owned();
    beside.push(".net");
    if beside.len() < 100 {
        return beside.into();
    }
    env::temp_dir().join(format!("bottle-net-{}.sock", std::process::id()))
}

/// The decider at `address`, asked as the bottle whose token is $BOTTLE_DECIDER_TOKEN (hex, not
/// an argument, which anyone can read in ps), as a machine of its own, this launch.
fn decider(address: std::net::SocketAddr) -> io::Result<bottle_agent_client::net::Decider> {
    let hex = env::var("BOTTLE_DECIDER_TOKEN").map_err(|_| {
        io::Error::other("--decider needs the bottle's token, in $BOTTLE_DECIDER_TOKEN")
    })?;
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| {
            hex.get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    let token: [u8; 32] = bytes
        .try_into()
        .map_err(|_| io::Error::other("$BOTTLE_DECIDER_TOKEN: not 32 bytes in hex"))?;
    let mut machine = [0; 16];
    fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut machine))?;
    Ok(bottle_agent_client::net::Decider {
        address,
        token,
        machine,
    })
}

/// Pointers to `strings`, then NULL, as C wants an array of strings.
fn nul_terminated(strings: &[CString]) -> Vec<*const c_char> {
    strings
        .iter()
        .map(|s| s.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

/// The agent bottle-cli was built with, if it was (see build.rs).
#[cfg(embedded_agent)]
const EMBEDDED_AGENT: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/agent")));
#[cfg(not(embedded_agent))]
const EMBEDDED_AGENT: Option<&[u8]> = None;

/// The agent the VM runs: $BOTTLE_GUEST_AGENT, else the one bottle-cli was built with, else
/// `bottle-agent` beside this program. libkrun serves it from this memory for as long as the VM
/// runs, which is as long as this process does.
fn guest_agent() -> io::Result<&'static [u8]> {
    let path = match env::var_os("BOTTLE_GUEST_AGENT") {
        Some(path) => PathBuf::from(path),
        None => {
            if let Some(agent) = EMBEDDED_AGENT {
                return Ok(agent);
            }
            let beside = env::current_exe()?.with_file_name("bottle-agent");
            if !beside.exists() {
                return Err(io::Error::other(format!(
                    "no agent for the VM: none built in, none at {}, and no $BOTTLE_GUEST_AGENT",
                    beside.display()
                )));
            }
            beside
        }
    };
    let agent =
        fs::read(&path).map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
    Ok(Box::leak(agent.into_boxed_slice()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Result<Args, String> {
        parse(line.split(' ').map(String::from))
    }

    #[test]
    fn parses_positionals_and_options_in_any_order() {
        let a = args(
            "/s --cpus 4 /rw /d0 --mem 4096 /d1 --hostname b --size 8 --env A=b=c --mount /h/x:/x",
        )
        .unwrap();
        assert_eq!(a.env, [("A".to_string(), "b=c".to_string())]);
        assert_eq!(a.mounts, [(PathBuf::from("/h/x"), "/x".to_string())]);
        assert_eq!(a.socket, PathBuf::from("/s"));
        assert_eq!(a.upper, PathBuf::from("/rw"));
        assert_eq!(a.disks, [PathBuf::from("/d0"), PathBuf::from("/d1")]);
        assert_eq!((a.cpus, a.mem_mib, a.size_gib), (4, 4096, 8));
        assert_eq!(a.hostname.as_deref(), Some("b"));
        assert_eq!(a.console, None);
    }

    #[test]
    fn the_network_is_a_decider_direct_or_none() {
        assert_eq!(args("/s /rw").unwrap().net, Net::None);
        assert_eq!(args("/s /rw --net-direct").unwrap().net, Net::Direct);
        let a = args("/s --decider 127.0.0.1:47329 /rw").unwrap();
        assert_eq!(a.net, Net::Decider("127.0.0.1:47329".parse().unwrap()));
        assert_eq!(a.net.args(), ["--decider", "127.0.0.1:47329"]);
        assert_eq!(
            args("/s /rw --decider nope").unwrap_err(),
            "--decider nope: not ADDRESS:PORT"
        );
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
