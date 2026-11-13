//! The bottle's network, out of a VM: the agent in PID, mount, UTS and network namespaces of its
//! own (this machine's own network untouched), its host side (bottle-agent-client's net) served
//! here on a unix socket mounted into the bottle, deciding by a policy that routes one port to a
//! service here, as bottled is one, and refuses what else it can't allow (a private address no name
//! leads to).
//!
//! Needs root, loop devices, mke2fs and python3, so it's skipped otherwise; to run it, as root but
//! built as you:
//!
//!     CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test -p bottle-agent --test net

use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, UdpSocket};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bottle_agent_client::net::{Network, serve};
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

/// This machine's address on its network, which the bottle reaches through the host side.
fn host_address() -> IpAddr {
    let probe = UdpSocket::bind("0.0.0.0:0").unwrap();
    probe.connect("192.0.2.1:9").unwrap();
    probe.local_addr().unwrap().ip()
}

struct Bottle {
    unshare: Child,
    agent_pid: u32,
    dir: PathBuf,
    loop_dev: Option<String>,
}

impl Bottle {
    fn exec(&self, argv: &[&str]) -> (String, Exit) {
        let socket = format!("/proc/{}/root/run/agent.sock", self.agent_pid);
        let mut conn = UnixStream::connect(socket).unwrap();
        ToGuest::Start(Start {
            argv: argv.iter().map(|a| a.to_string()).collect(),
            ..Start::default()
        })
        .write(&mut conn)
        .unwrap();
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

    fn ok(&self, argv: &[&str]) -> String {
        let (out, exit) = self.exec(argv);
        assert_eq!(exit, Exit::Code(0), "{argv:?}");
        out
    }
}

impl Drop for Bottle {
    fn drop(&mut self) {
        if self.agent_pid != 0 {
            // SAFETY: kill with a pid: the agent is its namespace's init, everything in there ends.
            unsafe { libc::kill(self.agent_pid as libc::pid_t, libc::SIGKILL) };
        }
        let _ = self.unshare.kill();
        let _ = self.unshare.wait();
        if let Some(dev) = self.loop_dev.take() {
            let _ = Command::new("losetup").args(["-d", &dev]).status();
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// What the service was given: the destination its PROXY header named, and the bytes after it.
type Given = Arc<Mutex<Vec<(IpAddr, u16, Vec<u8>)>>>;

/// A service at `socket`: it answers "bottled here, you said: " and the two bytes it was sent.
fn service(socket: &std::path::Path) -> Given {
    let listener = UnixListener::bind(socket).unwrap();
    let given: Given = Arc::default();
    let log = Arc::clone(&given);
    thread::spawn(move || {
        for conn in listener.incoming() {
            let mut s = conn.unwrap();
            let mut fixed = [0; 16];
            s.read_exact(&mut fixed).unwrap();
            assert_eq!(&fixed[..12], b"\r\n\r\n\0\r\nQUIT\n");
            let mut body = vec![0; u16::from_be_bytes([fixed[14], fixed[15]]) as usize];
            s.read_exact(&mut body).unwrap();
            let ip = IpAddr::from(<[u8; 4]>::try_from(&body[4..8]).unwrap());
            let port = u16::from_be_bytes([body[10], body[11]]);
            let mut first = vec![0; 2];
            s.read_exact(&mut first).unwrap();
            log.lock().unwrap().push((ip, port, first.clone()));
            s.write_all(b"bottled here, you said: ").unwrap();
            s.write_all(&first).unwrap();
        }
    });
    given
}

/// Connects to `to` from the bottle, sends "hi", and says what came back, or how it failed.
const CLIENT: &str = r#"
import socket, sys
s = socket.create_connection((sys.argv[1], int(sys.argv[2])), timeout=5)
s.sendall(b"hi")
try:
    data = b""
    while True:
        chunk = s.recv(4096)
        if not chunk:
            break
        data += chunk
    print(data.decode())
except ConnectionResetError:
    print("reset")
"#;

#[test]
fn connections_are_decided_by_the_host_and_names_resolved_there() {
    if !root() {
        eprintln!("skipped: needs root (see the top of agent/tests/net.rs)");
        return;
    }
    let host = host_address();
    let (routed, refused) = (4711, 4712);

    // Short: overlayfs takes only so many characters of lowerdir.
    let dir = PathBuf::from(format!("/tmp/ban{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("h")).unwrap();
    let given = service(&dir.join("service.sock"));
    let policy = serde_json::json!({"routes": [
        {"address": host, "port": routed, "socket": dir.join("service.sock")}]});
    let listener = UnixListener::bind(dir.join("h/net.sock")).unwrap();
    let network = Network::new(serde_json::from_value(policy).unwrap(), Some("ban".into()));
    thread::spawn(move || serve(listener, Some(network)));

    let upper = dir.join("rw.ext4");
    let mke2fs = std::env::var("BOTTLE_MKE2FS").unwrap_or("mke2fs".into());
    run(Command::new(mke2fs)
        .args(["-q", "-t", "ext4"])
        .arg(&upper)
        .arg("1G"));
    let loop_dev = run(Command::new("losetup").args(["-f", "--show"]).arg(&upper))
        .trim()
        .to_string();
    let config = dir.join("config.json");
    let json = serde_json::json!({
        "mounts": [{"source": dir.join("h"), "target": "/mnt/h"}],
        "net": {"address": "10.77.0.2/30", "host": "unix:/mnt/h/net.sock"},
    });
    fs::write(&config, json.to_string()).unwrap();
    let unshare = Command::new("unshare")
        .args(["-p", "-f", "-m", "-u", "-n", "--propagation", "private"])
        .arg(env!("CARGO_BIN_EXE_bottle-agent"))
        .args(["init", "--lower", "/", "--upper", &loop_dev, "--config"])
        .arg(&config)
        .args(["--unix", "/run/agent.sock"])
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let mut bottle = Bottle {
        unshare,
        agent_pid: 0,
        dir,
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
            if UnixStream::connect(format!("/proc/{pid}/root/run/agent.sock")).is_ok() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(20));
    }

    let ruleset = bottle.ok(&["nft", "list", "ruleset"]);
    assert!(ruleset.contains("table inet bottle"), "{ruleset}");
    assert!(ruleset.contains("redirect to :1"), "{ruleset}");
    let host = host.to_string();
    let connect = |port: u16| bottle.ok(&["python3", "-c", CLIENT, &host, &port.to_string()]);
    assert_eq!(connect(routed), "bottled here, you said: hi\n");
    // A private address no name allowed private leads to: refused, as a reset.
    assert_eq!(connect(refused), "reset\n");
    let given = given.lock().unwrap().clone();
    assert_eq!(given.len(), 1);
    let (ip, port, first) = &given[0];
    assert_eq!((ip.to_string(), *port), (host.clone(), routed));
    assert_eq!(
        first, b"hi",
        "the service is given the bytes after the header"
    );

    // The bottle's own names, then the host's: bottle.host, struck from the bottle's /etc/hosts.
    assert_eq!(
        bottle.ok(&["cat", "/etc/resolv.conf"]),
        "nameserver 127.0.0.53\n"
    );
    bottle.ok(&["sed", "-i", "/bottle.host/d", "/etc/hosts"]);
    let found = bottle.ok(&["getent", "ahostsv4", "bottle.host"]);
    assert!(found.starts_with("192.0.2.1"), "{found}");
    let (_, exit) = bottle.exec(&["getent", "hosts", "no-such-name.invalid"]);
    assert_ne!(exit, Exit::Code(0));
    drop(bottle);
}
