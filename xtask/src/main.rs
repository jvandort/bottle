//! `cargo xtask`: bottle's build steps past cargo's own, in one place.
//!
//!     cargo xtask dist [--out DIR] [--libkrun DIR]
//!
//! Builds the distribution into DIR (target/dist): bottle-cli, with the agent its VMs run built
//! into it (a static Linux build of bottle-agent for this machine's architecture, the VMs'), and
//! the mke2fs it runs (dist/mke2fs.sh), beside it; and libkrun and libkrunfw, copied from
//! --libkrun DIR, if given. On macOS bottle-cli is signed with the hypervisor entitlement. What it
//! needs: rustup's musl target for this architecture, and what dist/mke2fs.sh needs.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("dist") => {
            parse_dist(&args[1..]).and_then(|(out, libkrun)| dist(&out, libkrun.as_deref()))
        }
        _ => Err("usage: cargo xtask dist [--out DIR] [--libkrun DIR]".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

type Result<T> = std::result::Result<T, String>;

fn parse_dist(args: &[String]) -> Result<(PathBuf, Option<PathBuf>)> {
    let mut out = root().join("target/dist");
    let mut libkrun = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--out" => out = value()?.into(),
            "--libkrun" => libkrun = Some(value()?.into()),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    Ok((out, libkrun))
}

/// The workspace's root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn dist(out: &Path, libkrun: Option<&Path>) -> Result<()> {
    let root = root();
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;

    // The VMs run this machine's architecture: the agent is a Linux build of it.
    let guest = format!("{}-unknown-linux-musl", env::consts::ARCH);
    step(&format!("the agent, for {guest}"));
    cargo(
        &[
            "build",
            "--release",
            "--target",
            &guest,
            "-p",
            "bottle-agent",
        ],
        &[],
    )
    .map_err(|e| format!("{e} (is the target installed? rustup target add {guest})"))?;
    let agent = root
        .join("target")
        .join(&guest)
        .join("release/bottle-agent");

    step("bottle-cli, the agent built in");
    cargo(
        &["build", "--release", "-p", "bottle-cli"],
        &[("BOTTLE_EMBED_AGENT", agent.as_os_str())],
    )?;
    let cli = out.join("bottle-cli");
    copy(&root.join("target/release/bottle-cli"), &cli)?;
    if cfg!(target_os = "macos") {
        step("signing bottle-cli");
        let entitlements = root.join("cli/hypervisor.entitlements");
        run(Command::new("codesign")
            .args(["--force", "-s", "-", "--entitlements"])
            .arg(&entitlements)
            .arg(&cli))?;
    }

    // mke2fs takes a while: made again only when its script has changed since.
    let built = root.join("target/mke2fs");
    let script = root.join("dist/mke2fs.sh");
    if !newer(&built.join("mke2fs"), &script) {
        step("mke2fs");
        run(Command::new(&script).arg(&built))?;
    }
    copy(&built.join("mke2fs"), &out.join("mke2fs"))?;

    if let Some(dir) = libkrun {
        step("libkrun and libkrunfw");
        let names: &[&str] = if cfg!(target_os = "macos") {
            &["libkrun.dylib", "libkrunfw.5.dylib"]
        } else {
            &["libkrun.so.1", "libkrunfw.so.5"]
        };
        for name in names {
            copy(&dir.join(name), &out.join(name))?;
        }
    }
    eprintln!("xtask: {}", out.display());
    Ok(())
}

fn step(what: &str) {
    eprintln!("xtask: {what}");
}

fn cargo(args: &[&str], env: &[(&str, &std::ffi::OsStr)]) -> Result<()> {
    let mut command = Command::new(env::var_os("CARGO").unwrap_or("cargo".into()));
    command.args(args).current_dir(root());
    for (name, value) in env {
        command.env(name, value);
    }
    run(&mut command)
}

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .map_err(|e| format!("{:?}: {e}", command.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed ({status})", command.get_program()))
    }
}

fn copy(from: &Path, to: &Path) -> Result<()> {
    // Removed first: a running bottle-cli's file is replaced, not written over.
    let _ = fs::remove_file(to);
    fs::copy(from, to)
        .map(drop)
        .map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))
}

/// Whether `path` is there, and newer than `than`.
fn newer(path: &Path, than: &Path) -> bool {
    let modified = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    matches!((modified(path), modified(than)), (Some(a), Some(b)) if a > b)
}
