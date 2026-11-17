//! mke2fs, built from e2fsprogs' release: what a distribution has to make disks with.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs};

use super::linux::LinuxC;
use super::{copy, fetch, run, step};

use super::Result;

/// e2fsprogs' release bottle builds mke2fs from: where it is, and its sha256 (from the signed
/// sha256sums.asc beside it).
pub(super) const E2FSPROGS: (&str, &str, &str) = (
    "1.47.4",
    "https://www.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v1.47.4/e2fsprogs-1.47.4.tar.xz",
    "fd5bf388cbdbe006a3d3b318d983b2948382440acc85a87f1e7d108653e8db0b",
);

/// mke2fs, built in `dir` from e2fsprogs' release tarball if it isn't there already: what makes
/// the empty disks VMs then write. It needs nothing installed where it runs: on Linux it's static
/// (musl: musl-gcc, from Debian's musl-tools, and the kernel's headers, linux-libc-dev, are
/// needed to build it), on macOS it links only libSystem, which every Mac has. bottle runs it,
/// never links it: e2fsprogs' library is GPL-2 in parts, bottle is Apache-2.0.
pub(super) fn mke2fs(downloads: &Path, triple: &str, c: Option<&LinuxC>) -> Result<PathBuf> {
    let (version, url, sha256) = E2FSPROGS;
    let dir = downloads.join(triple);
    let built = dir.join(format!("mke2fs-{version}"));
    if built.exists() {
        return Ok(built);
    }
    step(&format!("mke2fs, from e2fsprogs {version}"));
    let work = dir.join("build");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).map_err(|e| format!("{}: {e}", work.display()))?;
    let tarball = fetch(downloads, (url, sha256))?;
    run(Command::new("tar")
        .arg("-xf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work))?;
    let source = work.join(format!("e2fsprogs-{version}"));
    let objects = work.join("objects");
    fs::create_dir_all(&objects).map_err(|e| e.to_string())?;

    let mut env: Vec<(&str, String)> = Vec::new();
    match c {
        Some(c) => {
            env.push(("CC", c.cc()));
            env.push(("LDFLAGS", "-static".into()));
            // musl brings no kernel headers; e2fsprogs wants linux/*.h and asm/*.h.
            let headers = work.join("kernel-headers");
            fs::create_dir_all(&headers).map_err(|e| e.to_string())?;
            for (from, to) in c.headers.iter().zip(["linux", "asm-generic", "asm"]) {
                std::os::unix::fs::symlink(from, headers.join(to)).map_err(|e| e.to_string())?;
            }
            env.push(("CPPFLAGS", format!("-idirafter {}", headers.display())));
        }
        // Apple supports no static executables: libSystem stays dynamic, all else is linked in.
        None => env.push(("CC", env::var("CC").unwrap_or("cc".into()))),
    }
    let quiet = |what: &str, command: &mut Command| -> Result<()> {
        let file = fs::File::create(work.join(format!("{what}.log"))).map_err(|e| e.to_string())?;
        command
            .current_dir(&objects)
            .envs(env.iter().map(|(k, v)| (*k, v.as_str())))
            .stdout(file.try_clone().map_err(|e| e.to_string())?)
            .stderr(file);
        run(command).map_err(|e| {
            let said = fs::read_to_string(work.join(format!("{what}.log"))).unwrap_or_default();
            let tail: Vec<&str> = said.lines().rev().take(30).collect();
            let tail: Vec<&str> = tail.into_iter().rev().collect();
            format!("{e}, building mke2fs ({what}):\n{}", tail.join("\n"))
        })
    };
    // --enable-libuuid and --enable-libblkid build e2fsprogs' own; --disable would mean the
    // system's.
    quiet(
        "configure",
        Command::new(source.join("configure"))
            .args(
                c.and_then(|c| c.host.as_ref())
                    .map(|host| format!("--host={host}")),
            )
            .args([
                "--without-libarchive",
                "--enable-libuuid",
                "--enable-libblkid",
                "--disable-nls",
                "--disable-fuse2fs",
                "--disable-e2initrd-helper",
                "--disable-debugfs",
                "--disable-imager",
                "--disable-resizer",
                "--disable-defrag",
                "--disable-uuidd",
                "--disable-tdb",
                "--disable-bmap-stats",
                "--disable-tls",
                "--disable-backtrace",
                "--without-pthread",
            ]),
    )?;
    let jobs = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .to_string();
    // `libs` builds the libraries one after another, each in parallel: each library's headers
    // are there before the next needs them.
    quiet("libs", Command::new("make").args(["-j", &jobs, "libs"]))?;
    quiet(
        "mke2fs",
        Command::new("make").args(["-j", &jobs, "-C", "misc", "mke2fs"]),
    )?;
    copy(&objects.join("misc/mke2fs"), &built)?;
    let _ = fs::remove_dir_all(&work);
    Ok(built)
}
