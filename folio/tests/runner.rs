//! `folio-cli runner` and `on`, end to end, out of a VM and without root: ssh is a stand-in that
//! runs each command here, in a home of the runner's own, and makes a forward (-R) a symlink; the
//! runner's bottle-cli is this workspace's, copied there as from a distribution; a machine's boot
//! is a stand-in running bottle-agent on the machine's socket, so exec reaches it.
//!
//! Linux only: the stand-in runner runs bottle-agent here, and bottle-agent is Linux's.
#![cfg(target_os = "linux")]

mod harness;

use harness::{eventually, manifest};

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// boot's arguments taken, bottle-agent serving its socket in the VM's place.
const FAKE_BOOT: &str = r#"#!/usr/bin/env bash
while [ $# -gt 0 ]; do
  case "$1" in
    --network|--ca|--cpus|--mem|--size|--hostname|--console|--env|--mount|--run) shift 2 ;;
    --net-direct) shift ;;
    *) exec "$BOTTLE_TEST_AGENT" --unix "$1" ;;
  esac
done
"#;

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str) -> Fixture {
        // Short: unix socket paths are.
        let dir = PathBuf::from(format!("/tmp/fc{tag}{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("remotes")).unwrap();
        // ssh: folio-standin's, which runs each command here, as if there, in a home of its
        // own, and makes a forward a symlink.
        std::os::unix::fs::symlink(harness::standin(), dir.join("ssh")).unwrap();
        fs::write(dir.join("boot"), FAKE_BOOT).unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(dir.join("boot"))
            .status()
            .unwrap();
        fs::write(dir.join("kvm"), "").unwrap();
        // The runner's distribution, for its architecture: this workspace's bottle-cli.
        let arch =
            String::from_utf8(Command::new("uname").arg("-m").output().unwrap().stdout).unwrap();
        let dist = dir.join(format!("dists/dist-{}-unknown-linux-musl", arch.trim()));
        fs::create_dir_all(&dist).unwrap();
        fs::copy(built("bottle-cli"), dist.join("bottle-cli")).unwrap();
        manifest(&dist);
        // An image: a store's stage, of nothing, which the stand-in boot never mounts.
        fs::create_dir_all(dir.join("store")).unwrap();
        fs::write(dir.join("store/s.ext4"), "").unwrap();
        fs::write(
            dir.join("store/s.json"),
            r#"{"lowers":[],"env":{},"entrypoint":null,"cmd":null,"labels":{}}"#,
        )
        .unwrap();
        Fixture { dir }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mke2fs = ["/usr/sbin/mke2fs", "/sbin/mke2fs"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .unwrap_or("mke2fs");
        let mut command = Command::new(env!("CARGO_BIN_EXE_folio"));
        command
            .args(args)
            .env("FOLIO_HOME", self.dir.join("folio"))
            .env("BOTTLE_HOME", self.dir.join("bottle"))
            .env("FOLIO_SSH", self.dir.join("ssh"))
            .env("FOLIO_RUNNER_DISTS", self.dir.join("dists"))
            .env("BOTTLE_BOOT", self.dir.join("boot"))
            .env("BOTTLE_TEST_AGENT", built("bottle-agent"))
            .env("BOTTLE_MKE2FS", mke2fs)
            .env("FAKE_REMOTES", self.dir.join("remotes"))
            .env("FAKE_KVM", self.dir.join("kvm"))
            .env("FAKE_SSH_LOG", self.dir.join("ssh.log"));
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn there(&self) -> PathBuf {
        self.dir.join("remotes/box")
    }

    fn dist(&self) -> PathBuf {
        fs::read_dir(self.dir.join("dists"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cli(&["on", "box", "delete", "m", "-f"]);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A program of bottle's, built in this workspace: run, as folio runs bottle's code.
fn built(program: &str) -> PathBuf {
    let profile = Path::new(env!("CARGO_BIN_EXE_folio"))
        .parent()
        .unwrap()
        .to_path_buf();
    let package = if program == "bottle-cli" {
        "bottle-cli"
    } else {
        "bottle-agent"
    };
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "-q", "-p", package]);
    if profile.ends_with("release") {
        build.arg("--release");
    }
    assert!(build.status().unwrap().success(), "building {package}");
    profile.join(program)
}

#[test]
fn a_runner_is_added_kept_up_to_date_and_asked_anything_bottle_cli_does() {
    let f = Fixture::new("r");
    f.ok(&["runner", "add", "box", "--ssh", "box"]);
    assert!(f.there().join(".bottle/dist/bottle-cli").exists());
    assert!(f.ok(&["runner", "ls"]).starts_with("box  box"));
    // Copied again only what's changed, file by file; nothing, when nothing has.
    let dist = f.dist();
    let copied = |out: &Output| String::from_utf8_lossy(&out.stderr).contains("copying");
    assert!(!copied(&f.cli(&["on", "box", "info", "--json"])));
    fs::write(dist.join("kernel"), "a kernel").unwrap();
    manifest(&dist);
    let out = f.cli(&["on", "box", "info", "--json"]);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("copying kernel to runner box"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(f.there().join(".bottle/dist/kernel")).unwrap(),
        "a kernel"
    );
    fs::remove_file(dist.join("kernel")).unwrap();
    manifest(&dist);
    f.ok(&["on", "box", "info", "--json"]);
    assert!(
        !f.there().join(".bottle/dist/kernel").exists(),
        "gone here, gone there"
    );
    let checked: serde_json::Value =
        serde_json::from_str(&f.ok(&["runner", "check", "box", "--json"])).unwrap();
    assert_eq!(
        (&checked["os"], &checked["kvm"], &checked["missing"]),
        (
            &serde_json::json!("Linux"),
            &serde_json::json!(true),
            &serde_json::json!([])
        )
    );
    // Any command, there: paths its own.
    let info: serde_json::Value =
        serde_json::from_str(&f.ok(&["on", "box", "info", "--json"])).unwrap();
    assert_eq!(
        info["home"].as_str().unwrap(),
        f.there().join(".bottle").display().to_string()
    );
    assert_eq!(f.ok(&["on", "box", "images", "--json"]), "[]\n");
    // Its user's home there: asked once, then kept.
    assert_eq!(
        f.ok(&["runner", "home", "box"]),
        format!("{}\n", f.there().display())
    );
    let asked = fs::read_to_string(f.dir.join("ssh.log"))
        .unwrap()
        .lines()
        .count();
    f.ok(&["runner", "home", "box"]);
    assert_eq!(
        fs::read_to_string(f.dir.join("ssh.log"))
            .unwrap()
            .lines()
            .count(),
        asked
    );

    // Its machines, by their names there.
    let image = f.dir.join("store/s.ext4").display().to_string();
    f.ok(&[
        "on",
        "box",
        "create",
        "m",
        "--image",
        &image,
        "--storage",
        "1",
    ]);
    f.ok(&["on", "box", "start", "m"]);
    assert_eq!(f.ok(&["on", "box", "status", "m"]), "running\n");
    assert_eq!(
        f.ok(&["on", "box", "exec", "m", "--", "echo", "hello there"]),
        "hello there\n"
    );
    let failed = f.cli(&["on", "box", "status", "nope"]);
    assert!(!failed.status.success());
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("bottle-cli: "),
        "bottle-cli's failure, not an unreachable runner"
    );

    // Detached while it runs, with no one to ask: nothing's stopped, and it stays attached.
    let refused = f.cli(&["runner", "detach", "box"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("m still running"));
    assert_eq!(f.ok(&["on", "box", "status", "m"]), "running\n");
    f.ok(&["on", "box", "stop", "m"]);

    // Detached, it isn't asked; used again, it's attached.
    assert!(
        f.ok(&["runner", "detach", "box"])
            .contains("safe to turn off")
    );
    assert!(f.ok(&["runner", "ls"]).contains("(detached)"));
    let used = f.cli(&["on", "box", "status", "m"]);
    assert!(
        String::from_utf8_lossy(&used.stderr).contains("runner box is back, and attached again")
    );
    assert_eq!(String::from_utf8_lossy(&used.stdout), "stopped\n");
    f.ok(&["runner", "detach", "box", "--offline"]);
    assert!(
        f.ok(&["runner", "attach", "box"])
            .contains("Runner box attached")
    );
    assert!(!f.ok(&["runner", "ls"]).contains("(detached)"));

    // Anything asked brings the runner's bottle-cli up to date first: an older one may not know
    // the command.
    fs::write(dist.join("kernel"), "a newer kernel").unwrap();
    manifest(&dist);
    let out = f.cli(&["on", "box", "delete", "m"]);
    assert!(
        out.status.success() && copied(&out),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    f.ok(&["runner", "rm", "box"]);
    assert_eq!(f.ok(&["runner", "ls"]), "");
}

#[test]
fn a_forward_makes_sockets_here_reachable_there_until_its_stopped() {
    let f = Fixture::new("f");
    f.ok(&["runner", "add", "box", "--ssh", "box"]);
    let here = f.dir.join("here.sock");
    let listener = UnixListener::bind(&here).unwrap();
    let there = f.there().join(".folio/services/git.sock");
    let pair = format!("{}={}", there.display(), here.display());
    // As folio serve starts it: the leader of a process group of its own.
    let mut forward = f
        .command(&["runner", "forward", "box", &pair])
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    eventually("the forward", || there.exists());
    let mut routed = std::os::unix::net::UnixStream::connect(&there).unwrap();
    routed.write_all(b"from the runner").unwrap();
    let (mut accepted, _) = listener.accept().unwrap();
    let mut got = [0; 15];
    accepted.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"from the runner");

    // Its ssh, dropped, is made again.
    let pid = forward.id() as i32;
    Command::new("pkill")
        .args(["-P", &pid.to_string(), "-f", "sleep"])
        .status()
        .unwrap();
    Command::new("pkill")
        .args(["-P", &pid.to_string()])
        .status()
        .unwrap();
    fs::remove_file(&there).unwrap();
    eventually("the forward made again", || there.exists());

    // Told to stop, it and its ssh end.
    harness::signal_group(pid as u32, rustix::process::Signal::TERM);
    forward.wait().unwrap();
    eventually("its ssh ended", || {
        let left = Command::new("pgrep")
            .args(["-g", &pid.to_string()])
            .output()
            .unwrap();
        left.stdout.is_empty()
    });
    let mut said = String::new();
    forward
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut said)
        .unwrap();
    assert!(said.contains("forwarding to box"), "{said}");
}

#[test]
fn a_build_sends_its_context_with_the_recipe_and_builds_there() {
    let f = Fixture::new("b");
    // The runner's bottle-cli, for this: says what the build it was given mounts.
    let dist = f.dist();
    fs::write(
        dist.join("bottle-cli"),
        "#!/bin/sh\ncase \"$1 $2\" in\n  \"image build\") echo \"built from $(cat \"$(dirname \"$3\")/context/install.sh\")\" >&2; exit 1 ;;\n  *) echo '{}' ;;\nesac\n",
    )
    .unwrap();
    manifest(&dist);
    f.ok(&["runner", "add", "box", "--ssh", "box"]);
    let context = f.dir.join("context");
    fs::create_dir_all(&context).unwrap();
    fs::write(context.join("install.sh"), "the context's install.sh").unwrap();
    let recipe = f.dir.join("recipe.json");
    fs::write(
        &recipe,
        format!(
            r#"{{"pull": "debian", "mount": {{"source": "{}", "target": "/run/b"}}, "steps": []}}"#,
            context.display()
        ),
    )
    .unwrap();
    let network = f.dir.join("network.json");
    fs::write(&network, r#"{"cache": []}"#).unwrap();
    let built = f.cli(&[
        "on",
        "box",
        "image",
        "build",
        recipe.to_str().unwrap(),
        "--tag",
        "t",
        "--network",
        network.to_str().unwrap(),
    ]);
    assert!(!built.status.success());
    let said = String::from_utf8_lossy(&built.stderr);
    assert!(
        said.contains("built from the context's install.sh"),
        "{said}"
    );
}

#[test]
fn a_runner_that_cant_be_reached_says_so() {
    let f = Fixture::new("u");
    fs::create_dir_all(f.dir.join("folio")).unwrap();
    fs::write(
        f.dir.join("folio/runners.json"),
        r#"{"version": 1, "runners": {"down": {"ssh": "down"}}}"#,
    )
    .unwrap();
    let asked = f.cli(&["on", "down", "info", "--json"]);
    assert!(!asked.status.success());
    let said = String::from_utf8_lossy(&asked.stderr);
    assert!(
        said.contains(
            "runner down is unreachable (ssh: connect to host down port 22: Connection refused"
        ),
        "{said}"
    );
}

#[test]
fn its_distribution_is_checked_once_a_connection() {
    let f = Fixture::new("d");
    f.ok(&["runner", "add", "box", "--ssh", "box"]);
    let checks = || {
        fs::read_to_string(f.dir.join("ssh.log"))
            .unwrap()
            .matches("sha256sum")
            .count()
    };
    f.ok(&["on", "box", "info", "--json"]);
    let after_first = checks();
    f.ok(&["on", "box", "info", "--json"]);
    f.ok(&["on", "box", "images", "--json"]);
    assert_eq!(
        checks(),
        after_first,
        "checked again on the same connection"
    );
    // A new connection (the runner rebooted, say): checked again.
    let control = fs::read_dir(f.dir.join("folio/ssh"))
        .unwrap()
        .flatten()
        .find(|e| e.path().extension().is_some_and(|x| x == "sock"))
        .unwrap()
        .path();
    fs::remove_file(&control).unwrap();
    f.ok(&["on", "box", "info", "--json"]);
    assert_eq!(checks(), after_first + 1);
    // This side's distribution changed: checked, and brought up to date.
    fs::write(f.dist().join("extra"), "new").unwrap();
    manifest(&f.dist());
    f.ok(&["on", "box", "info", "--json"]);
    assert_eq!(checks(), after_first + 2);
    assert!(f.there().join(".bottle/dist/extra").exists());
}
