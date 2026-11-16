//! A thread's notes: a repo its agent keeps, and you read in Obsidian.
//!
//! Every thread has one, at <thread>/notes in its folio directory (store.rs), made the first time
//! it's wanted: checked out in the thread's bottle at /notes, where the agent commits to main and
//! pushes it to its `agent` remote, the notes repo's own lane. Your copy's main tracks that lane
//! (remote `bottle`), so bottle's follow keeps the notes you open in Obsidian current.
//!
//! They're the thread's for good: they move with a rename, since they're in the thread's directory;
//! a delete leaves them; and a new thread of the same name finds them. The lane's namespaces are
//! named once for all, notes-out and notes-in, since the repo is one thread's alone.

use std::fs;
use std::path::{Path, PathBuf};

use crate::git::git;
use crate::lanes;

/// Where a thread's bottle has them.
pub const MOUNT: &str = "/notes";
const REMOTE: &str = "bottle";
pub const OUT: &str = "notes-out";
pub const IN: &str = "notes-in";
/// What Obsidian writes for whoever has the vault open: theirs, not the notes.
const GITIGNORE: &str = ".obsidian/\n.trash/\n";

/// The notes mount's remotes in the bottle: your main, its lane, and yours.
pub fn remotes() -> Vec<String> {
    vec![
        "host:fetch=heads".into(),
        format!("agent:fetch=ns/{OUT},push=ns/{OUT}"),
        format!("user:fetch=ns/{IN}"),
    ]
}

/// The notes repo at `path`, made with its first commit if it isn't there; its lane and tracking
/// set.
pub fn ensure(path: &Path, first: &str) -> Result<PathBuf, String> {
    let failed = |e: std::io::Error| format!("{}: {e}", path.display());
    if !path.join(".git").exists() {
        fs::create_dir_all(path).map_err(failed)?;
        git(&["init", "-q", "-b", "main", &path.to_string_lossy()], None)?;
        fs::write(path.join(".gitignore"), GITIGNORE).map_err(failed)?;
        // Obsidian's settings for it, so Obsidian takes it for a vault it knows, not a new one (see
        // obsidian.rs).
        fs::create_dir_all(path.join(".obsidian")).map_err(failed)?;
        git(&["add", ".gitignore"], Some(path))?;
        git(
            &[
                "-c",
                "user.name=folio",
                "-c",
                "user.email=folio@localhost",
                "commit",
                "-q",
                "-m",
                first,
            ],
            Some(path),
        )?;
    }
    lanes::register_remote(path, REMOTE, OUT, IN)?;
    // Config alone: the tracking branch needn't exist yet for a push to find the branch that tracks
    // it.
    git(&["config", "branch.main.remote", REMOTE], Some(path))?;
    git(
        &["config", "branch.main.merge", "refs/heads/main"],
        Some(path),
    )?;
    Ok(path.to_path_buf())
}
