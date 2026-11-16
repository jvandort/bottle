//! bottle's hooks, for a push from a bottle to its hub: `git receive-pack`, which folio serve runs
//! for the push (git.rs), runs them on this machine, never in the bottle, with core.hooksPath at
//! scripts folio serve writes in folio's home that run this program's `hook` (hooks_dir). The repo's
//! own hooks never run.
//!
//!     folio-cli hook pre-receive|post-receive
//!
//! pre-receive is bottle's one rule for a push: that it has a namespace. What confines a push is
//! GIT_NAMESPACE, not this hook: receive-pack reads and writes only refs/namespaces/<it>/, so the
//! hub's own refs are neither advertised to the bottle nor reachable by it, and git's ordinary
//! rules (fast-forward only, no deletes: git.rs's config) apply within the lane. A push without
//! one would write the hub's own refs, so it's refused, whole.
//!
//! post-receive brings the hub's checked-out branches up to what the bottle pushed, once the push's
//! refs have moved. Its stdin is receive-pack's `<old> <new> <ref>` lines, the refs as the bottle
//! named them, inside its namespace. What it says on stdout reaches the bottle as `remote:` lines in
//! its push's output, and goes to $FOLIO_HOOK_LOG for folio serve's log.
//!
//! A branch of the hub's follows a push when its upstream is a remote reading the push's namespace
//! -- folio's `bottle/<thread>`, whose uploadpack is `git --namespace=<thread>-out upload-pack` --
//! and the bottle pushed the branch of that name. That remote is fetched first, so its tracking
//! refs are current too. On unless the repo says `git config bottle.follow false`.
//!
//! Following is `git merge --ff-only`, in whichever worktree has the branch checked out, and
//! nothing else: a branch with commits of the hub's own, a change in the way, or a worktree in the
//! middle of a merge, rebase, cherry-pick, revert or bisect is left as it is, and the bottle is
//! told why. A branch checked out nowhere -- in curate's read mode, say, where HEAD is
//! curate/<branch> -- isn't touched here. The repo's hooks don't run, as for the push itself, but
//! for the fetch, which is what you'd run yourself: its `reference-transaction` hook is how curate
//! brings a branch you're reading along, and what the hooks say goes to the bottle too.
//!
//! The bottle never learns host paths from this: a worktree is named by its directory's name, and
//! only when it isn't the main one.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};
use std::sync::OnceLock;

const USAGE: &str = "hook pre-receive|post-receive|lane-pre-receive";
const ZERO: &str = "0000000000000000000000000000000000000000";
/// What a worktree may be in the middle of, by the file under its git dir that says so.
const IN_PROGRESS: [(&str, &str); 7] = [
    ("MERGE_HEAD", "a merge"),
    ("CHERRY_PICK_HEAD", "a cherry-pick"),
    ("REVERT_HEAD", "a revert"),
    ("BISECT_LOG", "a bisect"),
    ("rebase-merge", "a rebase"),
    ("rebase-apply", "a rebase"),
    ("index.lock", "another git command"),
];

/// The hooks folio serve has receive-pack run: in folio's home, scripts that run this program,
/// written once a process (this program may have moved since the last one).
pub fn hooks_dir() -> io::Result<PathBuf> {
    static WRITTEN: OnceLock<Result<PathBuf, String>> = OnceLock::new();
    WRITTEN
        .get_or_init(|| {
            write(
                &crate::home().join("hooks"),
                &[
                    ("pre-receive", "pre-receive"),
                    ("post-receive", "post-receive"),
                ],
            )
            .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(io::Error::other)
}

/// Git's `hooks` in `dir`, each (the hook, this program's `hook` command it is) a script that runs
/// this program as that command.
pub fn write(dir: &Path, hooks: &[(&str, &str)]) -> io::Result<PathBuf> {
    let me = crate::bottle_cli::me();
    fs::create_dir_all(dir)?;
    for (hook, command) in hooks {
        let script = format!(
            "#!/bin/sh\n# folio's (serve/hook.rs).\nexec '{}' hook {command}\n",
            me.replace('\'', r"'\''")
        );
        // Whole or not at all: a push may be running the one there.
        let partial = dir.join(format!(".{hook}.{}", std::process::id()));
        fs::write(&partial, script)?;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))?;
        fs::rename(&partial, dir.join(hook))?;
    }
    Ok(dir.to_path_buf())
}

pub fn main(mut args: impl Iterator<Item = String>) -> ExitCode {
    match (args.next().as_deref(), args.next()) {
        (Some("pre-receive"), None) => pre_receive(),
        (Some("post-receive"), None) => post_receive(),
        (Some("lane-pre-receive"), None) => lane_pre_receive(),
        _ => crate::usage_error(USAGE),
    }
}

/// A push to a thread's lane remote (lanes.rs): a branch it deletes from the user's lane is
/// deleted from the agent's too, if it's still what the user last fetched of it, in one
/// compare-and-delete, so a push from the agent since keeps it, and the push is refused, saying so.
/// The -c settings the remote's receive-pack runs with say which remote and lane: folio.remote and
/// folio.out.
fn lane_pre_receive() -> ExitCode {
    // The rest is the repo's own refs, outside the namespace; and a delete needs no objects, so
    // none of the quarantine a push with new ones gets.
    let git = |args: &[&str]| -> Option<String> {
        let out = Command::new("git")
            .args(args)
            .env_remove("GIT_NAMESPACE")
            .env_remove("GIT_QUARANTINE_PATH")
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let (Some(remote), Some(out)) = (
        git(&["config", "folio.remote"]),
        git(&["config", "folio.out"]),
    ) else {
        return ExitCode::FAILURE;
    };
    let mut kept = false;
    for line in io::stdin().lines().map_while(Result::ok) {
        let mut words = line.split_whitespace();
        let (Some(_old), Some(new), Some(name)) = (words.next(), words.next(), words.next()) else {
            continue;
        };
        if new.chars().any(|c| c != '0') {
            continue;
        }
        let branch = name.strip_prefix("refs/heads/").unwrap_or(name);
        let lane = format!("refs/namespaces/{out}/{name}");
        let Some(agent) = git(&["rev-parse", "-q", "--verify", &lane]) else {
            continue;
        };
        let fetched = git(&[
            "rev-parse",
            "-q",
            "--verify",
            &format!("refs/remotes/{remote}/{branch}"),
        ]);
        if fetched.as_deref() == Some(agent.as_str())
            && git(&["update-ref", "-d", &lane, &agent]).is_some()
        {
            println!("folio: deleted {branch} from the agent's lane");
        } else {
            println!("folio: kept {branch}: the agent has pushed to it since you fetched it");
            kept = true;
        }
    }
    if kept {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn pre_receive() -> ExitCode {
    if std::env::var("GIT_NAMESPACE")
        .unwrap_or_default()
        .is_empty()
    {
        eprintln!("folio: this push has no namespace; refusing it");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// The hub's own git: none of receive-pack's environment (its namespace, its git dir, bottle's
/// config for the push), and, but for `hooks`, none of the repo's hooks.
fn git(cwd: &Path, hooks: bool, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    if !hooks {
        command.args(["-c", "core.hooksPath=/dev/null"]);
    }
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("GIT_") {
            command.env_remove(name);
        }
    }
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| Output {
            status: std::os::unix::process::ExitStatusExt::from_raw(127 << 8),
            stdout: vec![],
            stderr: format!("git: {e}").into_bytes(),
        })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn post_receive() -> ExitCode {
    let git_dir = std::env::var_os("GIT_DIR").map_or_else(|| PathBuf::from("."), PathBuf::from);
    let git_dir = fs::canonicalize(&git_dir).unwrap_or(git_dir);
    let namespace = std::env::var("GIT_NAMESPACE").unwrap_or_default();
    let mut pushed = BTreeMap::new();
    for line in io::stdin().lock().lines().map_while(Result::ok) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if let [_, new, reference] = fields[..]
            && let Some(branch) = reference.strip_prefix("refs/heads/")
            && new != ZERO
        {
            pushed.insert(branch.to_string(), new.to_string());
        }
    }
    if pushed.is_empty() || namespace.is_empty() {
        return ExitCode::SUCCESS;
    }
    let said: Vec<String> = follow_push(&git_dir, &namespace, &pushed)
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    for line in &said {
        println!("folio: {line}");
    }
    if let Some(log) = std::env::var_os("FOLIO_HOOK_LOG")
        && !said.is_empty()
        && let Ok(mut file) = fs::OpenOptions::new().append(true).create(true).open(log)
    {
        let _ = file.write_all(
            said.iter()
                .map(|s| format!("{s}\n"))
                .collect::<String>()
                .as_bytes(),
        );
    }
    ExitCode::SUCCESS
}

/// The hub's branches brought up to `pushed`, the branches the bottle pushed into `namespace`:
/// what happened, a line each, for the bottle.
fn follow_push(git_dir: &Path, namespace: &str, pushed: &BTreeMap<String, String>) -> Vec<String> {
    // One push at a time moves a repo's branches, whichever bottle it's from.
    let Ok(lock) = fs::File::create(git_dir.join("bottle-follow.lock")) else {
        return vec![];
    };
    // Released at return, as `lock` closes.
    let _ = lock.lock();
    let main_worktree = (git_dir.file_name().is_some_and(|n| n == ".git"))
        .then(|| git_dir.parent().unwrap().to_path_buf());
    let mut said = Vec::new();
    let mut targets: BTreeMap<String, String> = BTreeMap::new();
    if stdout(&git(git_dir, false, &["config", "--bool", "bottle.follow"])) != "false" {
        let readers = readers(git_dir, namespace);
        for remote in &readers {
            // With the repo's own hooks, from its working tree, as `git fetch` would be run there.
            let fetched = git(
                main_worktree.as_deref().unwrap_or(git_dir),
                true,
                &["fetch", "--quiet", remote],
            );
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&fetched.stderr),
                String::from_utf8_lossy(&fetched.stdout)
            );
            said.extend(
                text.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(String::from),
            );
        }
        let upstreams = git(
            git_dir,
            false,
            &[
                "for-each-ref",
                "--format=%(refname:short)\t%(upstream)",
                "refs/heads",
            ],
        );
        for line in stdout(&upstreams).lines() {
            let (local, upstream) = line.split_once('\t').unwrap_or((line, ""));
            for remote in &readers {
                if let Some(branch) = upstream.strip_prefix(&format!("refs/remotes/{remote}/"))
                    && let Some(commit) = pushed.get(branch)
                {
                    targets
                        .entry(local.to_string())
                        .or_insert_with(|| commit.clone());
                }
            }
        }
    }
    let checked_out = worktrees(git_dir);
    for (branch, commit) in &targets {
        let Some(at) = checked_out.get(branch) else {
            continue;
        };
        let label = if Some(at) == main_worktree.as_ref() {
            String::new()
        } else {
            let name = at
                .file_name()
                .map_or(String::new(), |n| n.to_string_lossy().into_owned());
            format!(" (in worktree {name})")
        };
        said.push(follow(branch, commit, at, &label));
    }
    said
}

/// The hub's remotes that read `namespace`: their uploadpack runs upload-pack in it.
fn readers(git_dir: &Path, namespace: &str) -> Vec<String> {
    let out = git(
        git_dir,
        false,
        &["config", "--get-regexp", r"^remote\..*\.uploadpack$"],
    );
    let flag = format!("--namespace={namespace}");
    stdout(&out)
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            let reads = value.match_indices(&flag).any(|(at, _)| {
                value[at + flag.len()..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
            });
            let name = key.strip_prefix("remote.")?.strip_suffix(".uploadpack")?;
            reads.then(|| name.to_string())
        })
        .collect()
}

/// Each checked-out branch's worktree, by branch name.
fn worktrees(git_dir: &Path) -> BTreeMap<String, PathBuf> {
    let out = git(git_dir, false, &["worktree", "list", "--porcelain"]);
    let mut result = BTreeMap::new();
    let mut path = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if let (Some(branch), Some(p)) = (line.strip_prefix("branch refs/heads/"), &path) {
            result.insert(branch.to_string(), p.clone());
        }
    }
    result
}

/// What git operation the worktree at `at` is in the middle of, if any.
fn busy(at: &Path) -> Option<&'static str> {
    IN_PROGRESS.iter().find_map(|(name, what)| {
        let path = stdout(&git(at, false, &["rev-parse", "--git-path", name]));
        (!path.is_empty() && at.join(path).exists()).then_some(*what)
    })
}

/// `branch`, checked out at `at`, fast-forwarded to `commit`; what happened, for the bottle.
fn follow(branch: &str, commit: &str, at: &Path, label: &str) -> String {
    let head = stdout(&git(
        at,
        false,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
    ));
    if head == commit {
        return String::new();
    }
    if !git(at, false, &["merge-base", "--is-ancestor", &head, commit])
        .status
        .success()
    {
        return format!("didn't move the user's {branch}{label}: it has commits yours doesn't");
    }
    if let Some(doing) = busy(at) {
        return format!("didn't move the user's {branch}{label}: it's in the middle of {doing}");
    }
    let merged = git(at, false, &["merge", "--ff-only", "--quiet", commit]);
    if !merged.status.success() {
        let said = if merged.stderr.is_empty() {
            &merged.stdout
        } else {
            &merged.stderr
        };
        let said = String::from_utf8_lossy(said);
        let lines: Vec<&str> = said.lines().collect();
        let mut why = lines
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty())
            .map(|l| l.strip_prefix("error: ").unwrap_or(l).to_string())
            .unwrap_or_else(|| format!("exit {}", merged.status.code().unwrap_or(-1)));
        // git lists what's in the way, indented.
        let files: Vec<&str> = lines
            .iter()
            .filter(|l| l.starts_with('\t'))
            .map(|l| l.trim())
            .collect();
        if !files.is_empty() {
            why.push(' ');
            why.push_str(&files[..files.len().min(5)].join(", "));
            if files.len() > 5 {
                why.push_str(&format!(" and {} more", files.len() - 5));
            }
        }
        return format!("didn't move the user's {branch}{label}: {why}");
    }
    format!(
        "moved the user's {branch}{label} to {}, files and all",
        &commit[..12.min(commit.len())]
    )
}
