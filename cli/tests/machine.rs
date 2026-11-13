//! `bottle-cli machine` end to end, out of a VM: $BOTTLE_BOOT is a stand-in for `bottle-cli boot`
//! (a script, written here) that runs the agent in PID, mount and UTS namespaces of its own on loop
//! devices, its socket linked to where boot's would be. The image is a store's stage "host", which
//! the stand-in takes to be this machine's own root.
//!
//! Needs root, loop devices, mke2fs and python3, so it's skipped otherwise; to run it, as root but
//! built as you:
//!
//!     CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test -p bottle-cli --test machine

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// boot's arguments taken, the agent run in namespaces in a VM's place.
const STAND_IN: &str = r#"#!/usr/bin/env python3
import json, os, subprocess, sys, time
args = sys.argv[1:]
positional, opts, env, mounts, run = [], {}, [], [], None
i = 0
while i < len(args):
    a = args[i]
    if a in ("--cpus", "--mem", "--size", "--hostname", "--console", "--network", "--ca"):
        opts[a] = args[i + 1]; i += 2
    elif a == "--env":
        k, v = args[i + 1].split("=", 1); env.append([k, v]); i += 2
    elif a == "--run":
        run = json.loads(args[i + 1]); i += 2
    elif a == "--mount":
        s, t = args[i + 1].rsplit(":", 1); mounts.append({"source": s, "target": t}); i += 2
    else:
        positional.append(a); i += 1
sock, upper, disks = positional[0], positional[1], positional[2:]
here = os.path.dirname(upper)
config = os.path.join(here, "stand-in.json")
json.dump({"env": env, "mounts": mounts, "run": run}, open(config, "w"))
json.dump(opts, open(os.path.join(here, "boot-options.json"), "w"))
loops = []
def attach(path, *flags):
    dev = subprocess.check_output(["losetup", "-f", "--show", *flags, path], text=True).strip()
    loops.append(dev); return dev
init = ["init"]
for d in disks:
    init += ["--lower", "/" if os.path.basename(d) == "host.ext4" else attach(d, "-r")]
init += ["--upper", attach(upper), "--hostname", opts["--hostname"], "--config", config,
         "--unix", "/run/agent.sock"]
unshare = subprocess.Popen(["unshare", "-p", "-f", "-m", "-u", "--propagation", "private",
                            os.environ["BOTTLE_TEST_AGENT"], *init])
for _ in range(500):
    kids = open(f"/proc/{unshare.pid}/task/{unshare.pid}/children").read().split()
    if kids and os.path.exists(f"/proc/{kids[0]}/root/run/agent.sock"):
        os.symlink(f"/proc/{kids[0]}/root/run/agent.sock", sock)
        break
    time.sleep(0.02)
code = unshare.wait()
for dev in loops:
    subprocess.call(["losetup", "-d", dev])
sys.exit(code)
"#;

fn root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// bottle-agent, built here: another package's binary, which cargo doesn't hand this test.
fn agent() -> PathBuf {
    // Where cargo put bottle-cli: target/<profile>, whatever its build directory's layout.
    let profile = Path::new(env!("CARGO_BIN_EXE_bottle-cli"))
        .parent()
        .unwrap()
        .to_path_buf();
    let cargo = env!("CARGO");
    let mut command = match std::env::var("SUDO_USER") {
        Ok(user) => {
            // As whoever owns the target directory; sudo sets its own PATH, so cargo's own
            // directory, where rustc is, goes first.
            let bin = Path::new(cargo).parent().unwrap().display().to_string();
            let path = std::env::var("PATH").unwrap_or_default();
            let mut c = Command::new("sudo");
            c.args([
                "-E",
                "-u",
                &user,
                "env",
                &format!("PATH={bin}:{path}"),
                cargo,
            ]);
            c
        }
        Err(_) => Command::new(cargo),
    };
    command.args(["build", "-q", "-p", "bottle-agent"]);
    if profile.ends_with("release") {
        command.arg("--release");
    }
    assert!(command.status().unwrap().success(), "building bottle-agent");
    profile.join("bottle-agent")
}

struct Fixture {
    dir: PathBuf,
    agent: PathBuf,
}

impl Fixture {
    fn new() -> Fixture {
        // Short: overlayfs takes only so many characters of lowerdir.
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/bcm{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("store")).unwrap();
        fs::write(dir.join("store/host.ext4"), "").unwrap();
        fs::write(
            dir.join("store/host.json"),
            r#"{"lowers":[],"env":{"GREETING":"the image's","IMAGE_ONLY":"from the image"},"entrypoint":["sh","-c"],"cmd":["echo $GREETING > /run/ran; exec sleep 1000"],"labels":{}}"#,
        )
        .unwrap();
        fs::create_dir_all(dir.join("share")).unwrap();
        fs::write(dir.join("share/f"), "shared\n").unwrap();
        let stand_in = dir.join("boot");
        fs::write(&stand_in, STAND_IN).unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(&stand_in)
            .status()
            .unwrap();
        Fixture {
            dir,
            agent: agent(),
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_bottle-cli"))
            .args(args)
            .env("BOTTLE_HOME", self.dir.join("home"))
            .env("BOTTLE_BOOT", self.dir.join("boot"))
            .env("BOTTLE_TEST_AGENT", &self.agent)
            .env(
                "BOTTLE_MKE2FS",
                std::env::var("BOTTLE_MKE2FS").unwrap_or("mke2fs".into()),
            )
            .output()
            .unwrap()
    }

    /// Runs `args`, which must succeed: its stdout.
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A machine left running by a failure: killed, its namespace and loop devices with it.
        let _ = self.cli(&["delete", "m1", "-f"]);
        let _ = self.cli(&["delete", "m2", "-f"]);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn machines_are_made_started_used_stopped_and_deleted() {
    if !root() {
        eprintln!("skipped: needs root (see the top of cli/tests/machine.rs)");
        return;
    }
    let f = Fixture::new();
    let image = f.dir.join("store/host.ext4").display().to_string();
    let share = format!("{}:/mnt/share:ro", f.dir.join("share").display());
    f.ok(&[
        "create",
        "m1",
        "--image",
        &image,
        "--storage",
        "1",
        "-e",
        "GREETING=hello there",
        "-v",
        &share,
        "--workdir",
        "/mnt/share",
        "--label",
        "thread=fix-it",
    ]);
    assert_eq!(f.ok(&["status", "m1"]), "created\n");
    let listed: serde_json::Value = serde_json::from_str(&f.ok(&["ls", "--json"])).unwrap();
    assert_eq!(
        (
            &listed[0]["name"],
            &listed[0]["state"],
            &listed[0]["labels"]
        ),
        (
            &serde_json::json!("m1"),
            &serde_json::json!("created"),
            &serde_json::json!({"thread": "fix-it"})
        )
    );
    assert_eq!(listed[0]["image"], "host"); // its top stage: no tag names it
    assert!(f.ok(&["ls"]).contains("thread=fix-it"));
    assert!(!f.cli(&["create", "m1", "--image", &image]).status.success());

    f.ok(&["start", "m1"]);
    assert_eq!(
        f.ok(&["status", "m1", "--json"]),
        "{\"name\":\"m1\",\"state\":\"running\"}\n"
    );
    let exec = |script: &str| f.ok(&["exec", "m1", "--", "sh", "-c", script]);
    assert_eq!(
        exec("cat /proc/sys/kernel/hostname; echo $GREETING; echo $IMAGE_ONLY; cat /mnt/share/f"),
        "m1\nhello there\nfrom the image\nshared\n"
    );
    // The image's environment, the machine's -e over it.
    // What the image runs is running: its entrypoint and command, with the machine's env.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !f
        .cli(&["exec", "m1", "--", "test", "-e", "/run/ran"])
        .status
        .success()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the image's command didn't run"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(exec("cat /run/ran"), "hello there\n");
    // By name, at the top level, in its creator's directory unless the command says.
    assert_eq!(f.ok(&["exec", "m1", "--", "pwd"]), "/mnt/share\n");
    assert_eq!(f.ok(&["exec", "m1", "-w", "/", "--", "pwd"]), "/\n");
    assert_eq!(f.ok(&["status", "m1"]), "running\n");
    exec("echo kept > /kept");

    f.ok(&["stop", "m1"]);
    assert_eq!(f.ok(&["status", "m1"]), "stopped\n");
    let out = f.cli(&["shell", "m1"]);
    assert!(!out.status.success()); // not started for it: bottle starts bottles
    assert!(String::from_utf8_lossy(&out.stderr).contains("m1 isn't running"));
    // Stopped, it can be renamed, all of it; running, it can't.
    f.ok(&["rename", "m1", "m2"]);
    assert!(!f.cli(&["status", "m1"]).status.success());
    assert_eq!(f.ok(&["status", "m2"]), "stopped\n");
    f.ok(&["rename", "m2", "m1"]);
    let disk = f.dir.join("home/krun/machines/m1/disk.ext4");
    let fsck = Command::new("e2fsck")
        .arg("-fn")
        .arg(&disk)
        .output()
        .unwrap();
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stdout)
    );
    assert!(!f.cli(&["exec", "m1", "--", "true"]).status.success());

    // Its disk is kept between runs.
    f.ok(&["start", "m1"]);
    assert_eq!(exec("cat /kept"), "kept\n");
    f.ok(&["stop", "m1"]);

    // gc: what a machine uses stays, stopped or not, as does what's named; the rest goes.
    let store = f.dir.join("store");
    for name in ["named", "unused"] {
        fs::write(store.join(format!("{name}.ext4")), "").unwrap();
        fs::write(
            store.join(format!("{name}.json")),
            r#"{"lowers":[],"env":{},"entrypoint":null,"cmd":null,"labels":{}}"#,
        )
        .unwrap();
    }
    let store_arg = store.display().to_string();
    assert_eq!(
        f.ok(&["gc", "--store", &store_arg, "named"]),
        "deleted unused\n"
    );
    assert!(store.join("host.ext4").exists() && store.join("named.ext4").exists());
    assert!(!store.join("unused.ext4").exists() && !store.join("unused.json").exists());

    f.ok(&["delete", "m1"]);
    assert!(!f.dir.join("home/krun/machines/m1").exists());
    assert!(!f.cli(&["status", "m1"]).status.success());
    assert_eq!(f.ok(&["ls", "--json"]), "[]\n");
}

#[test]
fn a_machine_made_with_a_network_policy_starts_with_it_and_its_caching_ca() {
    if !root() {
        eprintln!("skipped: needs root (see the top of cli/tests/machine.rs)");
        return;
    }
    let f = Fixture::new();
    let image = f.dir.join("store/host.ext4").display().to_string();
    let bad = f.dir.join("bad.json");
    fs::write(&bad, r#"{"routes": [{"host": "x.example"}]}"#).unwrap();
    let refused = f.cli(&["create", "m2", "--image", &image, "--network"]);
    assert!(!refused.status.success());
    let refused = f.cli(&[
        "create",
        "m2",
        "--image",
        &image,
        "--network",
        &bad.display().to_string(),
    ]);
    assert!(!refused.status.success(), "a policy that isn't one");
    let policy = f.dir.join("network.json");
    fs::write(
        &policy,
        r#"{"cache": [{"host": "repo.example"}],
            "routes": [{"host": "api.example", "port": 443, "socket": "/nowhere.sock"}]}"#,
    )
    .unwrap();
    f.ok(&[
        "create",
        "m2",
        "--image",
        &image,
        "--storage",
        "1",
        "--network",
        &policy.display().to_string(),
    ]);
    // Its caching CA, for its policy's hosts: what its creator installs in it.
    let pem = f.ok(&["ca", "certificate", "m2"]);
    assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"));
    // Started with nothing but its name; the network's its own.
    let refused = f.cli(&["start", "m2", "--net-direct"]);
    assert!(!refused.status.success(), "another network than its own");
    f.ok(&["start", "m2"]);
    let machine = f.dir.join("home/krun/machines/m2");
    let options: serde_json::Value =
        serde_json::from_slice(&fs::read(machine.join("boot-options.json")).unwrap()).unwrap();
    assert_eq!(
        options["--network"],
        machine.join("network.json").display().to_string()
    );
    assert_eq!(options["--ca"], "m2");
    assert_eq!(f.ok(&["ca", "certificate", "m2"]), pem); // the same CA, as it started
    // Given again, stopped only: its CA follows its hosts.
    let other = f.dir.join("other.json");
    fs::write(&other, r#"{"cache": [{"host": "other.example"}]}"#).unwrap();
    let other = other.display().to_string();
    let refused = f.cli(&["configure", "m2", "--network", &other]);
    assert!(!refused.status.success(), "running");
    f.ok(&["stop", "m2"]);
    f.ok(&["configure", "m2", "--network", &other]);
    assert_ne!(f.ok(&["ca", "certificate", "m2"]), pem);
    // Stopped, it's started by what needs it running: nothing else is needed.
    f.ok(&["exec", "m2", "--", "true"]);
    assert_eq!(f.ok(&["status", "m2"]), "running\n");
    f.ok(&["stop", "m2"]);
    f.ok(&["start", "m2"]);
    assert_eq!(f.ok(&["status", "m2"]), "running\n");
    f.ok(&["delete", "m2", "-f"]);
}
