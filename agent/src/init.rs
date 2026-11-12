//! `bottle-agent init`: what the agent does first in a bottle's VM, run by libkrun's own init
//! (init.krun), which has mounted /dev, /proc and /sys on a root of libkrun's, in memory and
//! read-only. It stacks the bottle's disks into the root the bottle runs in, and moves into it:
//!
//! - each stage disk, read-only, bottom first (see bottle-disk);
//! - the bottle's own disk, read-write: its `rootfs/` overlayfs's upper directory over the stages'
//!   `rootfs/`s, its `work/` overlayfs's work directory.
//!
//! A disk is named by its virtio serial (the block id libkrun was given for it), or by a path to
//! its device; a lower may also be a directory, used as it is (out of a VM, to try this on). With
//! `--config FILE`, the bottle's environment, read-only mounts, network and what its image runs come
//! too (see Config). The upper disk's `rootfs/` is laid out as a stage disk's is: the upper of a build
//! is a stage.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// Where the disks are mounted, and stacked, before the move: a tmpfs of the agent's own, the root
/// being read-only. Short paths: overlayfs takes only so many characters of lowerdir.
const RUN: &str = "/run";

/// overlayfs's options but its directories. No index, metacopy or redirects: an upper is all of
/// what changed, by itself, as a stage has to be.
const OVERLAY: &str = "index=off,metacopy=off,redirect_dir=off";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stack {
    /// The stage disks, bottom first.
    pub lowers: Vec<String>,
    pub upper: String,
    pub hostname: Option<String>,
    /// The bottle's environment and mounts, from --config's file.
    pub config: Config,
}

/// What a bottle is given past its disks, as a file (`--config`), which libkrun serves from memory
/// in its first root: the kernel's command line has room for only so much.
#[derive(Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Set for everything run in the bottle, under what a command is given itself.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// The bottle's network; none if None.
    #[serde(default)]
    pub net: Option<crate::net::Config>,
    /// What its image runs, its entrypoint and command: started once the bottle's up, in the
    /// background, as root, in /.
    #[serde(default)]
    pub run: Option<Vec<String>>,
}

/// A directory of the host's, read-only in the bottle at `target`.
#[derive(Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// Its virtio-fs tag; or, out of a VM, a directory, bound.
    pub source: String,
    pub target: String,
}

/// Init's options, taken from `args`; what's left there is how to serve.
pub fn parse(args: &mut Vec<String>) -> Result<Stack, String> {
    let mut stack = Stack::default();
    let mut upper = None;
    let mut rest = Vec::new();
    let mut given = std::mem::take(args).into_iter();
    while let Some(arg) = given.next() {
        let mut value = || given.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--lower" => stack.lowers.push(value()?),
            "--upper" => upper = Some(value()?),
            "--hostname" => stack.hostname = Some(value()?),
            "--config" => {
                let path = value()?;
                let json = fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
                stack.config = serde_json::from_slice(&json).map_err(|e| format!("{path}: {e}"))?;
            }
            _ => rest.push(arg),
        }
    }
    stack.upper = upper.ok_or("no --upper")?;
    *args = rest;
    Ok(stack)
}

/// What an init does before anything, as the VM's (see main): /dev, /proc and /sys mounted,
/// which finding the disks takes, and loopback up.
pub fn early() -> io::Result<()> {
    for (source, target, fstype) in [
        ("devtmpfs", "/dev", "devtmpfs"),
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
    ] {
        // Mounted already is as good: the kernel mounts devtmpfs itself (DEVTMPFS_MOUNT).
        match mount_raw(Some(source), target, Some(fstype), 0, None) {
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {}
            other => other.map_err(|e| failed(&format!("mounting {target}"), e))?,
        }
    }
    crate::net::loopback_up()
}

/// Stacks the disks and moves into them: after this, `/` is the bottle's root. Returns the
/// bottle's own disk's root, open, for a Stop to make clean: unreachable by path once moved.
pub fn stack(stack: &Stack) -> io::Result<fs::File> {
    mount("tmpfs", RUN, "tmpfs", 0, "mode=0755")?;
    let mut lowers = Vec::new();
    for (i, disk) in stack.lowers.iter().enumerate() {
        // A directory, out of a VM, is a lower as it is: a host's own root, to try this on.
        if Path::new(disk).is_dir() {
            lowers.push(disk.clone());
            continue;
        }
        let dir = format!("{RUN}/{i}");
        fs::create_dir(&dir)?;
        mount(&device(disk)?, &dir, "ext4", libc::MS_RDONLY, "")?;
        lowers.push(format!("{dir}/rootfs"));
    }
    if lowers.is_empty() {
        // A stage made from nothing, the first of an image: overlayfs still wants a lower.
        let empty = format!("{RUN}/empty");
        fs::create_dir(&empty)?;
        lowers.push(empty);
    }
    let rw = format!("{RUN}/rw");
    fs::create_dir(&rw)?;
    mount(&device(&stack.upper)?, &rw, "ext4", 0, "")?;
    for dir in ["rootfs", "work"] {
        fs::create_dir_all(format!("{rw}/{dir}"))?;
    }
    // overlayfs's lowerdir is top first.
    lowers.reverse();
    let root = format!("{RUN}/root");
    fs::create_dir(&root)?;
    mount(
        "overlay",
        &root,
        "overlay",
        0,
        &format!(
            "lowerdir={},upperdir={rw}/rootfs,workdir={rw}/work,{OVERLAY}",
            lowers.join(":")
        ),
    )?;
    // The mounts, in the new root before the move: a directory to bind may be on a mount of its
    // own, which the old root takes with it. Those under /run after, on its tmpfs.
    let (under_run, elsewhere): (Vec<&Mount>, Vec<&Mount>) = stack
        .config
        .mounts
        .iter()
        .partition(|m| m.target == "/run" || m.target.starts_with("/run/"));
    for m in elsewhere {
        mount_share(m, &format!("{root}/{}", m.target.trim_start_matches('/')))?;
    }
    let upper = move_into(&root, &rw)?;
    mount_system()?;
    for m in under_run {
        mount_share(m, &m.target)?;
    }
    for (name, value) in &stack.config.env {
        // SAFETY: before any thread: the reaper and the connections start after init.
        unsafe { std::env::set_var(name, value) };
    }
    if let Some(net) = &stack.config.net {
        crate::net::start(net).map_err(|e| failed("the network", e))?;
    }
    if let Some(name) = &stack.hostname {
        // SAFETY: sethostname reads `len` bytes of `name`.
        if unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) } < 0 {
            return Err(failed("sethostname", io::Error::last_os_error()));
        }
        name_in_hosts(Path::new("/etc/hosts"), name).map_err(|e| failed("/etc/hosts", e))?;
    }
    if let Some(argv) = stack.config.run.as_deref().filter(|a| !a.is_empty()) {
        run(argv);
    }
    Ok(upper)
}

/// `name`, this machine's, in `hosts`, as Debian has it (127.0.1.1), unless it's there already:
/// sudo, for one, looks it up, and waits on DNS and complains if it isn't found.
fn name_in_hosts(hosts: &Path, name: &str) -> io::Result<()> {
    let text = match fs::read_to_string(hosts) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let named = text.lines().any(|line| {
        let line = line.split('#').next().unwrap_or_default();
        line.split_whitespace().skip(1).any(|n| n == name)
    });
    if named {
        return Ok(());
    }
    let gap = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(hosts, format!("{text}{gap}127.0.1.1\t{name}\n"))
}

/// Starts the image's entrypoint and command, in a session of its own: its output to the console,
/// its end its own (the reaper reaps it); a failure to start it is said there, not fatal.
fn run(argv: &[String]) {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .current_dir("/")
        .stdin(std::process::Stdio::null());
    // SAFETY: setsid between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    if let Err(e) = command.spawn() {
        eprintln!("bottle-agent init: running {argv:?}: {e}");
    }
}

/// Mounts `m` at `target`, read-only: a virtio-fs tag, or a directory (out of a VM), bound.
fn mount_share(m: &Mount, target: &str) -> io::Result<()> {
    fs::create_dir_all(target)?;
    if m.source.starts_with('/') {
        mount_raw(Some(&m.source), target, None, libc::MS_BIND, None)?;
        let ro = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY;
        mount_raw(None, target, None, ro, None)
    } else {
        mount(&m.source, target, "virtiofs", libc::MS_RDONLY, "")
    }
}

/// Makes `root` the root, of this process and of all whose root was the old one (init.krun too),
/// and lets go of the old one, whose mounts the overlay keeps what it needs of. The bottle's own
/// disk, at `rw`, comes along, to /run, where it's covered (see mount_system) but still there for
/// a Stop to make read-only: what's returned is it, open.
fn move_into(root: &str, rw: &str) -> io::Result<fs::File> {
    // pivot_root refuses a shared mount, and init.krun makes / shared.
    mount_raw(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;
    let run = format!("{root}/run");
    fs::create_dir_all(&run)?;
    mount_raw(Some(rw), &run, None, libc::MS_MOVE, None)
        .map_err(|e| failed("moving the bottle's disk", e))?;
    std::env::set_current_dir(root)?;
    // The old root is put on top of the new, then detached: pivot_root(".", ".").
    let dot = c".";
    // SAFETY: pivot_root and umount2 take paths, given as C strings.
    unsafe {
        if libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) < 0 {
            return Err(failed("pivot_root", io::Error::last_os_error()));
        }
        if libc::umount2(dot.as_ptr(), libc::MNT_DETACH) < 0 {
            return Err(failed("detaching the old root", io::Error::last_os_error()));
        }
    }
    std::env::set_current_dir("/")?;
    let upper = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open("/run")?;
    // Shared again, as init.krun had it, for what runs containers in the bottle.
    mount_raw(None, "/", None, libc::MS_REC | libc::MS_SHARED, None)?;
    Ok(upper)
}

/// /proc, /sys, /dev and the rest, again, in the bottle's root.
fn mount_system() -> io::Result<()> {
    let quiet = libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOSUID | libc::MS_RELATIME;
    let mounts: [(&str, &str, &str, libc::c_ulong, &str); 7] = [
        ("proc", "/proc", "proc", quiet, ""),
        ("sysfs", "/sys", "sysfs", quiet, ""),
        ("cgroup2", "/sys/fs/cgroup", "cgroup2", quiet, ""),
        ("devtmpfs", "/dev", "devtmpfs", libc::MS_RELATIME, ""),
        (
            "devpts",
            "/dev/pts",
            "devpts",
            libc::MS_NOEXEC | libc::MS_NOSUID | libc::MS_RELATIME,
            "gid=5,mode=620,ptmxmode=666",
        ),
        (
            "tmpfs",
            "/dev/shm",
            "tmpfs",
            libc::MS_NOEXEC | libc::MS_NOSUID | libc::MS_RELATIME,
            "",
        ),
        (
            "tmpfs",
            "/run",
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            "mode=0755",
        ),
    ];
    for (source, target, fstype, flags, data) in mounts {
        fs::create_dir_all(target)?;
        mount(source, target, fstype, flags, data)?;
    }
    let _ = std::os::unix::fs::symlink("/proc/self/fd", "/dev/fd");
    Ok(())
}

/// The device of `disk`: a path as it is, else the virtio disk whose serial it is.
fn device(disk: &str) -> io::Result<String> {
    if disk.starts_with('/') {
        return Ok(disk.into());
    }
    for entry in fs::read_dir("/sys/block")? {
        let entry = entry?;
        let Ok(serial) = fs::read(entry.path().join("serial")) else {
            continue;
        };
        let serial: &[u8] = serial
            .split(|&b| b == 0 || b == b'\n')
            .next()
            .unwrap_or(&[]);
        if serial == disk.as_bytes() {
            return Ok(Path::new("/dev")
                .join(entry.file_name())
                .to_string_lossy()
                .into_owned());
        }
    }
    Err(io::Error::other(format!("no disk {disk}")))
}

fn mount(
    source: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: &str,
) -> io::Result<()> {
    mount_raw(Some(source), target, Some(fstype), flags, Some(data))
        .map_err(|e| io::Error::other(format!("mounting {source} on {target} ({fstype}): {e}")))
}

fn mount_raw(
    source: Option<&str>,
    target: &str,
    fstype: Option<&str>,
    flags: libc::c_ulong,
    data: Option<&str>,
) -> io::Result<()> {
    let c = |s: &str| CString::new(s).map_err(|_| io::Error::other(format!("{s:?}: a NUL")));
    let source = source.map(c).transpose()?;
    let target_c = c(target)?;
    let fstype = fstype.map(c).transpose()?;
    let data = data.map(c).transpose()?;
    let ptr = |s: &Option<CString>| s.as_ref().map_or(std::ptr::null(), |s| s.as_ptr());
    // SAFETY: mount reads the C strings given, or takes NULL for those not.
    let ret = unsafe {
        libc::mount(
            ptr(&source),
            target_c.as_ptr(),
            ptr(&fstype),
            flags,
            ptr(&data).cast(),
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn failed(what: &str, e: io::Error) -> io::Error {
    io::Error::other(format!("{what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(String::from).collect()
    }

    #[test]
    fn the_hostname_is_put_in_etc_hosts_once() {
        let dir = std::env::temp_dir().join(format!("bottle-hosts-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let hosts = dir.join("hosts");
        name_in_hosts(&hosts, "b1").unwrap();
        assert_eq!(fs::read_to_string(&hosts).unwrap(), "127.0.1.1\tb1\n");
        fs::write(
            &hosts,
            "127.0.0.1 localhost\n# 1.2.3.4 b2\n192.0.2.1 bottle.host",
        )
        .unwrap();
        name_in_hosts(&hosts, "b2").unwrap();
        name_in_hosts(&hosts, "b2").unwrap();
        assert_eq!(
            fs::read_to_string(&hosts).unwrap(),
            "127.0.0.1 localhost\n# 1.2.3.4 b2\n192.0.2.1 bottle.host\n127.0.1.1\tb2\n"
        );
        name_in_hosts(&hosts, "localhost").unwrap();
        assert!(!fs::read_to_string(&hosts).unwrap().contains("\tlocalhost"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn takes_its_options_and_leaves_how_to_serve() {
        let mut a = args("--lower base --vsock 1024 --lower feat --upper rw --hostname b1");
        let stack = parse(&mut a).unwrap();
        assert_eq!(stack.lowers, ["base", "feat"]);
        assert_eq!(stack.upper, "rw");
        assert_eq!(stack.hostname.as_deref(), Some("b1"));
        assert_eq!(a, ["--vsock", "1024"]);
    }

    #[test]
    fn needs_an_upper() {
        assert_eq!(parse(&mut args("--lower base")).unwrap_err(), "no --upper");
        assert_eq!(
            parse(&mut args("--upper")).unwrap_err(),
            "--upper needs a value"
        );
    }
}
