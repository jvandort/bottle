//! What folio's user-facing tests share: folio, run as a person runs it, in a directory of its
//! own, against stand-ins for what it runs (folio-standin: bottle-cli, ssh, macOS's `security`),
//! and the files folio keeps, read as they are, without folio's own code.
//!
//! Each test's [`Folio`] has its own temporary directory with a repo in it to work in, its own
//! FOLIO_HOME (so its own folio serve, stopped when it's dropped), git's config isolated from the
//! user's, a bottle-cli whose machines are directories there, an empty Keychain, and no Obsidian.
#![allow(dead_code)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

mod serve;
// Each test binary uses what it does of these.
#[allow(unused_imports)]
pub use serve::{Serve, bind, header};

pub const FOLIO: &str = env!("CARGO_BIN_EXE_folio");
pub const STANDIN: &str = "the-egress-proxy-holds-the-real-one";

/// This workspace's target directory.
fn target() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
}

/// folio-standin, built (once a test binary).
pub fn standin() -> PathBuf {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-q", "-p", "folio-standin"])
            .status()
            .unwrap();
        assert!(status.success(), "building folio-standin");
    });
    target().join("debug").join("folio-standin")
}

/// git `args`, which must succeed; its stdout, trimmed.
pub fn git(args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// `path`, `-C`'d: git's arguments for a command in it.
pub fn git_in(path: &Path, args: &[&str]) -> String {
    let path = path.display().to_string();
    let mut all = vec!["-C", &path];
    all.extend(args);
    git(&all)
}

pub fn read_json(path: &Path) -> Option<Value> {
    fs::read_to_string(path)
        .ok()
        .map(|t| serde_json::from_str(&t).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

pub fn write_json(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_string_pretty(value).unwrap() + "\n").unwrap();
}

/// How a folio command ended, and what it said.
#[derive(Debug)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// A repo, as folio names and keys it: its origin's name, else its directory's, and a hash of its
/// path (repos.rs).
pub struct Repo {
    pub name: String,
    pub path: PathBuf,
}

impl Repo {
    pub fn of(cwd: &Path) -> Repo {
        let toplevel = PathBuf::from(git_in(cwd, &["rev-parse", "--show-toplevel"]));
        let url = Command::new("git")
            .arg("-C")
            .arg(&toplevel)
            .args(["remote", "get-url", "origin"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let from_url = url
            .trim_end_matches('/')
            .rsplit(['/', ':'])
            .next()
            .unwrap_or_default()
            .trim_end_matches(".git")
            .to_string();
        let name = if from_url.is_empty() {
            toplevel.file_name().unwrap().to_string_lossy().into_owned()
        } else {
            from_url
        };
        let name: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "._-".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let name = name.trim_matches(|c| "-._".contains(c)).to_string();
        Repo {
            name: if name.is_empty() { "repo".into() } else { name },
            path: toplevel,
        }
    }

    pub fn key(&self) -> String {
        format!("{}-{}", self.name, path_hash(&self.path))
    }
}

pub fn path_hash(path: &Path) -> String {
    Sha256::digest(path.as_os_str().as_encoded_bytes())[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The slot `current` points at in `directory`, if anything's been written there.
pub fn current_slot(directory: &Path) -> Option<PathBuf> {
    fs::read_link(directory.join("current"))
        .ok()
        .map(|slot| directory.join(slot))
}

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct Folio {
    /// The test's own directory: short, as unix sockets' paths are, and resolved, as git says it.
    pub tmp: PathBuf,
    /// Its FOLIO_HOME.
    pub home: PathBuf,
    /// The repo it works in, made with one commit on main.
    pub repo: PathBuf,
    /// The stand-in bottle-cli's state, for this machine.
    pub cli_state: PathBuf,
    pub env: Vec<(String, String)>,
}

impl Folio {
    pub fn new() -> Folio {
        let made = PathBuf::from(format!(
            "/tmp/ft{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&made);
        fs::create_dir_all(&made).unwrap();
        let tmp = made.canonicalize().unwrap();
        let bin = tmp.join("bin");
        fs::create_dir_all(&bin).unwrap();
        for name in ["bottle-cli", "security"] {
            std::os::unix::fs::symlink(standin(), bin.join(name)).unwrap();
        }
        let home = tmp.join("folio-home");
        let cli_state = tmp.join("fake-cli").join("state.json");
        let s = |p: &Path| p.display().to_string();
        let env = vec![
            ("FOLIO_HOME", s(&home)),
            ("BOTTLE_CLI", s(&bin.join("bottle-cli"))),
            ("FAKE_CLI_STATE", s(&cli_state)),
            ("BOTTLE_HOME", s(&tmp.join("bottle-home"))),
            (
                "PATH",
                format!(
                    "{}:{}",
                    bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            ),
            ("FAKE_KEYCHAIN", s(&tmp.join("keychain"))),
            // Never this machine's own Obsidian (a Mac's): not there to open, not running, its
            // vaults the test's; nor its clipboard.
            ("FOLIO_OPEN", "no-such-opener".into()),
            ("FOLIO_PGREP", "false".into()),
            ("FOLIO_OSASCRIPT", "false".into()),
            ("FOLIO_PBCOPY", "no-such-pbcopy".into()),
            ("FOLIO_OBSIDIAN_CONFIG", s(&tmp.join("obsidian.json"))),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_AUTHOR_NAME", "Test".into()),
            ("GIT_AUTHOR_EMAIL", "test@example.com".into()),
            ("GIT_COMMITTER_NAME", "Test".into()),
            ("GIT_COMMITTER_EMAIL", "test@example.com".into()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let mut folio = Folio {
            repo: PathBuf::new(),
            tmp,
            home,
            cli_state,
            env,
        };
        folio.repo = folio.make_repo("project");
        folio
    }

    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        self.env.retain(|(k, _)| k != key);
        self.env.push((key.into(), value));
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// A git command with the test's environment.
    pub fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .envs(self.env.iter().cloned())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    pub fn make_repo(&self, name: &str) -> PathBuf {
        let repo = self.tmp.join(name);
        fs::create_dir_all(&repo).unwrap();
        self.git(&["init", "-q", "-b", "main", &repo.display().to_string()]);
        self.commit(&repo, "initial");
        repo
    }

    pub fn commit(&self, repo: &Path, message: &str) -> String {
        let repo = repo.display().to_string();
        self.git(&["-C", &repo, "commit", "-q", "--allow-empty", "-m", message]);
        self.git(&["-C", &repo, "rev-parse", "HEAD"])
    }

    fn command(&self, args: &[&str], cwd: Option<&Path>, env: &[(&str, &str)]) -> Command {
        let mut command = Command::new(FOLIO);
        command
            .args(args)
            .current_dir(cwd.unwrap_or(&self.repo))
            .env_remove("FOLIO_CLI")
            .envs(self.env.iter().cloned())
            .envs(env.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        command
    }

    /// folio with `args`, in the repo, `input` its stdin (never this terminal: folio would think
    /// it had one to ask on), and `env` beside the test's.
    pub fn run(&self, args: &[&str], cwd: Option<&Path>, input: &str, env: &[(&str, &str)]) -> Out {
        let mut child = self
            .command(args, cwd, env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_string();
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
        let out = child.wait_with_output().unwrap();
        writer.join().unwrap();
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    pub fn folio(&self, args: &[&str]) -> Out {
        self.run(args, None, "", &[])
    }

    pub fn with(&self, args: &[&str], env: &[(&str, &str)]) -> Out {
        self.run(args, None, "", env)
    }

    /// folio with `args`, which must succeed: its stdout.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.folio(args);
        assert_eq!(out.code, 0, "folio {args:?}: {}", out.stderr);
        out.stdout
    }

    pub fn ok_in(&self, cwd: &Path, args: &[&str]) -> String {
        let out = self.run(args, Some(cwd), "", &[]);
        assert_eq!(out.code, 0, "folio {args:?}: {}", out.stderr);
        out.stdout
    }

    /// folio with `args` on a pseudo-terminal, as a person runs it: for each step, once what folio
    /// has written has its first part, the second typed. Its exit status, and all it wrote.
    pub fn on_terminal(&self, args: &[&str], steps: &[(&[u8], &[u8])]) -> (i32, String) {
        self.on_terminal_in(None, args, steps, &[])
    }

    pub fn on_terminal_in(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        steps: &[(&[u8], &[u8])],
        env: &[(&str, &str)],
    ) -> (i32, String) {
        let (mut master, slave) = (0, 0);
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let mut slave_fd = slave;
        // SAFETY: openpty fills in two descriptors this then owns.
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        };
        assert_eq!(opened, 0, "openpty");
        // SAFETY: each just opened, and owned from here on.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave_fd)) };
        let mut command = self.command(args, cwd, env);
        command
            .env("TERM", "xterm-256color")
            .env("LINES", "24")
            .env("COLUMNS", "80")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // SAFETY: setsid and the controlling terminal, between fork and exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        drop(slave);
        let mut terminal = fs::File::from(master);
        let fd = terminal.as_raw_fd();
        let (mut out, mut steps) = (Vec::new(), steps.to_vec());
        let deadline = Instant::now() + Duration::from_secs(20);
        // Until it's gone, and what it wrote with it: what it started (folio serve) may keep the
        // terminal open.
        let mut gone = None;
        while Instant::now() < deadline {
            if gone.is_none() && child.try_wait().unwrap().is_some() {
                gone = Some(Instant::now());
            }
            if gone.is_some_and(|at| at.elapsed() > Duration::from_millis(300)) {
                break;
            }
            let mut poll = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd, on a descriptor this holds.
            let ready = unsafe { libc::poll(&mut poll, 1, 200) };
            if ready > 0 {
                let mut chunk = [0u8; 4096];
                match terminal.read(&mut chunk) {
                    Ok(0) | Err(_) => break, // the child's gone
                    Ok(n) => out.extend_from_slice(&chunk[..n]),
                }
            }
            while let Some((wait_for, typed)) = steps.first() {
                if !out.windows(wait_for.len()).any(|w| w == *wait_for) {
                    break;
                }
                for key in typed.iter() {
                    terminal.write_all(&[*key]).unwrap();
                    std::thread::sleep(Duration::from_millis(20));
                }
                steps.remove(0);
            }
        }
        let code = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status.code().unwrap_or(-1);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break -1;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        (code, String::from_utf8_lossy(&out).replace("\r\n", "\n"))
    }

    /// The repo the test works in, as folio keys it.
    pub fn key(&self) -> String {
        Repo::of(&self.repo).key()
    }

    /// A thread's directory in folio's home.
    pub fn thread_dir(&self, thread: &str) -> PathBuf {
        self.home.join(self.key()).join(thread)
    }

    /// A thread's record.
    pub fn thread(&self, thread: &str) -> Value {
        let slot = current_slot(&self.thread_dir(thread)).expect("a thread record");
        read_json(&slot.join("thread.json")).unwrap()
    }

    /// A thread's bottle's record.
    pub fn record(&self, thread: &str) -> Value {
        let key = self.key();
        read_json(
            &self
                .thread_dir(thread)
                .join("bottles")
                .join(format!("{key}-{thread}.json")),
        )
        .unwrap()
    }

    pub fn bottle(&self, thread: &str) -> String {
        format!("{}-{thread}", self.key())
    }

    /// The stand-in bottle-cli's state, here.
    pub fn cli(&self) -> Value {
        read_json(&self.cli_state).unwrap_or_else(
            || json!({"machines": {}, "images": [], "calls": [], "execs": [], "builds": []}),
        )
    }

    pub fn set_cli(&self, state: &Value) {
        write_json(&self.cli_state, state);
    }

    /// A thread's machine, here.
    pub fn machine(&self, thread: &str) -> Value {
        self.cli()["machines"][self.bottle(thread)].clone()
    }

    /// Change a thread's machine, here, as if bottle-cli had.
    pub fn set_machine(&self, thread: &str, fields: Value) {
        let mut state = self.cli();
        let machine = &mut state["machines"][self.bottle(thread)];
        for (k, v) in fields.as_object().unwrap() {
            machine[k] = v.clone();
        }
        self.set_cli(&state);
    }

    /// What ran in a thread's machine, here, in order.
    pub fn execs(&self, thread: &str) -> Vec<Value> {
        let bottle = self.bottle(thread);
        self.cli()["execs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["machine"] == bottle.as_str())
            .cloned()
            .collect()
    }

    /// The setup steps that ran in a thread's machine, in order.
    pub fn steps(&self, thread: &str) -> Vec<String> {
        self.execs(thread)
            .iter()
            .filter_map(|e| {
                e["step"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            })
            .collect()
    }

    /// bottle-cli's calls here of `command`, each its arguments.
    pub fn calls(&self, command: &str) -> Vec<Vec<String>> {
        self.cli()["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c[0] == command)
            .map(|c| {
                c.as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect()
            })
            .collect()
    }

    /// A thread's machine's directory: where what runs in it runs.
    pub fn there(&self, thread: &str) -> PathBuf {
        self.cli_state
            .parent()
            .unwrap()
            .join("machines")
            .join(self.bottle(thread))
    }

    /// A thread's machine's workspace.
    pub fn workspace(&self, thread: &str) -> PathBuf {
        self.there(thread).join("workspace")
    }

    /// `folio list`'s rows: each thread's columns, by its name.
    pub fn rows(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.ok(&["list"])
            .lines()
            .skip(1)
            .map(|l| {
                let words: Vec<String> = l.split_whitespace().map(String::from).collect();
                (words[0].clone(), words[1..].to_vec())
            })
            .collect()
    }

    /// A runner added, `name`: ssh to it the stand-in, running "there" here, in a home of its own,
    /// and the bottle-cli folio copies there the stand-in, with a state of its own.
    pub fn add_runner(&mut self, name: &str) -> Runner {
        let short = self.tmp.join("runner");
        if !short.exists() {
            fs::create_dir_all(short.join("remotes")).unwrap();
            std::os::unix::fs::symlink(standin(), short.join("ssh")).unwrap();
            fs::write(short.join("kvm"), "").unwrap();
            // The stand-in runner's, as Linux names it: a Mac's arm64 is aarch64.
            let arch = String::from_utf8(Command::new("uname").arg("-m").output().unwrap().stdout)
                .unwrap()
                .trim()
                .replace("arm64", "aarch64");
            let dist = short
                .join("dists")
                .join(format!("dist-{arch}-unknown-linux-musl"));
            fs::create_dir_all(&dist).unwrap();
            // Each runner's own state: its home is $HOME there.
            fs::create_dir_all(short.join("bin")).unwrap();
            std::os::unix::fs::symlink(standin(), short.join("bin").join("bottle-cli")).unwrap();
            fs::write(
                dist.join("bottle-cli"),
                format!(
                    "#!/bin/sh\nFAKE_CLI_STATE=\"$HOME/.standin/state.json\" exec {} \"$@\"\n",
                    short.join("bin").join("bottle-cli").display()
                ),
            )
            .unwrap();
            Command::new("chmod")
                .arg("+x")
                .arg(dist.join("bottle-cli"))
                .status()
                .unwrap();
            manifest(&dist);
            let s = |p: PathBuf| p.display().to_string();
            for (k, v) in [
                ("FOLIO_SSH", s(short.join("ssh"))),
                ("FOLIO_RUNNER_DISTS", s(short.join("dists"))),
                ("FAKE_SSH_LOG", s(short.join("ssh.log"))),
                ("FAKE_REMOTES", s(short.join("remotes"))),
                ("FAKE_KVM", s(short.join("kvm"))),
                ("FAKE_SSH_OFF", s(short.join("off"))),
            ] {
                self.set(k, v);
            }
        }
        self.ok(&["runner", "add", name, "--ssh", name]);
        Runner {
            home: short.join("remotes").join(name),
            off: short.join("off"),
            key: self.key(),
        }
    }
}

impl Drop for Folio {
    fn drop(&mut self) {
        let _ = self
            .command(&["serve", "stop"], None, &[])
            .stdin(Stdio::null())
            .output();
        let _ = fs::remove_dir_all(&self.tmp);
    }
}

/// A runner added by a test: its home there, and the file whose presence turns every runner off.
pub struct Runner {
    pub home: PathBuf,
    pub off: PathBuf,
    key: String,
}

impl Runner {
    pub fn state_path(&self) -> PathBuf {
        self.home.join(".standin").join("state.json")
    }

    /// Its stand-in bottle-cli's state.
    pub fn cli(&self) -> Value {
        read_json(&self.state_path()).unwrap_or_else(
            || json!({"machines": {}, "images": [], "calls": [], "execs": [], "builds": []}),
        )
    }

    pub fn machine(&self, thread: &str) -> Value {
        self.cli()["machines"][format!("{}-{thread}", self.key)].clone()
    }

    pub fn set_machine(&self, thread: &str, fields: Value) {
        let mut state = self.cli();
        let machine = &mut state["machines"][format!("{}-{thread}", self.key)];
        for (k, v) in fields.as_object().unwrap() {
            machine[k] = v.clone();
        }
        write_json(&self.state_path(), &state);
    }

    /// Turned off, or on again.
    pub fn power(&self, on: bool) {
        if on {
            let _ = fs::remove_file(&self.off);
        } else {
            fs::write(&self.off, "").unwrap();
        }
    }
}

/// A distribution's manifest, as xtask writes it: each file's sha256 and name, as sha256sum writes
/// them.
pub fn manifest(dist: &Path) {
    let mut names: Vec<String> = fs::read_dir(dist)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    let lines: String = names
        .iter()
        .map(|n| {
            let digest = Sha256::digest(fs::read(dist.join(n)).unwrap());
            let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
            format!("{hex}  {n}\n")
        })
        .collect();
    fs::write(dist.join(".files"), lines).unwrap();
}

/// Wait until `done`, for as long as 20 seconds.
pub fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What each session runs, and the script that attaches to one (threads.rs).
pub const CLAUDE_SESSION: &str = r#"set --; if ls "$HOME/.claude/projects/$(pwd | sed "s|[/.]|-|g")"/*.jsonl >/dev/null 2>&1; then set -- --continue; fi; if command -v bottle-event >/dev/null; then set -- "$@" --dangerously-load-development-channels server:events; fi; exec claude "$@""#;
pub const SHELL_SESSION: &str = "exec bash -l";
pub const ATTACH: &str = r#"attach() {
  for i in $(seq 20); do [ "$(stty size)" = "0 0" ] || { shpool attach -f "$1"; return; }; sleep 0.1; done
  echo "folio: the terminal has no size, so it wasn't attached" >&2; exit 1
}
started=$(date +%s)
attach "$1" && exit 0
status=$?
[ $(($(date +%s) - started)) -le 1 ] || exit $status
echo "folio: shpool couldn't show the $1 session any more, so it's been started again" >&2
shpool kill "$1"
env TERM=xterm-256color shpool attach --background --dir "$3" --cmd "$2" "$1" || exit $status
attach "$1"
"#;

pub fn session(name: &str) -> &'static str {
    match name {
        "claude" => CLAUDE_SESSION,
        "shell" => SHELL_SESSION,
        _ => panic!("no session {name}"),
    }
}

/// `s`, quoted for sh as Python's shlex.quote has it.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// What a session runs, as shpool's --cmd takes it.
pub fn session_command(name: &str) -> String {
    ["sh", "-c", session(name)]
        .iter()
        .map(|w| shell_quote(w))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Folio {
    /// The repo's directory in folio's home.
    pub fn repo_dir(&self) -> PathBuf {
        self.home.join(self.key())
    }

    /// The repo's spec, as folio reads it: every one has the features folio needs. None if it has
    /// none saved.
    pub fn repo_spec(&self) -> Option<Value> {
        let mut spec = read_json(&self.repo_dir().join("spec.json"))?;
        let mut features: Vec<Value> = spec["features"].as_array().cloned().unwrap_or_default();
        if !features
            .iter()
            .any(|f| f.as_str().unwrap().split(':').next() == Some("tools"))
        {
            features.push(json!("tools"));
        }
        spec["features"] = json!(features);
        Some(spec)
    }

    pub fn save_spec(&self, spec: &Value) {
        let dir = self.repo_dir();
        if !dir.join("repo.json").exists() {
            let repo = Repo::of(&self.repo);
            write_json(
                &dir.join("repo.json"),
                &json!({"name": repo.name, "path": repo.path}),
            );
        }
        write_json(&dir.join("spec.json"), spec);
    }

    /// The argv of each command that ran in a thread's machine, here.
    pub fn ran(&self, thread: &str) -> Vec<Vec<String>> {
        self.execs(thread)
            .iter()
            .map(|e| {
                e["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect()
            })
            .collect()
    }
}

/// `words`, as owned strings, for comparing with what ran.
pub fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

/// What a snapshot is (snapshots.rs): its files.
pub const WORKSPACE_ARCHIVE: &str = "workspace.tar.gz";
pub const NOTES_ARCHIVE: &str = "notes.tar.gz";
pub const CLAUDE_ARCHIVE: &str = "claude.tar.gz";
pub const WORK: &str = "work.txt";
pub const SNAPSHOT: [&str; 4] = [WORKSPACE_ARCHIVE, NOTES_ARCHIVE, CLAUDE_ARCHIVE, WORK];

/// The script that writes a workspace's archive (snapshots.rs).
pub fn workspace_out(workspace: &str) -> String {
    format!(
        r#"cd {workspace} && {{ printf '.git\0'; git ls-files -z -co --exclude-standard | xargs -0 sh -c 'for f do if [ -e "$f" ] || [ -L "$f" ]; then printf "%s\0" "$f"; fi; done' sh; }} | tar -czf - --null -T -"#
    )
}

impl Folio {
    /// A bottle's record, kept with its thread's.
    pub fn bottle_record(&self, thread: &str, bottle: &str) -> Value {
        read_json(
            &self
                .thread_dir(thread)
                .join("bottles")
                .join(format!("{bottle}.json")),
        )
        .unwrap()
    }
}

impl Runner {
    /// Its stand-in bottle-cli's calls of `command`, each its arguments.
    pub fn calls(&self, command: &str) -> Vec<Vec<String>> {
        self.cli()["calls"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|c| c[0] == command)
            .map(|c| {
                c.as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect()
            })
            .collect()
    }

    /// The argv of each command that ran in machine `name` there.
    pub fn ran(&self, name: &str) -> Vec<Vec<String>> {
        self.cli()["execs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["machine"] == name)
            .map(|e| {
                e["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect()
            })
            .collect()
    }

    /// Machine `name` there, whatever its thread.
    pub fn machine_named(&self, name: &str) -> Value {
        self.cli()["machines"][name].clone()
    }

    pub fn set_machine_named(&self, name: &str, fields: Value) {
        let mut state = self.cli();
        for (k, v) in fields.as_object().unwrap() {
            state["machines"][name][k] = v.clone();
        }
        write_json(&self.state_path(), &state);
    }
}

impl Folio {
    /// Machine `name`'s directory, here: where what runs in it runs.
    pub fn machine_dir(&self, name: &str) -> PathBuf {
        self.cli_state.parent().unwrap().join("machines").join(name)
    }

    /// The argv of each command that ran in machine `name`, here.
    pub fn ran_in(&self, name: &str) -> Vec<Vec<String>> {
        self.cli()["execs"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["machine"] == name)
            .map(|e| {
                e["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_string())
                    .collect()
            })
            .collect()
    }

    /// The broker's directory: its repo's is where its vault used to be (broker.rs).
    pub fn broker_dir(&self) -> PathBuf {
        self.home
            .join(format!(
                "broker-{}",
                path_hash(&self.home.join("repos").join("broker"))
            ))
            .join("broker")
    }
}

/// A TCP server on 127.0.0.1 standing in for a bottle's machine: each connection carried to folio
/// serve's socket at `socket` as bottle-cli carries one the machine routes there, after a PROXY
/// header saying it was to bottle.host (192.0.2.1:80) from `machine`. Its port.
pub fn bridge(socket: PathBuf, machine: String) -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let (socket, machine) = (socket.clone(), machine.clone());
            std::thread::spawn(move || carry(client, &socket, &machine));
        }
    });
    port
}

fn carry(client: std::net::TcpStream, socket: &Path, machine: &str) {
    let Ok(mut there) = std::os::unix::net::UnixStream::connect(socket) else {
        return;
    };
    if there.write_all(&proxy_header(machine)).is_err() {
        return;
    }
    let (mut from_client, mut to_client) = (client.try_clone().unwrap(), client);
    let (mut from_there, mut to_there) = (there.try_clone().unwrap(), there);
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut from_client, &mut to_there);
        let _ = to_there.shutdown(std::net::Shutdown::Write);
    });
    let _ = std::io::copy(&mut from_there, &mut to_client);
    let _ = to_client.shutdown(std::net::Shutdown::Write);
    let _ = up.join();
}

/// A PROXY protocol v2 header, as bottle-cli writes one: to bottle.host, from `machine`.
pub fn proxy_header(machine: &str) -> Vec<u8> {
    header([192, 0, 2, 1], 80, Some("bottle.host"), false, machine)
}

/// The bottle-cli the tests with real VMs run: $BOTTLE_CLI, else this checkout's, built first if
/// its sources have changed (bin/bottle-build), so they never run an old one. None, said why, unless
/// asked for, with FOLIO_VM_TESTS=1, on a Mac: they boot VMs, and the first build takes minutes.
pub fn real_bottle_cli() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        eprintln!("skipped: boots VMs, which a Mac's bottle-cli does");
        return None;
    }
    if std::env::var("FOLIO_VM_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: boots VMs; run with FOLIO_VM_TESTS=1");
        return None;
    }
    if let Some(cli) = std::env::var_os("BOTTLE_CLI").filter(|c| !c.is_empty()) {
        return Some(PathBuf::from(cli));
    }
    let repo = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.."));
    let status = Command::new(repo.join("bin/bottle-build"))
        .status()
        .unwrap();
    assert!(status.success(), "bin/bottle-build");
    Some(target().join("dist").join("bottle-cli"))
}
