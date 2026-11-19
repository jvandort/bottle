//! The sandbox engine: everything folio's threads ask of bottles.
//!
//! What they need is small -- validate a spec, create a bottle, read its state, start and delete
//! it, run commands in it with pipes or a terminal. A command runs as bottle-cli's exec, on the
//! machine the bottle is; the rest is bottles.rs's.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::bottle_cli::Cli;
use crate::bottles::{self, Known, Status};
use crate::specs::Spec;

pub use crate::bottles::{MountRequest as Mount, Order};

/// What a command run with pipes said, and how it ended.
#[derive(Debug)]
pub struct Ran {
    pub code: i32,
    pub stdout: String,
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

// --- what threads ask -------------------------------------------------------------

/// `command`, its stdout read, `input` its stdin (else nothing); its stderr shown, or dropped
/// without `check`.
fn piped(command: &mut Command, input: Option<&str>, check: bool) -> Result<Ran, String> {
    crate::interrupt::guard(command)?;
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(if check {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .spawn()
        .map_err(|e| format!("{}: {e}", command.get_program().to_string_lossy()))?;
    if let Some(input) = input {
        let mut stdin = child.stdin.take().unwrap();
        let input = input.to_string();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        crate::interrupt::check()?;
    }
    Ok(Ran {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

/// `command`, run to its end: its status. Refused if folio was interrupted, and interrupted if
/// that's why it failed.
fn status_of(command: &mut Command) -> Result<std::process::ExitStatus, String> {
    crate::interrupt::guard(command)?;
    let program = command.get_program().to_string_lossy().into_owned();
    let status = command.status().map_err(|e| format!("{program}: {e}"))?;
    if !status.success() {
        crate::interrupt::check()?;
    }
    Ok(status)
}

/// The spec in canonical form, or the reason it isn't one.
pub fn validate_spec(spec: &Spec) -> Result<Spec, String> {
    spec.canonical(None, "this machine")
}

/// What threads ask of bottles, for one command: what it learns of them (bottles::Known) kept
/// while it runs, so a bottle is asked about once a command, not once a step. Made by each command.
#[derive(Default)]
pub struct Engine {
    known: Known,
}

impl Engine {
    /// Make the bottle `order` asks for. On this terminal: an image build shows, and a login can
    /// ask.
    pub fn create(&self, order: &Order) -> Result<(), String> {
        self.known.forget(order.name);
        crate::log::say!("bottles", "Creating bottle {}...", order.name);
        bottles::create(&self.known, order)?;
        Ok(())
    }

    /// Each named bottle's status (bottles::status); Missing if there's no such bottle. Asked once a
    /// command: then known.
    pub fn status(&self, names: &[String]) -> Result<BTreeMap<String, Status>, String> {
        let unknown: Vec<String> = names
            .iter()
            .filter(|n| self.known.status(n).is_none())
            .cloned()
            .collect();
        if !unknown.is_empty() {
            for row in bottles::status(&unknown)? {
                self.known.learn(row);
            }
        }
        Ok(names
            .iter()
            .filter_map(|n| self.known.status(n).map(|row| (n.clone(), row)))
            .collect())
    }

    /// Start a stopped bottle. On this terminal, as `new` is: starting can need a login.
    pub fn start(&self, name: &str) -> Result<(), String> {
        self.known.forget(name);
        bottles::start(&self.known, name).map(|_| ())
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        bottles::delete(&self.known, name)
    }

    /// What the bottle says to the snapshot address, until the Listening is dropped.
    pub fn listen(&self, name: &str) -> Result<crate::daemon::Listening, String> {
        self.known.serve.listen(name)
    }

    /// Record that the bottle is being used now.
    pub fn touch(&self, name: &str) -> Result<(), String> {
        bottles::touch(name)
    }

    /// The command line that runs `argv` in the bottle (which starts it, if it's stopped).
    fn command(&self, name: &str, argv: &[String], tty: bool) -> Result<Vec<String>, String> {
        let command = bottles::exec_command(&self.known, name, argv, tty)?;
        self.known.started(name);
        Ok(command)
    }

    /// Run a command in the bottle, with pipes. Without `check`, a failure is the caller's to expect,
    /// so its stderr is dropped rather than shown as if something went wrong.
    pub fn run(
        &self,
        name: &str,
        argv: &[String],
        input: Option<&str>,
        check: bool,
    ) -> Result<Ran, String> {
        let command = self.command(name, argv, false)?;
        let ran = piped(Command::new(&command[0]).args(&command[1..]), input, check)?;
        if check && ran.code != 0 {
            return Err(failed(name, argv, ran.code));
        }
        Ok(ran)
    }

    /// Run a command in the bottle with its stdout, bytes and all, going to `path`.
    pub fn run_to_file(&self, name: &str, argv: &[String], path: &Path) -> Result<(), String> {
        let command = self.command(name, argv, false)?;
        let out = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let status = status_of(
            Command::new(&command[0])
                .args(&command[1..])
                .stdin(Stdio::null())
                .stdout(out),
        )?;
        if !status.success() {
            return Err(failed(name, argv, status.code().unwrap_or(-1)));
        }
        Ok(())
    }

    /// Run a command in the bottle with `path` as its stdin.
    pub fn run_from_file(&self, name: &str, argv: &[String], path: &Path) -> Result<(), String> {
        let command = self.command(name, argv, false)?;
        let input = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let status = status_of(
            Command::new(&command[0])
                .args(&command[1..])
                .stdin(input)
                .stdout(Stdio::null()),
        )?;
        if !status.success() {
            return Err(failed(name, argv, status.code().unwrap_or(-1)));
        }
        Ok(())
    }

    /// Replace folio with a command in the bottle, on this terminal: given one (`tty`; None, if this is
    /// one).
    pub fn attach(
        &self,
        name: &str,
        argv: &[String],
        tty: Option<bool>,
    ) -> Result<std::convert::Infallible, String> {
        let tty = tty
            .unwrap_or_else(|| std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
        // Flushed: what's buffered goes with this process.
        let _ = std::io::stdout().flush();
        let command = self.command(name, argv, tty)?;
        crate::interrupt::check()?;
        crate::log::log!(
            "command",
            "becomes {}",
            command
                .iter()
                .map(|w| crate::shell::quote(w))
                .collect::<Vec<_>>()
                .join(" ")
        );
        let error = Command::new(&command[0]).args(&command[1..]).exec();
        Err(format!("{}: {error}", command[0]))
    }

    /// Copy a file or a directory, as `cp -r` would, between here and bottles: each side (a bottle's
    /// name, a path in it; or None, a path here). Where it went: a path here, or in `dst`'s bottle.
    pub fn copy(
        &self,
        src: (Option<&str>, &str),
        dst: (Option<&str>, &str),
    ) -> Result<String, String> {
        let sides = [self.cp_side(src)?, self.cp_side(dst)?];
        let commands = copy_commands(&sides[0], &sides[1]);
        let (code, sent, stdout) = if commands.len() == 1 {
            let ran = piped(
                Command::new(&commands[0][0]).args(&commands[0][1..]),
                None,
                true,
            )?;
            (ran.code, 0, ran.stdout)
        } else {
            let mut sender = Command::new(&commands[0][0]);
            crate::interrupt::guard(&mut sender)?;
            let mut sender = sender
                .args(&commands[0][1..])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("{}: {e}", commands[0][0]))?;
            let pipe = sender.stdout.take().unwrap();
            let mut receiver = Command::new(&commands[1][0]);
            crate::interrupt::guard(&mut receiver)?;
            let receiver = receiver
                .args(&commands[1][1..])
                .stdin(pipe)
                .stdout(Stdio::piped())
                .output()
                .map_err(|e| format!("{}: {e}", commands[1][0]))?;
            let sent = sender
                .wait()
                .map_err(|e| e.to_string())?
                .code()
                .unwrap_or(-1);
            (
                receiver.status.code().unwrap_or(-1),
                sent,
                String::from_utf8_lossy(&receiver.stdout).into_owned(),
            )
        };
        if code != 0 || sent != 0 {
            crate::interrupt::check()?;
            return Err("copying failed".into()); // bottle-cli has said why
        }
        let went = stdout.trim();
        Ok(match dst.0 {
            Some(_) => went
                .split_once(':')
                .map_or(went, |(_, path)| path)
                .to_string(),
            None => went.to_string(),
        })
    }

    /// A side of a copy, as bottle-cli's cp takes it, with the bottle-cli it's that one's: a bottle's
    /// (started, if it's stopped) MACHINE:PATH, on its runner's; else a path here, on this machine's.
    fn cp_side(&self, (name, path): (Option<&str>, &str)) -> Result<(Cli, String), String> {
        let Some(name) = name else {
            return Ok((Cli::Local, local_side(path)));
        };
        let bottle = bottles::ensure_running(&self.known, name)?;
        let runner = bottles::runner_of(&bottle)?;
        Ok((
            runner.cli,
            format!(
                "{}:{}",
                bottle.machine(),
                if path.is_empty() { "." } else { path }
            ),
        ))
    }
}

/// That a command in a bottle failed; what it said, it said on stderr.
fn failed(name: &str, argv: &[String], code: i32) -> String {
    format!("`{}` in {name} failed (exit {code})", argv[0])
}

/// A path here, as bottle-cli's cp takes it: a colon in a relative path would name a machine.
fn local_side(path: &str) -> String {
    if path.starts_with('/') || !path.contains(':') {
        path.to_string()
    } else {
        format!("./{path}")
    }
}

/// The command lines that copy `src` to `dst`, each a side as bottle-cli's cp takes it with the
/// bottle-cli it's that one's: one cp, if they're one bottle-cli's; else two, the first's tar on
/// stdout piped to the second.
pub fn copy_commands(src: &(Cli, String), dst: &(Cli, String)) -> Vec<Vec<String>> {
    let ((src_cli, src_spec), (dst_cli, dst_spec)) = (src, dst);
    if src_cli.command(&[]) == dst_cli.command(&[]) {
        return vec![src_cli.command(&strings(&["cp", src_spec, dst_spec]))];
    }
    vec![
        src_cli.command(&strings(&["cp", src_spec, "-"])),
        dst_cli.command(&strings(&["cp", "-", dst_spec])),
    ]
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use crate::tempdir;

    #[test]
    fn one_cp_on_one_bottle_cli_else_a_pipe_of_two() {
        let env = tempdir::isolated();
        tempdir::set(&env, "BOTTLE_CLI", "/opt/bottle-cli");
        tempdir::set(&env, "FOLIO_CLI", "/opt/folio");
        let side = |cli: &Cli, path: &str| (cli.clone(), path.to_string());
        let (here, boxed, other) = (Cli::Local, Cli::runner("box"), Cli::runner("other"));
        assert_eq!(
            copy_commands(&side(&here, "./a"), &side(&here, "m:b")),
            [["/opt/bottle-cli", "cp", "./a", "m:b"]]
        );
        assert_eq!(
            copy_commands(&side(&boxed, "m:a"), &side(&boxed, "n:b")),
            [["/opt/folio", "on", "box", "cp", "m:a", "n:b"]]
        );
        assert_eq!(
            copy_commands(&side(&boxed, "m:a"), &side(&here, "b")),
            [
                vec!["/opt/folio", "on", "box", "cp", "m:a", "-"],
                vec!["/opt/bottle-cli", "cp", "-", "b"]
            ]
        );
        assert_eq!(
            copy_commands(&side(&boxed, "m:a"), &side(&other, "n:b"))[1],
            ["/opt/folio", "on", "other", "cp", "-", "n:b"]
        );
        tempdir::set(&env, "FOLIO_CLI", "");
        assert_eq!(local_side("a:b"), "./a:b");
        assert_eq!(local_side("/a:b"), "/a:b");
    }
}
