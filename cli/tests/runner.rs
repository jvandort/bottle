//! `bottle-cli runner` and a runner's machines, `RUNNER/NAME`, end to end, out of a VM and without
//! root: ssh is a stand-in that runs each command here, in a home of the runner's own, and makes a
//! forward (-R) a symlink; the runner's bottle-cli is this one, copied there as from a
//! distribution; a machine's boot is a stand-in running bottle-agent on the machine's socket, so
//! exec reaches it.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// ssh, as far as bottle-cli uses it: options up to --, the destination, and a command, run here
/// as if there, in its own home. A forward is a symlink there to the socket here, held while this
/// runs. /dev/kvm is what $FAKE_KVM names.
const FAKE_SSH: &str = r#"#!/usr/bin/env bash
forwards=()
while [ "$1" != "--" ]; do [ "$1" = -R ] && forwards+=("$2"); shift; done
shift; destination=$1; shift
echo "$destination $*" >> "$FAKE_LOG"
home="$FAKE_REMOTES/$destination"; mkdir -p "$home"
if [ ${#forwards[@]} -gt 0 ]; then
  for f in "${forwards[@]}"; do ln -sfn "${f#*:}" "${f%%:*}"; done
  exec sleep 3600
fi
script="$*"; script=${script//\/dev\/kvm/$FAKE_KVM}
cd "$home" && exec env -u BOTTLE_HOME HOME="$home" bash -c "$script"
"#;

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
    fn new() -> Fixture {
        // Short: unix socket paths are.
        let dir = PathBuf::from(format!("/tmp/bcr{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("remotes")).unwrap();
        for (name, script) in [("ssh", FAKE_SSH), ("boot", FAKE_BOOT)] {
            fs::write(dir.join(name), script).unwrap();
            Command::new("chmod")
                .arg("+x")
                .arg(dir.join(name))
                .status()
                .unwrap();
        }
        fs::write(dir.join("kvm"), "").unwrap();
        // The runner's distribution, for its architecture: this bottle-cli.
        let arch =
            String::from_utf8(Command::new("uname").arg("-m").output().unwrap().stdout).unwrap();
        let dist = dir.join(format!("dists/dist-{}-unknown-linux-musl", arch.trim()));
        fs::create_dir_all(&dist).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_bottle-cli"), dist.join("bottle-cli")).unwrap();
        fs::write(dist.join(".sources"), "these sources").unwrap();
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

    fn cli(&self, args: &[&str]) -> Output {
        let mke2fs = ["/usr/sbin/mke2fs", "/sbin/mke2fs"]
            .into_iter()
            .find(|p| Path::new(p).exists())
            .unwrap_or("mke2fs");
        Command::new(env!("CARGO_BIN_EXE_bottle-cli"))
            .args(args)
            .env("BOTTLE_HOME", self.dir.join("home"))
            .env("BOTTLE_SSH", self.dir.join("ssh"))
            .env("BOTTLE_BOOT", self.dir.join("boot"))
            .env("BOTTLE_TEST_AGENT", agent())
            .env("BOTTLE_RUNNER_DISTS", self.dir.join("dists"))
            .env("BOTTLE_MKE2FS", mke2fs)
            .env("FAKE_REMOTES", self.dir.join("remotes"))
            .env("FAKE_KVM", self.dir.join("kvm"))
            .env("FAKE_LOG", self.dir.join("ssh.log"))
            .output()
            .unwrap()
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.cli(&["machine", "delete", "--name", "box/m", "-f"]);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// bottle-agent, built here.
fn agent() -> PathBuf {
    let profile = Path::new(env!("CARGO_BIN_EXE_bottle-cli"))
        .parent()
        .unwrap()
        .to_path_buf();
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "-q", "-p", "bottle-agent"]);
    if profile.ends_with("release") {
        build.arg("--release");
    }
    assert!(build.status().unwrap().success(), "building bottle-agent");
    profile.join("bottle-agent")
}

fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_runners_machines_are_this_machines_by_runner_slash_name_their_routes_forwarded_here() {
    let f = Fixture::new();
    f.ok(&["runner", "add", "box", "--ssh", "box"]);
    assert!(f.there().join(".bottle/dist/bottle-cli").exists());
    assert!(f.ok(&["runner", "ls"]).starts_with("box  box"));

    // Made with a route to a socket here: there, to one of its own, forwarded while it runs.
    let here = f.dir.join("here.sock");
    let policy = f.dir.join("network.json");
    fs::write(
        &policy,
        format!(
            r#"{{"routes": [{{"address": "192.0.2.1", "port": 80, "socket": "{}"}}]}}"#,
            here.display()
        ),
    )
    .unwrap();
    let image = f.dir.join("store/s.ext4").display().to_string();
    f.ok(&[
        "machine",
        "create",
        "--name",
        "box/m",
        "--image",
        &image,
        "--storage",
        "1",
        "--network",
        &policy.display().to_string(),
    ]);
    let network: serde_json::Value = serde_json::from_slice(
        &fs::read(f.there().join(".bottle/krun/machines/m/network.json")).unwrap(),
    )
    .unwrap();
    let there = network["routes"][0]["socket"].as_str().unwrap().to_string();
    assert!(
        there.starts_with(&format!("{}/.bottle/routes/", f.there().display())),
        "{there}"
    );
    let listed: serde_json::Value = serde_json::from_str(&f.ok(&["ls", "--json"])).unwrap();
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "box/m" && m["state"] == "created"),
        "{listed}"
    );

    let listener = UnixListener::bind(&here).unwrap();
    f.ok(&["start", "box/m"]);
    assert_eq!(f.ok(&["status", "box/m"]), "running\n");
    eventually("the forward", || Path::new(&there).exists());
    let mut routed = std::os::unix::net::UnixStream::connect(&there).unwrap();
    routed.write_all(b"from the runner").unwrap();
    let (mut accepted, _) = listener.accept().unwrap();
    let mut got = [0; 15];
    accepted.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"from the runner");

    // Run in it, there, as here.
    assert_eq!(
        f.ok(&["exec", "box/m", "--", "echo", "hello there"]),
        "hello there\n"
    );

    // Its forward, dropped, is made again.
    let record = f.dir.join("home/krun/runners/box/m");
    let log = || fs::read_to_string(record.join("forward.log")).unwrap_or_default();
    let pid = fs::read_to_string(record.join("forward.pid")).unwrap();
    Command::new("pkill")
        .args(["-P", pid.trim()])
        .status()
        .unwrap();
    eventually("the forward made again", || {
        log().matches("forwarding box/m").count() >= 2
    });

    // Stopped, its forward ends with it.
    let pid: i32 = pid.trim().parse().unwrap();
    f.ok(&["stop", "box/m"]);
    // SAFETY: kill with signal 0 only asks whether the process is there.
    eventually("the forward ended", || unsafe { libc::kill(pid, 0) } != 0);
    // Nothing of its forward is left: its ssh ended with it.
    eventually("its ssh ended", || {
        let left = Command::new("pgrep")
            .args(["-g", &pid.to_string()])
            .output()
            .unwrap();
        left.stdout.is_empty()
    });

    assert!(
        !f.cli(&["runner", "rm", "box"]).status.success(),
        "it has machines"
    );
    f.ok(&["machine", "delete", "--name", "box/m"]);
    assert!(!record.exists());
    f.ok(&["runner", "rm", "box"]);
    assert_eq!(f.ok(&["runner", "ls"]), "");
}
