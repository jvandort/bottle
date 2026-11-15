//! What you can type, and what happens when you do.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde_json::{Value, json};

use crate::repos::{self, Repo};
use crate::threads::{self, Sidecar, Thread};
use crate::{auth, broker, daemon, engine, features, notes, obsidian, picker};

const PICK: &str = "choose one from a list";
const DISK_HELP: &str = "the disk it writes to, in G, e.g. 50G (default: 20G)";
const RUNNER_HELP: &str = "the runner to make its bottle on (default: the repo's, from `folio configure`, else this machine)";

type Outcome = Result<i32, String>;

fn thread_arg(optional: bool) -> Arg {
    let arg = Arg::new("thread").value_name("THREAD");
    if optional {
        arg.required(false)
            .help(format!("the thread (default: {PICK})"))
    } else {
        arg.required(true).help("the thread")
    }
}

fn exec_and_cp(sidecar: bool) -> [Command; 2] {
    let (what, when) = if sidecar {
        ("the thread's sidecar", "while the thread runs")
    } else {
        (
            "the thread's bottle",
            "the thread brought to running first, as `folio shell` does",
        )
    };
    let exec = Command::new("exec")
        .about(format!("run a command in {what}"))
        .long_about(format!(
            "Run a command in {what}, as genie, at its first mount, and exit with its status: {when}. Put -- \
             before the command if it takes options of its own."
        ))
        .arg(Arg::new("tty").short('t').action(ArgAction::SetTrue).help("give it a terminal"))
        .arg(thread_arg(false))
        .arg(
            Arg::new("argv")
                .value_name("COMMAND")
                .num_args(0..)
                .trailing_var_arg(true)
                .allow_hyphen_values(true)
                .help("the command, and its arguments"),
        );
    let cp = Command::new("cp")
        .about(format!("copy into or out of {what}"))
        .long_about(format!(
            "Copy a file or a directory as `cp -r` would: into DST if it's a directory, else as DST. THREAD:PATH \
             is a path in {what} (a relative one from its first mount), wherever it runs; any other is a path \
             here: {when}."
        ))
        .arg(Arg::new("src").value_name("SRC").required(true).help("THREAD:PATH, or a path here"))
        .arg(Arg::new("dst").value_name("DST").required(true).help("THREAD:PATH, or a path here"));
    [exec, cp]
}

fn configure_arguments(command: Command, runner_help: &'static str) -> Command {
    command
        .arg(
            Arg::new("feature")
                .long("feature")
                .value_name("FEATURE")
                .action(ArgAction::Append)
                .help("add a feature, optionally with options, e.g. jvm:version=21; one there already gets these options (repeatable)"),
        )
        .arg(
            Arg::new("no-feature")
                .long("no-feature")
                .value_name("FEATURE")
                .action(ArgAction::Append)
                .help("take a feature away, by its name alone, e.g. jvm (repeatable)"),
        )
        .arg(Arg::new("memory").long("memory").value_name("MEMORY").help(format!(
            "memory for each bottle, in M or G, e.g. 8G (default: {})",
            repos::DEFAULT_MEMORY
        )))
        .arg(Arg::new("disk").long("disk").value_name("DISK").help(DISK_HELP).conflicts_with("no-disk"))
        .arg(Arg::new("no-disk").long("no-disk").action(ArgAction::SetTrue).help("the default disk again"))
        .arg(Arg::new("runner").long("runner").value_name("RUNNER").help(runner_help).conflicts_with("no-runner"))
        .arg(Arg::new("no-runner").long("no-runner").action(ArgAction::SetTrue).help("this machine again"))
}

fn command() -> Command {
    let mut attaches = Vec::new();
    for (name, what) in [("claude", "Claude"), ("shell", "shell")] {
        attaches.push(
            Command::new(name)
                .about(format!("attach to a thread's {what}"))
                .long_about(format!(
                    "Attach to the thread's {what}, starting its bottle and the session if they've stopped, or \
                     starting the thread itself, as `folio new` does, if there's none of that name. Closing the \
                     terminal only detaches: what's running in it carries on."
                ))
                .arg(thread_arg(true))
                .arg(Arg::new("runner").long("runner").value_name("RUNNER").help(format!(
                    "{RUNNER_HELP}; a paused thread resumes there"
                ))),
        );
    }
    let mut sidecar = Command::new("sidecar")
        .about("a thread's sidecar: a bottle of its own on another runner, which the thread ssh-es to")
        .long_about(
            "A thread's sidecar: a bottle with a spec and runner of its own (a bigger machine, say), the thread's \
             repos fetch-only, which the thread's bottle reaches as `ssh sidecar`. It's scratch: made and started \
             with the thread's bottle, and deleted with it, a pause included. A change reaches a thread with a \
             bottle when it's paused and resumed.",
        )
        .subcommand_required(true)
        .subcommand(configure_arguments(
            Command::new("configure")
                .about("the thread's sidecar, as `folio configure` the repo's spec: --runner gives it one")
                .long_about(
                    "The thread's sidecar: shown without options, changed with them, as `folio configure` does the \
                     repo's spec; a thread without one is given one, which needs --runner. Bottle checks its spec \
                     before it's saved.",
                )
                .arg(thread_arg(true)),
            "the runner it's made on (`folio runner ls` lists them)",
        ))
        .subcommand(Command::new("remove").about("take the thread's sidecar away").arg(thread_arg(true)))
        .subcommand(
            Command::new("shell")
                .about("a shell in the running thread's sidecar, at /workspace, for as long as you're in it")
                .arg(thread_arg(true)),
        );
    for c in exec_and_cp(true) {
        sidecar = sidecar.subcommand(c);
    }
    let [exec, cp] = exec_and_cp(false);
    Command::new("folio")
        .about("Threads of work in bottles: each one a bottle, a lane for its commits, and Claude.")
        .subcommand_required(true)
        .subcommand(
            Command::new("new")
                .about("start a thread")
                .long_about(
                    "Start a thread in the repo you're in: a bottle for it, and a lane for its commits, fetched from \
                     the remote bottle/THREAD. Then `folio claude THREAD` or `folio shell THREAD` to work in it.",
                )
                .arg(
                    Arg::new("thread")
                        .value_name("THREAD")
                        .required(true)
                        .help("its name: lowercase words joined by '-', e.g. fix-flaky-test"),
                )
                .arg(
                    Arg::new("from")
                        .long("from")
                        .value_name("BRANCH")
                        .help("the branch to start from (default: origin's default branch)"),
                )
                .arg(Arg::new("runner").long("runner").value_name("RUNNER").help(RUNNER_HELP)),
        )
        .subcommands(attaches)
        .subcommand(exec)
        .subcommand(cp)
        .subcommand(
            Command::new("pause")
                .about("snapshot a thread and delete its bottle")
                .long_about(
                    "Snapshot the thread -- its workspace, every branch and stash, uncommitted changes, and \
                     Claude's conversation -- and delete its bottle. Only running processes are lost. `folio \
                     claude` or `folio shell` resumes it, in a new bottle made from the repo's spec as it is then.",
                )
                .arg(thread_arg(true)),
        )
        .subcommand(
            Command::new("notes")
                .about("open a thread's notes in Obsidian")
                .long_about(
                    "Open the thread's notes -- the repo its agent keeps at /notes -- in Obsidian, adding them as a \
                     vault the first time. A deleted thread's notes are kept, and open the same way.",
                )
                .arg(thread_arg(true)),
        )
        .subcommand(
            Command::new("rename")
                .about("rename a paused thread")
                .long_about(
                    "Rename a thread: its record, its lane, and the bottle/THREAD remote, which your branches \
                     tracking it follow. Its branches keep their names. A thread with a bottle is paused first: \
                     `folio pause THREAD`.",
                )
                .arg(thread_arg(false))
                .arg(
                    Arg::new("new")
                        .value_name("NEW")
                        .required(true)
                        .help("its new name: lowercase words joined by '-'"),
                ),
        )
        .subcommand(
            Command::new("list")
                .visible_aliases(["ls", "status"])
                .about("the repo's threads")
                .long_about("The repo's threads, most recently active first."),
        )
        .subcommand(
            Command::new("delete")
                .about("delete a thread")
                .long_about(
                    "Delete a thread: its bottle, its lane and the bottle/THREAD remote, and what folio knows about \
                     it. Says first what that would lose, and asks you to type the thread's name.",
                )
                .arg(thread_arg(true))
                .arg(
                    Arg::new("yes")
                        .long("yes")
                        .action(ArgAction::SetTrue)
                        .help("don't ask (needed without a terminal)"),
                ),
        )
        .subcommand(configure_arguments(
            Command::new("configure")
                .about("the repo's spec: its threads' features, memory and disk, and its runner")
                .long_about(
                    "The repo's spec: shown without options, changed with them. Bottle checks it before it's saved. \
                     Threads get it in their next bottle; a bottle already running keeps what it has.",
                ),
            "the runner the repo's threads' bottles are made on, unless a thread is given its own (`folio runner \
             ls` lists them; default: this machine)",
        ))
        .subcommand(sidecar)
        .subcommand(
            Command::new("broker")
                .about("the broker: the thread that talks to the others, and keeps the notes")
                .long_about(
                    "The broker: one thread outside your repos, whose agent talks to the others' and keeps a \
                     long-term memory of it all, as notes in its vault, an Obsidian vault folio owns. `claude` (the \
                     default) and `shell` attach to it, starting it if need be; `pause` pauses it, as `folio pause` \
                     does a thread; `notes` opens its vault in Obsidian.",
                )
                .arg(
                    Arg::new("command")
                        .default_value("claude")
                        .value_parser(["claude", "shell", "pause", "notes"])
                        .help("what to do (default: claude)"),
                ),
        )
        .subcommand(
            Command::new("repo").about("the repos folio knows").subcommand_required(true).subcommand(
                Command::new("list")
                    .visible_alias("ls")
                    .about("every repo folio has state for, wherever you are"),
            ),
        )
        .subcommand(
            Command::new("serve")
                .about("what bottles' machines route to folio: their git, credentials, ssh and messages")
                .long_about(
                    "Start or stop folio serve, which answers what bottles' machines route to it, on sockets in \
                     $FOLIO_HOME/services: bottle.host (their repos' git, messages between bottles), ssh to bottles, \
                     and credentials' hosts. bottle starts it when it's needed; its log is \
                     $FOLIO_HOME/logs/serve.log.",
                )
                .subcommand_required(true)
                .subcommand(
                    Command::new("start").about("start it in the background").arg(
                        Arg::new("foreground")
                            .long("foreground")
                            .action(ArgAction::SetTrue)
                            .help("run it in this process instead"),
                    ),
                )
                .subcommand(Command::new("stop").about("stop it; bottles' machines go on, but for what they route")),
        )
        .subcommand(
            Command::new("auth")
                .about("credentials: set once; bottles get those their features need")
                .subcommand_required(true)
                .subcommand(Command::new("list").about("list credentials features declare, and which are set"))
                .subcommand(
                    Command::new("set")
                        .about("store a credential in the Keychain, read from stdin")
                        .long_about(
                            "Store a credential in the Keychain, read from stdin (asked for, unechoed, on a \
                             terminal): `claude setup-token` makes claude's.",
                        )
                        .arg(
                            Arg::new("name")
                                .value_name("CREDENTIAL")
                                .required(true)
                                .help("the credential, e.g. claude"),
                        ),
                )
                .subcommand(
                    Command::new("remove").visible_alias("rm").about("remove a stored credential").arg(
                        Arg::new("name")
                            .value_name("CREDENTIAL")
                            .required(true)
                            .help("the credential, e.g. claude"),
                    ),
                ),
        )
        .subcommand(
            Command::new("runner")
                .about("the other machines folio makes bottles on: add|ls|rm|ssh|check|detach|attach")
                .long_about(
                    "The other machines folio makes bottles on, over ssh (`folio runner add NAME --ssh DESTINATION`, \
                     `ls`, `rm NAME`, `ssh NAME [COMMAND]`, `check NAME|DESTINATION`, `detach NAME [--offline]`, \
                     `attach NAME`). A runner folio has bottles on is removed only once they're deleted.",
                )
                .disable_help_flag(true)
                .arg(
                    Arg::new("arguments")
                        .value_name("...")
                        .num_args(0..)
                        .trailing_var_arg(true)
                        .allow_hyphen_values(true),
                ),
        )
}

/// folio's commands, with `args` (the program's name first).
pub fn main(args: Vec<String>) -> ExitCode {
    let matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(e) => e.exit(),
    };
    crate::interrupt::install();
    let (name, sub) = matches.subcommand().unwrap();
    let outcome = match name {
        "new" => new(sub),
        "claude" => attach(sub, threads::CLAUDE, "claude"),
        "shell" => attach(sub, threads::SHELL, "shell"),
        "exec" => exec(sub, false, "exec"),
        "cp" => cp(sub, false, "cp"),
        "pause" => pause(sub),
        "notes" => notes_command(sub),
        "rename" => rename(sub),
        "list" => list(),
        "delete" => delete(sub),
        "configure" => configure(sub),
        "sidecar" => {
            let (name, sub) = sub.subcommand().unwrap();
            match name {
                "configure" => sidecar_configure(sub),
                "remove" => sidecar_remove(sub),
                "shell" => sidecar_shell(sub),
                "exec" => exec(sub, true, "sidecar exec"),
                _ => cp(sub, true, "sidecar cp"),
            }
        }
        "broker" => broker_command(sub),
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
    match outcome {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("folio: error: {e}");
            // Interrupted, as a shell has it: 128 and the signal.
            ExitCode::from(if crate::interrupt::happened() { 130 } else { 1 })
        }
    }
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

fn exec(matches: &ArgMatches, sidecar: bool, command: &str) -> Outcome {
    let repo = current()?;
    let mut argv = args(matches, "argv");
    if argv.first().map(String::as_str) == Some("--") {
        argv.remove(0);
    }
    if argv.is_empty() {
        return Err(format!(
            "give the command to run: `folio {command} THREAD -- COMMAND`"
        ));
    }
    let thread = threads::load(&repo, arg(matches, "thread").unwrap())?;
    match threads::exec(&repo, &thread, &argv, sidecar, flag(matches, "tty"))? {}
}

fn cp(matches: &ArgMatches, sidecar: bool, command: &str) -> Outcome {
    let repo = current()?;
    // THREAD:PATH, if what's before the first colon is a thread of the repo's; else a path here.
    let side = |given: &str| -> Result<(Option<Thread>, String), String> {
        if let Some((name, path)) = given.split_once(':')
            && threads::exists(&repo, name)
        {
            return Ok((
                Some(threads::load(&repo, name)?),
                if path.is_empty() {
                    ".".into()
                } else {
                    path.into()
                },
            ));
        }
        Ok((None, given.to_string()))
    };
    let (src, dst) = (
        side(arg(matches, "src").unwrap())?,
        side(arg(matches, "dst").unwrap())?,
    );
    if src.0.is_none() && dst.0.is_none() {
        return Err(format!(
            "`folio {command}` copies into or out of a thread's {}: name one, THREAD:PATH",
            if sidecar { "sidecar" } else { "bottle" }
        ));
    }
    let went = threads::copy(
        &repo,
        (src.0.as_ref(), &src.1),
        (dst.0.as_ref(), &dst.1),
        sidecar,
    )?;
    match &dst.0 {
        Some(thread) => println!("{}:{went}", thread.name),
        None => println!("{went}"),
    }
    Ok(0)
}

/// folio's runner commands (runner.rs): but for removing a runner folio still has bottles on,
/// refused here.
fn runner_command(matches: &ArgMatches) -> ExitCode {
    let arguments = args(matches, "arguments");
    if arguments.first().map(String::as_str) == Some("rm") && arguments.len() > 1 {
        let name = &arguments[1];
        let there: Vec<String> = match crate::bottles::load() {
            Ok(bottles) => bottles
                .into_values()
                .filter(|b| &b.runner == name)
                .map(|b| b.name)
                .collect(),
            Err(e) => {
                eprintln!("folio: error: {e}");
                return ExitCode::from(1);
            }
        };
        if !there.is_empty() {
            eprintln!(
                "folio: error: runner {name} has bottles: {}; delete their threads first",
                there.join(", ")
            );
            return ExitCode::from(1);
        }
    }
    crate::runner::main(arguments.into_iter())
}

fn new(matches: &ArgMatches) -> Outcome {
    let thread = start(
        &current()?,
        arg(matches, "thread").unwrap(),
        arg(matches, "from"),
        arg(matches, "runner"),
    )?;
    println!(
        "Work in it with: folio claude {name}  (or folio shell {name})",
        name = thread.name
    );
    Ok(0)
}

fn start(
    repo: &Repo,
    name: &str,
    base: Option<&str>,
    runner: Option<&str>,
) -> Result<Thread, String> {
    eprintln!("Starting thread {name} in {}...", repo.path.display());
    let thread = threads::create(repo, name, base, false, runner)?;
    println!(
        "Started thread {name} in bottle {}. Its branches: git fetch bottle/{name}",
        thread.bottle,
        name = thread.name
    );
    // Flushed: attaching next replaces this process, and with it anything still buffered.
    let _ = std::io::stdout().flush();
    Ok(thread)
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// A line typed after `prompt`; None at the end of input, or if Ctrl-C ends it (as Python's folio had
/// it: a prompt interrupted is a prompt not answered).
pub fn input(prompt: &str) -> Option<String> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INTERRUPTED: AtomicBool = AtomicBool::new(false);
    extern "C" fn interrupted(_: libc::c_int) {
        INTERRUPTED.store(true, Ordering::Relaxed);
    }
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    INTERRUPTED.store(false, Ordering::Relaxed);
    // SAFETY: sigaction with a handler that only sets an atomic; restored before this returns. No
    // SA_RESTART, so a read it interrupts returns.
    let previous = unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = interrupted as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        let mut previous: libc::sigaction = std::mem::zeroed();
        libc::sigaction(libc::SIGINT, &action, &mut previous);
        previous
    };
    let mut line = Vec::new();
    let answered = loop {
        let mut byte = 0u8;
        // SAFETY: reads one byte into `byte`.
        let n = unsafe { libc::read(libc::STDIN_FILENO, (&mut byte as *mut u8).cast(), 1) };
        match n {
            1 if byte == b'\n' => break true,
            1 => line.push(byte),
            0 => break false,
            _ if INTERRUPTED.load(Ordering::Relaxed) => break false,
            _ if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {
                continue;
            }
            _ => break false,
        }
    };
    // SAFETY: putting back what was there.
    unsafe { libc::sigaction(libc::SIGINT, &previous, std::ptr::null_mut()) };
    if !answered {
        println!();
        return None;
    }
    let line = String::from_utf8_lossy(&line).into_owned();
    Some(line.trim_end_matches('\r').to_string())
}

/// The thread the command names (started, if it's new and the command may start one), else the
/// one chosen in the picker; None if none was.
fn thread_of(
    matches: &ArgMatches,
    repo: &Repo,
    command: &str,
    new: bool,
) -> Result<Option<Thread>, String> {
    if let Some(name) = arg(matches, "thread") {
        if new && !threads::exists(repo, name) {
            let runner = matches
                .try_get_one::<String>("runner")
                .ok()
                .flatten()
                .map(String::as_str);
            return start(repo, name, None, runner).map(Some);
        }
        return threads::load(repo, name).map(Some);
    }
    if !interactive() {
        return Err(format!(
            "give the thread: `folio {command} THREAD` (choosing one needs a terminal)"
        ));
    }
    let all = threads::all(repo)?;
    if all.is_empty() && !new {
        return Err(format!(
            "{} has no threads; start one with `folio new THREAD`",
            repo.path.display()
        ));
    }
    let states = threads::states(repo, &all)?;
    let all = threads::recent(repo, all, &states);
    let width = all
        .iter()
        .map(|t| t.name.chars().count())
        .max()
        .unwrap_or(0);
    let mut choices = if new {
        vec![picker::new_thread()]
    } else {
        Vec::new()
    };
    for t in &all {
        let row = &states[&t.name];
        choices.push(picker::Choice {
            label: format!(
                "{:<width$}  {:<8}  {}",
                t.name,
                row["state"].as_str().unwrap_or_default(),
                ago(threads::last_active(repo, t, row))
            ),
            thread: Some(t.name.clone()),
        });
    }
    let Some(choice) = picker::pick(
        &format!("folio {command}: choose a thread (j/k, Enter; q to leave)"),
        choices,
    )?
    else {
        return Ok(None);
    };
    if let Some(name) = choice.thread {
        return threads::load(repo, &name).map(Some);
    }
    match input("The new thread's name: ").map(|n| n.trim().to_string()) {
        Some(name) if !name.is_empty() => start(repo, &name, None, None).map(Some),
        _ => Ok(None),
    }
}

fn attach(matches: &ArgMatches, session: &str, command: &str) -> Outcome {
    let repo = current()?;
    let Some(mut thread) = thread_of(matches, &repo, command, true)? else {
        return Ok(0);
    };
    if let Some(runner) = arg(matches, "runner") {
        thread = threads::place(&repo, &thread, runner)?;
    }
    match threads::attach(&repo, &thread, session)? {}
}

fn pause(matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let Some(thread) = thread_of(matches, &repo, "pause", false)? else {
        return Ok(0);
    };
    eprintln!("Pausing thread {}...", thread.name);
    threads::pause(&repo, &thread)?;
    println!(
        "Paused thread {name}. Resume it with: folio claude {name}",
        name = thread.name
    );
    Ok(0)
}

fn broker_command(matches: &ArgMatches) -> Outcome {
    let command = arg(matches, "command").unwrap_or("claude");
    let identity = broker::repo();
    let mut thread = if threads::exists(&identity, broker::NAME) {
        Some(threads::load(&identity, broker::NAME)?)
    } else {
        None
    };
    let state = match &thread {
        Some(t) => threads::states(&identity, std::slice::from_ref(t))?[broker::NAME]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
        None => "new".into(),
    };
    let notes_path = broker::notes_path(&identity);
    if matches!(state.as_str(), "new" | "missing" | "paused") && broker::migrate(&identity)? {
        eprintln!("Moved the broker's notes to {}", notes_path.display());
    } else if broker::old_vault().is_some() && command != "pause" {
        eprintln!(
            "The broker's notes move to {} when it's next paused (`folio broker pause`) and resumed",
            notes_path.display()
        );
    }
    if command == "notes" {
        let path = match broker::old_vault() {
            Some(old) => old,
            None => notes::ensure(&notes_path, "The broker's notes")?,
        };
        return open_notes(&path);
    }
    if thread.is_none() && command == "pause" {
        return Err("there's no broker to pause; `folio broker` starts it".into());
    }
    if thread.is_none() {
        eprintln!("Starting the broker...");
        thread = Some(threads::create(&identity, broker::NAME, None, true, None)?);
        println!("Started the broker. Its notes: {}", notes_path.display());
        let _ = std::io::stdout().flush();
    }
    let thread = thread.unwrap();
    if command == "pause" {
        eprintln!("Pausing the broker...");
        threads::pause(&identity, &thread)?;
        if broker::migrate(&identity)? {
            eprintln!("Moved the broker's notes to {}", notes_path.display());
        }
        println!("Paused the broker. Resume it with: folio broker");
        return Ok(0);
    }
    let session = if command == "claude" {
        threads::CLAUDE
    } else {
        threads::SHELL
    };
    match threads::attach(&identity, &thread, session)? {}
}

/// Open the vault in Obsidian, if it's installed; else say where it is.
fn open_notes(path: &Path) -> Outcome {
    if obsidian::installed() {
        println!("{}", obsidian::open_vault(path)?);
    } else {
        println!(
            "The notes are in {}: open that folder as a vault in Obsidian.",
            path.display()
        );
    }
    Ok(0)
}

fn notes_command(matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let name = match arg(matches, "thread") {
        Some(name) => name.to_string(), // a deleted thread's notes too
        None => match thread_of(matches, &repo, "notes", false)? {
            Some(thread) => thread.name,
            None => return Ok(0),
        },
    };
    let path = threads::notes_dir(&repo, &name);
    if !path.exists() {
        return Err(format!(
            "thread {name} has no notes yet: its bottle starts them"
        ));
    }
    open_notes(&path)
}

fn rename(matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let old = arg(matches, "thread").unwrap();
    let thread = threads::rename(
        &repo,
        &threads::load(&repo, old)?,
        arg(matches, "new").unwrap(),
    )?;
    println!(
        "Renamed thread {old} to {name}. Its branches: git fetch bottle/{name}; resume it with: folio claude {name}",
        name = thread.name
    );
    Ok(0)
}

fn list() -> Outcome {
    let repo = current()?;
    let all = threads::all(&repo)?;
    let states = threads::states(&repo, &all)?;
    let all = threads::recent(&repo, all, &states);
    let mut rows = vec![
        [
            "THREAD", "STATE", "RUNNER", "SIDECAR", "SNAPSHOT", "ACTIVE", "FEATURES",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for thread in &all {
        let row = &states[&thread.name];
        let state = row["state"].as_str().unwrap_or_default();
        let size = if state == "paused" {
            threads::snapshot_size(&repo, thread)
        } else {
            None
        };
        // Where its bottle is; for one without, where its next is made.
        let runner = match row
            .get("runner")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
        {
            Some(runner) => runner.to_string(),
            None => threads::runner_of(&repo, thread)?.unwrap_or_else(|| "local".into()),
        };
        // Its sidecar's runner, and how it is, unless there's none to speak of (a paused thread's).
        let sidecar = match &thread.sidecar {
            None => "-".to_string(),
            Some(s) => match row["sidecar"].as_str() {
                Some("missing") | None => s.runner.clone(),
                Some(state) => format!("{} ({state})", s.runner),
            },
        };
        let features: Vec<&str> = row
            .get("features")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        rows.push(vec![
            thread.name.clone(),
            state.to_string(),
            runner,
            sidecar,
            size.map_or("-".into(), human_size),
            ago(threads::last_active(&repo, thread, row)),
            if features.is_empty() {
                "-".into()
            } else {
                features.join(" ")
            },
        ]);
    }
    table(&rows);
    Ok(0)
}

fn delete(matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let Some(thread) = thread_of(matches, &repo, "delete", false)? else {
        return Ok(0);
    };
    if !flag(matches, "yes") {
        if !std::io::stdin().is_terminal() {
            return Err(format!(
                "deleting {} needs --yes when there's no terminal to ask on",
                thread.name
            ));
        }
        let lost = threads::losses(&repo, &thread)?;
        if lost.is_empty() {
            println!(
                "Deleting {} loses nothing that isn't on your branches or origin's.",
                thread.name
            );
        } else {
            println!("Deleting {} loses:", thread.name);
            for item in &lost {
                println!("  {item}");
            }
        }
        let typed =
            input(&format!("Type {} to delete it: ", thread.name)).map(|t| t.trim().to_string());
        if typed.as_deref() != Some(thread.name.as_str()) {
            println!("Not deleted.");
            return Ok(1);
        }
    }
    threads::delete(&repo, &thread)?;
    println!("Deleted thread {}", thread.name);
    Ok(0)
}

fn changes_spec(matches: &ArgMatches) -> bool {
    !args(matches, "feature").is_empty()
        || !args(matches, "no-feature").is_empty()
        || arg(matches, "memory").is_some()
        || arg(matches, "disk").is_some()
        || flag(matches, "no-disk")
}

/// Whether a configure command was given anything to change: else it shows.
fn changes(matches: &ArgMatches) -> bool {
    changes_spec(matches) || arg(matches, "runner").is_some() || flag(matches, "no-runner")
}

/// A usage error, as argparse said one: the command's usage, then why, and exit 2.
fn usage_error(command: &str, message: &str) -> ! {
    let mut folio = self::command();
    let sub = command
        .split(' ')
        .fold(&mut folio, |c, name| c.find_subcommand_mut(name).unwrap());
    eprintln!("{}", sub.render_usage());
    eprintln!("folio {command}: error: {message}");
    std::process::exit(2)
}

/// `spec` as a configure command changes it: features added (one there already gets the options
/// given) or taken away, memory or disk set, or the disk the default again.
fn changed(spec: &Value, matches: &ArgMatches, command: &str) -> Result<Value, String> {
    let mut spec = spec.clone();
    let (given, taken) = (args(matches, "feature"), args(matches, "no-feature"));
    for name in &taken {
        if name.contains(':') || name.contains('=') {
            usage_error(
                command,
                &format!(
                    "--no-feature takes a feature's name alone, e.g. {}",
                    repos::name_of(name)
                ),
            );
        }
    }
    let mut both: Vec<&str> = given
        .iter()
        .map(|g| repos::name_of(g))
        .filter(|n| taken.iter().any(|t| t == n))
        .collect();
    both.sort();
    both.dedup();
    if !both.is_empty() {
        usage_error(
            command,
            &format!("--feature and --no-feature both name {}", both.join(", ")),
        );
    }
    let features = repos::merge_features(&repos::features_of(&spec), &given);
    let there: Vec<&str> = features.iter().map(|f| repos::name_of(f)).collect();
    for name in &taken {
        if !there.contains(&name.as_str()) {
            return Err(format!(
                "there's no feature {name} to take away (features: {})",
                if there.is_empty() {
                    "none".to_string()
                } else {
                    there.join(", ")
                }
            ));
        }
    }
    let kept: Vec<String> = features
        .iter()
        .filter(|f| !taken.iter().any(|t| t == repos::name_of(f)))
        .cloned()
        .collect();
    spec["features"] = json!(kept);
    if let Some(memory) = arg(matches, "memory") {
        spec["memory"] = memory.into();
    }
    if let Some(disk) = arg(matches, "disk") {
        spec["disk"] = disk.into();
    }
    if flag(matches, "no-disk")
        && let Some(map) = spec.as_object_mut()
    {
        map.remove("disk");
    }
    Ok(spec)
}

fn configure(matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    if !changes(matches) {
        let shown = match repo.spec()? {
            Some(spec) => spec_text(&spec),
            None => format!(
                "{} (the default; nothing set yet)",
                spec_text(&repos::default_spec())
            ),
        };
        println!("{shown}{}", runner_text(&repo)?);
        return Ok(0);
    }
    let mut spec = Value::Null;
    if changes_spec(matches) {
        // Checked before anything's saved, the runner included.
        let current = repo.spec()?.unwrap_or_else(repos::default_spec);
        spec = engine::validate_spec(&repos::check_repo_spec(
            changed(&current, matches, "configure")?,
            false,
        )?)?;
    }
    if arg(matches, "runner").is_some() || flag(matches, "no-runner") {
        repo.save_runner(arg(matches, "runner"))?;
    }
    if changes_spec(matches) {
        repo.save_spec(&spec)?;
        println!(
            "Set {}'s spec: {}{}",
            repo.name,
            spec_text(&spec),
            runner_text(&repo)?
        );
    } else {
        println!(
            "Set {}'s runner: {}",
            repo.name,
            repo.runner()?.unwrap_or_else(|| "this machine".into())
        );
    }
    Ok(0)
}

fn sidecar_configure(matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let Some(thread) = thread_of(matches, &repo, "sidecar configure", false)? else {
        return Ok(0);
    };
    if !changes(matches) {
        let Some(sidecar) = &thread.sidecar else {
            println!("Thread {} has no sidecar", thread.name);
            return Ok(0);
        };
        let name = thread.sidecar_bottle();
        let state = engine::status(std::slice::from_ref(&name))?[&name]["state"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        println!("{}; bottle {name}, {state}", sidecar_text(sidecar));
        return Ok(0);
    }
    if flag(matches, "no-runner") {
        return Err(format!(
            "a sidecar is on a runner of its own; `folio sidecar remove {}` takes it away",
            thread.name
        ));
    }
    let runner = arg(matches, "runner");
    if thread.sidecar.is_none() && runner.is_none() {
        return Err(format!(
            "thread {name} has no sidecar; give it one with `folio sidecar configure {name} --runner RUNNER ...`",
            name = thread.name
        ));
    }
    let current = thread.sidecar.clone().unwrap_or_else(|| Sidecar {
        runner: runner.unwrap().into(),
        spec: json!({"features": [], "memory": repos::DEFAULT_MEMORY}),
    });
    let sidecar = Sidecar {
        runner: runner.map_or(current.runner.clone(), String::from),
        spec: changed(&current.spec, matches, "sidecar configure")?,
    };
    let thread = threads::set_sidecar(&repo, &thread, Some(sidecar))?;
    println!(
        "Set thread {}'s sidecar: {}{}",
        thread.name,
        sidecar_text(thread.sidecar.as_ref().unwrap()),
        when(&repo, &thread)?
    );
    Ok(0)
}

fn sidecar_remove(matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let Some(thread) = thread_of(matches, &repo, "sidecar remove", false)? else {
        return Ok(0);
    };
    if thread.sidecar.is_none() {
        println!("Thread {} has no sidecar", thread.name);
        return Ok(0);
    }
    let thread = threads::set_sidecar(&repo, &thread, None)?;
    println!(
        "Took thread {}'s sidecar away{}",
        thread.name,
        when(&repo, &thread)?
    );
    Ok(0)
}

fn sidecar_shell(matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let Some(thread) = thread_of(matches, &repo, "sidecar shell", false)? else {
        return Ok(0);
    };
    match threads::sidecar_shell(&repo, &thread)? {}
}

fn sidecar_text(sidecar: &Sidecar) -> String {
    format!("{}, runner {}", spec_text(&sidecar.spec), sidecar.runner)
}

/// When a change to its sidecar reaches the thread: at once, for one with no bottle.
fn when(repo: &Repo, thread: &Thread) -> Result<String, String> {
    let state = threads::states(repo, std::slice::from_ref(thread))?[&thread.name]["state"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    if matches!(state.as_str(), "paused" | "missing") {
        return Ok(String::new());
    }
    Ok(format!(
        ". It comes with the thread's next bottle: `folio pause {name}`, then `folio claude {name}`",
        name = thread.name
    ))
}

fn runner_text(repo: &Repo) -> Result<String, String> {
    Ok(repo
        .runner()?
        .map(|r| format!(", runner {r}"))
        .unwrap_or_default())
}

fn serve_start(matches: &ArgMatches) -> Outcome {
    if !flag(matches, "foreground") {
        println!(
            "{}",
            if daemon::start()? {
                "Started folio serve"
            } else {
                "folio serve is already running"
            }
        );
        return Ok(0);
    }
    daemon::run()
}

fn serve_stop() -> Outcome {
    println!(
        "{}",
        if daemon::stop()? {
            "Stopped folio serve"
        } else {
            "folio serve isn't running"
        }
    );
    Ok(0)
}

fn auth_list() -> Outcome {
    // Every credential is held by folio serve, so what's worth showing is where it's spent: the
    // hosts it's attached to, as the feature declares them (${option} is filled in from a bottle's
    // own options).
    let mut rows = vec![
        [
            "CREDENTIAL",
            "FEATURE",
            "STATUS",
            "ATTACHED TO",
            "DESCRIPTION",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for (declared, set) in auth::status()? {
        rows.push(vec![
            declared.credential.name.clone(),
            declared.feature.clone(),
            credential_status(&declared, set)?,
            declared.credential.inject.hosts.join(", "),
            declared.credential.description.clone(),
        ]);
    }
    table(&rows);
    Ok(0)
}

/// Whether the Keychain has it, and the file read before the Keychain, if the credential has one.
fn credential_status(declared: &auth::Declared, set: bool) -> Result<String, String> {
    let status = if set { "set" } else { "not set" };
    let Some(properties) = &declared.credential.properties else {
        return Ok(status.into());
    };
    let mut path = properties.clone();
    for (name, value) in features::load(&declared.feature)?.values() {
        path = path.replace(&format!("${{{name}}}"), &value.text());
    }
    Ok(format!("{path}, else {status}"))
}

/// A line from the terminal, unechoed, after `prompt`.
fn getpass(prompt: &str) -> Result<String, String> {
    let fd = libc::STDIN_FILENO;
    // SAFETY: tcgetattr and tcsetattr read and write the struct they're given, for stdin.
    let saved = unsafe {
        let mut term: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut term) != 0 {
            return Err("can't read the terminal's settings".into());
        }
        let mut quiet = term;
        quiet.c_lflag &= !libc::ECHO;
        libc::tcsetattr(fd, libc::TCSAFLUSH, &quiet);
        term
    };
    eprint!("{prompt}");
    let mut line = String::new();
    let read = std::io::stdin().lock().read_line(&mut line);
    // SAFETY: as above, putting back what was there.
    unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &saved) };
    eprintln!();
    read.map_err(|e| e.to_string())?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

fn auth_set(matches: &ArgMatches) -> Outcome {
    let name = arg(matches, "name").unwrap();
    let value = if std::io::stdin().is_terminal() {
        getpass(&format!("{name}: "))?
    } else {
        let mut value = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut value)
            .map_err(|e| e.to_string())?;
        value
    };
    auth::set_value(name, &value)?;
    println!("Stored {name}. Bottles with it get it when they next start.");
    Ok(0)
}

fn auth_remove(matches: &ArgMatches) -> Outcome {
    let name = arg(matches, "name").unwrap();
    if auth::remove(name)? {
        println!("Removed {name}");
    } else {
        println!("{name} wasn't set");
    }
    Ok(0)
}

fn repo_list() -> Outcome {
    let mut rows = vec![["REPO", "PATH", "THREADS"].map(String::from).to_vec()];
    for repo in repos::known() {
        let place = if repo.path == repos::owned_path(broker::NAME) {
            "(the broker)".to_string()
        } else if repo.path.is_dir() {
            repo.path.display().to_string()
        } else {
            format!("{} (gone)", repo.path.display())
        };
        rows.push(vec![
            repo.key(),
            place,
            threads::all(&repo)?.len().to_string(),
        ]);
    }
    table(&rows);
    Ok(0)
}

fn spec_text(spec: &Value) -> String {
    let disk = spec
        .get("disk")
        .and_then(Value::as_str)
        .map(|d| format!(", disk {d}"))
        .unwrap_or_default();
    format!(
        "features [{}], memory {}{disk}",
        repos::features_of(spec).join(", "),
        spec.get("memory")
            .and_then(Value::as_str)
            .unwrap_or_default()
    )
}

fn human_size(size: u64) -> String {
    for (unit, scale) in [("G", 1u64 << 30), ("M", 1 << 20), ("K", 1 << 10)] {
        if size >= scale {
            return format!("{:.1}{unit}", size as f64 / scale as f64);
        }
    }
    format!("{size}B")
}

fn ago(when: f64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let seconds = (now - when).max(0.0) as u64;
    for (unit, size) in [("d", 86400), ("h", 3600), ("m", 60)] {
        if seconds >= size {
            return format!("{}{unit} ago", seconds / size);
        }
    }
    "just now".into()
}

fn table(rows: &[Vec<String>]) {
    let widths: Vec<usize> = (0..rows[0].len())
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in rows {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect();
        println!("{}", cells.join("  ").trim_end());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
