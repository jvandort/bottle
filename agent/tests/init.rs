//! `bottle-agent init`, unpack and Stop, out of a VM: the agent stacks this machine's own root (a
//! directory lower) under a fresh disk on a loop device, in PID, mount and UTS namespaces of its
//! own, and is driven over its unix socket, found through /proc. After its Stop, the disk must be
//! clean, and stack on the root as a stage would.
//!
//! Needs root, loop devices and mke2fs, so it's skipped otherwise; to run it, as root but built
//! as you:
//!
//!     CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test -p bottle-agent --test init
//!
//! (x86_64's variable, on x86_64.)

use std::fs;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use bottle_agent_proto::{Exit, Start, ToGuest, ToHost};

fn root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

fn run(command: &mut Command) -> String {
    let out = command.output().unwrap();
    assert!(
        out.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The agent in its namespaces, and what it's on: stopped, detached and removed when dropped.
struct Bottle {
    unshare: Child,
    agent_pid: u32,
    dir: PathBuf,
    upper: PathBuf,
    loop_dev: Option<String>,
}

impl Bottle {
    fn start() -> Bottle {
        // Short: overlayfs takes only so many characters of lowerdir.
        let dir = PathBuf::from(format!("/tmp/bai{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let upper = dir.join("rw.ext4");
        let mke2fs = std::env::var("BOTTLE_MKE2FS").unwrap_or("mke2fs".into());
        run(Command::new(mke2fs)
            .args(["-q", "-t", "ext4", "-E", "root_owner=0:0"])
            .arg(&upper)
            .arg("1G"));
        let loop_dev = run(Command::new("losetup").args(["-f", "--show"]).arg(&upper))
            .trim()
            .to_string();
        let unshare = Command::new("unshare")
            .args(["-p", "-f", "-m", "-u", "--propagation", "private"])
            .arg(env!("CARGO_BIN_EXE_bottle-agent"))
            .args([
                "init",
                "--lower",
                "/",
                "--upper",
                &loop_dev,
                "--hostname",
                "checked",
            ])
            .args(["--unix", "/run/agent.sock"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let mut bottle = Bottle {
            unshare,
            agent_pid: 0,
            dir,
            upper,
            loop_dev: Some(loop_dev),
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "the agent didn't listen");
            let children =
                fs::read_to_string(format!("/proc/{0}/task/{0}/children", bottle.unshare.id()))
                    .unwrap_or_default();
            if let Some(pid) = children.split_whitespace().next() {
                bottle.agent_pid = pid.parse().unwrap();
                if UnixStream::connect(bottle.socket()).is_ok() {
                    return bottle;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn socket(&self) -> PathBuf {
        PathBuf::from(format!("/proc/{}/root/run/agent.sock", self.agent_pid))
    }

    /// Runs `argv` in the bottle with `stdin`: its stdout and exit.
    fn exec(&self, argv: &[&str], stdin: &[u8]) -> (String, Exit) {
        let mut conn = UnixStream::connect(self.socket()).unwrap();
        ToGuest::Start(Start {
            argv: argv.iter().map(|a| a.to_string()).collect(),
            ..Start::default()
        })
        .write(&mut conn)
        .unwrap();
        for chunk in stdin.chunks(64 * 1024) {
            ToGuest::Stdin(chunk.to_vec()).write(&mut conn).unwrap();
        }
        ToGuest::StdinEof.write(&mut conn).unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        loop {
            match ToHost::read(&mut conn).unwrap() {
                Some(ToHost::Stdout(d)) => out.extend(d),
                Some(ToHost::Stderr(d)) => err.extend(d),
                Some(ToHost::Exit(exit)) => {
                    if exit != Exit::Code(0) {
                        eprintln!("{argv:?}: {}", String::from_utf8_lossy(&err));
                    }
                    return (String::from_utf8(out).unwrap(), exit);
                }
                Some(ToHost::Error(e)) => panic!("{argv:?}: {e}"),
                Some(ToHost::Started { .. }) => {}
                None => panic!("{argv:?}: the agent went away"),
            }
        }
    }

    fn ok(&self, argv: &[&str], stdin: &[u8]) -> String {
        let (out, exit) = self.exec(argv, stdin);
        assert_eq!(exit, Exit::Code(0), "{argv:?}");
        out
    }

    /// Stops it: the disk is clean after, and detached.
    fn stop(&mut self) {
        let mut conn = UnixStream::connect(self.socket()).unwrap();
        ToGuest::Stop.write(&mut conn).unwrap();
        assert_eq!(
            ToHost::read(&mut conn).unwrap(),
            Some(ToHost::Exit(Exit::Code(0)))
        );
        let status = self.unshare.wait().unwrap();
        assert!(status.success(), "{status}");
        if let Some(dev) = self.loop_dev.take() {
            run(Command::new("losetup").args(["-d", &dev]));
        }
    }
}

impl Drop for Bottle {
    fn drop(&mut self) {
        // The agent is its PID namespace's init: killing it ends everything in there.
        if self.agent_pid != 0 {
            // SAFETY: kill with a pid.
            unsafe { libc::kill(self.agent_pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = self.unshare.kill();
        let _ = self.unshare.wait();
        for m in ["st", "m"] {
            let _ = Command::new("umount").arg(self.dir.join(m)).status();
        }
        if let Some(dev) = self.loop_dev.take() {
            let _ = Command::new("losetup").args(["-d", &dev]).status();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A tar of a directory, a file, an executable, a symlink, a hard link, a fifo, a character device
/// and a file with an xattr, under x/.
fn files() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    use tar::EntryType::*;
    add(&mut b, "x", Directory, 0o750, b"", None);
    add(&mut b, "x/file", Regular, 0o644, b"hello\n", None);
    add(
        &mut b,
        "x/tool",
        Regular,
        0o4755,
        b"#!/bin/sh\necho tool\n",
        None,
    );
    add(&mut b, "x/sym", Symlink, 0o777, b"", Some("file"));
    add(&mut b, "x/hard", Link, 0o644, b"", Some("x/file"));
    add(&mut b, "x/fifo", Fifo, 0o600, b"", None);
    add(&mut b, "x/null", Char, 0o666, b"", None);
    b.append_pax_extensions([("SCHILY.xattr.user.bottle", &b"yes"[..])])
        .unwrap();
    add(&mut b, "x/tagged", Regular, 0o644, b"t\n", None);
    b.into_inner().unwrap()
}

fn add(
    b: &mut tar::Builder<Vec<u8>>,
    path: &str,
    kind: tar::EntryType,
    mode: u32,
    data: &[u8],
    link: Option<&str>,
) {
    let mut h = tar::Header::new_gnu();
    h.set_entry_type(kind);
    h.set_mode(mode);
    h.set_uid(1000);
    h.set_gid(1000);
    h.set_mtime(1_700_000_000);
    h.set_size(data.len() as u64);
    if kind == tar::EntryType::Char {
        h.set_device_major(1).unwrap();
        h.set_device_minor(3).unwrap();
    }
    match link {
        Some(target) => b.append_link(&mut h, path, target).unwrap(),
        None => b.append_data(&mut h, path, data).unwrap(),
    }
}

/// An OCI layer deleting y/a, and everything that was in z.
fn deletions() -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for path in ["y/.wh.a", "z/.wh..wh..opq", "z/new"] {
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o644);
        h.set_size(0);
        b.append_data(&mut h, path, &[][..]).unwrap();
    }
    b.into_inner().unwrap()
}

#[test]
fn stacks_unpacks_and_stops_clean() {
    if !root() {
        eprintln!("skipped: needs root (see the top of agent/tests/init.rs)");
        return;
    }
    let mut bottle = Bottle::start();
    assert_eq!(
        bottle.ok(&["cat", "/proc/sys/kernel/hostname"], b""),
        "checked\n"
    );
    let root_line = bottle.ok(&["sh", "-c", "grep ' / / ' /proc/self/mountinfo"], b"");
    assert!(root_line.contains(" - overlay "), "{root_line}");

    bottle.ok(&["/proc/self/exe", "unpack", "/"], &files());
    let listing = bottle.ok(
        &["sh", "-c", "cd /x && stat -c '%n %F %a %u:%g %Y' file tool sym hard fifo null && cat sym && stat -c %h file && stat -c %t,%T null"],
        b"",
    );
    assert_eq!(
        listing,
        "file regular file 644 1000:1000 1700000000\n\
         tool regular file 4755 1000:1000 1700000000\n\
         sym symbolic link 777 1000:1000 1700000000\n\
         hard regular file 644 1000:1000 1700000000\n\
         fifo fifo 600 1000:1000 1700000000\n\
         null character special file 666 1000:1000 1700000000\n\
         hello\n2\n1,3\n"
    );
    assert_eq!(
        bottle.ok(
            &[
                "getfattr",
                "--only-values",
                "-n",
                "user.bottle",
                "/x/tagged"
            ],
            b""
        ),
        "yes"
    );

    bottle.ok(
        &["sh", "-c", "mkdir -p /y /z && touch /y/a /y/b /z/old"],
        b"",
    );
    bottle.ok(&["/proc/self/exe", "unpack", "/", "--oci"], &deletions());
    assert_eq!(
        bottle.ok(&["sh", "-c", "ls /y /z"], b""),
        "/y:\nb\n\n/z:\nnew\n"
    );

    // Something still running when it stops is stopped.
    bottle.exec(&["sh", "-c", "sleep 1000 > /dev/null 2>&1 &"], b"");
    bottle.stop();

    let fsck = Command::new("e2fsck")
        .arg("-fn")
        .arg(&bottle.upper)
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stdout)
    );
    let header = run(Command::new("dumpe2fs").arg("-h").arg(&bottle.upper));
    assert!(
        header.contains("Filesystem state:         clean"),
        "{header}"
    );
    assert!(!header.contains("needs_recovery"), "{header}");

    // The disk, as a stage on the root: what was made is there, what was deleted isn't.
    let (m, st) = (bottle.dir.join("m"), bottle.dir.join("st"));
    fs::create_dir_all(&m).unwrap();
    fs::create_dir_all(&st).unwrap();
    run(Command::new("mount")
        .args(["-o", "ro,loop"])
        .arg(&bottle.upper)
        .arg(&m));
    run(Command::new("mount")
        .args(["-t", "overlay", "overlay", "-o"])
        .arg(format!("ro,lowerdir={}:/", m.join("rootfs").display()))
        .arg(&st));
    assert_eq!(fs::read_to_string(st.join("x/file")).unwrap(), "hello\n");
    assert!(!Path::new(&st.join("y/a")).exists());
    assert!(st.join("y/b").exists());
    assert!(!st.join("z/old").exists());
    assert!(st.join("z/new").exists());
}
