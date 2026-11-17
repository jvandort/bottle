//! `cargo xtask`: bottle's build steps past cargo's own, in one place.
//!
//!     cargo xtask dist [--release] [--target TRIPLE]... [--out DIR]
//!     cargo xtask test [--root] [--vm] [-- CARGO_TEST_ARGS...]
//!
//! `test` is the workspace's tests, with the programs they run built for them (see test.rs).
//!
//! Builds a distribution for each TRIPLE (this machine's own, if none is given) into
//! $CARGO_TARGET_DIR's dist for this machine's own, or dist-TRIPLE for another (target/, if it isn't
//! set); into DIR instead, for one TRIPLE. A distribution is bottle-cli, with the agent its VMs run built into it (a static Linux build of
//! bottle-agent for the target's architecture, the VMs'), and libkrun linked into it, the mke2fs it
//! runs, built from e2fsprogs' release, beside it, and the kernel the VMs boot: libkrunfw's, a
//! release of its pinned below, downloaded and checked, as `kernel`: an arm64 Image, out of
//! upstream's prebuilt bundle, or, on x86_64, an ELF of the kernel in Fedora's package.
//!
//! TRIPLE is one of TARGETS: aarch64-apple-darwin, built on a Mac, with bottle-cli signed with the
//! hypervisor entitlement; or Linux on arm64 or x86_64, static with musl, so it runs on any
//! distribution. On Linux, Linux's are built here, for either architecture, all of them by one cargo
//! (it builds what they share once, and the rest in parallel). On a Mac, Linux's are built so in a
//! Linux VM of this Mac's bottle-cli (see builder.rs), so `cargo xtask dist` comes first.
//!
//! What it needs: rustup's musl target for the VMs' architecture, curl and make; on a Mac, Xcode's
//! command line tools; on Linux, gcc, musl's specs for the target (Debian's musl-tools, or for
//! another architecture its cross gcc, gcc-x86-64-linux-gnu say, with musl-dev:amd64 and
//! linux-libc-dev-amd64-cross), the kernel's headers, and for x86_64's kernel bsdtar, or rpm2cpio
//! and cpio, to open Fedora's package. Cargo's builds, and this step's, are in $CARGO_TARGET_DIR
//! if it's set.
//!
//! Incremental: cargo builds what's changed, and a file of the distribution is copied (and on a Mac
//! signed) only when what it's copied from has. One build of a distribution at a time: another
//! waits for it. Whether one is needed at all, without starting cargo, is bin/bottle-build's, which
//! builds every distribution this machine builds.
//!
//! With --release, a release's (by default, of every TRIPLE this machine builds): the same files, built with cargo's `distribution` profile (no
//! symbols, optimized across crates: smaller, and slower to build), with folio, built so too, beside
//! bottle-cli (copied to runners with the rest; the dev shims build their own), and Linux's mke2fs
//! stripped,
//! into $CARGO_TARGET_DIR's release-dists, as dist and dist-TRIPLE there (so a release's
//! bottle-cli finds the other distributions it copies to runners among the release's, beside it).
//! Without it, cargo's release profile, which the dev shims build, quickly.
//! Every distribution has a manifest of its files (MANIFEST): each one's sha256 and name, as
//! sha256sum writes them, which is what a runner's copy is compared with (bottle-cli's runner.rs).

use kernel::kernel;
use linux::{LinuxC, linux_dists};
use mac::mac_dist;
use mke2fs::mke2fs;

mod builder;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

mod kernel;
mod linux;
mod mac;
mod mke2fs;
mod test;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("dist") => {
            parse_dist(&args[1..]).and_then(|(profile, targets, out)| dist(profile, targets, out))
        }
        Some("test") => test::test(&args[1..]),
        _ => Err(
            "usage: cargo xtask dist [--release] [--target TRIPLE]... [--out DIR]\n       \
                  cargo xtask test [--root] [--vm] [-- CARGO_TEST_ARGS...]"
                .into(),
        ),
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

/// How a distribution's built: as the dev shims have it, or as a release.
#[derive(Clone, Copy, PartialEq)]
enum Profile {
    /// Cargo's release profile: quick to build.
    Dev,
    /// Cargo's distribution profile, and mke2fs stripped: small.
    Release,
}

impl Profile {
    /// How cargo's told to build with it.
    fn cargo_args(self) -> &'static [&'static str] {
        match self {
            Profile::Dev => &["--release"],
            Profile::Release => &["--profile", "distribution"],
        }
    }

    /// The directory of cargo's target directory (or a target's in it) its builds are in.
    fn dir(self) -> &'static str {
        match self {
            Profile::Dev => "release",
            Profile::Release => "distribution",
        }
    }

    /// Where its distributions go, in `target_dir`, by default (see out_of).
    fn dists(self, target_dir: &Path) -> PathBuf {
        match self {
            Profile::Dev => target_dir.to_path_buf(),
            Profile::Release => target_dir.join("release-dists"),
        }
    }

    /// What cargo builds for a distribution: bottle-cli, and for a release, folio too (the dev
    /// shims build their own folio).
    fn packages(self) -> &'static [&'static str] {
        match self {
            Profile::Dev => &["-p", "bottle-cli"],
            Profile::Release => &["-p", "bottle-cli", "-p", "folio"],
        }
    }

    /// How `cargo xtask dist` is told to build with it.
    fn flag(self) -> &'static str {
        match self {
            Profile::Dev => "",
            Profile::Release => " --release",
        }
    }
}

fn parse_dist(args: &[String]) -> Result<(Profile, Vec<String>, Option<PathBuf>)> {
    let (mut profile, mut targets, mut out) = (Profile::Dev, vec![], None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--release" => profile = Profile::Release,
            "--target" => targets.push(args.next().ok_or("--target needs a value")?.clone()),
            "--out" => out = Some(args.next().ok_or("--out needs a value")?.into()),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    Ok((profile, targets, out))
}

/// What a distribution is built for.
const TARGETS: [&str; 3] = [
    "aarch64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
];

/// This machine's own target.
fn host() -> String {
    let arch = env::consts::ARCH;
    if cfg!(target_os = "macos") {
        format!("{arch}-apple-darwin")
    } else {
        format!("{arch}-unknown-linux-musl")
    }
}

/// The workspace's root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// Where cargo builds, and the downloads and builds of this step are kept: $CARGO_TARGET_DIR, as
/// cargo has it (a runner keeps one apart from the code it's sent), else the workspace's target.
fn target() -> PathBuf {
    env::var_os("CARGO_TARGET_DIR").map_or_else(|| root().join("target"), |dir| root().join(dir))
}

/// Where `target`'s distribution goes, by default, from a machine whose own target is `host`:
/// `target_dir`/dist for its own, `target_dir`/dist-TARGET for another.
fn out_of(target: &str, host: &str, target_dir: &Path) -> PathBuf {
    match target == host {
        true => target_dir.join("dist"),
        false => target_dir.join(format!("dist-{target}")),
    }
}

fn dist(profile: Profile, mut targets: Vec<String>, out: Option<PathBuf>) -> Result<()> {
    let host = host();
    // By default, this machine's own; a release, every one this machine builds (a Mac's, on a
    // Mac, and Linux's, either way).
    if targets.is_empty() {
        match profile {
            Profile::Dev => targets.push(host.clone()),
            Profile::Release => targets.extend(
                TARGETS
                    .iter()
                    .filter(|t| cfg!(target_os = "macos") || !t.ends_with("-apple-darwin"))
                    .map(|t| t.to_string()),
            ),
        }
    }
    targets.dedup();
    if out.is_some() && targets.len() > 1 {
        return Err("--out is for one --target".into());
    }
    if let Some(target) = targets.iter().find(|t| !TARGETS.contains(&t.as_str())) {
        return Err(format!(
            "no dist for {target}: one of {}",
            TARGETS.join(", ")
        ));
    }
    let outs: Vec<(String, PathBuf)> = targets
        .iter()
        .map(|t| {
            let dists = profile.dists(&target());
            let out = out.clone().unwrap_or_else(|| out_of(t, &host, &dists));
            (t.clone(), out)
        })
        .collect();
    // One build of each at a time: another waits.
    let mut locks = vec![];
    for (_, out) in &outs {
        fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
        let name = out
            .file_name()
            .map_or("dist".into(), |n| n.to_string_lossy());
        let lock = out.with_file_name(format!(".{name}.lock"));
        let lock = fs::File::create(&lock).map_err(|e| format!("{}: {e}", lock.display()))?;
        lock.lock()
            .map_err(|e| format!("locking {}: {e}", out.display()))?;
        locks.push(lock);
    }
    let release = if profile == Profile::Release {
        "a release of "
    } else {
        ""
    };
    eprintln!(
        "xtask: building {release}bottle-cli for {}...",
        targets.join(", ")
    );
    let (mac, linux): (Vec<_>, Vec<_>) = outs
        .iter()
        .cloned()
        .partition(|(t, _)| t.ends_with("-apple-darwin"));
    if let Some((target, out)) = mac.first() {
        if !cfg!(target_os = "macos") {
            return Err(format!("{target}'s dist is built on a Mac"));
        }
        mac_dist(profile, out)?;
    }
    if !linux.is_empty() {
        match cfg!(target_os = "macos") {
            true => builder::dist(profile, &linux)?,
            false => linux_dists(profile, &linux)?,
        }
    }
    for (_, out) in &outs {
        manifest(out)?;
        eprintln!("xtask: {}", out.display());
    }
    Ok(())
}

/// Beside a distribution's files: each one's sha256 and name, as sha256sum writes them.
const MANIFEST: &str = ".files";

/// Writes `out`'s manifest (MANIFEST): every file in it but those whose names start with a dot,
/// by name.
fn manifest(out: &Path) -> Result<()> {
    let mut names: Vec<String> = fs::read_dir(out)
        .map_err(|e| format!("{}: {e}", out.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    let mut listed = String::new();
    for name in names {
        listed += &format!("{}  {name}\n", digest(&out.join(&name))?);
    }
    fs::write(out.join(MANIFEST), listed).map_err(|e| format!("{}: {e}", out.display()))
}

/// The agent, a static Linux build for each of `arches` (the VMs' architectures), by one cargo.
fn agents(profile: Profile, arches: &[&str]) -> Result<Vec<PathBuf>> {
    let guests: Vec<String> = arches
        .iter()
        .map(|arch| format!("{arch}-unknown-linux-musl"))
        .collect();
    step(&format!("the agent, for {}", guests.join(", ")));
    let mut args = vec!["build", "-p", "bottle-agent"];
    args.extend(profile.cargo_args());
    for guest in &guests {
        args.extend(["--target", guest]);
    }
    cargo(&args, &[]).map_err(|e| {
        format!(
            "{e} (are the targets installed? rustup target add {})",
            guests.join(" ")
        )
    })?;
    Ok(guests
        .iter()
        .map(|guest| {
            target()
                .join(guest)
                .join(profile.dir())
                .join("bottle-agent")
        })
        .collect())
}

/// What a distribution has beside bottle-cli, none of it cargo's: mke2fs, built for `triple`, and
/// the kernel for `arch`, the VMs' architecture. A release's Linux mke2fs is stripped (static, it
/// carries musl's symbols too); a Mac's links libSystem, and is left as the linker signed it.
fn beside_cli(
    profile: Profile,
    triple: &str,
    arch: &str,
    c: Option<&LinuxC>,
    out: &Path,
) -> Result<()> {
    let mke2fs = mke2fs(&target().join("mke2fs"), triple, c)?;
    let copied = out.join("mke2fs");
    if copy(&mke2fs, &copied)?
        && profile == Profile::Release
        && let Some(c) = c
    {
        step(&format!("stripping {triple}'s mke2fs"));
        run(Command::new(&c.strip).arg(&copied))?;
        // Stripping changed it: its time says again what it's a copy of (see copy).
        same_time(&mke2fs, &copied)?;
    }
    kernel(arch, &target().join("kernel"), out)
}

/// The file at `(url, sha256)`, downloaded into `cache` if it isn't there, and checked.
fn fetch(cache: &Path, (url, sha256): (&str, &str)) -> Result<PathBuf> {
    let file = cache.join(url.rsplit('/').next().unwrap());
    if !file.exists() || digest(&file)? != sha256 {
        // Its own partial: two threads may fetch one file at once (e2fsprogs', for two targets).
        static FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = FETCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let partial = file.with_extension(format!("partial-{}-{n}", std::process::id()));
        run(Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(&partial)
            .arg(url))?;
        fs::rename(&partial, &file).map_err(|e| e.to_string())?;
    }
    let got = digest(&file)?;
    if got != sha256 {
        return Err(format!("{url}: sha256 {got}, not {sha256}"));
    }
    Ok(file)
}

fn digest(file: &Path) -> Result<String> {
    use sha2::Digest;
    let data = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    Ok(sha2::Sha256::digest(&data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn step(what: &str) {
    eprintln!("xtask: {what}");
}

fn cargo(args: &[&str], env: &[(String, OsString)]) -> Result<()> {
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

/// `from` copied to `to`, unless `to` is a copy of it already: one with its modification time,
/// which a copy is given. Whether it was copied.
fn copy(from: &Path, to: &Path) -> Result<bool> {
    let time = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
    if time(to).is_some() && time(to) == time(from) {
        return Ok(false);
    }
    // Removed first: a running bottle-cli's file is replaced, not written over.
    let _ = fs::remove_file(to);
    fs::copy(from, to)
        .map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))?;
    same_time(from, to)?;
    Ok(true)
}

/// `to` given `from`'s modification time.
fn same_time(from: &Path, to: &Path) -> Result<()> {
    let time = fs::metadata(from)
        .and_then(|m| m.modified())
        .map_err(|e| format!("{}: {e}", from.display()))?;
    fs::File::options()
        .write(true)
        .open(to)
        .and_then(|f| f.set_modified(time))
        .map_err(|e| format!("{}: {e}", to.display()))
}

#[cfg(test)]
mod tests {
    use super::kernel::{KERNEL_X86_64_SIZE, elf_symbol};
    use super::*;

    #[test]
    fn a_library_s_symbol_is_found_by_name() {
        let so = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("target/kernel/libkrunfw.so.5.6.2");
        let Ok(so) = fs::read(&so) else {
            eprintln!(
                "skipped: no {} (cargo xtask dist for x86_64 fetches it)",
                so.display()
            );
            return;
        };
        assert_eq!(
            elf_symbol(&so, "KERNEL_BUNDLE").unwrap().len(),
            KERNEL_X86_64_SIZE + 1
        );
        assert!(elf_symbol(&so, "NOT_THERE").is_err());
        assert!(elf_symbol(b"not an elf", "KERNEL_BUNDLE").is_err());
    }

    /// The kernel's ELF, loaded as libkrun loads it: its bytes where libkrunfw says, started where
    /// it says.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_kernel_s_elf_loads_as_libkrunfw_s_bundle_would() {
        use linux_loader::loader::{Elf, KernelLoader};
        use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

        let image: Vec<u8> = (0..0x3000u32).map(|i| (i % 251) as u8).collect();
        let elf = elf_kernel(&image, KERNEL_X86_64_LOAD, KERNEL_X86_64_ENTRY);
        let memory = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x200_0000)]).unwrap();
        let loaded = Elf::load(&memory, None, &mut std::io::Cursor::new(elf), None).unwrap();
        assert_eq!(loaded.kernel_load, GuestAddress(KERNEL_X86_64_ENTRY));
        assert_eq!(loaded.kernel_end, KERNEL_X86_64_LOAD + image.len() as u64);
        let mut there = vec![0u8; image.len()];
        memory
            .read_slice(&mut there, GuestAddress(KERNEL_X86_64_LOAD))
            .unwrap();
        assert_eq!(there, image);
    }
}
