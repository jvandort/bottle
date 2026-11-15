//! Running git, as folio does: in a repo, its stdout stripped, its failure saying what git said.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// git `args` (in `repo`, if given), and its stripped stdout; its failure as folio says it: `git
/// ARGS: what git said`.
pub fn git(args: &[&str], repo: Option<&Path>) -> Result<String, String> {
    run(args, repo, None, true)
}

/// git `args` in `repo`, given `input` on stdin.
pub fn git_input(args: &[&str], repo: &Path, input: &str) -> Result<String, String> {
    run(args, Some(repo), Some(input), true)
}

/// Whether git `args` (in `repo`) succeeds; what it says is dropped.
pub fn succeeds(args: &[&str], repo: Option<&Path>) -> bool {
    command(args, repo)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// git `args` in `repo`, its stripped stdout; its failure as bottle's modules said it: what git
/// said, alone.
pub fn bottle_git(args: &[&str], repo: Option<&Path>) -> Result<String, String> {
    let output = command(args, repo)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if said.is_empty() {
            let mut line = vec!["git".to_string()];
            if let Some(repo) = repo {
                line.extend(["-C".to_string(), repo.display().to_string()]);
            }
            line.extend(args.iter().map(|a| a.to_string()));
            format!("{} failed", line.join(" "))
        } else {
            said
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn command(args: &[&str], repo: Option<&Path>) -> Command {
    let mut git = Command::new("git");
    if let Some(repo) = repo {
        git.arg("-C").arg(repo);
    }
    git.args(args);
    git
}

fn run(args: &[&str], repo: Option<&Path>, input: Option<&str>, check: bool) -> Result<String, String> {
    let mut child = command(args, repo)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if let Some(input) = input {
        // What git doesn't read is its to refuse: a failure here shows in its status.
        let _ = child.stdin.take().unwrap().write_all(input.as_bytes());
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("git {}: {e}", args.join(" ")))?;
    if check && !output.status.success() {
        let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let why = if said.is_empty() {
            format!("exit {}", output.status.code().unwrap_or(-1))
        } else {
            said
        };
        return Err(format!("git {}: {why}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
