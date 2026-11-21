//! `cargo xtask libkrun`: libkrun as bottle-cli links it, at .libkrun, where the workspace's
//! Cargo.toml takes it from: upstream's release, pinned below, with this repo's patches (xtask/libkrun,
//! applied with `git am`, in their names' order).
//!
//!     cargo xtask libkrun
//!
//! Cargo reads that path when it loads the workspace, before it builds anything, so it's made here,
//! before cargo: by this, and first by `dist` and `test` (and bin/folio-build, which runs cargo
//! itself). Made again only when what it's made from has changed, the pin or a patch, which a stamp
//! in it (STAMP) says; a change to it means libkrun's crates are built again. The builder's xtask
//! (builder.rs), which has this checkout read-only, finds the one this Mac's made before it started.
//!
//! .libkrun is a git checkout, each patch a commit: a patch is worked on there, then written back
//! with `git format-patch --zero-commit --no-signature -o ../xtask/libkrun v1.19.8` (anything not
//! written back goes when it's made again; those flags keep a patch's file the same however
//! often it's written).
//!
//! What it needs: git.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use crate::{Result, digest, root, run, step};

/// Upstream, and the release of it: its tag, and the commit the tag's checked against (a tag can
/// be moved).
const UPSTREAM: &str = "https://github.com/libkrun/libkrun";
const TAG: &str = "v1.19.8";
const COMMIT: &str = "c4d283c233bc0ae1993c289ec2a1b4da2242f1f8";

/// In .libkrun: what it was made from (made_from).
const STAMP: &str = ".xtask-made-from";

/// .libkrun, made if what it's made from has changed since it was.
pub fn libkrun() -> Result<()> {
    let dir = root().join(".libkrun");
    let made_from = made_from()?;
    let made = || fs::read_to_string(dir.join(STAMP)).is_ok_and(|s| s == made_from);
    // Before the lock: the builder's checkout is read-only, and there it's made already.
    if made() {
        return Ok(());
    }
    // One making at a time: another waits, then finds it made.
    let lock = root().join(".libkrun.lock");
    let lock = fs::File::create(&lock).map_err(|e| format!("{}: {e}", lock.display()))?;
    lock.lock().map_err(|e| format!("locking .libkrun: {e}"))?;
    if made() {
        return Ok(());
    }

    step(&format!("libkrun {TAG}, with xtask/libkrun's patches"));
    let partial = root().join(format!(".libkrun.partial-{}", std::process::id()));
    let _ = fs::remove_dir_all(&partial);
    run(Command::new("git")
        .args([
            "-c",
            "advice.detachedHead=false",
            "clone",
            "--quiet",
            "--depth",
            "1",
        ])
        .args(["--branch", TAG, UPSTREAM])
        .arg(&partial))?;
    let head = Command::new("git")
        .arg("-C")
        .arg(&partial)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|e| format!("git rev-parse: {e}"))?;
    let head = String::from_utf8_lossy(&head.stdout);
    if head.trim() != COMMIT {
        return Err(format!("libkrun's {TAG} is {}, not {COMMIT}", head.trim()));
    }
    let patches = patches()?;
    if !patches.is_empty() {
        run(Command::new("git")
            .arg("-C")
            .arg(&partial)
            .args(["-c", "user.name=xtask", "-c", "user.email=xtask@localhost"])
            .args(["-c", "commit.gpgsign=false", "am", "--quiet"])
            .args(&patches))
        .map_err(|e| {
            format!(
                "{e}: xtask/libkrun's patches don't apply to libkrun {TAG} (see {})",
                partial.display()
            )
        })?;
    }
    fs::write(partial.join(STAMP), &made_from).map_err(|e| e.to_string())?;
    let _ = fs::remove_dir_all(&dir);
    fs::rename(&partial, &dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// The patches, in their names' order.
fn patches() -> Result<Vec<PathBuf>> {
    let dir = root().join("xtask/libkrun");
    let mut patches: Vec<PathBuf> = fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "patch"))
        .collect();
    patches.sort();
    Ok(patches)
}

/// What .libkrun's made from: the release, and each patch's sha256 and name.
fn made_from() -> Result<String> {
    let mut made_from = format!("{UPSTREAM} {TAG} {COMMIT}\n");
    for patch in patches()? {
        let name = patch.file_name().unwrap().to_string_lossy();
        made_from += &format!("{}  {name}\n", digest(&patch)?);
    }
    Ok(made_from)
}
