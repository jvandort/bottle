//! `bottle-cli exec` end to end, against the agent listening on a unix socket.
//!
//! The agent is another package's binary, which cargo doesn't hand a test: it's built here (cargo
//! does nothing if it's up to date), and found beside this test's own profile directory.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

fn agent_binary() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        // target/<profile>/deps/<this test>
        let profile = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let mut cargo = match std::env::var("SUDO_USER") {
            // Run as root (a sudo runner, for the tests that need it): built as whoever owns the
            // target directory, and sudo sets its own PATH, so cargo's own directory, where rustc
            // is, goes first.
            Ok(user) => {
                let bin = std::path::Path::new(env!("CARGO"))
                    .parent()
                    .unwrap()
                    .display()
                    .to_string();
                let path = std::env::var("PATH").unwrap_or_default();
                let mut c = Command::new("sudo");
                c.args([
                    "-E",
                    "-u",
                    &user,
                    "env",
                    &format!("PATH={bin}:{path}"),
                    env!("CARGO"),
                ]);
                c
            }
            Err(_) => Command::new(env!("CARGO")),
        };
        cargo.args(["build", "-q", "-p", "bottle-agent"]);
        if profile.ends_with("release") {
            cargo.arg("--release");
        }
        assert!(cargo.status().unwrap().success(), "building bottle-agent");
        profile.join("bottle-agent")
    })
}

struct Agent {
    child: Child,
    dir: PathBuf,
}

impl Agent {
    fn start() -> Agent {
        Agent::start_with(None)
    }

    /// An agent that may have only `open_files` descriptors open, if Some.
    fn start_with(open_files: Option<u64>) -> Agent {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bottle-cli-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("exec.sock");
        let mut command = Command::new(agent_binary());
        command.arg("--unix").arg(&socket);
        if let Some(n) = open_files {
            // SAFETY: setrlimit between fork and exec, reading an rlimit.
            unsafe {
                command.pre_exec(move || {
                    let limit = libc::rlimit {
                        rlim_cur: n,
                        rlim_max: n,
                    };
                    if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "the agent didn't listen");
            thread::sleep(Duration::from_millis(10));
        }
        Agent { child, dir }
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("exec.sock")
    }

    /// `bottle-cli exec`, for this agent, with `args` after the socket.
    fn exec(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_bottle-cli"));
        command.arg("exec").arg(self.socket()).args(args);
        command
    }

    fn output(&self, args: &[&str], stdin: &[u8]) -> Output {
        let mut child = self
            .exec(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let stdin = stdin.to_vec();
        let feeder = thread::spawn(move || {
            let _ = input.write_all(&stdin);
        });
        let output = child.wait_with_output().unwrap();
        feeder.join().unwrap();
        output
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn passes_output_and_status_through() {
    let agent = Agent::start();
    let out = agent.output(&["sh", "-c", "echo out; echo err >&2; exit 7"], b"");
    assert_eq!(text(&out.stdout), "out\n");
    assert_eq!(text(&out.stderr), "err\n");
    assert_eq!(out.status.code(), Some(7));
}

#[test]
fn passes_stdin_through_to_its_end() {
    let agent = Agent::start();
    let data: Vec<u8> = (0..2_000_000u32).map(|i| (i % 253) as u8).collect();
    let out = agent.output(&["--", "cat"], &data);
    assert!(
        out.stdout == data,
        "{} bytes back of {}",
        out.stdout.len(),
        data.len()
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn exits_128_and_the_signal_for_a_command_a_signal_killed() {
    let agent = Agent::start();
    assert_eq!(
        agent
            .output(&["sh", "-c", "kill -KILL $$"], b"")
            .status
            .code(),
        Some(137)
    );
}

#[test]
fn sets_the_environment_and_working_directory() {
    let agent = Agent::start();
    let out = agent.output(
        &[
            "-e",
            "A=one two",
            "-w",
            "/tmp",
            "sh",
            "-c",
            "echo \"$A\"; pwd",
        ],
        b"",
    );
    assert_eq!(text(&out.stdout), "one two\n/tmp\n");
}

#[test]
fn exits_255_saying_why_when_the_command_cant_run() {
    let agent = Agent::start();
    let out = agent.output(&["/no/such/program"], b"");
    assert_eq!(out.status.code(), Some(255));
    assert!(
        text(&out.stderr).starts_with("bottle-cli: /no/such/program: "),
        "{}",
        text(&out.stderr)
    );

    let out = Command::new(env!("CARGO_BIN_EXE_bottle-cli"))
        .args(["exec", "/no/such/socket", "true"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(255));
    assert!(text(&out.stderr).starts_with("bottle-cli: "));

    let out = Command::new(env!("CARGO_BIN_EXE_bottle-cli"))
        .args(["exec", "/s"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(255));
    assert!(text(&out.stderr).contains("usage: bottle-cli exec"));
}

#[test]
fn passes_signals_on_to_a_command_without_a_terminal() {
    let agent = Agent::start();
    let script = "trap 'echo got TERM; exit 0' TERM; echo ready; while :; do sleep 0.05; done";
    let mut child = agent
        .exec(&["sh", "-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut seen = Vec::new();
    read_until(&mut stdout, &mut seen, "ready\n");
    // SAFETY: kill on our own child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    read_until(&mut stdout, &mut seen, "got TERM\n");
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

/// Reads into `seen` until it holds `wanted`, for at most 5 seconds.
fn read_until(r: &mut impl Read, seen: &mut Vec<u8>, wanted: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = [0; 4096];
    while !text(seen).contains(wanted) {
        assert!(
            Instant::now() < deadline,
            "waiting for {wanted:?}, saw {:?}",
            text(seen)
        );
        match r.read(&mut buf) {
            Ok(0) => panic!("ended waiting for {wanted:?}, saw {:?}", text(seen)),
            Ok(n) => seen.extend_from_slice(&buf[..n]),
            Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                panic!("terminal closed waiting for {wanted:?}")
            }
            Err(e) => panic!("{e}"),
        }
    }
}

/// A terminal, as a person would run bottle-cli in: its master side for the test, and the side
/// bottle-cli gets.
fn terminal(rows: u16, cols: u16) -> (OwnedFd, OwnedFd) {
    let size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty writes the two descriptors; the size is read.
    let ret = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    assert_eq!(ret, 0);
    // SAFETY: descriptors openpty just made.
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

fn termios(fd: &OwnedFd) -> libc::termios {
    // SAFETY: tcgetattr writes the termios.
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::tcgetattr(fd.as_raw_fd(), &mut t) }, 0);
    t
}

#[test]
fn gives_the_command_this_terminal_raw_and_sized_and_sets_it_back() {
    let agent = Agent::start();
    let (master, slave) = terminal(30, 100);
    assert_ne!(termios(&slave).c_lflag & libc::ICANON, 0);

    let mut command = agent.exec(&["-t", "sh", "-c", "stty size; read line; stty size"]);
    let end = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
    command
        .stdin(end(&slave))
        .stdout(end(&slave))
        .stderr(end(&slave));
    // SAFETY: setsid and TIOCSCTTY between fork and exec: the terminal is bottle-cli's own.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let mut screen = File::from(master.try_clone().unwrap());
    let mut seen = Vec::new();
    read_until(&mut screen, &mut seen, "30 100");
    // Raw while the command runs: typing goes to it as it is.
    assert_eq!(termios(&slave).c_lflag & (libc::ICANON | libc::ECHO), 0);

    let size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCSWINSZ reads a winsize; bottle-cli, in the foreground, gets SIGWINCH.
    unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size) };
    thread::sleep(Duration::from_millis(300));
    screen.write_all(b"go\r").unwrap();
    read_until(&mut screen, &mut seen, "40 120");
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert_ne!(
        termios(&slave).c_lflag & libc::ICANON,
        0,
        "the terminal wasn't set back"
    );
}

#[test]
fn ctrl_c_typed_interrupts_the_command() {
    let agent = Agent::start();
    let (master, slave) = terminal(30, 100);
    let mut command = agent.exec(&[
        "-t",
        "sh",
        "-c",
        "trap 'echo interrupted; exit 3' INT; echo ready; while :; do sleep 0.1; done",
    ]);
    let end = |fd: &OwnedFd| Stdio::from(fd.try_clone().unwrap());
    command
        .stdin(end(&slave))
        .stdout(end(&slave))
        .stderr(end(&slave));
    // SAFETY: setsid and TIOCSCTTY between fork and exec: the terminal is bottle-cli's own.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let mut screen = File::from(master.try_clone().unwrap());
    let mut seen = Vec::new();
    read_until(&mut screen, &mut seen, "ready");
    // Raw here, so it's the command's terminal that makes it SIGINT.
    screen.write_all(b"\x03").unwrap();
    read_until(&mut screen, &mut seen, "interrupted");
    assert_eq!(child.wait().unwrap().code(), Some(3));
}

#[test]
fn running_out_of_descriptors_passes_and_the_agent_serves_on() {
    // Booted, the agent serving no more powers the VM off: a burst of connections (the bottle's
    // network, DNS lookups) that ran it out of descriptors did, once.
    let mut agent = Agent::start_with(Some(40));
    let held: Vec<UnixStream> = (0..100)
        .map(|_| UnixStream::connect(agent.socket()).unwrap())
        .collect();
    thread::sleep(Duration::from_millis(300));
    assert!(agent.child.try_wait().unwrap().is_none(), "the agent ended");
    drop(held);
    thread::sleep(Duration::from_millis(300));
    let out = agent.output(&["echo", "still here"], b"");
    assert_eq!(text(&out.stdout), "still here\n");
}

#[test]
fn gives_a_terminal_even_when_stdin_isnt_one() {
    let agent = Agent::start();
    let out = agent.output(&["-t", "sh", "-c", "test -t 1 && stty size"], b"");
    assert_eq!(text(&out.stdout), "24 80\r\n");
    assert_eq!(out.status.code(), Some(0));
}
