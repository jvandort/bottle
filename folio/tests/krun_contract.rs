//! What folio relies on bottle-cli for (bottle_cli.rs says), against a real bottle-cli: only where
//! it can run VMs, a Mac, and only with FOLIO_VM_TESTS=1 (harness::real_bottle_cli). Checks it as
//! folio drives it, word for word, with a stand-in for folio serve's services socket, on one alpine
//! machine made for the test and deleted after, its BOTTLE_HOME a temporary directory. Run it
//! after changing bottle-cli, the agent or libkrun.
//!
//! One test, its checks in order: they share the machine, which is deleted however it ends.

mod harness;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// Where a bottle reaches folio serve's git service, as folio's policy routes it: by name.
const HOST: &str = "git.bottle.internal";
const HOST_ADDRESS: &str = "192.0.2.1";
/// Where a build's directory is in its VM.
const MOUNT: &str = "/run/bottle-build";
const STANDIN: &str = "the-egress-proxy-holds-the-real-one";
/// An address the machine's policy refuses (private, no name allowed there), to see what a refused
/// flow does.
const DENIED: &str = "192.0.2.99";

/// A connection the stand-in services socket took: where to, the name and machine its PROXY header
/// said, and its first line.
#[derive(Debug, Clone)]
struct Flow {
    address: String,
    port: u16,
    name: Option<String>,
    machine: Option<String>,
    first: Vec<u8>,
}

/// folio serve's services socket, as little as bottle-cli needs: what the machine's policy routes
/// here is answered as folio serve answers git, and each connection recorded.
fn stand_in(path: &Path) -> Arc<Mutex<Vec<Flow>>> {
    let flows = Arc::new(Mutex::new(Vec::new()));
    let listener = UnixListener::bind(path).unwrap();
    let recorded = flows.clone();
    std::thread::spawn(move || {
        for connection in listener.incoming().flatten() {
            let recorded = recorded.clone();
            std::thread::spawn(move || {
                let mut stream = BufReader::new(connection);
                let mut fixed = [0u8; 16];
                if stream.read_exact(&mut fixed).is_err()
                    || &fixed[..12] != b"\r\n\r\n\x00\r\nQUIT\n"
                {
                    return;
                }
                let mut body = vec![0u8; u16::from_be_bytes([fixed[14], fixed[15]]) as usize];
                stream.read_exact(&mut body).unwrap();
                let address = format!("{}.{}.{}.{}", body[4], body[5], body[6], body[7]);
                let port = u16::from_be_bytes([body[10], body[11]]);
                let (mut name, mut machine, mut at) = (None, None, 12);
                while at + 3 <= body.len() {
                    let length = u16::from_be_bytes([body[at + 1], body[at + 2]]) as usize;
                    let value =
                        String::from_utf8_lossy(&body[at + 3..at + 3 + length]).into_owned();
                    match body[at] {
                        0x02 => name = Some(value),
                        0xE0 => machine = Some(value),
                        _ => {}
                    }
                    at += 3 + length;
                }
                let mut first = Vec::new();
                let _ = stream.read_until(b'\n', &mut first);
                recorded.lock().unwrap().push(Flow {
                    address,
                    port,
                    name,
                    machine,
                    first,
                });
                let body = b"answered by the service\n";
                let mut out = stream.into_inner();
                let _ = out.write_all(
                    &[
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .as_bytes(),
                        body,
                    ]
                    .concat(),
                );
            });
        }
    });
    flows
}

/// The contract's machine, and what it's checked with; deleted, and its home with it, when dropped.
struct Contract {
    cli: PathBuf,
    work: PathBuf,
    home: PathBuf,
    machine: String,
    flows: Arc<Mutex<Vec<Flow>>>,
}

impl Contract {
    /// bottle-cli with `args`, its stdout; its own message, if it fails.
    fn run(&self, args: &[&str], input: &str) -> Result<String, String> {
        let mut child = Command::new(&self.cli)
            .args(args)
            .env("BOTTLE_HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        if !out.status.success() {
            let said = String::from_utf8_lossy(&out.stderr);
            return Err(said
                .trim()
                .lines()
                .last()
                .unwrap_or_default()
                .trim_start_matches("bottle-cli: ")
                .to_string());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn ok(&self, args: &[&str]) -> String {
        self.run(args, "")
            .unwrap_or_else(|e| panic!("bottle-cli {args:?}: {e}"))
    }

    /// A machine's policy, as folio makes one: git by its name, and ssh to bottles, routed to the stand-in,
    /// nothing cached.
    fn network(&self) -> String {
        let socket = self.work.join("services.sock");
        json!({"allow_private": [], "cache": [], "routes": [
            {"host": HOST, "port": 80, "socket": socket},
            {"network": "192.0.2.0/24", "port": 22, "socket": socket},
        ]})
        .to_string()
    }

    fn create(
        &self,
        name: &str,
        image: &str,
        mounts: &[(&Path, &str)],
        env: &[(&str, &str)],
        cpus: u32,
    ) {
        let cpus = cpus.to_string();
        let mut args = vec![
            "create".to_string(),
            name.into(),
            "--image".into(),
            image.into(),
            "--cpus".into(),
            cpus,
        ];
        for (source, target) in mounts {
            args.extend(["-v".into(), format!("{}:{target}:ro", source.display())]);
        }
        for (key, value) in env {
            args.extend(["-e".into(), format!("{key}={value}")]);
        }
        args.extend(["--network".into(), "-".into()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args, &self.network())
            .unwrap_or_else(|e| panic!("create {name}: {e}"));
    }

    fn state(&self, name: &str) -> Option<String> {
        match self.run(&["status", name, "--json"], "") {
            Ok(out) => Some(
                serde_json::from_str::<Value>(&out).unwrap()["state"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            ),
            Err(e) if e.starts_with("no machine ") => None,
            Err(e) => panic!("status {name}: {e}"),
        }
    }

    fn delete(&self, name: &str) {
        if self.state(name).is_some() {
            let _ = self.run(&["delete", name, "-f"], "");
        }
    }

    fn exec_args(
        name: &str,
        argv: &[&str],
        user: Option<&str>,
        workdir: Option<&str>,
        env: &[(&str, &str)],
        tty: bool,
    ) -> Vec<String> {
        let mut args = vec!["exec".to_string(), name.into()];
        if tty {
            args.push("-t".into());
        }
        if let Some(user) = user {
            args.extend(["-u".into(), user.into()]);
        }
        if let Some(workdir) = workdir {
            args.extend(["-w".into(), workdir.into()]);
        }
        for (key, value) in env {
            args.extend(["-e".into(), format!("{key}={value}")]);
        }
        args.push("--".into());
        args.extend(argv.iter().map(|a| a.to_string()));
        args
    }

    fn exec(
        &self,
        name: &str,
        argv: &[&str],
        user: Option<&str>,
        workdir: Option<&str>,
        env: &[(&str, &str)],
    ) -> Result<String, String> {
        let args = Contract::exec_args(name, argv, user, workdir, env, false);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args, "")
    }

    /// `argv` in the machine, as bottle runs a command for a caller (bottle exec), bytes in and out.
    fn bytes(&self, argv: &[&str], input: &[u8]) -> Output {
        let args = Contract::exec_args(&self.machine, argv, None, None, &[], false);
        let mut child = Command::new(&self.cli)
            .args(&args)
            .env("BOTTLE_HOME", &self.home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_vec();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap();
        out
    }

    /// `argv` in the machine with a terminal of `rows` x `columns`, `react` shown what it's written
    /// so far, and the terminal, each time it writes more; till it ends. What it wrote, and how it
    /// ended (-1: it hadn't, after a minute, and was killed).
    fn on_terminal(
        &self,
        argv: &[&str],
        rows: u16,
        columns: u16,
        react: impl Fn(&[u8], &fs::File),
    ) -> (Vec<u8>, i32) {
        let (master, slave) = harness::terminal(rows, columns);
        let args = Contract::exec_args(&self.machine, argv, None, None, &[], true);
        let mut command = Command::new(&self.cli);
        command
            .args(&args)
            .env("BOTTLE_HOME", &self.home)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        harness::in_terminal(&mut command);
        let mut child = command.spawn().unwrap();
        drop(slave);
        let mut terminal = fs::File::from(master);
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if harness::readable(&terminal, 1000) {
                let mut chunk = [0u8; 1024];
                match terminal.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out.extend_from_slice(&chunk[..n]),
                }
            }
            react(&out, &terminal);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return (out, -1);
        }
        (out, child.wait().unwrap().code().unwrap_or(-1))
    }
}

impl Drop for Contract {
    fn drop(&mut self) {
        self.delete(&format!("{}-built", self.machine));
        self.delete(&self.machine);
        let _ = fs::remove_dir_all(&self.work);
    }
}

fn has(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn bottle_cli_does_what_folio_relies_on() {
    let Some(cli) = harness::real_bottle_cli() else {
        return;
    };
    let work = std::env::temp_dir().join(format!("krun-contract-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(work.join("ro")).unwrap();
    fs::write(work.join("ro/file"), "from the host\n").unwrap();
    let flows = stand_in(&work.join("services.sock"));
    let unique: String = format!(
        "{:08x}",
        std::process::id() ^ Instant::now().elapsed().subsec_nanos()
    );
    let c = Contract {
        cli,
        home: work.join("home"),
        work: work.clone(),
        machine: format!("contract-{unique}"),
        flows,
    };
    let alpine = c
        .ok(&["image", "pull", "alpine"])
        .split_whitespace()
        .last()
        .unwrap()
        .to_string();
    c.create(
        &c.machine,
        &alpine,
        &[(&work.join("ro"), "/mnt/ro")],
        &[("STANDIN", STANDIN)],
        2,
    );
    c.ok(&["start", &c.machine]);
    c.exec(
        &c.machine,
        &[
            "sh",
            "-c",
            &format!("echo '{HOST_ADDRESS} {HOST}' >> /etc/hosts"),
        ],
        Some("root"),
        None,
        &[],
    )
    .unwrap();

    a_built_image_boots(&c, &alpine);
    decided_egress(&c);
    exec(&c);
    bytes(&c);
    tty(&c);
    a_read_only_mount(&c);
    stop_and_start_keep_the_disk(&c);
}

/// folio builds images with bottle-cli (builds.rs): a recipe's steps in a VM on a registry's image,
/// its directory mounted where they run, its connections decided; then a machine boots the stage it
/// made, with the environment the recipe set.
fn a_built_image_boots(c: &Contract, _alpine: &str) {
    let context = c.work.join("probe");
    fs::create_dir_all(&context).unwrap();
    fs::write(
        context.join("install.sh"),
        "#!/bin/sh\nset -e\nwget -q -T 10 -O /dev/null https://example.com\ncat message > /hello\n",
    )
    .unwrap();
    fs::write(context.join("message"), "built by bottle-cli\n").unwrap();
    let recipe = json!({"pull": "alpine", "mount": {"source": context, "target": MOUNT},
                        "labels": {"bottle.inputs": "probe"},
                        "steps": [{"env": {"PROBE": "set by the recipe"}}, {"run": ["/bin/sh", "./install.sh"], "cwd": MOUNT}]});
    fs::write(c.work.join("recipe.json"), recipe.to_string()).unwrap();
    fs::write(
        c.work.join("build-network.json"),
        json!({"cache": []}).to_string(),
    )
    .unwrap();
    let built = Command::new(&c.cli)
        .args(["image", "build"])
        .arg(c.work.join("recipe.json"))
        .args(["--tag", "localhost/bottle/probe:latest", "--network"])
        .arg(c.work.join("build-network.json"))
        .env("BOTTLE_HOME", &c.home)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    assert!(built.status.success(), "image build");
    let disk = String::from_utf8_lossy(&built.stdout)
        .split_whitespace()
        .last()
        .unwrap()
        .to_string();
    let images: Value = serde_json::from_str(&c.ok(&["images", "--json"])).unwrap();
    let probe = images
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["tag"] == "localhost/bottle/probe:latest")
        .unwrap();
    assert_eq!(probe["labels"]["bottle.inputs"], "probe");
    let name = format!("{}-built", c.machine);
    c.create(&name, &disk, &[], &[], 1);
    c.ok(&["start", &name]);
    assert_eq!(
        c.exec(&name, &["cat", "/hello"], None, None, &[]).unwrap(),
        "built by bottle-cli\n"
    );
    assert_eq!(
        c.exec(&name, &["sh", "-c", "echo \"$PROBE\""], None, None, &[])
            .unwrap(),
        "set by the recipe\n"
    );
    c.delete(&name);
}

fn decided_egress(c: &Contract) {
    // git.bottle.internal is routed to the services socket by the name the request asks for, and
    // answered.
    let answer = c
        .exec(
            &c.machine,
            &["wget", "-qO-", "-T", "5", "http://git.bottle.internal/x"],
            None,
            None,
            &[],
        )
        .unwrap();
    assert_eq!(answer, "answered by the service\n");
    let flows: Vec<Flow> = c
        .flows
        .lock()
        .unwrap()
        .iter()
        .filter(|f| has(&f.first, b"GET /x "))
        .cloned()
        .collect();
    assert_eq!(flows.len(), 1, "{flows:?}");
    // Where the bottle connected, the name it asked for, and the machine: what bottle-cli says first.
    let flow = &flows[0];
    assert_eq!(
        (
            flow.address.as_str(),
            flow.port,
            flow.name.as_deref(),
            flow.machine.as_deref()
        ),
        (HOST_ADDRESS, 80, Some(HOST), Some(c.machine.as_str()))
    );
    assert!(
        flow.first.starts_with(b"GET /x "),
        "{:?}",
        String::from_utf8_lossy(&flow.first)
    );

    // Public flows go direct: nothing of the services socket's.
    c.exec(
        &c.machine,
        &[
            "wget",
            "-q",
            "-T",
            "10",
            "-O",
            "/dev/null",
            "https://example.com",
        ],
        None,
        None,
        &[],
    )
    .unwrap();
    assert!(!c.flows.lock().unwrap().iter().any(|f| f.port == 443));

    // A denied flow fails at once: reset, not left to time out.
    let started = Instant::now();
    let out = c.bytes(
        &[
            "wget",
            "-q",
            "-T",
            "10",
            "-O",
            "/dev/null",
            &format!("http://{DENIED}/"),
        ],
        b"",
    );
    assert!(!out.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

fn exec(c: &Contract) {
    let m = &c.machine;
    // As a user, in a directory.
    assert_eq!(
        c.exec(m, &["id", "-un"], Some("root"), None, &[])
            .unwrap()
            .trim(),
        "root"
    );
    assert_eq!(
        c.exec(
            m,
            &["sh", "-c", "echo \"$(id -un) $(pwd)\""],
            Some("nobody"),
            Some("/tmp"),
            &[]
        )
        .unwrap(),
        "nobody /tmp\n"
    );
    // The machine's environment reaches every user, and an exec's own overrides it.
    for user in [None, Some("nobody")] {
        assert_eq!(
            c.exec(m, &["sh", "-c", "echo \"$STANDIN\""], user, None, &[])
                .unwrap(),
            format!("{STANDIN}\n"),
            "{user:?}"
        );
    }
    assert_eq!(
        c.exec(
            m,
            &["sh", "-c", "echo \"$STANDIN\""],
            None,
            None,
            &[("STANDIN", "override")]
        )
        .unwrap(),
        "override\n"
    );
    // The command's status comes back.
    for status in [0, 1, 7] {
        assert_eq!(
            c.bytes(&["sh", "-c", &format!("exit {status}")], b"")
                .status
                .code(),
            Some(status)
        );
    }
    // bottle-cli's own failures say so.
    let none = format!("no-such-{m}");
    assert_eq!(c.state(&none), None);
    let failed = c.exec(&none, &["true"], None, None, &[]).unwrap_err();
    assert!(
        failed.starts_with(&format!("no machine {none}")),
        "{failed}"
    );
}

/// Snapshots and builds' output go through exec, so it carries bytes whole, both ways.
fn bytes(c: &Contract) {
    let mut blob = vec![0u8; 50_000_000];
    let mut random = fs::File::open("/dev/urandom").unwrap();
    random.read_exact(&mut blob).unwrap();
    let out = c.bytes(&["sha256sum"], &blob);
    let digest: String = sha2_hex(&blob);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap(),
        digest
    );
    let out = c.bytes(&["head", "-c", "50000000", "/dev/zero"], b"");
    assert_eq!((out.status.code(), out.stdout.len()), (Some(0), 50_000_000));
    // Stdin's end reaches the command.
    assert_eq!(
        String::from_utf8_lossy(&c.bytes(&["wc", "-c"], b"abc").stdout).trim(),
        "3"
    );
}

fn sha2_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn tty(c: &Contract) {
    // The terminal's size, and following it.
    let resized = std::cell::Cell::new(false);
    let (out, _) = c.on_terminal(
        &["sh", "-c", "stty size; sleep 4; stty size"],
        40,
        100,
        |out, terminal| {
            if !resized.get() && has(out, b"40 100") {
                harness::resize(terminal, 30, 90);
                resized.set(true);
            }
        },
    );
    let text = String::from_utf8_lossy(&out).replace('\r', "");
    assert_eq!(
        text.lines().take(2).collect::<Vec<_>>(),
        ["40 100", "30 90"],
        "{text}"
    );

    // Ctrl-C typed interrupts the command: raw here, the terminal in the machine makes it SIGINT.
    let typed = std::cell::Cell::new(false);
    let script = "trap 'echo interrupted; exit 3' INT; echo ready; while :; do sleep 0.1; done";
    let (out, code) = c.on_terminal(&["sh", "-c", script], 24, 80, |out, mut terminal| {
        if !typed.get() && has(out, b"ready") {
            terminal.write_all(b"\x03").unwrap();
            typed.set(true);
        }
    });
    assert!(
        has(&out, b"interrupted"),
        "{}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(code, 3);
}

fn a_read_only_mount(c: &Contract) {
    assert_eq!(
        c.exec(&c.machine, &["cat", "/mnt/ro/file"], None, None, &[])
            .unwrap(),
        "from the host\n"
    );
    assert!(
        !c.bytes(&["sh", "-c", "echo x > /mnt/ro/new"], b"")
            .status
            .success()
    );
    assert!(!c.work.join("ro/new").exists());
}

fn stop_and_start_keep_the_disk(c: &Contract) {
    c.exec(
        &c.machine,
        &["sh", "-c", "echo still here > /root/marker"],
        None,
        None,
        &[],
    )
    .unwrap();
    c.ok(&["stop", &c.machine]);
    assert_ne!(c.state(&c.machine).as_deref(), Some("running"));
    c.ok(&["start", &c.machine]);
    assert_eq!(c.state(&c.machine).as_deref(), Some("running"));
    assert_eq!(
        c.exec(&c.machine, &["cat", "/root/marker"], None, None, &[])
            .unwrap(),
        "still here\n"
    );
}
