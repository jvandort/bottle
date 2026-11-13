//! Linux's distribution, built on a Mac: in a Linux VM of this Mac's bottle-cli.
//!
//! First this Mac's own distribution (target/dist), whose bottle-cli then builds the builder, an
//! image of Debian with what `cargo xtask dist` takes on Linux to build for both of Linux's
//! targets (the `cc` and `rust` features, as a bottle's are installed, and the C toolchains: the
//! VM's own, and a cross one for the other architecture), and runs it as a machine, this repo
//! read-only at /src. There one `cargo xtask dist --target TRIPLE...` builds the distributions
//! asked for, by one cargo, and each comes out as a tar, over exec, into its directory here.
//!
//! The builder's machines and images are bottle-cli's in their own home, target/builder, apart from
//! bottle's: the machine is kept, stopped, between builds, its disk with cargo's builds and
//! downloads (WORK), so the next build builds only what's changed. An image built from other
//! scripts is a new machine, as is one with other memory or CPUs: $BOTTLE_BUILDER_MEM (MiB,
//! default MEM_MIB) and $BOTTLE_BUILDER_CPUS (default, all of this Mac's), to find what builds
//! best. With $BOTTLE_BUILDER_WORK=mount, WORK is this Mac's target/builder/work instead, mounted
//! there, cargo's home (its crates and git checkouts) in it too, so a new machine starts with all
//! of it: what to measure against the machine's own disk, which compiles faster or slower. Its network is
//! direct (--net-direct): it's this repo's build, fetching crates and what xtask downloads, as on a
//! Mac.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use crate::{Result, mac_dist, out_of, root, run, step, target};

/// The builder's distribution of Debian.
const DEBIAN: &str = "debian:trixie-slim";
/// Where the repo is in the builder.
const SOURCE: &str = "/src";
/// Where the builder keeps cargo's builds and the distributions it makes, on its own disk.
const WORK: &str = "/var/cache/bottle-dist";
/// The builder's memory, by default: rustc's, building libkrun, for both targets at once (killed
/// with 2048 MiB, from nothing; 3072 builds).
const MEM_MIB: &str = "4096";
/// Linux's targets, all built in the builder.
const ARCHES: [&str; 2] = ["aarch64", "x86_64"];

/// Linux's distributions, each target's into its directory, built in the builder.
pub fn dist(dists: &[(String, PathBuf)]) -> Result<()> {
    let mac = root().join("target/dist");
    mac_dist(&mac)?;
    let cli = mac.join("bottle-cli");
    // Its own home, so its images are in its own default store.
    let home = target().join("builder");
    let bottle_cli = |args: &[&str]| {
        let mut command = Command::new(&cli);
        command.args(args).env("BOTTLE_HOME", &home);
        command
    };

    step(&format!("the builder, {DEBIAN} with what builds for Linux"));
    let debian = last_line(bottle_cli(&["image", "pull", DEBIAN, "--net-direct"]))?;
    let recipe = recipe(stem(&debian)?)?;
    let file = home.join("builder.json");
    fs::write(&file, serde_json(&recipe)).map_err(|e| e.to_string())?;
    let image = last_line(bottle_cli(&[
        "image",
        "build",
        file.to_str().unwrap(),
        "--net-direct",
    ]))?;

    // A machine per image, memory and CPUs: one of others is new, and the old one goes (and any
    // from when there was a builder per architecture).
    let mem = env::var("BOTTLE_BUILDER_MEM").unwrap_or_else(|_| MEM_MIB.into());
    let cpus = env::var("BOTTLE_BUILDER_CPUS").unwrap_or_else(|_| {
        std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .to_string()
    });
    let mount = match env::var("BOTTLE_BUILDER_WORK").as_deref() {
        Ok("mount") => true,
        Ok("disk") | Err(_) => false,
        Ok(other) => return Err(format!("BOTTLE_BUILDER_WORK={other}: disk or mount")),
    };
    let work = home.join("work");
    let machine = format!(
        "builder-{}-{mem}m-{cpus}c{}",
        &stem(&image)?[..12],
        if mount { "-mount" } else { "" }
    );
    let listed: serde_json::Value = serde_json::from_slice(&output(bottle_cli(&["ls", "--json"]))?)
        .map_err(|e| format!("bottle-cli ls: {e}"))?;
    let machines: Vec<&str> = listed
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["name"].as_str())
        .collect();
    for old in machines
        .iter()
        .filter(|name| name.starts_with("builder-") && **name != machine)
    {
        run(&mut bottle_cli(&["delete", old, "-f"]))?;
    }
    if !machines.contains(&machine.as_str()) {
        let source = format!("{}:{SOURCE}:ro", root().display());
        let mut create = bottle_cli(&[
            "create", &machine, "--image", &image, "--cpus", &cpus, "--mem", &mem, "-v", &source,
        ]);
        if mount {
            fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
            create.arg("-v").arg(format!("{}:{WORK}", work.display()));
        }
        run(&mut create)?;
    }
    run(&mut bottle_cli(&["start", &machine, "--net-direct"]))?;
    let built = build_in(&bottle_cli, &machine, mount, dists);
    let stopped = run(&mut bottle_cli(&["stop", &machine]));
    built.and(stopped)?;
    // The stages the builder's machine isn't made of.
    run(&mut bottle_cli(&["gc", &image]))
}

/// One `cargo xtask dist` in the builder, for every target of `dists`; each distribution, out of
/// it, into its directory.
fn build_in(
    bottle_cli: &dyn Fn(&[&str]) -> Command,
    machine: &str,
    mount: bool,
    dists: &[(String, PathBuf)],
) -> Result<()> {
    let triples: Vec<&str> = dists.iter().map(|(t, _)| t.as_str()).collect();
    step(&format!("{}' dists, in the builder", triples.join(", ")));
    let work = Path::new(WORK).join("target");
    let target_dir = format!("CARGO_TARGET_DIR={}", work.display());
    // Mounted, cargo's home is kept there too; else it's the machine's own.
    let cargo_home = format!("CARGO_HOME={WORK}/cargo");
    let exec = |argv: &[&str]| {
        let mut args = vec!["exec", machine, "-w", SOURCE, "-e", &target_dir];
        if mount {
            args.extend(["-e", &cargo_home]);
        }
        args.push("--");
        args.extend_from_slice(argv);
        bottle_cli(&args)
    };
    let targets: String = triples.iter().map(|t| format!(" --target {t}")).collect();
    // Its progress on stderr: stdout is the tars, after.
    run(exec(&[
        "sh",
        "-c",
        &format!("cargo xtask dist{targets} 1>&2"),
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null()))?;
    // Where that put each: as this Mac's xtask would, the VM's architecture being this Mac's.
    let vm = format!("{}-unknown-linux-musl", env::consts::ARCH);
    for (triple, out) in dists {
        let there = out_of(triple, &vm, &work);
        take_out(&exec, &there, out)?;
    }
    Ok(())
}

/// The distribution at `there`, in the builder, into `out`, replacing what was there.
fn take_out(exec: &dyn Fn(&[&str]) -> Command, there: &Path, out: &Path) -> Result<()> {
    // Replaced whole: what an older one had is gone.
    let _ = fs::remove_dir_all(out);
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut tar = exec(&["tar", "-C", there.to_str().unwrap(), "-c", "."])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut extract = Command::new("tar");
    extract
        .arg("-x")
        .arg("-C")
        .arg(out)
        .stdin(tar.stdout.take().unwrap());
    let extracted = run(&mut extract);
    let status = tar.wait().map_err(|e| e.to_string())?;
    extracted?;
    if !status.success() {
        return Err(format!(
            "taking {} out of the builder failed ({status})",
            there.display()
        ));
    }
    Ok(())
}

/// The builder's recipe, on Debian's stage `debian`, for building for both of Linux's targets
/// (ARCHES): the `cc` and `rust` features as bottle installs features, their packages (and the rest
/// of what `cargo xtask dist` takes on Linux) in one apt step, then their install scripts (each's
/// directory mounted, read-only, where it runs). Labelled with a hash of all of that, so other
/// scripts make another stage.
fn recipe(debian: &str) -> Result<serde_json::Value> {
    // folio's features, borrowed: the same cc and rust a bottle gets.
    let features = root().join("folio/features");
    let user = BTreeMap::from([
        ("_REMOTE_USER", "root"),
        ("_REMOTE_USER_HOME", "/root"),
        ("HOME", "/root"),
        ("DEBIAN_FRONTEND", "noninteractive"),
    ]);
    let musl: Vec<String> = ARCHES
        .iter()
        .map(|arch| format!("{arch}-unknown-linux-musl"))
        .collect();
    let musl = musl.join(",");
    let rust = BTreeMap::from([
        ("TOOLCHAIN", "stable"),
        ("COMPONENTS", ""),
        ("TARGETS", musl.as_str()),
    ]);
    // What the features install with apt (their customizations.bottle.apt's packages), and the
    // toolchain, first and in one step, as bottle installs features'.
    let mut packages = vec![];
    for feature in ["cc", "rust"] {
        let path = features.join(feature).join("devcontainer-feature.json");
        let spec: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).map_err(|e| e.to_string())?)
                .map_err(|e| format!("{}: {e}", path.display()))?;
        let declared = &spec["customizations"]["bottle"]["apt"]["packages"];
        packages.extend(
            declared
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str())
                .map(String::from),
        );
    }
    let toolchain = toolchain(&packages);
    let mut inputs = Sha256::new();
    inputs.update(&toolchain);
    inputs.update(&musl);
    let mut steps = vec![serde_json::json!({ "run": ["/bin/sh", "-c", toolchain], "env": user })];
    for (feature, options) in [("cc", BTreeMap::new()), ("rust", rust)] {
        let script =
            fs::read(features.join(feature).join("install.sh")).map_err(|e| e.to_string())?;
        inputs.update(&script);
        let env: BTreeMap<_, _> = user.iter().chain(options.iter()).collect();
        steps.push(serde_json::json!({
            "run": ["/bin/sh", "./install.sh"],
            "env": env,
            "cwd": format!("/run/features/{feature}"),
        }));
    }
    // What the rust feature's devcontainer-feature.json gives every command.
    steps.push(serde_json::json!({ "env": { "RUSTUP_HOME": "/usr/local/rustup" } }));
    let hash: String = inputs
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(serde_json::json!({
        "from": debian,
        "mount": { "source": features, "target": "/run/features" },
        "steps": steps,
        "labels": { "bottle.builder": hash },
    }))
}

/// The shell that installs `features`' packages and what else `cargo xtask dist` takes on Linux,
/// building for each of ARCHES in a VM of this Mac's architecture: for another, Debian's cross gcc
/// and that architecture's musl and kernel headers.
fn toolchain(features: &[String]) -> String {
    let mut packages: Vec<String> = features.to_vec();
    packages.extend(
        [
            "make",
            "curl",
            "ca-certificates",
            "xz-utils",
            "libarchive-tools",
            "linux-libc-dev",
            "musl-tools",
        ]
        .map(String::from),
    );
    let mut architectures = String::new();
    for arch in ARCHES.iter().filter(|arch| **arch != env::consts::ARCH) {
        let debian = match *arch {
            "x86_64" => "amd64",
            _ => "arm64",
        };
        architectures += &format!("dpkg --add-architecture {debian}\n");
        packages.extend([
            format!("gcc-{}-linux-gnu", arch.replace('_', "-")),
            format!("musl-dev:{debian}"),
            format!("linux-libc-dev-{debian}-cross"),
        ]);
    }
    format!(
        "set -e\n{architectures}apt-get update\napt-get install -y --no-install-recommends {}\nrm -rf /var/lib/apt/lists/*\n",
        packages.join(" ")
    )
}

/// A stage's name, from its disk's path.
fn stem(disk: &str) -> Result<&str> {
    Path::new(disk)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| format!("not a stage's disk: {disk}"))
}

fn serde_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap()
}

/// What `command` prints, its progress on stderr; it must succeed.
fn output(mut command: Command) -> Result<Vec<u8>> {
    let done = command
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("{:?}: {e}", command.get_program()))?;
    if !done.status.success() {
        return Err(format!(
            "{:?} failed ({})",
            command.get_program(),
            done.status
        ));
    }
    Ok(done.stdout)
}

/// The last line `command` prints: the disk of the stage a pull or a build made.
fn last_line(command: Command) -> Result<String> {
    let printed = String::from_utf8_lossy(&output(command)?).to_string();
    printed
        .lines()
        .last()
        .map(|line| line.trim().to_string())
        .ok_or_else(|| "no stage made".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builder_installs_the_features_packages_and_the_toolchain_then_runs_their_scripts() {
        let recipe = recipe("debian-stage").unwrap();
        assert_eq!(recipe["from"], "debian-stage");
        let steps = recipe["steps"].as_array().unwrap();
        let script = steps[0]["run"][2].as_str().unwrap();
        // cc's and rust's own, from their devcontainer-feature.json, with the toolchain's.
        for package in ["gcc", "libc6-dev", "curl", "musl-tools"] {
            assert!(
                script.contains(&format!(" {package}")),
                "{package} in {script}"
            );
        }
        assert_eq!(script.matches("apt-get update").count(), 1);
        // The other architecture's cross toolchain.
        let other = match env::consts::ARCH {
            "x86_64" => "musl-dev:arm64",
            _ => "musl-dev:amd64",
        };
        assert!(script.contains(other), "{other} in {script}");
        assert_eq!(steps[1]["cwd"], "/run/features/cc");
        assert_eq!(steps[2]["cwd"], "/run/features/rust");
        assert_eq!(
            steps[2]["env"]["TARGETS"],
            "aarch64-unknown-linux-musl,x86_64-unknown-linux-musl"
        );
        assert_eq!(steps[2]["env"]["_REMOTE_USER"], "root");
        assert_eq!(steps[3]["env"]["RUSTUP_HOME"], "/usr/local/rustup");
    }

    #[test]
    fn a_stage_is_named_by_its_disk() {
        assert_eq!(stem("/s/stages/abc123.ext4").unwrap(), "abc123");
    }
}
