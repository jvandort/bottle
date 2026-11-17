//! `on NAME COMMAND...`: bottle-cli's COMMAND there; an image build's recipe and what it mounts sent with it.

use std::io::Write;
use std::os::unix::process::CommandExt;

use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::{env, fs, io, thread};

use crate::FAILED;

use super::ssh::Runner;
use super::{CONNECT_TIMEOUT, DIST, USAGE, other};

/// `on [--connect-timeout SECONDS] NAME COMMAND [ARG]...`: bottle-cli's COMMAND on runner NAME
/// (see the module's doc): its exit status.
pub fn on(mut args: impl Iterator<Item = String>) -> ExitCode {
    let mut name = args.next();
    let mut connect_timeout = CONNECT_TIMEOUT;
    if name.as_deref() == Some("--connect-timeout") {
        match args.next().and_then(|s| s.parse().ok()) {
            Some(seconds) if seconds > 0 => connect_timeout = seconds,
            _ => return crate::usage_error(USAGE),
        }
        name = args.next();
    }
    let Some(name) = name else {
        return crate::usage_error(USAGE);
    };
    let args: Vec<String> = args.collect();
    if args.is_empty() {
        return crate::usage_error(USAGE);
    }
    let run = || -> io::Result<ExitCode> {
        let mut runner = Runner::named(&name)?;
        runner.connect_timeout = connect_timeout;
        runner.ensure_dist()?;
        // Attached wholly, a terminal and all: this process becomes the ssh.
        let attached = args[0] == "shell" || (args[0] == "exec" && args.iter().any(|a| a == "-t"));
        if attached {
            return Err(runner.cli(&args, Some(true)).exec());
        }
        let (mut command, input) = if args.starts_with(&["image".to_string(), "build".to_string()])
        {
            build_there(&runner, &args)?
        } else {
            (runner.cli(&args, Some(false)), None)
        };
        let mut child = command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::inherit()
            })
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(&input)?;
        }
        // What it says, said here as it says it; its last lines kept, to tell ssh's failure
        // from its own.
        let stderr = child.stderr.take().unwrap();
        let said = thread::spawn(move || {
            let mut kept: Vec<String> = Vec::new();
            for line in io::BufRead::lines(io::BufReader::new(stderr)).map_while(Result::ok) {
                eprintln!("{line}");
                kept.push(line);
                if kept.len() > 20 {
                    kept.remove(0);
                }
            }
            kept
        });
        let status = child.wait()?;
        let said = said.join().unwrap_or_default();
        let lines: Vec<&str> = said.iter().map(String::as_str).collect();
        runner.reached_by(status.code(), &lines)?;
        Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
    };
    run().unwrap_or_else(|e| {
        eprintln!("folio: error: {e}");
        ExitCode::from(FAILED)
    })
}

/// `image build RECIPE ...` there: the recipe, what it mounts and its --network file sent as a
/// tar, built from a directory there it's unpacked in.
pub(super) fn build_there(
    runner: &Runner,
    args: &[String],
) -> io::Result<(Command, Option<Vec<u8>>)> {
    let recipe_path = args
        .get(2)
        .ok_or_else(|| other("image build needs RECIPE"))?;
    let mut recipe: bottle_formats::recipe::Recipe =
        serde_json::from_slice(&fs::read(recipe_path)?)
            .map_err(|e| other(format!("{recipe_path}: {e}")))?;
    let staged = env::temp_dir().join(format!("folio-on-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staged);
    fs::create_dir_all(&staged)?;
    let made = (|| -> io::Result<Vec<u8>> {
        if let Some(mount) = &mut recipe.mount {
            let source = mount.source.clone();
            let source = if source.is_relative() {
                PathBuf::from(recipe_path)
                    .parent()
                    .unwrap_or(&PathBuf::from("."))
                    .join(source)
            } else {
                source
            };
            // A copy: what's sent is the directory's content, under a name of its own.
            let copied = Command::new("cp")
                .arg("-R")
                .arg(&source)
                .arg(staged.join("context"))
                .status()?;
            if !copied.success() {
                return Err(other(format!("copying {} failed", source.display())));
            }
            mount.source = "context".into();
        }
        fs::write(
            staged.join("recipe.json"),
            serde_json::to_vec(&recipe).unwrap(),
        )?;
        if let Some(at) = args.iter().position(|a| a == "--network")
            && let Some(network) = args.get(at + 1)
        {
            fs::copy(network, staged.join("network.json"))?;
        }
        let tar = crate::packing_tar()
            .arg("-cf")
            .arg("-")
            .arg("-C")
            .arg(&staged)
            .arg(".")
            .output()?;
        if !tar.status.success() {
            return Err(other("tar of the build's recipe failed"));
        }
        Ok(tar.stdout)
    })();
    let _ = fs::remove_dir_all(&staged);
    let tar = made?;
    let mut there: Vec<String> = args.to_vec();
    there[2] = "\"$d/recipe.json\"".into();
    if let Some(at) = there.iter().position(|a| a == "--network") {
        there[at + 1] = "\"$d/network.json\"".into();
    }
    let quoted: Vec<String> = there
        .iter()
        .map(|a| {
            if a.starts_with("\"$d/") {
                a.clone()
            } else {
                crate::shell::quote(a)
            }
        })
        .collect();
    let script = format!(
        "set -e; d=$(mktemp -d); trap 'rm -rf \"$d\"' EXIT; tar -xf - -C \"$d\"; \
         \"$HOME/{DIST}/bottle-cli\" {}",
        quoted.join(" ")
    );
    let mut ssh = runner.ssh(Some(false));
    ssh.arg(script);
    Ok((ssh, Some(tar)))
}
