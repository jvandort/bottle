//! Builds and imports, out of a VM: the "VM" is the agent in PID, mount and UTS namespaces of its
//! own, on loop devices, as tests/init.rs in bottle-agent has it. The store's stage "host" stands
//! for this machine's own root, so a build has something to run on.
//!
//! Needs root, loop devices and mke2fs, so it's skipped otherwise; to run it, as root but built
//! as you:
//!
//!     CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER='sudo -E' cargo test -p bottle-disk --test build

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::Instant;

use bottle_disk::build::{Launch, Mount, Recipe, Stage, Step, Store, Vm, wait_for_agent};
use bottle_disk::{Archive, oci::sha256};
use flate2::Compression;
use flate2::write::GzEncoder;

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

/// bottle-agent as a VM gets it, a static Linux build (a stage made on nothing has no dynamic
/// loader to run anything else): another package's binary, which cargo doesn't hand this test,
/// so built here, as whoever owns the target directory rather than root.
fn agent() -> &'static PathBuf {
    static BUILT: OnceLock<PathBuf> = OnceLock::new();
    BUILT.get_or_init(|| {
        // target/<profile>/deps/<this test>
        let profile = std::env::current_exe().unwrap();
        let profile = profile.parent().unwrap().parent().unwrap();
        let target = profile.parent().unwrap();
        let triple = format!("{}-unknown-linux-musl", std::env::consts::ARCH);
        let mut args = vec!["build", "-q", "-p", "bottle-agent", "--target", &triple];
        if profile.ends_with("release") {
            args.push("--release");
        }
        let cargo = env!("CARGO");
        let mut command = match std::env::var("SUDO_USER") {
            Ok(user) => {
                // sudo sets its own PATH: cargo's own directory, where rustc is, goes first.
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
        command.args(&args);
        assert!(
            command.status().unwrap().success(),
            "building bottle-agent for {triple}"
        );
        target
            .join(&triple)
            .join(profile.file_name().unwrap())
            .join("bottle-agent")
    })
}

/// Launches the agent in namespaces: each disk on a loop device, but the store's "host", which is
/// this machine's root.
struct Namespaces;

struct Running {
    unshare: Child,
    agent_pid: u32,
    socket: PathBuf,
    loops: Vec<String>,
}

impl Launch for Namespaces {
    fn launch(
        &self,
        lowers: &[PathBuf],
        upper: &Path,
        mount: Option<&Mount>,
    ) -> std::io::Result<Box<dyn Vm>> {
        // Before any loop device: nothing to leak if this fails.
        let agent = agent();
        let mut loops = Vec::new();
        let mut args = vec!["init".to_string()];
        for disk in lowers {
            if disk.file_name().unwrap() == "host.ext4" {
                args.extend(["--lower".into(), "/".into()]);
                continue;
            }
            let dev = run(Command::new("losetup")
                .args(["-f", "-r", "--show"])
                .arg(disk))
            .trim()
            .to_string();
            args.extend(["--lower".into(), dev.clone()]);
            loops.push(dev);
        }
        let dev = run(Command::new("losetup").args(["-f", "--show"]).arg(upper))
            .trim()
            .to_string();
        args.extend(["--upper".into(), dev.clone()]);
        if let Some(m) = mount {
            // The recipe's mount, a directory bound: its source must be where the bottle's root
            // (this machine's /, as a lower) shows it, on the root filesystem, not a mount of its own.
            let config = upper.with_extension("config.json");
            let json = serde_json::json!({"mounts": [{"source": m.source, "target": m.target}]});
            fs::write(&config, json.to_string())?;
            args.extend(["--config".into(), config.display().to_string()]);
        }
        args.extend(["--unix".into(), "/run/agent.sock".into()]);
        loops.push(dev);
        let unshare = Command::new("unshare")
            .args(["-p", "-f", "-m", "-u", "--propagation", "private"])
            .arg(agent)
            .args(&args)
            .stdin(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let agent_pid = loop {
            let children = fs::read_to_string(format!("/proc/{0}/task/{0}/children", unshare.id()))
                .unwrap_or_default();
            if let Some(pid) = children.split_whitespace().next() {
                break pid.parse().unwrap();
            }
            assert!(Instant::now() < deadline, "the agent didn't start");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let socket = PathBuf::from(format!("/proc/{agent_pid}/root/run/agent.sock"));
        let mut running = Running {
            unshare,
            agent_pid,
            socket: socket.clone(),
            loops,
        };
        // Dropped if it doesn't answer: stopped, its loop devices detached.
        wait_for_agent(&socket, || matches!(running.unshare.try_wait(), Ok(None)))?;
        Ok(Box::new(running))
    }
}

impl Vm for Running {
    fn socket(&self) -> &Path {
        &self.socket
    }

    fn wait(mut self: Box<Self>) -> std::io::Result<()> {
        let status = self.unshare.wait()?;
        for dev in self.loops.drain(..) {
            run(Command::new("losetup").args(["-d", &dev]));
        }
        assert!(status.success(), "{status}");
        Ok(())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: kill with a pid: the agent is its namespace's init, everything in there ends.
        unsafe { libc::kill(self.agent_pid as libc::pid_t, libc::SIGKILL) };
        let _ = self.unshare.wait();
        for dev in &self.loops {
            let _ = Command::new("losetup").args(["-d", dev]).status();
        }
    }
}

/// An OCI archive of `layers` (tars), with `env` in its config.
fn archive(path: &Path, layers: &[Vec<u8>], env: &[&str]) {
    let mut blobs = BTreeMap::new();
    let mut descriptors = Vec::new();
    for tar in layers {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(tar).unwrap();
        let blob = gz.finish().unwrap();
        descriptors.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": sha256(&blob), "size": blob.len()}));
        blobs.insert(sha256(&blob), blob);
    }
    let diff_ids: Vec<String> = layers.iter().map(|t| sha256(t)).collect();
    let config = serde_json::to_vec(&serde_json::json!({
        "rootfs": {"type": "layers", "diff_ids": diff_ids},
        "config": {"Env": env, "Cmd": ["sh"]}}))
    .unwrap();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json",
                   "digest": sha256(&config), "size": config.len()},
        "layers": descriptors}))
    .unwrap();
    let index = serde_json::to_vec(&serde_json::json!({"schemaVersion": 2, "manifests": [
        {"mediaType": "application/vnd.oci.image.manifest.v1+json",
         "digest": sha256(&manifest), "size": manifest.len()}]}))
    .unwrap();
    blobs.insert(sha256(&config), config);
    blobs.insert(sha256(&manifest), manifest);
    let mut b = tar::Builder::new(Vec::new());
    let mut add = |name: &str, data: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        b.append_data(&mut h, name, data).unwrap();
    };
    add("index.json", &index);
    for (digest, blob) in &blobs {
        add(&format!("blobs/sha256/{}", &digest[7..]), blob);
    }
    fs::write(path, b.into_inner().unwrap()).unwrap();
}

/// A layer of `files` (path, content); a path ending in / is a directory.
fn layer(files: &[(&str, &str)]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (path, content) in files {
        let mut h = tar::Header::new_gnu();
        h.set_mtime(1_700_000_000);
        h.set_uid(0);
        h.set_gid(0);
        if let Some(dir) = path.strip_suffix('/') {
            h.set_entry_type(tar::EntryType::Directory);
            h.set_mode(0o755);
            h.set_size(0);
            b.append_data(&mut h, dir, &[][..]).unwrap();
        } else {
            h.set_mode(0o644);
            h.set_size(content.len() as u64);
            b.append_data(&mut h, path, content.as_bytes()).unwrap();
        }
    }
    b.into_inner().unwrap()
}

/// The store, in a short path (overlayfs takes only so many characters of lowerdir), with a
/// "host" stage: removed when dropped.
struct Fixture {
    store: Store,
}

impl Fixture {
    fn new() -> Fixture {
        // On the root filesystem, which the builds' bottles see as their lower, so the recipe's
        // mount can be bound from there; short, as overlayfs wants lowerdir.
        let dir = PathBuf::from(format!("/var/tmp/bdb{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("host.ext4"), "").unwrap();
        fs::write(
            dir.join("host.json"),
            serde_json::to_vec(&Stage::default()).unwrap(),
        )
        .unwrap();
        let mke2fs = std::env::var("BOTTLE_MKE2FS")
            .unwrap_or("mke2fs".into())
            .into();
        Fixture {
            store: Store { dir, mke2fs },
        }
    }

    /// Runs `check` on the stack of stage `name`'s disks, mounted read-only.
    fn look(&self, name: &str, check: impl FnOnce(&Path)) {
        let disks = self.store.disks(name).unwrap();
        let mut lowers = Vec::new();
        let mut mounts = Vec::new();
        for (i, disk) in disks.iter().enumerate() {
            if disk.file_name().unwrap() == "host.ext4" {
                lowers.push("/".to_string());
                continue;
            }
            let m = self.store.dir.join(format!("m{i}"));
            fs::create_dir_all(&m).unwrap();
            run(Command::new("mount")
                .args(["-o", "ro,loop"])
                .arg(disk)
                .arg(&m));
            lowers.push(m.join("rootfs").display().to_string());
            mounts.push(m);
        }
        let st = self.store.dir.join("st");
        fs::create_dir_all(&st).unwrap();
        if lowers.len() == 1 {
            run(Command::new("mount")
                .args(["--bind", "-o", "ro", &lowers[0]])
                .arg(&st));
        } else {
            lowers.reverse();
            run(Command::new("mount")
                .args(["-t", "overlay", "overlay", "-o"])
                .arg(format!("ro,lowerdir={}", lowers.join(":")))
                .arg(&st));
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&st)));
        let _ = Command::new("umount").arg(&st).status();
        for m in mounts {
            let _ = Command::new("umount").arg(&m).status();
        }
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.store.dir);
    }
}

#[test]
fn imports_an_image_then_builds_on_a_stage() {
    if !root() {
        eprintln!("skipped: needs root (see the top of disk/tests/build.rs)");
        return;
    }
    let f = Fixture::new();

    let path = f.store.dir.join("image.tar");
    archive(
        &path,
        &[
            layer(&[
                ("etc/", ""),
                ("etc/a", "a\n"),
                ("d/", ""),
                ("d/x", "x\n"),
                ("d/y", "y\n"),
                ("e/", ""),
                ("e/old", ""),
            ]),
            layer(&[("d/.wh.x", ""), ("e/.wh..wh..opq", ""), ("e/new", "new\n")]),
        ],
        &["PATH=/usr/bin:/bin", "LANG=C.UTF-8"],
    );
    let archive = Archive::open(&path).unwrap();
    let image = archive.image().unwrap();
    let imported = f.store.import(&archive, &image, &Namespaces).unwrap();
    let stage = f.store.stage(&imported).unwrap();
    assert_eq!(stage.lowers, Vec::<String>::new());
    assert_eq!(stage.env.get("LANG").map(String::as_str), Some("C.UTF-8"));
    assert_eq!(stage.cmd, Some(vec!["sh".to_string()]));
    f.look(&imported, |root| {
        assert_eq!(fs::read_to_string(root.join("etc/a")).unwrap(), "a\n");
        assert!(!root.join("d/x").exists());
        assert!(root.join("d/y").exists());
        assert!(!root.join("e/old").exists());
        assert_eq!(fs::read_to_string(root.join("e/new")).unwrap(), "new\n");
    });
    // Imported again, it's the stage there.
    assert_eq!(
        f.store.import(&archive, &image, &Namespaces).unwrap(),
        imported
    );

    // A features directory, as bottle has one: each feature's install.sh, and what's beside it.
    let features = f.store.dir.join("features");
    fs::create_dir_all(features.join("thing")).unwrap();
    fs::write(features.join("thing/payload"), "from beside install.sh\n").unwrap();
    fs::write(
        features.join("thing/install.sh"),
        "#!/bin/sh\nset -e\ncp ./payload /payload\necho installed > /installed\n",
    )
    .unwrap();
    run(Command::new("chmod")
        .arg("+x")
        .arg(features.join("thing/install.sh")));
    let recipe = Recipe {
        from: Some("host".into()),
        mount: Some(Mount {
            source: features.clone(),
            target: "/run/features".into(),
        }),
        steps: vec![
            Step::Run {
                run: vec!["./install.sh".into()],
                env: BTreeMap::new(),
                cwd: Some("/run/features/thing".into()),
            },
            Step::Env {
                env: [("GREETING".into(), "hello".into())].into(),
            },
            Step::Run {
                run: vec![
                    "sh".into(),
                    "-c".into(),
                    "echo $GREETING $EXTRA > /greeting && rm -rf /etc/motd".into(),
                ],
                env: [("EXTRA".into(), "there".into())].into(),
                cwd: None,
            },
        ],
        cmd: Some(vec!["bash".into()]),
        ..Recipe::default()
    };
    let built = f.store.build(&recipe, &Namespaces).unwrap();
    let stage = f.store.stage(&built).unwrap();
    assert_eq!(stage.lowers, ["host"]);
    assert_eq!(stage.env.get("GREETING").map(String::as_str), Some("hello"));
    assert!(
        !stage.env.contains_key("EXTRA"),
        "a step's own env isn't the image's"
    );
    assert_eq!(stage.cmd, Some(vec!["bash".to_string()]));
    f.look(&built, |root| {
        assert_eq!(
            fs::read_to_string(root.join("installed")).unwrap(),
            "installed\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("greeting")).unwrap(),
            "hello there\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("payload")).unwrap(),
            "from beside install.sh\n"
        );
        assert!(!root.join("etc/motd").exists());
        // What was mounted isn't in the stage.
        assert!(!root.join("run/features").exists());
    });
    // Built again, it's the stage there, not another build.
    let start = Instant::now();
    assert_eq!(f.store.build(&recipe, &Namespaces).unwrap(), built);
    assert!(start.elapsed().as_secs() < 2);
}
