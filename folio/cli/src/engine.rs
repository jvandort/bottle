//! The sandbox engine: everything folio's threads ask of bottles.
//!
//! What they need is small -- validate a spec, create a bottle, read its state, start and delete
//! it, run commands in it with pipes or a terminal. A command runs as bottle-cli's exec, on the
//! machine the bottle is; the rest is bottles.rs's.
//!
//! $FOLIO_BOTTLE, if it's set, is a bottle CLI that stands in for bottles.rs (folio's tests' does:
//! tests/fake_bottle.py): `spec validate --json FILE`, `new ...`, `status --json NAME...`,
//! `start|touch|delete NAME`, `exec [-t] NAME -- ARGV`, and `cp SRC DST`. Its errors are its own:
//! its stderr reaches the user as it wrote it, and folio adds only which command failed.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{IsTerminal, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::bottle_cli::Cli;
use crate::bottles::{self, MountRequest};
use crate::runners;

/// The bottle CLI standing in for bottles.rs, if one is.
fn stand_in() -> Option<String> {
    std::env::var("FOLIO_BOTTLE").ok().filter(|b| !b.is_empty())
}

/// A repo for a bottle: checked out at `path`, from `checkout` (None: bottle's default), served
/// `remotes`.
pub struct Mount {
    pub path: String,
    pub repo: PathBuf,
    pub checkout: Option<String>,
    pub remotes: Vec<String>,
}

/// What a command run with pipes said, and how it ended.
#[derive(Debug)]
pub struct Ran {
    pub code: i32,
    pub stdout: String,
}

// --- the stand-in -----------------------------------------------------------------

/// That a stand-in's command failed; it has already said why, on stderr.
fn stand_in_failed(args: &[String], code: i32) -> String {
    let words: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .take_while(|a| !a.starts_with('-') && !a.contains('/'))
        .collect();
    format!("`bottle {}` failed (exit {code})", words.join(" "))
}

/// The stand-in, reading its stdout. Without `check`, a failure is the caller's to expect, so its
/// stderr is dropped rather than shown as if something went wrong.
fn stand_in_run(
    program: &str,
    args: &[String],
    input: Option<&str>,
    check: bool,
) -> Result<Ran, String> {
    // Never this terminal for stdin: an exec passes it through to the bottle, which would read what's
    // typed at folio's own prompts.
    let ran = piped(Command::new(program).args(args), input, check)?;
    if check && ran.code != 0 {
        return Err(stand_in_failed(args, ran.code));
    }
    Ok(ran)
}

/// A temporary file holding `content`, for as long as the stand-in needs to read it.
struct TempFile(PathBuf);

impl TempFile {
    fn new(content: &str) -> Result<TempFile, String> {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "folio-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(&path, content).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(TempFile(path))
    }

    fn path(&self) -> String {
        self.0.display().to_string()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

// --- what threads ask -------------------------------------------------------------

/// `command`, its stdout read, `input` its stdin (else nothing); its stderr shown, or dropped
/// without `check`.
fn piped(command: &mut Command, input: Option<&str>, check: bool) -> Result<Ran, String> {
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
    Ok(Ran {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

/// The spec in canonical form, or the reason it isn't one.
pub fn validate_spec(spec: &Value) -> Result<Value, String> {
    let Some(program) = stand_in() else {
        return crate::specs::canonical(spec, None, "this machine");
    };
    let file = TempFile::new(&spec.to_string())?;
    let ran = stand_in_run(
        &program,
        &strings(&["spec", "validate", "--json", &file.path()]),
        None,
        true,
    )?;
    serde_json::from_str(&ran.stdout).map_err(|e| format!("bottle spec validate: {e}"))
}

/// Create a bottle for the thread whose directory is `owner`, where its record is kept, on
/// `runner` (None: this machine), that may ssh to each bottle `ssh` gives, by the name given. On
/// this terminal: an image build shows, and a login can ask.
pub fn new(
    owner: &Path,
    name: &str,
    spec: &Value,
    mounts: &[Mount],
    context: &str,
    runner: Option<&str>,
    ssh: &[(String, String)],
) -> Result<(), String> {
    let Some(program) = stand_in() else {
        eprintln!("Creating bottle {name}...");
        let requests: Vec<MountRequest> = mounts
            .iter()
            .map(|m| MountRequest {
                path: m.path.clone(),
                repo: m.repo.clone(),
                checkout: m.checkout.clone(),
                remotes: if m.remotes.is_empty() {
                    None
                } else {
                    Some(m.remotes.clone())
                },
            })
            .collect();
        bottles::create(
            owner,
            spec,
            name,
            &requests,
            context,
            runner.unwrap_or(runners::LOCAL),
            ssh,
        )?;
        return Ok(());
    };
    let spec_file = TempFile::new(&spec.to_string())?;
    let context_file = TempFile::new(context)?;
    let mut args = strings(&["new", "--spec", &spec_file.path(), "--name", name]);
    if let Some(runner) = runner {
        args.extend(strings(&["--runner", runner]));
    }
    for m in mounts {
        args.extend(["--mount".into(), format!("{}={}", m.path, m.repo.display())]);
        if let Some(checkout) = &m.checkout {
            args.extend(["--checkout".into(), format!("{}={checkout}", m.path)]);
        }
        for remote in &m.remotes {
            args.extend(["--git-remote".into(), format!("{}:{remote}", m.path)]);
        }
    }
    args.extend(["--context".into(), context_file.path()]);
    for (host, bottle) in ssh {
        args.extend(["--ssh".into(), format!("{host}={bottle}")]);
    }
    let status = Command::new(&program)
        .args(&args)
        .status()
        .map_err(|e| format!("{program}: {e}"))?;
    if !status.success() {
        return Err(stand_in_failed(
            &strings(&["new", name]),
            status.code().unwrap_or(-1),
        ));
    }
    Ok(())
}

/// Each named bottle's status row (bottles::status); state "missing" if there's no such bottle.
pub fn status(names: &[String]) -> Result<BTreeMap<String, Value>, String> {
    if names.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows = match stand_in() {
        None => bottles::status(names)?,
        Some(program) => {
            let mut args = strings(&["status", "--json"]);
            args.extend(names.iter().cloned());
            let ran = stand_in_run(&program, &args, None, true)?;
            serde_json::from_str::<Vec<Value>>(&ran.stdout)
                .map_err(|e| format!("bottle status: {e}"))?
        }
    };
    Ok(rows
        .into_iter()
        .map(|row| (row["name"].as_str().unwrap_or_default().to_string(), row))
        .collect())
}

/// Start a stopped bottle. On this terminal, as `new` is: starting can need a login.
pub fn start(name: &str) -> Result<(), String> {
    let Some(program) = stand_in() else {
        return bottles::start(name).map(|_| ());
    };
    let status = Command::new(&program)
        .args(["start", name])
        .stdout(Stdio::null())
        .status()
        .map_err(|e| format!("{program}: {e}"))?;
    if !status.success() {
        return Err(stand_in_failed(
            &strings(&["start", name]),
            status.code().unwrap_or(-1),
        ));
    }
    Ok(())
}

pub fn delete(name: &str) -> Result<(), String> {
    match stand_in() {
        None => bottles::delete(name),
        Some(program) => {
            stand_in_run(&program, &strings(&["delete", name]), None, true).map(|_| ())
        }
    }
}

/// Record that the bottle is being used now.
pub fn touch(name: &str) -> Result<(), String> {
    match stand_in() {
        None => bottles::touch(name),
        Some(program) => stand_in_run(&program, &strings(&["touch", name]), None, true).map(|_| ()),
    }
}

/// The command line that runs `argv` in the bottle.
fn command(name: &str, argv: &[String], tty: bool) -> Result<Vec<String>, String> {
    match stand_in() {
        None => bottles::exec_command(name, argv, tty),
        Some(program) => {
            let mut command = vec![program, "exec".into()];
            if tty {
                command.push("-t".into());
            }
            command.extend([name.to_string(), "--".into()]);
            command.extend(argv.iter().cloned());
            Ok(command)
        }
    }
}

/// That a command in a bottle failed; what it said, it said on stderr.
fn failed(name: &str, argv: &[String], code: i32) -> String {
    match stand_in() {
        None => format!("`{}` in {name} failed (exit {code})", argv[0]),
        Some(_) => stand_in_failed(&[String::from("exec"), name.to_string()], code),
    }
}

/// Run a command in the bottle, with pipes. Without `check`, a failure is the caller's to expect,
/// so its stderr is dropped rather than shown as if something went wrong.
pub fn run(name: &str, argv: &[String], input: Option<&str>, check: bool) -> Result<Ran, String> {
    let command = command(name, argv, false)?;
    let ran = piped(Command::new(&command[0]).args(&command[1..]), input, check)?;
    if check && ran.code != 0 {
        return Err(failed(name, argv, ran.code));
    }
    Ok(ran)
}

/// Run a command in the bottle with its stdout, bytes and all, going to `path`.
pub fn run_to_file(name: &str, argv: &[String], path: &Path) -> Result<(), String> {
    let command = command(name, argv, false)?;
    let out = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let status = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::null())
        .stdout(out)
        .status()
        .map_err(|e| format!("{}: {e}", command[0]))?;
    if !status.success() {
        return Err(failed(name, argv, status.code().unwrap_or(-1)));
    }
    Ok(())
}

/// Run a command in the bottle with `path` as its stdin.
pub fn run_from_file(name: &str, argv: &[String], path: &Path) -> Result<(), String> {
    let command = command(name, argv, false)?;
    let input = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let status = Command::new(&command[0])
        .args(&command[1..])
        .stdin(input)
        .stdout(Stdio::null())
        .status()
        .map_err(|e| format!("{}: {e}", command[0]))?;
    if !status.success() {
        return Err(failed(name, argv, status.code().unwrap_or(-1)));
    }
    Ok(())
}

/// Replace folio with a command in the bottle, on this terminal: given one (`tty`; None, if this is
/// one).
pub fn attach(
    name: &str,
    argv: &[String],
    tty: Option<bool>,
) -> Result<std::convert::Infallible, String> {
    // The stand-in is given one only when it's asked for: it never looks for this one.
    let tty = match tty {
        Some(tty) => tty,
        None => {
            stand_in().is_none()
                && std::io::stdin().is_terminal()
                && std::io::stdout().is_terminal()
        }
    };
    // Flushed: what's buffered goes with this process.
    let _ = std::io::stdout().flush();
    let command = command(name, argv, tty)?;
    let error = Command::new(&command[0]).args(&command[1..]).exec();
    Err(format!("{}: {error}", command[0]))
}

/// Copy a file or a directory, as `cp -r` would, between here and bottles: each side (a bottle's
/// name, a path in it; or None, a path here). Where it went: a path here, or in `dst`'s bottle.
pub fn copy(src: (Option<&str>, &str), dst: (Option<&str>, &str)) -> Result<String, String> {
    let went = match stand_in() {
        Some(program) => {
            let side = |(name, path): (Option<&str>, &str)| match name {
                None => local_side(path),
                Some(name) => format!("{name}:{}", if path.is_empty() { "." } else { path }),
            };
            let ran = piped(
                Command::new(&program).args(["cp", &side(src), &side(dst)]),
                None,
                true,
            )?;
            if ran.code != 0 {
                return Err("copying failed".into());
            }
            ran.stdout
        }
        None => {
            let sides = [cp_side(src)?, cp_side(dst)?];
            let commands = copy_commands(&sides[0], &sides[1]);
            let (code, sent, stdout) = if commands.len() == 1 {
                let ran = piped(
                    Command::new(&commands[0][0]).args(&commands[0][1..]),
                    None,
                    true,
                )?;
                (ran.code, 0, ran.stdout)
            } else {
                let mut sender = Command::new(&commands[0][0])
                    .args(&commands[0][1..])
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("{}: {e}", commands[0][0]))?;
                let pipe = sender.stdout.take().unwrap();
                let receiver = Command::new(&commands[1][0])
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
                return Err("copying failed".into()); // bottle-cli has said why
            }
            stdout
        }
    };
    let went = went.trim();
    Ok(match dst.0 {
        Some(_) => went
            .split_once(':')
            .map_or(went, |(_, path)| path)
            .to_string(),
        None => went.to_string(),
    })
}

/// A path here, as bottle-cli's cp takes it: a colon in a relative path would name a machine.
fn local_side(path: &str) -> String {
    if path.starts_with('/') || !path.contains(':') {
        path.to_string()
    } else {
        format!("./{path}")
    }
}

/// A side of a copy, as bottle-cli's cp takes it, with the bottle-cli it's that one's: a bottle's
/// (started, if it's stopped) MACHINE:PATH, on its runner's; else a path here, on this machine's.
fn cp_side((name, path): (Option<&str>, &str)) -> Result<(Cli, String), String> {
    let Some(name) = name else {
        return Ok((Cli::Local, local_side(path)));
    };
    let bottle = bottles::ensure_running(name)?;
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
mod tests {
    use super::*;
    use crate::tempdir;
    use serde_json::json;

    const FAKE_CLI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../tests/fake_bottle_cli.py");

    /// A bottle, `b`, made in process (no stand-in engine) on the stand-in bottle-cli, folio serve
    /// as the unit tests have it.
    fn with_a_bottle() -> (tempdir::Isolated, tempdir::Dir) {
        let env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        tempdir::set(&env, "BOTTLE_CLI", FAKE_CLI);
        tempdir::set(
            &env,
            "FAKE_CLI_STATE",
            tmp.path().join("state.json").to_str().unwrap(),
        );
        crate::daemon::fake::reset();
        let owner = env.home().join("repo-0").join("thread");
        new(
            &owner,
            "b",
            &json!({"features": [], "memory": "1G"}),
            &[],
            "",
            None,
            &[],
        )
        .unwrap();
        (env, tmp)
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    fn execs(tmp: &tempdir::Dir) -> Vec<Value> {
        let state: Value =
            serde_json::from_str(&std::fs::read_to_string(tmp.path().join("state.json")).unwrap())
                .unwrap();
        state["execs"].as_array().unwrap().clone()
    }

    #[test]
    fn a_command_runs_as_the_bottles_exec_never_with_this_terminal_for_stdin() {
        let (_env, tmp) = with_a_bottle();
        // An exec hands stdin to the bottle, which would eat what's typed at folio's prompts.
        assert_eq!(run("b", &argv(&["cat"]), None, true).unwrap().stdout, "");
        assert_eq!(
            run("b", &argv(&["cat"]), Some("data"), true)
                .unwrap()
                .stdout,
            "data"
        );
        let last = execs(&tmp).last().unwrap().clone();
        assert_eq!(
            (last["user"].as_str(), last["workdir"].as_str()),
            (Some("genie"), Some("/home/genie"))
        );
        assert_eq!(last["argv"], json!(["cat"]));
    }

    #[test]
    fn a_bottles_failures_are_folios_saying_what_it_said() {
        let (_env, _tmp) = with_a_bottle();
        assert_eq!(
            run("b", &argv(&["sh", "-c", "exit 1"]), None, true).unwrap_err(),
            "`sh` in b failed (exit 1)"
        );
        // A failure the caller expects says nothing, and is the caller's to read.
        assert_eq!(
            run(
                "b",
                &argv(&["sh", "-c", "echo said >&2; exit 3"]),
                None,
                false
            )
            .unwrap()
            .code,
            3
        );
        assert!(
            validate_spec(&json!({"features": "tools"}))
                .unwrap_err()
                .contains("features is a list")
        );
        assert_eq!(
            status(&["gone".to_string()]).unwrap()["gone"],
            json!({"name": "gone", "state": "missing"})
        );
    }

    #[test]
    fn a_bottles_record_is_where_its_owner_says_on_this_machine_its_remotes_its_default() {
        let (env, tmp) = with_a_bottle();
        assert!(env.home().join("repo-0/thread/bottles/b.json").exists());
        let repo = tempdir::repo(&tmp.path().join("r"));
        let owner = env.home().join("repo-0").join("other");
        let mount = Mount {
            path: "/workspace".into(),
            repo,
            checkout: None,
            remotes: vec![],
        };
        new(
            &owner,
            "c",
            &json!({"features": [], "memory": "1G"}),
            &[mount],
            "",
            None,
            &[],
        )
        .unwrap();
        let row = status(&["c".to_string()]).unwrap().remove("c").unwrap();
        assert_eq!(row["runner"], "local");
        assert_eq!(
            row["mounts"][0]["remotes"],
            json!(["origin:fetch=upstream", "host:fetch=heads"])
        );
    }

    #[test]
    fn the_stand_in_is_asked_as_folios_tests_have_it() {
        let env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        let log = tmp.path().join("log");
        let script = tmp.path().join("bottle");
        std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> {}\ncat\n[ \"$1\" != exec ] || [ \"$4\" != fail ] || exit 4\n", log.display())).unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        tempdir::set(&env, "FOLIO_BOTTLE", script.to_str().unwrap());
        assert_eq!(
            run("b", &argv(&["cat"]), Some("in"), true).unwrap().stdout,
            "in"
        );
        assert_eq!(
            run("b", &argv(&["fail"]), None, true).unwrap_err(),
            "`bottle exec b` failed (exit 4)"
        );
        touch("b").unwrap();
        let said = std::fs::read_to_string(&log).unwrap();
        assert_eq!(said, "exec b -- cat\nexec b -- fail\ntouch b\n");
        tempdir::set(&env, "FOLIO_BOTTLE", "");
    }
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
