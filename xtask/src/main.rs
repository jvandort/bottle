//! `cargo xtask`: bottle's build steps past cargo's own, in one place.
//!
//!     cargo xtask dist [--out DIR]
//!
//! Builds the distribution into DIR (target/dist): bottle-cli, with the agent its VMs run built
//! into it (a static Linux build of bottle-agent for this machine's architecture, the VMs'), and
//! libkrun linked into it, the mke2fs it runs (dist/mke2fs.sh), beside it, and the kernel the VMs
//! boot: libkrunfw's, a release of its pinned below, downloaded and checked, as `kernel` (an arm64
//! Image, out of upstream's prebuilt bundle) or, on x86_64, `libkrunfw.so.5` (out of Fedora's
//! package), which libkrun loads. On macOS bottle-cli is signed with the hypervisor entitlement. What it
//! needs: rustup's musl target for this architecture, and what dist/mke2fs.sh needs.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("dist") => parse_dist(&args[1..]).and_then(|out| dist(&out)),
        _ => Err("usage: cargo xtask dist [--out DIR]".into()),
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

fn parse_dist(args: &[String]) -> Result<PathBuf> {
    let mut out = root().join("target/dist");
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = args.next().ok_or("--out needs a value")?.into(),
            _ => return Err(format!("unknown option {arg}")),
        }
    }
    Ok(out)
}

/// The workspace's root.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn dist(out: &Path) -> Result<()> {
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

    kernel(&root.join("target/kernel"), out)?;
    eprintln!("xtask: {}", out.display());
    Ok(())
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

/// The kernel, into `out`, by way of `cache`.
fn kernel(cache: &Path, out: &Path) -> Result<()> {
    fs::create_dir_all(cache).map_err(|e| format!("{}: {e}", cache.display()))?;
    match env::consts::ARCH {
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
            copy(&image, &out.join("kernel"))
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
            copy(&library, &out.join("libkrunfw.so.5"))
        }
        arch => Err(format!("no kernel for {arch}")),
    }
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
