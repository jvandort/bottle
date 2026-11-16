//! ssh, as far as folio uses it: options up to `--`, the destination, and a command for its shell,
//! run here as if at the destination, in a home of its own ($FAKE_REMOTES/DESTINATION). The
//! destination is Linux, on what this machine is (a Mac's arm64 is aarch64 there), and its shell
//! sees $FAKE_KVM where it says /dev/kvm, so a test says whether it has KVM.
//!
//! A forward (-R THERE:HERE) is a symlink there to the socket here, held while this runs. A shared
//! connection's control socket (-o ControlPath=..., ControlMaster=auto) is a file, made by the
//! first ssh that'd be its master. "down" can't be reached; nor can anything while $FAKE_SSH_OFF
//! names a file that's there (the runner's off). Each call is logged in $FAKE_SSH_LOG (its
//! destination and command) and $FAKE_SSH_LOG.options (its options), if that's set.

use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

/// What the destination's shell is told first: that it's Linux.
const LINUX: &str = r#"uname() { case "$1" in -s) echo Linux ;; -m) m=$(command uname -m); [ "$m" = arm64 ] && m=aarch64; echo "$m" ;; *) command uname "$@" ;; esac; }"#;

fn log(suffix: &str, line: &str) {
    let Some(log) = std::env::var_os("FAKE_SSH_LOG") else {
        return;
    };
    let mut path = log;
    path.push(suffix);
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{line}").unwrap();
}

pub fn main(args: Vec<String>) -> ExitCode {
    let dash = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let (options, rest) = (&args[..dash], args.get(dash + 1..).unwrap_or_default());
    log(".options", &options.join(" "));
    let mut forwards = Vec::new();
    let (mut control, mut master) = (None, false);
    for (i, option) in options.iter().enumerate() {
        if option == "-R"
            && let Some(forward) = options.get(i + 1)
        {
            forwards.push(forward.clone());
        }
        if let Some(path) = option.strip_prefix("ControlPath=") {
            control = Some(PathBuf::from(path));
        }
        master |= option == "ControlMaster=auto";
    }
    let Some((destination, command)) = rest.split_first() else {
        eprintln!("ssh: the stand-in needs a destination");
        return ExitCode::from(255);
    };
    let script = command.join(" ");
    log("", &format!("{destination} {script}"));
    let off = std::env::var_os("FAKE_SSH_OFF").is_some_and(|f| PathBuf::from(f).exists());
    if destination == "down" || off {
        eprintln!("ssh: connect to host {destination} port 22: Connection refused");
        return ExitCode::from(255);
    }
    if master
        && let Some(control) = control
        && !control.exists()
    {
        let _ = fs::write(&control, "");
    }
    let home =
        PathBuf::from(std::env::var_os("FAKE_REMOTES").expect("FAKE_REMOTES")).join(destination);
    fs::create_dir_all(&home).unwrap();
    if !forwards.is_empty() {
        for forward in &forwards {
            if let Some((there, here)) = forward.split_once(':') {
                let _ = fs::remove_file(there);
                std::os::unix::fs::symlink(here, there).unwrap();
            }
        }
        let error = Command::new("sleep").arg("3600").exec();
        eprintln!("ssh: sleep: {error}");
        return ExitCode::from(255);
    }
    let kvm = std::env::var("FAKE_KVM").unwrap_or_else(|_| "/dev/kvm".into());
    let script = script.replace("/dev/kvm", &kvm);
    let error = Command::new("bash")
        .arg("-c")
        .arg(format!("{LINUX}; {script}"))
        .current_dir(&home)
        .env_remove("BOTTLE_HOME")
        .env("HOME", &home)
        .exec();
    eprintln!("ssh: bash: {error}");
    ExitCode::from(255)
}
