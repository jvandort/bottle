//! Bottles as bottle-cli's machines: every `bottle-cli` invocation folio makes, but serve's.
//!
//! bottle-cli runs each machine as a libkrun VM of its own, booting the image's stage disks stacked
//! by overlayfs, with bottle's agent as its init. folio relies on, and tests/test_krun_contract.py
//! checks: decided egress (a machine decides each TCP connection by the policy it was made with,
//! caching what it says, and gives what it routes to folio serve's sockets; it has no UDP); DNS,
//! answered in the guest from the runner's resolver at the time of each lookup; exec, its stdin,
//! stdout and stderr streamed, and exit status kept; read-only mounts, the image's environment and
//! -e reaching every exec, and a disk kept across stops.
//!
//! Each function takes the bottle-cli it runs (`Cli`): this machine's, or a runner's (which this
//! program reaches over ssh, `on RUNNER`), a machine named as that bottle-cli names it. This
//! machine's is $BOTTLE_CLI's, else `bottle-cli` on PATH.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use bottle_formats::listing::{self, State};
use bottle_formats::policy::Policy;
use bottle_formats::recipe::{self, Recipe};

/// Where bottle-cli runs: on this machine, or a runner's (reached through this program's `on`).
#[derive(Debug, Clone, PartialEq)]
pub enum Cli {
    Local,
    Runner {
        name: String,
        /// Seconds; None: `on`'s own.
        connect_timeout: Option<u32>,
    },
}

/// bottle-cli here: $BOTTLE_CLI's, else the one on PATH.
pub fn binary() -> String {
    std::env::var("BOTTLE_CLI")
        .ok()
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| "bottle-cli".into())
}

/// This program, as a command line names it: $FOLIO_CLI, if it's set, else the one running.
pub fn me() -> String {
    if let Some(given) = std::env::var("FOLIO_CLI").ok().filter(|c| !c.is_empty()) {
        return given;
    }
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "folio".into())
}

/// A failure bottle-cli said: no such machine, or anything else.
#[derive(Debug)]
pub enum Error {
    Missing(String),
    Failed(String),
}

impl From<Error> for String {
    fn from(e: Error) -> String {
        match e {
            Error::Missing(m) | Error::Failed(m) => m,
        }
    }
}

impl Cli {
    pub fn runner(name: &str) -> Cli {
        Cli::Runner {
            name: name.into(),
            connect_timeout: None,
        }
    }

    /// The command line that runs bottle-cli with `args` there.
    pub fn command(&self, args: &[String]) -> Vec<String> {
        let mut command = match self {
            Cli::Local => vec![binary()],
            Cli::Runner {
                name,
                connect_timeout,
            } => {
                let mut command = vec![me(), "on".into()];
                if let Some(seconds) = connect_timeout {
                    command.extend(["--connect-timeout".into(), seconds.to_string()]);
                }
                command.push(name.clone());
                command
            }
        };
        command.extend(args.iter().cloned());
        command
    }

    /// What's said when the program this runs isn't there.
    pub fn missing(&self) -> String {
        match self {
            Cli::Local => format!(
                "no bottle-cli at {}: install it, or set BOTTLE_CLI (bin/folio does, from a checkout)",
                binary()
            ),
            Cli::Runner { .. } => format!("no folio at {}: install it, or set FOLIO_CLI", me()),
        }
    }

    /// bottle-cli with `args` there, its output read, `input` its stdin (else nothing).
    pub fn run(&self, args: &[String], input: Option<&[u8]>) -> Result<Output, String> {
        let command = self.command(args);
        let mut cli = Command::new(&command[0]);
        crate::interrupt::guard(&mut cli)?;
        let mut child = cli
            .args(&command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|_| self.missing())?;
        let mut stdin = child.stdin.take().unwrap();
        let input = input.unwrap_or_default().to_vec();
        // Written beside the reading, so a big input never waits on output nobody reads.
        let writer = std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let output = child.wait_with_output().map_err(|e| e.to_string())?;
        let _ = writer.join();
        Ok(output)
    }

    /// `bottle-cli image build` of `recipe`, `context` mounted where its mount says, by
    /// `network`'s policy, tagged `tag`: its progress on this process's stderr, the disks it made
    /// on stdout.
    pub fn build(
        &self,
        recipe: &Recipe,
        network: &Policy,
        context: &Path,
        mount: &str,
        tag: &str,
    ) -> Result<Output, String> {
        let here = tempdir()?;
        let mut made = recipe.clone();
        let source = context
            .canonicalize()
            .unwrap_or_else(|_| context.to_path_buf());
        made.mount = Some(recipe::Mount {
            source,
            target: mount.into(),
        });
        let write = |name: &str, json: String| {
            std::fs::write(here.join(name), json).map_err(|e| e.to_string())
        };
        write(
            "recipe.json",
            serde_json::to_string(&made).expect("a recipe is JSON"),
        )?;
        write("network.json", policy_json(network))?;
        let args: Vec<String> = [
            "image",
            "build",
            &format!("{}/recipe.json", here.display()),
            "--tag",
            tag,
            "--network",
            &format!("{}/network.json", here.display()),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let command = self.command(&args);
        let mut cli = Command::new(&command[0]);
        crate::interrupt::guard(&mut cli)?;
        let output = cli
            .args(&command[1..])
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|_| self.missing());
        let _ = std::fs::remove_dir_all(&here);
        output
    }
}

fn tempdir() -> Result<PathBuf, String> {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "build-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}

/// bottle-cli's or this program's message, from the last line either wrote on stderr
/// ("bottle-cli: ...", "folio: error: ..."), else stderr as it is.
pub fn error_of(stderr: &str) -> String {
    let stderr = stderr.trim();
    for line in stderr.lines().rev() {
        for program in ["bottle-cli: ", "folio: error: "] {
            if let Some(message) = line.strip_prefix(program) {
                return message.to_string();
            }
        }
    }
    stderr.to_string()
}

fn run(cli: &Cli, args: &[String], input: Option<&[u8]>) -> Result<String, Error> {
    let output = cli.run(args, input).map_err(Error::Failed)?;
    if !output.status.success() {
        crate::interrupt::check().map_err(Error::Failed)?;
        let message = error_of(&String::from_utf8_lossy(&output.stderr));
        if message.starts_with("no machine ") {
            return Err(Error::Missing(message));
        }
        return Err(Error::Failed(if message.is_empty() {
            format!(
                "bottle-cli {} failed (exit {})",
                args.iter().take(2).cloned().collect::<Vec<_>>().join(" "),
                output.status.code().unwrap_or(-1)
            )
        } else {
            message
        }));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// A repo's objects, mounted read-only at `target`.
#[derive(Debug, Clone, PartialEq)]
pub struct Mount {
    pub source: PathBuf,
    pub target: String,
}

/// What a machine is made with.
pub struct Create<'a> {
    pub name: &'a str,
    pub image: &'a str,
    pub mounts: Vec<Mount>,
    pub env: BTreeMap<String, String>,
    /// MiB; None: bottle-cli's default.
    pub memory_mib: Option<u64>,
    pub cpus: u64,
    /// GiB; None: bottle-cli's default.
    pub disk_gib: Option<u64>,
    pub user: Option<&'a str>,
    pub workdir: Option<&'a str>,
    pub labels: BTreeMap<String, String>,
}

/// Create a stopped machine from `image` (a tag in bottle-cli's store where the machine is), its
/// mounts read-only. The disk is its own, sparse, where what the machine writes goes. `user` and
/// `workdir` are who and where its commands run unless they say; `labels`, what `bottle-cli ls`
/// shows of it; `network`, the policy every connection it makes is decided by, kept with it.
pub fn create(machine: &Create, network: Option<&Policy>, cli: &Cli) -> Result<(), String> {
    let mut args = strings(&["create", machine.name, "--image", machine.image, "--cpus"]);
    args.push(machine.cpus.to_string());
    if let Some(mib) = machine.memory_mib {
        args.extend(["--mem".into(), mib.to_string()]);
    }
    if let Some(gib) = machine.disk_gib {
        args.extend(["--storage".into(), gib.to_string()]);
    }
    if let Some(user) = machine.user {
        args.extend(["--user".into(), user.into()]);
    }
    if let Some(workdir) = machine.workdir {
        args.extend(["--workdir".into(), workdir.into()]);
    }
    for (key, value) in &machine.labels {
        args.extend(["--label".into(), format!("{key}={value}")]);
    }
    for m in &machine.mounts {
        args.extend([
            "-v".into(),
            format!("{}:{}:ro", m.source.display(), m.target),
        ]);
    }
    for (key, value) in &machine.env {
        args.extend(["-e".into(), format!("{key}={value}")]);
    }
    let input = network.map(|n| {
        args.extend(["--network".into(), "-".into()]);
        policy_json(n)
    });
    run(cli, &args, input.as_deref().map(str::as_bytes))?;
    Ok(())
}

/// Give a stopped machine the policy its connections are decided by (one made before it had one,
/// say).
pub fn configure(machine: &str, network: &Policy, cli: &Cli) -> Result<(), String> {
    run(
        cli,
        &strings(&["configure", machine, "--network", "-"]),
        Some(policy_json(network).as_bytes()),
    )?;
    Ok(())
}

fn policy_json(policy: &Policy) -> String {
    serde_json::to_string(policy).expect("a policy is JSON")
}

/// Start a machine: by the policy it was made with, nothing else needed.
pub fn start(machine: &str, cli: &Cli) -> Result<(), String> {
    run(cli, &strings(&["start", machine]), None)?;
    Ok(())
}

/// Delete a machine; a no-op if there's none.
pub fn delete(machine: &str, cli: &Cli) -> Result<(), String> {
    if state(machine, cli)?.is_some() {
        run(cli, &strings(&["delete", machine, "-f"]), None)?;
    }
    Ok(())
}

/// Every machine there, whoever made them, by name: its state, as state says it.
pub fn states(cli: &Cli) -> Result<BTreeMap<String, State>, String> {
    let listed = run(cli, &strings(&["ls", "--json"]), None)?;
    let machines: Vec<listing::Machine> = serde_json::from_str(if listed.trim().is_empty() {
        "[]"
    } else {
        &listed
    })
    .map_err(|e| format!("bottle-cli ls: {e}"))?;
    Ok(machines.into_iter().map(|m| (m.name, m.state)).collect())
}

/// Where `machine` is; None if there's no such machine.
pub fn state(machine: &str, cli: &Cli) -> Result<Option<State>, String> {
    match run(cli, &strings(&["status", machine, "--json"]), None) {
        Ok(said) => {
            let status: listing::Status =
                serde_json::from_str(&said).map_err(|e| format!("bottle-cli status: {e}"))?;
            Ok(Some(status.state))
        }
        Err(Error::Missing(_)) => Ok(None),
        Err(Error::Failed(m)) => Err(m),
    }
}

/// How a command runs in a machine: as whom, where, with what.
#[derive(Default)]
pub struct Exec<'a> {
    pub user: Option<&'a str>,
    pub workdir: Option<&'a str>,
    pub env: BTreeMap<String, String>,
    pub tty: bool,
}

/// bottle-cli's exec arguments.
pub fn exec_args(machine: &str, argv: &[String], how: &Exec) -> Vec<String> {
    let mut args = vec!["exec".to_string(), machine.to_string()];
    if how.tty {
        args.push("-t".into());
    }
    if let Some(user) = how.user {
        args.extend(["-u".into(), user.into()]);
    }
    if let Some(workdir) = how.workdir {
        args.extend(["-w".into(), workdir.into()]);
    }
    for (key, value) in &how.env {
        args.extend(["-e".into(), format!("{key}={value}")]);
    }
    args.push("--".into());
    args.extend(argv.iter().cloned());
    args
}

/// Run a command in a running machine and return its output; `input` is its stdin.
pub fn exec(
    machine: &str,
    argv: &[String],
    how: &Exec,
    input: Option<&str>,
    cli: &Cli,
) -> Result<String, String> {
    Ok(run(
        cli,
        &exec_args(machine, argv, how),
        Some(input.unwrap_or_default().as_bytes()),
    )?)
}

/// The command line that runs `argv` in a machine with stdin attached (and a terminal, if
/// `how.tty`), for other tools to run.
pub fn exec_command(machine: &str, argv: &[String], how: &Exec, cli: &Cli) -> Vec<String> {
    cli.command(&exec_args(machine, argv, how))
}

/// The machine's memory (memory_mib) and cores (cpus), as bottle-cli there sees them.
pub fn info(cli: &Cli) -> Result<listing::Info, String> {
    let said = run(cli, &strings(&["info", "--json"]), None)?;
    serde_json::from_str(&said).map_err(|e| format!("bottle-cli info: {e}"))
}

/// bottle-cli `args` there, its stdout; its failure as images said one: `bottle-cli A B: stderr`.
pub fn images_cli(cli: &Cli, args: &[&str]) -> Result<String, String> {
    let output = cli.run(&strings(args), None)?;
    if !output.status.success() {
        return Err(format!(
            "bottle-cli {}: {}",
            args.iter().take(2).cloned().collect::<Vec<_>>().join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir;

    #[test]
    fn the_binary_is_bottle_clis_unless_named_and_a_runners_is_through_this_program() {
        let env = tempdir::isolated();
        tempdir::set(&env, "BOTTLE_CLI", "");
        assert_eq!(binary(), "bottle-cli");
        tempdir::set(&env, "BOTTLE_CLI", "/opt/bottle-cli");
        assert_eq!(binary(), "/opt/bottle-cli");
        tempdir::set(&env, "FOLIO_CLI", "/opt/folio");
        assert_eq!(
            Cli::runner("box").command(&["ls".into()]),
            ["/opt/folio", "on", "box", "ls"]
        );
        let quick = Cli::Runner {
            name: "box".into(),
            connect_timeout: Some(3),
        };
        assert_eq!(
            quick.command(&[]),
            ["/opt/folio", "on", "--connect-timeout", "3", "box"]
        );
        tempdir::set(&env, "FOLIO_CLI", "");
    }

    #[test]
    fn an_exec_as_bottle_cli_takes_it() {
        let how = Exec {
            user: Some("genie"),
            workdir: Some("/workspace"),
            env: BTreeMap::from([("A".into(), "1".into())]),
            tty: true,
        };
        assert_eq!(
            exec_args("m", &["git".into(), "push".into()], &how),
            [
                "exec",
                "m",
                "-t",
                "-u",
                "genie",
                "-w",
                "/workspace",
                "-e",
                "A=1",
                "--",
                "git",
                "push"
            ]
        );
    }

    #[test]
    fn its_message_is_the_last_line_it_said() {
        assert_eq!(
            error_of("warming up\nbottle-cli: no machine m\n"),
            "no machine m"
        );
        assert_eq!(error_of("something else\n"), "something else");
    }
}
