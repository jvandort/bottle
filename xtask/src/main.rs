//! `cargo xtask`: bottle's build steps past cargo's own, in one place.
//!
//!     cargo xtask dist [--target TRIPLE] [--out DIR]
//!
//! Builds a distribution into DIR (target/dist for this machine's own, target/dist-TRIPLE for
//! another): bottle-cli, with the agent its VMs run built into it (a static Linux build of
//! bottle-agent for the target's architecture, the VMs'), and libkrun linked into it, the mke2fs it
//! runs, built from e2fsprogs' release, beside it, and the kernel the VMs boot: libkrunfw's, a
//! release of its pinned below, downloaded and checked, as `kernel`: an arm64 Image, out of
//! upstream's prebuilt bundle, or, on x86_64, an ELF of the kernel in Fedora's package.
//!
//! TRIPLE is one of TARGETS: aarch64-apple-darwin, built on a Mac, with bottle-cli signed with the
//! hypervisor entitlement; or Linux on arm64 or x86_64, static with musl, so it runs on any
//! distribution. On Linux, Linux's is built here, for either architecture. On a Mac, Linux's is
//! built in a Linux VM of this Mac's bottle-cli (see builder.rs), so `cargo xtask dist` comes first.
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
//! waits for it. Whether one is needed at all, without starting cargo, is bin/bottle-build's.
//! Every distribution has a manifest of its files (MANIFEST): each one's sha256 and name, as
//! sha256sum writes them, which is what a runner's copy is compared with (bottle-cli's runner.rs).

mod builder;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("dist") => parse_dist(&args[1..]).and_then(|(target, out)| dist(target, out)),
        _ => Err("usage: cargo xtask dist [--target TRIPLE] [--out DIR]".into()),
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

fn parse_dist(args: &[String]) -> Result<(Option<String>, Option<PathBuf>)> {
    let (mut target, mut out) = (None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--target" => target = Some(args.next().ok_or("--target needs a value")?.clone()),
            "--out" => out = Some(args.next().ok_or("--out needs a value")?.into()),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    Ok((target, out))
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

fn dist(target: Option<String>, out: Option<PathBuf>) -> Result<()> {
    let host = host();
    let target = target.unwrap_or_else(|| host.clone());
    if !TARGETS.contains(&target.as_str()) {
        return Err(format!(
            "no dist for {target}: one of {}",
            TARGETS.join(", ")
        ));
    }
    let out = out.unwrap_or_else(|| match target == host {
        true => root().join("target/dist"),
        false => root().join(format!("target/dist-{target}")),
    });
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    // One build of it at a time: another waits.
    let name = out
        .file_name()
        .map_or("dist".into(), |n| n.to_string_lossy());
    let lock = out.with_file_name(format!(".{name}.lock"));
    let lock = fs::File::create(&lock).map_err(|e| format!("{}: {e}", lock.display()))?;
    lock.lock()
        .map_err(|e| format!("locking {}: {e}", out.display()))?;
    eprintln!("xtask: building bottle-cli for {target}...");
    let arch = target.split('-').next().unwrap();
    if target.ends_with("-apple-darwin") {
        if !cfg!(target_os = "macos") {
            return Err(format!("{target}'s dist is built on a Mac"));
        }
        mac_dist(&out)?;
    } else if cfg!(target_os = "macos") {
        builder::dist(&target, &out)?;
    } else {
        linux_dist(arch, &target, &out)?;
    }
    manifest(&out)?;
    eprintln!("xtask: {}", out.display());
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

/// The agent, a static Linux build for `arch`, the VMs' architecture.
fn agent(arch: &str) -> Result<PathBuf> {
    let guest = format!("{arch}-unknown-linux-musl");
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
    Ok(target().join(&guest).join("release/bottle-agent"))
}

/// The Mac's distribution, built on a Mac.
fn mac_dist(out: &Path) -> Result<()> {
    let agent = agent(env::consts::ARCH)?;
    step("bottle-cli, the agent built in");
    cargo(
        &["build", "--release", "-p", "bottle-cli"],
        &[("BOTTLE_EMBED_AGENT".into(), agent.into())],
    )?;
    let built = target().join("release/bottle-cli");
    let cli = out.join("bottle-cli");
    if copy(&built, &cli)? {
        step("signing bottle-cli");
        let entitlements = root().join("xtask/hypervisor.entitlements");
        run(Command::new("codesign")
            .args(["--force", "-s", "-", "--entitlements"])
            .arg(&entitlements)
            .arg(&cli))?;
        // Signing changed it: its time says again what it's a copy of (see copy).
        same_time(&built, &cli)?;
    }
    let mke2fs = mke2fs(&target().join("mke2fs"), "aarch64-apple-darwin", None)?;
    copy(&mke2fs, &out.join("mke2fs"))?;
    kernel(env::consts::ARCH, &target().join("kernel"), out)
}

/// Linux's distribution for `arch`, built on Linux: static, with musl, its C built by `LinuxC`.
fn linux_dist(arch: &str, triple: &str, out: &Path) -> Result<()> {
    let c = LinuxC::new(arch)?;
    let agent = agent(arch)?;
    step(&format!("bottle-cli, for {triple}, the agent built in"));
    // What cc-rs builds for libkrun (its x86_64 kernel decoders' C): with musl's headers.
    let key = triple.replace('-', "_");
    cargo(
        &["build", "--release", "--target", triple, "-p", "bottle-cli"],
        &[
            ("BOTTLE_EMBED_AGENT".into(), agent.into()),
            (format!("CC_{key}"), c.gcc.clone().into()),
            (format!("CFLAGS_{key}"), c.specs().into()),
            (format!("AR_{key}"), c.ar.clone().into()),
        ],
    )?;
    copy(
        &target().join(triple).join("release/bottle-cli"),
        &out.join("bottle-cli"),
    )?;
    let mke2fs = mke2fs(&target().join("mke2fs"), triple, Some(&c))?;
    copy(&mke2fs, &out.join("mke2fs"))?;
    kernel(arch, &target().join("kernel"), out)
}

/// How C is built for a static Linux program for `arch`, on Linux: gcc with musl's specs, which
/// put musl's headers and libc in place of glibc's, as musl-gcc does. For another architecture,
/// Debian's cross gcc, with that architecture's musl (multiarch) and kernel headers.
struct LinuxC {
    gcc: String,
    ar: String,
    /// musl's specs for gcc.
    specs: PathBuf,
    /// Where the kernel's headers are: linux/, asm-generic/ and asm/.
    headers: [PathBuf; 3],
    /// For another architecture than this machine's: configure's --host.
    host: Option<String>,
}

impl LinuxC {
    fn new(arch: &str) -> Result<LinuxC> {
        let gnu = format!("{arch}-linux-gnu");
        let debian = match arch {
            "x86_64" => "amd64",
            _ => "arm64",
        };
        let cross = arch != env::consts::ARCH;
        let (gcc, ar, include, asm) = match cross {
            true => (
                format!("{gnu}-gcc"),
                format!("{gnu}-ar"),
                PathBuf::from(format!("/usr/{gnu}/include")),
                PathBuf::from(format!("/usr/{gnu}/include/asm")),
            ),
            false => (
                "gcc".into(),
                "ar".into(),
                PathBuf::from("/usr/include"),
                PathBuf::from(format!("/usr/include/{gnu}/asm")),
            ),
        };
        let specs = PathBuf::from(format!("/usr/lib/{arch}-linux-musl/musl-gcc.specs"));
        if !specs.exists() {
            return Err(match cross {
                true => format!(
                    "no musl for {arch} ({}): on Debian, dpkg --add-architecture {debian}, then \
                     apt install gcc-{}-linux-gnu musl-dev:{debian} linux-libc-dev-{debian}-cross",
                    specs.display(),
                    arch.replace('_', "-")
                ),
                false => format!(
                    "no musl ({}): on Debian, apt install musl-tools",
                    specs.display()
                ),
            });
        }
        Ok(LinuxC {
            gcc,
            ar,
            specs,
            headers: [include.join("linux"), include.join("asm-generic"), asm],
            host: cross.then(|| format!("{arch}-linux-musl")),
        })
    }

    fn specs(&self) -> String {
        format!("-specs {}", self.specs.display())
    }

    /// gcc as configure takes it: with musl's specs.
    fn cc(&self) -> String {
        format!("{} {}", self.gcc, self.specs())
    }
}

/// e2fsprogs' release bottle builds mke2fs from: where it is, and its sha256 (from the signed
/// sha256sums.asc beside it).
const E2FSPROGS: (&str, &str, &str) = (
    "1.47.4",
    "https://www.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v1.47.4/e2fsprogs-1.47.4.tar.xz",
    "fd5bf388cbdbe006a3d3b318d983b2948382440acc85a87f1e7d108653e8db0b",
);

/// mke2fs, built in `dir` from e2fsprogs' release tarball if it isn't there already: what makes
/// the empty disks VMs then write. It needs nothing installed where it runs: on Linux it's static
/// (musl: musl-gcc, from Debian's musl-tools, and the kernel's headers, linux-libc-dev, are
/// needed to build it), on macOS it links only libSystem, which every Mac has. bottle runs it,
/// never links it: e2fsprogs' library is GPL-2 in parts, bottle is Apache-2.0.
fn mke2fs(downloads: &Path, triple: &str, c: Option<&LinuxC>) -> Result<PathBuf> {
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

/// libkrunfw's release the VMs boot, for each architecture: where it is, and its sha256.
const KERNEL_AARCH64: (&str, &str) = (
    "https://github.com/containers/libkrunfw/releases/download/v5.6.2/libkrunfw-prebuilt-aarch64.tgz",
    "adef6739cc3bda2a1b57f04c08ccbfdf206a6c831d76b32de1beddc0e7984e86",
);
const KERNEL_X86_64: (&str, &str) = (
    "https://kojipkgs.fedoraproject.org/packages/libkrunfw/5.6.2/1.fc43/x86_64/libkrunfw-5.6.2-1.fc43.x86_64.rpm",
    "484f811b7a476040a2fc8d48df3d8daddced3d76cc49cca5889e0beb7d913c60",
);

/// Where libkrunfw 5.6.2's x86_64 kernel goes in guest memory, where it starts, and its size: what
/// its `krunfw_get_kernel` says (by disassembly of Fedora's build, whose sha256 is pinned above).
const KERNEL_X86_64_LOAD: u64 = 0x100_0000;
const KERNEL_X86_64_ENTRY: u64 = 0x100_0123;
const KERNEL_X86_64_SIZE: usize = 0x16b_0000;

/// The kernel for `arch`, into `out`, by way of `cache`.
fn kernel(arch: &str, cache: &Path, out: &Path) -> Result<()> {
    fs::create_dir_all(cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    // What an older dist had beside bottle-cli, which libkrun loaded.
    let _ = fs::remove_file(out.join("libkrunfw.so.5"));
    match arch {
        "aarch64" => {
            let image = cache.join("kernel-5.6.2-aarch64");
            if !image.exists() {
                step("the kernel, libkrunfw 5.6.2's");
                let bundle = fetch(cache, KERNEL_AARCH64)?;
                run(Command::new("tar")
                    .args(["-xzf"])
                    .arg(&bundle)
                    .arg("-C")
                    .arg(cache)
                    .arg("libkrunfw/kernel.c"))?;
                let c = fs::read(cache.join("libkrunfw/kernel.c")).map_err(|e| e.to_string())?;
                let partial = image.with_extension("partial");
                fs::write(&partial, decode_bundle(&c)?).map_err(|e| e.to_string())?;
                fs::rename(&partial, &image).map_err(|e| e.to_string())?;
                let _ = fs::remove_dir_all(cache.join("libkrunfw"));
            }
            copy(&image, &out.join("kernel")).map(drop)
        }
        "x86_64" => {
            let library = cache.join("libkrunfw.so.5.6.2");
            if !library.exists() {
                step("the kernel, libkrunfw 5.6.2's, Fedora's build");
                let rpm = fetch(cache, KERNEL_X86_64)?;
                // An rpm is a cpio, which bsdtar reads, or rpm2cpio turns into one.
                let extract = format!(
                    "cd {dir} && (bsdtar -xf {rpm} ./usr/lib64/libkrunfw.so.5.6.2 2>/dev/null || \
                     rpm2cpio {rpm} | cpio -id ./usr/lib64/libkrunfw.so.5.6.2) && \
                     mv usr/lib64/libkrunfw.so.5.6.2 . && rm -rf usr",
                    dir = cache.display(),
                    rpm = rpm.display()
                );
                run(Command::new("sh").arg("-c").arg(&extract))
                    .map_err(|e| format!("{e} (bsdtar, or rpm2cpio and cpio, is needed)"))?;
            }
            let image = cache.join("kernel-5.6.2-x86_64");
            if !image.exists() {
                let so = fs::read(&library).map_err(|e| e.to_string())?;
                let bundle = elf_symbol(&so, "KERNEL_BUNDLE")?;
                // A C string: the kernel, then a NUL.
                if bundle.len() != KERNEL_X86_64_SIZE + 1 {
                    return Err(format!(
                        "libkrunfw's KERNEL_BUNDLE is {:#x} bytes, not {:#x}",
                        bundle.len(),
                        KERNEL_X86_64_SIZE + 1
                    ));
                }
                let elf = elf_kernel(
                    &bundle[..KERNEL_X86_64_SIZE],
                    KERNEL_X86_64_LOAD,
                    KERNEL_X86_64_ENTRY,
                );
                let partial = image.with_extension("partial");
                fs::write(&partial, elf).map_err(|e| e.to_string())?;
                fs::rename(&partial, &image).map_err(|e| e.to_string())?;
            }
            copy(&image, &out.join("kernel")).map(drop)
        }
        arch => Err(format!("no kernel for {arch}")),
    }
}

/// The bytes of `name`, a symbol in the dynamic symbol table of ELF64 shared library `so`.
fn elf_symbol<'a>(so: &'a [u8], name: &str) -> Result<&'a [u8]> {
    let bad = || format!("not an ELF64 library with {name}");
    let at = |offset: u64, size: usize| -> Result<u64> {
        let bytes = so
            .get(offset as usize..offset as usize + size)
            .ok_or_else(bad)?;
        let mut value = [0u8; 8];
        value[..size].copy_from_slice(bytes);
        Ok(u64::from_le_bytes(value))
    };
    if so.get(..6) != Some(b"\x7fELF\x02\x01") {
        return Err(bad());
    }
    let (phoff, phentsize, phnum) = (at(0x20, 8)?, at(0x36, 2)?, at(0x38, 2)?);
    let (shoff, shentsize, shnum) = (at(0x28, 8)?, at(0x3a, 2)?, at(0x3c, 2)?);
    let section = |i: u64| shoff + i * shentsize;
    // SHT_DYNSYM, and the string table it links to.
    let dynsym = (0..shnum)
        .map(section)
        .find(|&s| at(s + 4, 4).ok() == Some(11))
        .ok_or_else(bad)?;
    let strtab = at(section(at(dynsym + 0x28, 4)?) + 0x18, 8)?;
    let (symbols, size, entsize) = (
        at(dynsym + 0x18, 8)?,
        at(dynsym + 0x20, 8)?,
        at(dynsym + 0x38, 8)?,
    );
    for symbol in (symbols..symbols + size).step_by(entsize.max(1) as usize) {
        let start = (strtab + at(symbol, 4)?) as usize;
        let end = start + so[start..].iter().position(|&b| b == 0).ok_or_else(bad)?;
        if &so[start..end] != name.as_bytes() {
            continue;
        }
        let (value, length) = (at(symbol + 8, 8)?, at(symbol + 16, 8)?);
        // Where it is in the file: by the PT_LOAD segment that holds it.
        for header in (0..phnum).map(|i| phoff + i * phentsize) {
            let (offset, vaddr, filesz) = (
                at(header + 8, 8)?,
                at(header + 0x10, 8)?,
                at(header + 0x20, 8)?,
            );
            if at(header, 4)? == 1 && vaddr <= value && value + length <= vaddr + filesz {
                let start = (offset + value - vaddr) as usize;
                return so.get(start..start + length as usize).ok_or_else(bad);
            }
        }
    }
    Err(bad())
}

/// An x86_64 ELF executable of `image`, loaded at physical address `load` and entered at `entry`:
/// how libkrun takes an x86_64 kernel from a file (krun_set_kernel's ELF format, which loads each
/// PT_LOAD segment at its physical address and starts at the entry, as it does libkrunfw's).
fn elf_kernel(image: &[u8], load: u64, entry: u64) -> Vec<u8> {
    const PAGE: u64 = 0x1000;
    let mut elf = Vec::with_capacity(PAGE as usize + image.len());
    // The ELF header: 64-bit, little-endian, version 1; an executable for x86-64.
    elf.extend_from_slice(b"\x7fELF\x02\x01\x01\0\0\0\0\0\0\0\0\0");
    for (value, size) in [
        (2, 2),     // e_type: ET_EXEC
        (62, 2),    // e_machine: EM_X86_64
        (1, 4),     // e_version
        (entry, 8), // e_entry
        (64, 8),    // e_phoff: right after this header
        (0, 8),     // e_shoff: no sections
        (0, 4),     // e_flags
        (64, 2),    // e_ehsize
        (56, 2),    // e_phentsize
        (1, 2),     // e_phnum
        (64, 2),    // e_shentsize
        (0, 2),     // e_shnum
        (0, 2),     // e_shstrndx
        // The one program header: the image, read, written and run.
        (1, 4),                  // p_type: PT_LOAD
        (7, 4),                  // p_flags: RWX
        (PAGE, 8),               // p_offset
        (load, 8),               // p_vaddr
        (load, 8),               // p_paddr
        (image.len() as u64, 8), // p_filesz
        (image.len() as u64, 8), // p_memsz
        (PAGE, 8),               // p_align
    ] {
        elf.extend_from_slice(&u64::to_le_bytes(value)[..size]);
    }
    elf.resize(PAGE as usize, 0);
    elf.extend_from_slice(image);
    elf
}

/// The file at `(url, sha256)`, downloaded into `cache` if it isn't there, and checked.
fn fetch(cache: &Path, (url, sha256): (&str, &str)) -> Result<PathBuf> {
    let file = cache.join(url.rsplit('/').next().unwrap());
    if !file.exists() || digest(&file)? != sha256 {
        let partial = file.with_extension("partial");
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

/// The bytes of libkrunfw's kernel.c's KERNEL_BUNDLE: a C string of nothing but \x escapes, from
/// its `KERNEL_BUNDLE[] =` to its `;`.
fn decode_bundle(c: &[u8]) -> Result<Vec<u8>> {
    let start = find(c, b"KERNEL_BUNDLE[] =").ok_or("kernel.c: no KERNEL_BUNDLE")?;
    let mut out = Vec::with_capacity(c.len() / 4);
    let mut i = start;
    let mut quoted = false;
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    while i < c.len() {
        match (quoted, c[i]) {
            (false, b';') => return Ok(out),
            (false, b'"') | (true, b'"') => quoted = !quoted,
            (true, b'\\') if c.get(i + 1) == Some(&b'x') => {
                let mut value = 0u8;
                let mut digits = 0;
                while digits < 2
                    && let Some(d) = c.get(i + 2 + digits).copied().and_then(hex)
                {
                    value = value * 16 + d;
                    digits += 1;
                }
                if digits == 0 {
                    return Err("kernel.c: a \\x with no digits".into());
                }
                out.push(value);
                i += 1 + digits;
            }
            (true, b) => out.push(b),
            (false, _) => {}
        }
        i += 1;
    }
    Err("kernel.c: KERNEL_BUNDLE doesn't end".into())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
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
