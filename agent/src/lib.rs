//! The agent in a bottle's VM: it runs commands for the host, one per connection (see
//! bottle-agent-proto), and, as PID 1, reaps every orphan in the VM.

pub mod init;
pub mod reaper;
pub mod socket;
pub mod user;

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
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

/// Serves connections on `listener`, each on a thread of its own, until accepting fails.
pub fn serve(listener: Listener, reaper: Reaper) -> io::Result<()> {
    loop {
        let conn = match listener.accept() {
            Ok(conn) => conn,
            Err(e) if matches!(e.raw_os_error(), Some(libc::EINTR | libc::ECONNABORTED)) => {
                continue;
            }
            Err(e) => return Err(e),
        };
        let reaper = reaper.clone();
        thread::spawn(move || {
            if let Err(e) = handle(conn, &reaper) {
                eprintln!("bottle-agent: {e}");
            }
        });
    }
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

fn handle(conn: Socket, reaper: &Reaper) -> io::Result<()> {
    let mut incoming = conn.try_clone()?;
    let outgoing = Arc::new(Mutex::new(conn.try_clone()?));
    let send = |message: ToHost| message.write(&mut *outgoing.lock().unwrap());

    let start = match ToGuest::read(&mut incoming)? {
        Some(ToGuest::Start(start)) => start,
        Some(other) => return refuse(&conn, incoming, format!("expected a Start, got {other:?}")),
        None => return Ok(()),
    };
    let running = match spawn(&start, reaper) {
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
            ToGuest::Start(_) => {}
        }
    }
    // After the exit, this signals nothing: the pidfd refers to that process, reaped or not, and
    // never to another that has its pid since.
    signal_pidfd(&input.pidfd, libc::SIGHUP);
}

fn signal_pidfd(pidfd: &OwnedFd, signal: i32) {
    // SAFETY: pidfd_send_signal with no siginfo, on a pidfd this owns.
    unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<()>(),
            0,
        )
    };
}

fn set_size(tty: &File, size: WinSize) {
    let size = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads a winsize.
    unsafe { libc::ioctl(tty.as_raw_fd(), libc::TIOCSWINSZ, &size) };
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
            .env("LOGNAME", &user.name);
        // SAFETY: geteuid has no preconditions.
        let euid = unsafe { libc::geteuid() };
        if user.uid != euid {
            if euid != 0 {
                return Err(format!("can't run as {name}: the agent isn't root"));
            }
            let groups: Vec<libc::gid_t> = user.groups.clone();
            let (uid, gid) = (user.uid, user.gid);
            // SAFETY: between fork and exec, only async-signal-safe calls, on data made before the fork.
            unsafe {
                command.pre_exec(move || {
                    if libc::setgroups(groups.len(), groups.as_ptr()) < 0
                        || libc::setgid(gid) < 0
                        || libc::setuid(uid) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
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
            // SAFETY: setsid and TIOCSCTTY between fork and exec: a session of its own, whose
            // controlling terminal is the one on its stdin.
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
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
/// across exec, so no other command keeps this one's terminal open.
fn openpty(size: WinSize) -> io::Result<(OwnedFd, OwnedFd)> {
    let size = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty writes the two descriptors; no name is asked for, and the size is read.
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: descriptors openpty just made, owned by nothing else.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [&master, &slave] {
        // SAFETY: on descriptors these own.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    Ok((master, slave))
}
