//! The agent in a bottle's VM: it runs commands for the host, one per connection (see
//! bottle-agent-proto), and, as PID 1, reaps every orphan in the VM. Linux only, as the VM is:
//! elsewhere it's empty.
#![cfg(target_os = "linux")]

pub mod init;
pub mod net;
pub mod reaper;
pub mod socket;
pub mod unpack;
pub mod user;

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

use bottle_agent_proto::{Exit, Start, ToGuest, ToHost, WinSize};

use crate::reaper::Reaper;
use crate::socket::{Listener, Socket};

/// Output is sent in pieces of at most this much.
const CHUNK: usize = 64 * 1024;

/// End-of-file at the start of a line, for a terminal: what closes its stdin.
const VEOF: u8 = 0x04;

/// Serves connections on `listener`, each on a thread of its own, until accepting fails, or a
/// Stop. `upper` is the bottle's own disk, made clean on a Stop, if there is one (see init).
pub fn serve(listener: Listener, reaper: Reaper, upper: Option<File>) -> io::Result<()> {
    let upper = Arc::new(upper);
    loop {
        let conn = match listener.accept() {
            Ok(conn) => conn,
            Err(e) if matches!(e.raw_os_error(), Some(libc::EINTR | libc::ECONNABORTED)) => {
                continue;
            }
            // Out of descriptors or memory for now (the bottle's network can use a lot of them at
            // once): it passes. Booted, serving ending would power the VM off.
            Err(e) if is_exhaustion(&e) => {
                thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(e) => return Err(e),
        };
        let reaper = reaper.clone();
        let upper = Arc::clone(&upper);
        thread::spawn(move || {
            if let Err(e) = handle(conn, &reaper, &upper) {
                crate::say!("bottle-agent: {e}");
            }
        });
    }
}

/// Whether `e` is running out of something that comes back: descriptors, memory, buffers.
pub fn is_exhaustion(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOMEM | libc::ENOBUFS)
    )
}

/// The most files the agent, and everything it starts, may have open: the kernel's default for
/// init, 1024, the bottle's network alone can use up (each connection and lookup in flight holds
/// some). Soft, what processes in the bottle inherit; hard, what they may raise it to.
const OPEN_FILES: (u64, u64) = (65_536, 1_048_576);

/// Raises the limit on open files to OPEN_FILES, as far as the kernel lets it.
pub fn raise_open_files() {
    use rustix::process::{Resource, Rlimit, setrlimit};
    let limit = Rlimit {
        current: Some(OPEN_FILES.0),
        maximum: Some(OPEN_FILES.1),
    };
    let _ = setrlimit(Resource::Nofile, limit);
}

/// What a command writes, and whether that's stderr.
type Output = (Box<dyn Read + Send>, bool);

/// A command running for a connection.
struct Running {
    pidfd: OwnedFd,
    pid: u32,
    exited: Receiver<Exit>,
    outputs: Vec<Output>,
    input: Input,
}

/// Where the host's stdin, window sizes and signals go.
struct Input {
    /// The command's stdin, or its terminal.
    stdin: Option<Box<dyn Write + Send>>,
    /// The terminal, if the command has one.
    tty: Option<Arc<File>>,
    pidfd: OwnedFd,
}

fn handle(conn: Socket, reaper: &Reaper, upper: &Option<File>) -> io::Result<()> {
    let mut incoming = conn.try_clone()?;
    let outgoing = Arc::new(Mutex::new(conn.try_clone()?));
    let send = |message: ToHost| message.write(&mut *outgoing.lock().unwrap());

    let start = match ToGuest::read(&mut incoming)? {
        Some(ToGuest::Stop) => {
            let exit = match stop(upper.as_ref()) {
                Ok(()) => ToHost::Exit(Exit::Code(0)),
                Err(e) => ToHost::Error(format!("stopping: {e}")),
            };
            let _ = send(exit);
            conn.shutdown_write();
            end(0);
        }
        Some(ToGuest::Start(start)) => start,
        Some(other) => return refuse(&conn, incoming, format!("expected a Start, got {other:?}")),
        None => return Ok(()),
    };
    // Nothing starts once a stop has begun: it would hold the upper disk the stop makes clean.
    let stopping = STOPPING.read().unwrap_or_else(|e| e.into_inner());
    if *stopping {
        drop(stopping);
        return refuse(&conn, incoming, "stopping".into());
    }
    let spawned = spawn(&start, reaper);
    drop(stopping);
    let running = match spawned {
        Ok(running) => running,
        Err(message) => return refuse(&conn, incoming, message),
    };
    send(ToHost::Started { pid: running.pid })?;

    let pumps: Vec<_> = running
        .outputs
        .into_iter()
        .map(|(reader, stderr)| {
            let outgoing = Arc::clone(&outgoing);
            thread::spawn(move || pump(reader, stderr, &outgoing))
        })
        .collect();
    let input = running.input;
    let feeder = thread::spawn(move || feed(incoming, input));

    // The reaper always sends the status of a child it was told about.
    let exit = running.exited.recv().unwrap_or(Exit::Code(255));
    // All of the output, before the exit: a pipe ends when the last process holding it does,
    // which may be after the command, if it left something running that kept it.
    for pump in pumps {
        let _ = pump.join();
    }
    let sent = send(ToHost::Exit(exit));
    // Nothing more: the host closes once it's read the exit, which ends the feeder. Closed here
    // with what the host sent unread, the host would read a reset, not the end.
    conn.shutdown_write();
    let _ = feeder.join();
    drop(running.pidfd);
    sent
}

/// Whether this is the VM's init (see main): then its end is the VM's.
pub static BOOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Ends this process, with `code`; as the VM's init, the VM, everything synced. By a reboot, as
/// libkrun's own init ends it: libkrun ends the VM on a reset, which on x86_64 the kernel makes
/// through the keyboard controller (reboot=k), where a power off only halts it (no ACPI).
pub fn end(code: i32) -> ! {
    if BOOTED.load(std::sync::atomic::Ordering::Relaxed) {
        // reboot doesn't return when it works.
        rustix::fs::sync();
        let _ = rustix::system::reboot(rustix::system::RebootCommand::Restart);
    }
    std::process::exit(code)
}

/// Says something of the agent's own, as `eprintln!` would. As the VM's init, in the kernel's
/// log instead, at error level: so it's on the VM's console whatever init's stderr is (on x86_64
/// it can be a console nothing reads) and through `quiet`, which shows only errors.
#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => { $crate::said(&format!($($arg)*)) };
}

pub fn said(message: &str) {
    if BOOTED.load(std::sync::atomic::Ordering::Relaxed) {
        use std::io::Write;
        // One write, one record.
        let record = format!("<3>{message}\n");
        if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg")
            && kmsg.write_all(record.as_bytes()).is_ok()
        {
            return;
        }
    }
    eprintln!("{message}");
}

/// How long what's running gets to end after SIGTERM, before SIGKILL.
const GRACE: std::time::Duration = std::time::Duration::from_secs(3);
/// How long a stop waits for what's ended to let go of the upper disk.
const RELEASE: std::time::Duration = std::time::Duration::from_secs(5);

/// Stops everything but this process and pid 1 (libkrun's init, in a VM), then syncs and remounts
/// `upper`, the root of the bottle's disk, read-only: ext4 is then clean on the disk, nothing left
/// in its journal to recover. (Freezing would do as well, but a frozen filesystem outlives its
/// mounts, holding its disk, until thawed.)
/// Whether a stop has begun: held for reading while a command starts, so none starts after.
static STOPPING: std::sync::RwLock<bool> = std::sync::RwLock::new(false);

fn stop(upper: Option<&File>) -> io::Result<()> {
    *STOPPING.write().unwrap_or_else(|e| e.into_inner()) = true;
    use rustix::process::{Pid, Signal, kill_process};
    for (signal, wait) in [(Signal::TERM, GRACE), (Signal::KILL, GRACE)] {
        if everything_is_mine() {
            // SAFETY: kill(-1, ...) signals every process this one may, but itself and pid 1
            // (rustix has no kill(-1)).
            unsafe { libc::kill(-1, signal.as_raw()) };
        } else {
            for pid in mine() {
                if let Some(pid) = Pid::from_raw(pid as i32) {
                    let _ = kill_process(pid, signal);
                }
            }
        }
        let deadline = std::time::Instant::now() + wait;
        while others_running() && std::time::Instant::now() < deadline {
            thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let Some(upper) = upper else {
        rustix::fs::sync();
        return Ok(());
    };
    // The mount is covered, so it's reached through the descriptor.
    let path = format!("/proc/self/fd/{}", upper.as_raw_fd());
    // A process gone can still have files open for writing a while: those of its io_uring (Node's
    // libuv uses one) are let go by a kernel worker after it ends. Until then it's EBUSY.
    let deadline = std::time::Instant::now() + RELEASE;
    let mut tries = 0;
    loop {
        tries += 1;
        rustix::fs::sync();
        let ret = rustix::mount::mount_remount(&path, rustix::mount::MountFlags::RDONLY, "");
        if ret.is_ok() {
            if tries > 1 {
                crate::say!("bottle-agent: stop: the upper disk read-only after {tries} tries");
            }
            return Ok(());
        }
        let e = io::Error::from(ret.unwrap_err());
        if e.raw_os_error() != Some(libc::EBUSY) || std::time::Instant::now() > deadline {
            let left = still_running();
            crate::say!(
                "bottle-agent: stop: the upper disk still busy after {tries} tries: {e}; running: {left:?}"
            );
            return Err(if left.is_empty() {
                e
            } else {
                io::Error::new(
                    e.kind(),
                    format!("{e}, with {} still running", left.join(", ")),
                )
            });
        }
        if tries == 1 {
            // Busy with no one left (it has been, after a bottle ran a while): what the kernel
            // still holds of it, cached, deleted files overlayfs or the dentry cache hasn't let go
            // of among them, is let go of. What came of it is on the console (console.log.1,
            // once the machine's started again).
            crate::say!("bottle-agent: stop: the upper disk busy ({e}); dropping caches");
            let _ = std::fs::write("/proc/sys/vm/drop_caches", "3");
        }
        thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Whether every process this one may signal is the machine's, to stop: as the VM's init, or the
/// init of a pid namespace of its own. Out of either (a test running it as a stand-in), only its
/// own descendants are: kill(-1) would end every process of its user.
fn everything_is_mine() -> bool {
    BOOTED.load(std::sync::atomic::Ordering::Relaxed) || std::process::id() == 1
}

/// Whether anything of the machine's is still running but this process and pid 1: kernel threads
/// (which kill(-1) counts, never ending) and zombies (dead, to be reaped) aside.
fn others_running() -> bool {
    !still_running().is_empty()
}

/// A process, as /proc has it.
struct Process {
    pid: u32,
    ppid: u32,
    comm: String,
    /// Not a zombie, and not a kernel thread.
    live: bool,
}

fn processes() -> Vec<Process> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            // "pid (comm) state ppid pgrp session tty tpgid flags ...", comm perhaps with spaces.
            let (head, rest) = stat.rsplit_once(')')?;
            let comm = head.split_once('(').map_or("", |(_, c)| c).to_string();
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let state = fields.first().copied().unwrap_or("Z");
            let ppid = fields.get(1).and_then(|f| f.parse().ok()).unwrap_or(0);
            let flags: u64 = fields.get(6).and_then(|f| f.parse().ok()).unwrap_or(0);
            /// PF_KTHREAD, from linux/sched.h.
            const KERNEL_THREAD: u64 = 0x0020_0000;
            let live = !matches!(state, "Z" | "X") && flags & KERNEL_THREAD == 0;
            Some(Process {
                pid,
                ppid,
                comm,
                live,
            })
        })
        .collect()
}

/// This process's descendants, live: what's the machine's when not everything is.
fn mine() -> Vec<u32> {
    let all = processes();
    let mut found = vec![std::process::id()];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        found.extend(all.iter().filter(|p| p.ppid == parent).map(|p| p.pid));
        i += 1;
    }
    found.remove(0);
    let live: std::collections::HashSet<u32> =
        all.iter().filter(|p| p.live).map(|p| p.pid).collect();
    found.into_iter().filter(|p| live.contains(p)).collect()
}

/// What's still running of the machine's but this process and pid 1, kernel threads and zombies
/// aside: "pid comm"s.
fn still_running() -> Vec<String> {
    let me = std::process::id();
    let scope: Option<std::collections::HashSet<u32>> =
        (!everything_is_mine()).then(|| mine().into_iter().collect());
    processes()
        .into_iter()
        .filter(|p| p.live && p.pid != 1 && p.pid != me)
        .filter(|p| scope.as_ref().is_none_or(|s| s.contains(&p.pid)))
        .map(|p| format!("{} {}", p.pid, p.comm))
        .collect()
}

/// Tells the host why its command won't run, and reads what it sends until it closes.
fn refuse(conn: &Socket, mut incoming: Socket, message: String) -> io::Result<()> {
    let sent = ToHost::Error(message).write(&mut conn.try_clone()?);
    conn.shutdown_write();
    while let Ok(Some(_)) = ToGuest::read(&mut incoming) {}
    sent
}

/// Sends what `reader` gives as output frames, until it ends.
fn pump(mut reader: Box<dyn Read + Send>, stderr: bool, outgoing: &Mutex<Socket>) {
    let mut buf = vec![0; CHUNK];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // EIO: a terminal whose other side every process has closed.
            Err(_) => return,
        };
        let data = buf[..n].to_vec();
        let message = if stderr {
            ToHost::Stderr(data)
        } else {
            ToHost::Stdout(data)
        };
        if message.write(&mut *outgoing.lock().unwrap()).is_err() {
            // The host is gone: keep reading, so the command isn't blocked writing.
            continue;
        }
    }
}

/// Passes on what the host sends, until it sends nothing more. A host that goes away while the
/// command runs hangs it up, as a terminal closing would.
fn feed(mut incoming: Socket, mut input: Input) {
    while let Ok(Some(message)) = ToGuest::read(&mut incoming) {
        match message {
            ToGuest::Stdin(data) => {
                if let Some(stdin) = input.stdin.as_mut() {
                    // The command may have closed its stdin; what it doesn't read is dropped.
                    if stdin.write_all(&data).is_err() {
                        input.stdin = None;
                    }
                }
            }
            ToGuest::StdinEof => match &input.tty {
                Some(tty) => {
                    let _ = (&**tty).write_all(&[VEOF]);
                }
                None => input.stdin = None,
            },
            ToGuest::Resize(size) => {
                if let Some(tty) = &input.tty {
                    set_size(tty, size);
                }
            }
            ToGuest::Signal(signal) => signal_pidfd(&input.pidfd, signal),
            // Only ever first, on a connection of their own.
            ToGuest::Start(_) | ToGuest::Stop => {}
        }
    }
    // After the exit, this signals nothing: the pidfd refers to that process, reaped or not, and
    // never to another that has its pid since.
    signal_pidfd(&input.pidfd, libc::SIGHUP);
}

/// Sends `signal` to the process `pidfd` refers to; a signal with no name (a real-time one, or
/// none at all) isn't sent.
fn signal_pidfd(pidfd: &OwnedFd, signal: i32) {
    if let Some(signal) = rustix::process::Signal::from_named_raw(signal) {
        let _ = rustix::process::pidfd_send_signal(pidfd, signal);
    }
}

fn set_size(tty: impl AsFd, size: WinSize) {
    let size = rustix::termios::Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let _ = rustix::termios::tcsetwinsize(tty, size);
}

/// Starts the command `start` asks for, or says why it can't.
fn spawn(start: &Start, reaper: &Reaper) -> Result<Running, String> {
    let program = start.argv.first().ok_or("an empty command")?;
    let mut command = Command::new(program);
    command.args(&start.argv[1..]);

    let mut home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    if let Some(name) = &start.user {
        let user = user::lookup(Path::new("/"), name)?;
        home = user.home.clone();
        command
            .env("HOME", &user.home)
            .env("USER", &user.name)
            .env("LOGNAME", &user.name)
            .env("SHELL", &user.shell);
        let euid = rustix::process::geteuid().as_raw();
        if user.uid != euid {
            if euid != 0 {
                return Err(format!("can't run as {name}: the agent isn't root"));
            }
            use rustix::process::{Gid, Uid};
            use rustix::thread::{set_thread_gid, set_thread_groups, set_thread_uid};
            let groups: Vec<Gid> = user.groups.iter().map(|&g| Gid::from_raw(g)).collect();
            let (uid, gid) = (Uid::from_raw(user.uid), Gid::from_raw(user.gid));
            // SAFETY: between fork and exec, only system calls, on data made before the fork. The
            // thread's ids are the process's: after the fork it's the only thread. (std's own
            // uid and gid are set before this runs, and setgroups needs root.)
            unsafe {
                command.pre_exec(move || {
                    set_thread_groups(&groups)?;
                    set_thread_gid(gid)?;
                    set_thread_uid(uid)?;
                    Ok(())
                });
            }
        }
    }
    // A home that isn't there (nobody's /nonexistent) is no reason not to run: / instead.
    let cwd = start.cwd.clone().unwrap_or(if Path::new(&home).is_dir() {
        home
    } else {
        "/".into()
    });
    command.current_dir(cwd);
    command.envs(start.env.iter().map(|(k, v)| (k, v)));

    let tty = match start.tty {
        Some(size) => {
            let (master, slave) = openpty(size).map_err(|e| format!("making a terminal: {e}"))?;
            let end = |f: &OwnedFd| f.try_clone().map(Stdio::from).map_err(|e| e.to_string());
            command
                .stdin(end(&slave)?)
                .stdout(end(&slave)?)
                .stderr(end(&slave)?);
            // SAFETY: setsid and TIOCSCTTY between fork and exec, system calls with no
            // allocation: a session of its own, whose controlling terminal is the one on its stdin.
            unsafe {
                command.pre_exec(|| {
                    rustix::process::setsid()?;
                    rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
                    Ok(())
                });
            }
            Some(Arc::new(File::from(master)))
        }
        None => {
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            None
        }
    };

    let (mut child, pidfd, exited) = reaper
        .spawn(&mut command)
        .map_err(|e| format!("{program}: {e}"))?;
    // The terminal's other side stays with the command only: its output ends when the command's does.
    drop(command);
    let (outputs, stdin): (Vec<Output>, Option<Box<dyn Write + Send>>) = match &tty {
        Some(master) => {
            let clone = || master.try_clone().map_err(|e| e.to_string());
            (vec![(Box::new(clone()?), false)], Some(Box::new(clone()?)))
        }
        None => (
            vec![
                (Box::new(child.stdout.take().expect("piped")), false),
                (Box::new(child.stderr.take().expect("piped")), true),
            ],
            Some(Box::new(child.stdin.take().expect("piped"))),
        ),
    };
    let input = Input {
        stdin,
        tty,
        pidfd: pidfd.try_clone().map_err(|e| e.to_string())?,
    };
    Ok(Running {
        pidfd,
        pid: child.id(),
        exited,
        outputs,
        input,
    })
}

/// A terminal of `size`: its master side, and the side the command gets. Neither is inherited
/// across exec, from the moment it's opened: commands start from several connections at once, and
/// one that inherited this one's terminal would keep it open after this one's command ended.
fn openpty(size: WinSize) -> io::Result<(OwnedFd, OwnedFd)> {
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC)?;
    grantpt(&master)?;
    unlockpt(&master)?;
    let path = ptsname(&master, Vec::new())?;
    let slave = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(std::ffi::OsStr::from_bytes(path.as_bytes()))?;
    set_size(&slave, size);
    Ok((master, OwnedFd::from(slave)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminals_sides_are_never_inherited() {
        let (master, slave) = openpty(WinSize { rows: 24, cols: 80 }).unwrap();
        for fd in [&master, &slave] {
            let flags = rustix::io::fcntl_getfd(fd).unwrap();
            assert!(flags.contains(rustix::io::FdFlags::CLOEXEC));
        }
        let size = rustix::termios::tcgetwinsize(&slave).unwrap();
        assert_eq!((size.ws_row, size.ws_col), (24, 80));
    }
}
