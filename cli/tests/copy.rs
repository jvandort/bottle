//! `bottle-cli cp`, end to end, out of a VM and without root: a machine's boot is a stand-in running
//! bottle-agent on the machine's socket, so exec, which cp is a shorthand for, reaches it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
        let dir = PathBuf::from(format!("/tmp/bc{tag}{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("boot"), FAKE_BOOT).unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(dir.join("boot"))
            .status()
            .unwrap();
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
            .env("BOTTLE_BOOT", self.dir.join("boot"))
            .env("BOTTLE_TEST_AGENT", agent())
            .env("BOTTLE_MKE2FS", mke2fs)
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
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for machine in ["there", "here"] {
            let _ = self.cli(&["delete", machine, "-f"]);
        }
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

#[test]
fn cp_copies_into_out_of_and_between_machines() {
    let f = Fixture::new("c");
    let image = f.dir.join("store/s.ext4").display().to_string();
    let work = f.dir.join("work");
    fs::create_dir_all(&work).unwrap();
    let workdir = work.display().to_string();
    for name in ["here", "there"] {
        f.ok(&[
            "create",
            name,
            "--image",
            &image,
            "--storage",
            "1",
            "--workdir",
            &workdir,
        ]);
    }
    // The stand-in boot runs bottle-agent here, so "in" either machine is this one's files: each
    // copies to a directory of its own under the workdir.
    let out = f.dir.join("out");
    fs::create_dir_all(out.join("tree/sub")).unwrap();
    fs::write(out.join("tree/sub/f"), "a file").unwrap();
    let tree = out.join("tree").display().to_string();
    f.ok(&["start", "here"]);
    f.ok(&["start", "there"]);

    // In: started machines, relative to their workdir.
    fs::create_dir_all(work.join("a")).unwrap();
    assert_eq!(f.ok(&["cp", &tree, "here:a"]), "here:a/tree\n");
    assert_eq!(
        fs::read_to_string(work.join("a/tree/sub/f")).unwrap(),
        "a file"
    );
    // Between: from one machine to another.
    assert_eq!(f.ok(&["cp", "here:a/tree/sub", "there:b"]), "there:b\n");
    assert_eq!(fs::read_to_string(work.join("b/f")).unwrap(), "a file");
    // Out: into a directory here, and as a name.
    let back = f.dir.join("back");
    fs::create_dir_all(&back).unwrap();
    let to = back.display().to_string();
    assert_eq!(f.ok(&["cp", "there:b/f", &to]), format!("{to}/f\n"));
    assert_eq!(
        f.ok(&["cp", "there:b/f", &format!("{to}/g")]),
        format!("{to}/g\n")
    );
    assert_eq!(fs::read_to_string(back.join("g")).unwrap(), "a file");
    assert_eq!(
        fs::read_dir(&back).unwrap().count(),
        2,
        "nothing left beside it"
    );

    // Failures leave things as they were, saying why.
    let failed = f.cli(&["cp", "here:nope", &to]);
    assert!(!failed.status.success());
    assert!(String::from_utf8_lossy(&failed.stderr).contains("couldn't copy here:nope"));
    let failed = f.cli(&["cp", &tree, "here:a"]);
    assert!(String::from_utf8_lossy(&failed.stderr).contains("is a directory already"));
    assert!(!f.cli(&["cp", "x", "y"]).status.success());
    assert_eq!(fs::read_dir(&back).unwrap().count(), 2);

    f.ok(&["stop", "here"]);
    f.ok(&["stop", "there"]);
}
