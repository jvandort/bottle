//! folio's command line, as clap parses it.

use clap::{Arg, ArgAction, Command};

use crate::repos;

use super::{DISK_HELP, PICK, RUNNER_HELP};

pub(super) fn thread_arg(optional: bool) -> Arg {
    let arg = Arg::new("thread").value_name("THREAD");
    if optional {
        arg.required(false)
            .help(format!("the thread (default: {PICK})"))
    } else {
        arg.required(true).help("the thread")
    }
}

pub(super) fn exec_and_cp(sidecar: bool) -> [Command; 2] {
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

pub(super) fn configure_arguments(command: Command, runner_help: &'static str) -> Command {
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
                .help("take a feature away, by its name alone, e.g. jvm; a thread's own only, as one the repo gives can't be (repeatable)"),
        )
        .arg(Arg::new("memory").long("memory").value_name("MEMORY").conflicts_with("no-memory").help(format!(
            "memory for each bottle, in M or G, e.g. 8G (default: {})",
            repos::DEFAULT_MEMORY
        )))
        .arg(Arg::new("no-memory").long("no-memory").action(ArgAction::SetTrue).help(
            "the memory it'd have without: the default, for the repo; the repo's, for a thread",
        ))
        .arg(Arg::new("disk").long("disk").value_name("DISK").help(DISK_HELP).conflicts_with("no-disk"))
        .arg(Arg::new("no-disk").long("no-disk").action(ArgAction::SetTrue).help(
            "the disk it'd have without: the default, for the repo; the repo's, for a thread",
        ))
        .arg(Arg::new("runner").long("runner").value_name("RUNNER").help(runner_help).conflicts_with("no-runner"))
        .arg(Arg::new("no-runner").long("no-runner").action(ArgAction::SetTrue).help(
            "the runner it'd have without: this machine, for the repo; the repo's, for a thread",
        ))
}

pub(super) fn command() -> Command {
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
                .about("the thread's sidecar, as `folio configure THREAD` the thread: --runner gives it one")
                .override_usage("folio sidecar configure [THREAD] [OPTIONS]")
                .long_about(
                    "The thread's sidecar: shown without options, changed with them, as `folio configure THREAD` \
                     does the thread, over what a new sidecar has (no features, and 2G of memory) where the thread's \
                     are over the repo's; a thread without one is given one, which needs --runner. Bottle checks \
                     its spec before it's saved.",
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
                     Claude's conversation -- and delete its bottle. Only running processes are lost. Its agent \
                     is asked first to get ready, and the snapshot waits for it to say it is; --force doesn't \
                     ask. `folio claude` or `folio shell` resumes it, in a new bottle made from the repo's spec \
                     as it is then.",
                )
                .arg(thread_arg(true))
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("don't ask the agent to get ready: snapshot it as it is"),
                ),
        )
        .subcommand(
            Command::new("takeover")
                .about("resume a thread from its snapshot, its bottle's runner being off")
                .long_about(
                    "Resume a thread whose bottle is on a runner that's off (detached, or not answering) from its \
                     latest snapshot, in a new bottle here (or on --runner), its runner from then on. Whatever the \
                     bottle on the runner that's off did since the snapshot is thrown away: it says what that \
                     is, as far as it can, and you type the thread's name to agree. The old bottle is left there, \
                     answered by nothing, and deleted when the runner's attached again (`folio runner attach`).",
                )
                .arg(thread_arg(true))
                .arg(
                    Arg::new("runner")
                        .long("runner")
                        .value_name("RUNNER")
                        .help("where its new bottle is made (default: this machine, local)"),
                ),
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
                .about("the repo's spec and runner, or a thread's: features, memory and disk, and where it runs")
                .override_usage("folio configure [THREAD] [OPTIONS]")
                .long_about(
                    "The repo's spec and runner, or, given a thread, the thread's: shown without options, changed \
                     with them. Bottle checks a spec before it's saved. A thread has the repo's, but for what it's \
                     given of its own: features it adds or gives other options (it can't take away one the repo \
                     gives), and its memory, disk and runner, each shown as overridden; each --no-... takes its \
                     override away, and it has the repo's again. Threads get a change in their next bottle; a bottle already running keeps what \
                     it has, and one on a runner is paused to move.",
                )
                .arg(
                    Arg::new("thread")
                        .value_name("THREAD")
                        .help("the thread to configure (default: the repo)"),
                ),
            "the runner bottles are made on (`folio runner ls` lists them; `local`: this machine; default: the \
             repo's, else this machine)",
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
                     $FOLIO_HOME/services, one for each service: git (their repos' remotes), messages between \
                     bottles, the bottles running, ssh to bottles, and credentials' hosts. bottle starts it when it's needed; its log is \
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
