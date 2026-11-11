//! `bottle-agent init`: what the agent does first in a bottle's VM, run by libkrun's own init
//! (init.krun), which has mounted /dev, /proc and /sys on a root of libkrun's, in memory and
//! read-only. It stacks the bottle's disks into the root the bottle runs in, and moves into it:
//!
//! - each stage disk, read-only, bottom first (see bottle-disk);
//! - the bottle's own disk, read-write: its `rootfs/` overlayfs's upper directory over the stages'
//!   `rootfs/`s, its `work/` overlayfs's work directory.
//!
//! A disk is named by its virtio serial (the block id libkrun was given for it), or by a path to
//! its device. The upper disk's `rootfs/` is laid out as a stage disk's is: the upper of a build
//! is a stage.

use std::ffi::CString;
use std::fs;
use std::io;
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
            _ => rest.push(arg),
        }
    }
    stack.upper = upper.ok_or("no --upper")?;
    *args = rest;
    Ok(stack)
}

/// Stacks the disks and moves into them: after this, `/` is the bottle's root.
pub fn stack(stack: &Stack) -> io::Result<()> {
    mount("tmpfs", RUN, "tmpfs", 0, "mode=0755")?;
    let mut lowers = Vec::new();
    for (i, disk) in stack.lowers.iter().enumerate() {
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
    move_into(&root)?;
    mount_system()?;
    if let Some(name) = &stack.hostname {
        // SAFETY: sethostname reads `len` bytes of `name`.
        if unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) } < 0 {
            return Err(failed("sethostname", io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Makes `root` the root, of this process and of all whose root was the old one (init.krun too),
/// and lets go of the old one, whose mounts the overlay keeps what it needs of.
fn move_into(root: &str) -> io::Result<()> {
    // pivot_root refuses a shared mount, and init.krun makes / shared.
    mount_raw(None, "/", None, libc::MS_REC | libc::MS_PRIVATE, None)?;
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
    // Shared again, as init.krun had it, for what runs containers in the bottle.
    mount_raw(None, "/", None, libc::MS_REC | libc::MS_SHARED, None)
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
