//! `cargo xtask test`: the workspace's tests, with what they run.
//!
//!     cargo xtask test [--root] [--vm] [-- CARGO_TEST_ARGS...]
//!
//! `cargo test` alone runs the tests that need nothing but their own package. The others run another
//! package's program, which cargo doesn't hand a test: they're the `integration` feature's (bottle-cli's,
//! bottle-disk's and folio's), and this builds those programs, as whoever runs it, and gives them
//! to the tests by their paths, in XTASK_* variables:
//!
//! ```text
//! XTASK_BOTTLE_AGENT        bottle-agent, for this machine (the agent as a process: bottle-cli's
//!                           exec, copy and machine tests; folio's runner)
//! XTASK_BOTTLE_CLI          bottle-cli (folio's runner)
//! XTASK_FOLIO               folio (folio's own tests that run it as another program would)
//! XTASK_FOLIO_STANDIN       folio-standin (folio's tests: bottle-cli, ssh and `security` stood in for)
//! XTASK_BOTTLE_GUEST_AGENT  with --root, on Linux: bottle-agent as a VM gets it, a static build
//!                           (bottle-disk's build)
//! XTASK_VM_BOTTLE_CLI       with --vm, on a Mac: this checkout's distribution's bottle-cli, for
//!                           folio's tests with real VMs (krun_contract, end_to_end); minutes, the
//!                           first time
//! ```
//!
//! With --root, on Linux, the tests run as root (sudo, as cargo's runner), with /usr/sbin and /sbin
//! on PATH (mke2fs, losetup): those that need root are skipped otherwise. Everything's built before,
//! as whoever runs this, so nothing in the target directory is root's. CARGO_TEST_ARGS go to `cargo
//! test` as they are: `-p folio`, say, tests folio alone, `-- --nocapture` passes to the tests.

use std::env;
use std::ffi::OsString;
use std::process::Command;

use crate::{Profile, Result, cargo, dist, root, step, target};

/// The packages with an `integration` feature.
const INTEGRATION: [&str; 3] = ["bottle-cli", "bottle-disk", "folio"];

pub fn test(args: &[String]) -> Result<()> {
    let (mut as_root, mut vm) = (false, false);
    let mut args = args.iter();
    let mut passed: Vec<String> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => as_root = true,
            "--vm" => vm = true,
            "--" => passed.extend(args.by_ref().cloned()),
            _ => return Err(format!("unknown option {arg} (cargo test's go after --)")),
        }
    }
    if is_root()? {
        return Err(
            "run it as yourself: with --root, the tests are run as root, built as you".into(),
        );
    }
    if as_root && !cfg!(target_os = "linux") {
        return Err("--root is Linux's: the tests that need root are Linux's".into());
    }
    if vm && !cfg!(target_os = "macos") {
        return Err("--vm is a Mac's: its bottle-cli runs the VMs".into());
    }

    step("what the tests run");
    cargo(
        &[
            "build",
            "-p",
            "bottle-agent",
            "-p",
            "bottle-cli",
            "-p",
            "folio",
            "-p",
            "folio-standin",
        ],
        &[],
    )?;
    let debug = target().join("debug");
    let mut env: Vec<(String, OsString)> = vec![
        (
            "XTASK_BOTTLE_AGENT".into(),
            debug.join("bottle-agent").into(),
        ),
        ("XTASK_BOTTLE_CLI".into(), debug.join("bottle-cli").into()),
        ("XTASK_FOLIO".into(), debug.join("folio").into()),
        (
            "XTASK_FOLIO_STANDIN".into(),
            debug.join("folio-standin").into(),
        ),
    ];
    if as_root {
        let guest = format!("{}-unknown-linux-musl", env::consts::ARCH);
        step(&format!("the agent, for {guest}"));
        cargo(&["build", "-p", "bottle-agent", "--target", &guest], &[])
            .map_err(|e| format!("{e} (is the target installed? rustup target add {guest})"))?;
        let agent = target().join(&guest).join("debug").join("bottle-agent");
        env.push(("XTASK_BOTTLE_GUEST_AGENT".into(), agent.into()));
        let host = rustc_host()?;
        let runner = format!(
            "CARGO_TARGET_{}_RUNNER",
            host.to_uppercase().replace('-', "_")
        );
        env.push((runner, "sudo -E".into()));
        let path = env::var("PATH").unwrap_or_default();
        env.push(("PATH".into(), format!("{path}:/usr/sbin:/sbin").into()));
    }
    if vm {
        dist(Profile::Dev, vec![], None)?;
        env.push((
            "XTASK_VM_BOTTLE_CLI".into(),
            target().join("dist").join("bottle-cli").into(),
        ));
    }

    // The packages tested: those named after --, else all of them.
    let named: Vec<&str> = passed
        .iter()
        .zip(passed.iter().skip(1))
        .filter(|(flag, _)| *flag == "-p" || *flag == "--package")
        .map(|(_, package)| package.as_str())
        .collect();
    let features: Vec<String> = INTEGRATION
        .iter()
        .filter(|p| named.is_empty() || named.contains(p))
        .map(|p| format!("{p}/integration"))
        .collect();
    let mut command: Vec<String> = vec!["test".into()];
    if named.is_empty() {
        command.push("--workspace".into());
    }
    if !features.is_empty() {
        command.extend(["--features".into(), features.join(",")]);
    }
    command.extend(passed.iter().cloned());
    step(&format!("cargo {}", command.join(" ")));
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    cargo(&command, &env)
}

/// Whether this runs as root.
fn is_root() -> Result<bool> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| format!("id -u: {e}"))?;
    Ok(out.stdout.trim_ascii() == b"0")
}

/// The target cargo builds for here, by default: the tests'.
fn rustc_host() -> Result<String> {
    let rustc = env::var_os("RUSTC").unwrap_or("rustc".into());
    let out = Command::new(&rustc)
        .arg("-vV")
        .current_dir(root())
        .output()
        .map_err(|e| format!("rustc -vV: {e}"))?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: ").map(str::to_string))
        .ok_or_else(|| "rustc -vV: no host".into())
}
