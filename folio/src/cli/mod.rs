//! What you can type, and what happens when you do.

use admin::{auth_list, auth_remove, auth_set, repo_list, runner_command, serve_start, serve_stop};
use commands::command;
use configure::{configure, sidecar_configure, sidecar_remove, sidecar_shell};
use thread_commands::{
    attach, broker_command, cp, delete, exec, list, new, notes_command, pause, rename,
};

use std::process::ExitCode;

use clap::ArgMatches;

use crate::engine::Engine;
use crate::repos::{self, Repo};
use crate::threads::{self};

mod admin;
mod commands;
mod configure;
mod output;
mod thread_commands;

const PICK: &str = "choose one from a list";
const DISK_HELP: &str = "the disk it writes to, in G, e.g. 50G (default: 20G)";
const RUNNER_HELP: &str = "the runner to make its bottle on (default: the repo's, from `folio configure`, else this machine)";

type Outcome = Result<i32, String>;

/// folio's commands, with `args` (the program's name first).
pub fn main(args: Vec<String>) -> ExitCode {
    let asked = shell_words(&args);
    let matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(e) => e.exit(),
    };
    // Runners' commands are runner.rs's, and so are their signals: a forward ends on SIGTERM.
    if matches.subcommand_name() != Some("runner") {
        crate::interrupt::install();
    }
    crate::log::to_file("folio");
    crate::log::write("command", &asked);
    let began = std::time::Instant::now();
    // What folio keeps, in the format it reads, before it reads any.
    if let Err(e) = crate::migrate::ensure() {
        crate::log::write("command", &format!("failed converting folio's home: {e}"));
        eprintln!("folio: error: {e}");
        return ExitCode::from(crate::FAILED);
    }
    let (name, sub) = matches.subcommand().unwrap();
    // What this command learns of bottles, kept while it runs.
    let engine = &Engine::default();
    let outcome = match name {
        "new" => new(engine, sub),
        "claude" => attach(engine, sub, threads::CLAUDE, "claude"),
        "shell" => attach(engine, sub, threads::SHELL, "shell"),
        "exec" => exec(engine, sub, false, "exec"),
        "cp" => cp(engine, sub, false, "cp"),
        "pause" => pause(engine, sub),
        "notes" => notes_command(engine, sub),
        "rename" => rename(engine, sub),
        "list" => list(engine),
        "delete" => delete(engine, sub),
        "configure" => configure(engine, sub),
        "sidecar" => {
            let (name, sub) = sub.subcommand().unwrap();
            match name {
                "configure" => sidecar_configure(engine, sub),
                "remove" => sidecar_remove(engine, sub),
                "shell" => sidecar_shell(engine, sub),
                "exec" => exec(engine, sub, true, "sidecar exec"),
                _ => cp(engine, sub, true, "sidecar cp"),
            }
        }
        "broker" => broker_command(engine, sub),
        "repo" => repo_list(),
        "serve" => match sub.subcommand().unwrap() {
            ("start", sub) => serve_start(sub),
            _ => serve_stop(),
        },
        "auth" => match sub.subcommand().unwrap() {
            ("list", _) => auth_list(),
            ("set", sub) => auth_set(sub),
            (_, sub) => auth_remove(sub),
        },
        _ => return runner_command(sub),
    };
    let took = began.elapsed().as_secs_f64();
    match outcome {
        Ok(code) => {
            crate::log::write("command", &format!("exited {code} in {took:.1}s"));
            ExitCode::from(code as u8)
        }
        Err(e) => {
            crate::log::write("command", &format!("failed in {took:.1}s: {e}"));
            eprintln!("folio: error: {e}");
            // Interrupted, as a shell has it: 128 and the signal.
            ExitCode::from(if crate::interrupt::happened() { 130 } else { 1 })
        }
    }
}

/// A command line, as a shell would take it back.
fn shell_words(args: &[String]) -> String {
    let mut words = vec!["folio".to_string()];
    words.extend(args.iter().skip(1).map(|a| crate::shell::quote(a)));
    words.join(" ")
}

fn arg<'a>(matches: &'a ArgMatches, name: &str) -> Option<&'a str> {
    matches.get_one::<String>(name).map(String::as_str)
}

fn args(matches: &ArgMatches, name: &str) -> Vec<String> {
    matches
        .get_many::<String>(name)
        .into_iter()
        .flatten()
        .cloned()
        .collect()
}

fn flag(matches: &ArgMatches, name: &str) -> bool {
    matches.get_flag(name)
}

fn current() -> Result<Repo, String> {
    repos::current(None)
}

/// A usage error: the command's usage, then why, and exit 2, as clap's own are.
fn usage_error(command: &str, message: &str) -> ! {
    let mut folio = self::command();
    let sub = command
        .split(' ')
        .fold(&mut folio, |c, name| c.find_subcommand_mut(name).unwrap());
    eprintln!("{}", sub.render_usage());
    eprintln!("folio {command}: error: {message}");
    std::process::exit(2)
}

#[cfg(test)]
mod tests {
    use super::commands::command;
    use super::output::{ago, human_size};
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn sizes_and_ages_as_folio_said_them() {
        assert_eq!(human_size(512), "512B");
        assert_eq!(human_size(1536), "1.5K");
        assert_eq!(human_size(3 << 30), "3.0G");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert_eq!(ago(now), "just now");
        assert_eq!(ago(now - 7200.5), "2h ago");
        assert_eq!(ago(now + 100.0), "just now");
    }

    #[test]
    fn the_commands_hold_together() {
        command().debug_assert();
    }

    #[test]
    fn an_exec_takes_everything_after_its_thread() {
        let m = command()
            .try_get_matches_from(["folio", "exec", "-t", "t", "ls", "-la", "--", "x"])
            .unwrap();
        let (_, sub) = m.subcommand().unwrap();
        assert!(sub.get_flag("tty"));
        assert_eq!(args(sub, "argv"), ["ls", "-la", "--", "x"]);
        let m = command()
            .try_get_matches_from(["folio", "runner", "add", "box", "--ssh", "box"])
            .unwrap();
        assert_eq!(
            args(m.subcommand().unwrap().1, "arguments"),
            ["add", "box", "--ssh", "box"]
        );
    }
}
