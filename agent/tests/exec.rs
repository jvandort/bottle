//! The agent end to end: the binary, listening on a unix socket, and a client speaking bottle-agent-proto.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use bottle_agent_proto::{Exit, Start, ToGuest, ToHost, WinSize};

/// The agent, running until dropped.
struct Agent {
    child: Child,
    dir: PathBuf,
}

impl Agent {
    fn start() -> Agent {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "bottle-agent-exec-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("exec.sock");
        let child = Command::new(env!("CARGO_BIN_EXE_bottle-agent"))
            .arg("--unix")
            .arg(&socket)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "the agent didn't listen");
            thread::sleep(Duration::from_millis(10));
        }
        Agent { child, dir }
    }

    fn connect(&self) -> UnixStream {
        UnixStream::connect(self.dir.join("exec.sock")).unwrap()
    }

    /// Runs `start`, sending `stdin` and then its end, and returns what came back.
    fn run(&self, start: Start, stdin: &[u8]) -> Outcome {
        let mut conn = self.connect();
        ToGuest::Start(start).write(&mut conn).unwrap();
        let mut writer = conn.try_clone().unwrap();
        let stdin = stdin.to_vec();
        // On a thread, as a real client would: a command may write before it reads.
        let feeder = thread::spawn(move || {
            for chunk in stdin.chunks(100_000) {
                if ToGuest::Stdin(chunk.to_vec()).write(&mut writer).is_err() {
                    return;
                }
            }
            let _ = ToGuest::StdinEof.write(&mut writer);
        });
        let outcome = Outcome::read(&mut conn);
        feeder.join().unwrap();
        outcome
    }

    /// The pids of this agent's children.
    fn children(&self) -> Vec<u32> {
        let pid = self.child.id();
        let mut children = Vec::new();
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
            let path = task.unwrap().path().join("children");
            let text = std::fs::read_to_string(path).unwrap_or_default();
            children.extend(text.split_whitespace().map(|p| p.parse::<u32>().unwrap()));
        }
        children
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug, Default)]
struct Outcome {
    pid: Option<u32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: Option<Exit>,
    error: Option<String>,
}

impl Outcome {
    fn read(conn: &mut UnixStream) -> Outcome {
        let mut outcome = Outcome::default();
        while let Some(message) = ToHost::read(conn).unwrap() {
            match message {
                ToHost::Started { pid } => outcome.pid = Some(pid),
                ToHost::Stdout(data) => outcome.stdout.extend(data),
                ToHost::Stderr(data) => outcome.stderr.extend(data),
                ToHost::Exit(exit) => outcome.exit = Some(exit),
                ToHost::Error(message) => outcome.error = Some(message),
            }
        }
        outcome
    }

    fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

fn sh(script: &str) -> Start {
    Start {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        ..Start::default()
    }
}

#[test]
fn runs_a_command_and_returns_its_output_and_status() {
    let agent = Agent::start();
    let out = agent.run(sh("echo out; echo err >&2; exit 3"), b"");
    assert!(out.pid.is_some());
    assert_eq!(out.stdout(), "out\n");
    assert_eq!(out.stderr, b"err\n");
    assert_eq!(out.exit, Some(Exit::Code(3)));
    assert_eq!(out.error, None);
}

#[test]
fn runs_argv_as_it_is_with_no_shell() {
    let agent = Agent::start();
    let start = Start {
        argv: vec![
            "printf".into(),
            "%s|".into(),
            "a b".into(),
            "$HOME".into(),
            "".into(),
        ],
        ..Start::default()
    };
    assert_eq!(agent.run(start, b"").stdout(), "a b|$HOME||");
}

#[test]
fn passes_stdin_through_to_its_end() {
    let agent = Agent::start();
    let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let out = agent.run(
        Start {
            argv: vec!["cat".into()],
            ..Start::default()
        },
        &data,
    );
    assert_eq!(out.stdout.len(), data.len());
    assert!(out.stdout == data);
    assert_eq!(out.exit, Some(Exit::Code(0)));
}

/// smolvm's exec, without -i, cut output at 11 MiB and said nothing.
#[test]
fn streams_large_output_whole() {
    let agent = Agent::start();
    let out = agent.run(sh("head -c 50000000 /dev/zero; echo done >&2"), b"");
    assert_eq!(out.stdout.len(), 50_000_000);
    assert_eq!(out.stderr, b"done\n");
    assert_eq!(out.exit, Some(Exit::Code(0)));
}

#[test]
fn reports_a_command_killed_by_a_signal() {
    let agent = Agent::start();
    assert_eq!(
        agent.run(sh("kill -KILL $$"), b"").exit,
        Some(Exit::Signal(9))
    );
}

#[test]
fn passes_a_signal_on() {
    let agent = Agent::start();
    let mut conn = agent.connect();
    ToGuest::Start(sh("echo ready; exec sleep 30"))
        .write(&mut conn)
        .unwrap();
    // Once it's running: its first output.
    loop {
        match ToHost::read(&mut conn).unwrap().unwrap() {
            ToHost::Stdout(data) if data == b"ready\n" => break,
            _ => {}
        }
    }
    let started = Instant::now();
    ToGuest::Signal(libc::SIGTERM).write(&mut conn).unwrap();
    let out = Outcome::read(&mut conn);
    assert_eq!(out.exit, Some(Exit::Signal(libc::SIGTERM)));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn hangs_up_a_command_whose_host_goes_away() {
    let agent = Agent::start();
    let marker = agent.dir.join("hung-up");
    let mut conn = agent.connect();
    let script = format!(
        "trap 'touch {}; exit 0' HUP; echo ready; while :; do sleep 0.1; done",
        marker.display()
    );
    ToGuest::Start(sh(&script)).write(&mut conn).unwrap();
    loop {
        if let ToHost::Stdout(data) = ToHost::read(&mut conn).unwrap().unwrap()
            && data == b"ready\n"
        {
            break;
        }
    }
    drop(conn);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the command wasn't hung up");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sets_the_environment_and_working_directory() {
    let agent = Agent::start();
    let start = Start {
        env: vec![("GREETING".into(), "hello there".into())],
        cwd: Some("/tmp".into()),
        ..sh("echo \"$GREETING\"; pwd")
    };
    assert_eq!(agent.run(start, b"").stdout(), "hello there\n/tmp\n");
}

#[test]
fn says_why_a_command_cant_run() {
    let agent = Agent::start();
    let out = agent.run(
        Start {
            argv: vec!["/no/such/program".into()],
            ..Start::default()
        },
        b"",
    );
    assert_eq!(out.exit, None);
    assert!(
        out.error
            .as_deref()
            .unwrap()
            .starts_with("/no/such/program: "),
        "{:?}",
        out.error
    );

    let out = agent.run(
        Start {
            argv: vec![],
            ..Start::default()
        },
        b"",
    );
    assert_eq!(out.error.as_deref(), Some("an empty command"));

    let out = agent.run(
        Start {
            user: Some("no-such-user-here".into()),
            ..sh("true")
        },
        b"",
    );
    assert_eq!(
        out.error.as_deref(),
        Some("no such user: no-such-user-here")
    );
}

#[test]
fn runs_as_another_user_when_root() {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("skipped: needs root");
        return;
    }
    let agent = Agent::start();
    // nobody's home, /nonexistent, isn't there: it starts in / instead.
    let start = Start {
        user: Some("nobody".into()),
        ..sh("id -un; echo $USER; pwd")
    };
    assert_eq!(agent.run(start, b"").stdout(), "nobody\nnobody\n/\n");
}

#[test]
fn gives_a_command_a_terminal() {
    let agent = Agent::start();
    let start = Start {
        tty: Some(WinSize {
            rows: 33,
            cols: 101,
        }),
        ..sh("test -t 0 && test -t 1 && stty size")
    };
    let out = agent.run(start, b"");
    assert_eq!(out.stdout(), "33 101\r\n");
    assert_eq!(out.exit, Some(Exit::Code(0)));
}

#[test]
fn resizes_a_terminal() {
    let agent = Agent::start();
    let mut conn = agent.connect();
    let start = Start {
        tty: Some(WinSize { rows: 24, cols: 80 }),
        ..sh("stty size; read line; stty size")
    };
    ToGuest::Start(start).write(&mut conn).unwrap();
    let mut seen = Vec::new();
    while !String::from_utf8_lossy(&seen).contains("24 80") {
        if let ToHost::Stdout(data) = ToHost::read(&mut conn).unwrap().unwrap() {
            seen.extend(data);
        }
    }
    ToGuest::Resize(WinSize {
        rows: 50,
        cols: 132,
    })
    .write(&mut conn)
    .unwrap();
    ToGuest::Stdin(b"go\n".to_vec()).write(&mut conn).unwrap();
    let out = Outcome::read(&mut conn);
    assert!(out.stdout().ends_with("50 132\r\n"), "{:?}", out.stdout());
}

#[test]
fn ends_a_terminal_commands_stdin() {
    let agent = Agent::start();
    let start = Start {
        tty: Some(WinSize { rows: 24, cols: 80 }),
        ..sh("stty -echo; wc -c; echo eof")
    };
    let out = agent.run(start, b"some input\n");
    // The terminal turns the newline into one byte for wc, and its output's into \r\n.
    assert!(
        out.stdout().ends_with("11\r\neof\r\n"),
        "{:?}",
        out.stdout()
    );
    assert_eq!(out.exit, Some(Exit::Code(0)));
}

#[test]
fn reaps_orphans() {
    let agent = Agent::start();
    // The background sleep outlives its shell, and becomes the agent's.
    let out = agent.run(sh("sleep 0.3 >/dev/null 2>&1 & echo started"), b"");
    assert_eq!(out.stdout(), "started\n");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !agent.children().is_empty() {
        assert!(
            Instant::now() < deadline,
            "children left: {:?}",
            agent.children()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn waits_for_output_from_what_a_command_leaves_running() {
    let agent = Agent::start();
    let out = agent.run(sh("(sleep 0.3; echo late) & echo early"), b"");
    assert_eq!(out.stdout(), "early\nlate\n");
}

#[test]
fn runs_commands_at_once() {
    let agent = Agent::start();
    let started = Instant::now();
    let runs: Vec<_> = (0..8)
        .map(|i| {
            let socket = agent.dir.join("exec.sock");
            thread::spawn(move || {
                let mut conn = UnixStream::connect(socket).unwrap();
                ToGuest::Start(sh(&format!("sleep 0.5; echo {i}")))
                    .write(&mut conn)
                    .unwrap();
                ToGuest::StdinEof.write(&mut conn).unwrap();
                Outcome::read(&mut conn).stdout()
            })
        })
        .collect();
    let outputs: Vec<_> = runs.into_iter().map(|run| run.join().unwrap()).collect();
    assert_eq!(
        outputs,
        (0..8).map(|i| format!("{i}\n")).collect::<Vec<_>>()
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_command_starts_quickly() {
    let agent = Agent::start();
    agent.run(
        Start {
            argv: vec!["true".into()],
            ..Start::default()
        },
        b"",
    );
    let n = 50;
    let started = Instant::now();
    for _ in 0..n {
        assert_eq!(
            agent
                .run(
                    Start {
                        argv: vec!["true".into()],
                        ..Start::default()
                    },
                    b""
                )
                .exit,
            Some(Exit::Code(0))
        );
    }
    let each = started.elapsed() / n;
    eprintln!("a round trip running `true`: {each:?}");
    assert!(each < Duration::from_millis(50), "{each:?}");
}

#[test]
fn survives_connections_that_close_early_or_dont_start() {
    let agent = Agent::start();
    drop(agent.connect());
    // A first frame that isn't a Start: refused, with the reason.
    let mut conn = agent.connect();
    ToGuest::StdinEof.write(&mut conn).unwrap();
    let out = Outcome::read(&mut conn);
    assert!(out.error.unwrap().starts_with("expected a Start"));
    assert_eq!(
        agent.run(sh("echo still here"), b"").stdout(),
        "still here\n"
    );
}
