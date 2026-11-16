//! Snapshots: what a paused thread keeps of its bottle, in an archive per mount and one of Claude's.
//!
//! Each mount is archived the same way: `workspace.tar.gz` is /workspace, and `notes.tar.gz` /notes
//! (a snapshot from before notes has none, and a resume then checks the notes out afresh). An
//! archive holds the checkout's `.git` (config, HEAD, the index, every ref, the stash, the reflog,
//! and the objects the bottle made itself -- the ones it borrows from the repo through alternates
//! aren't copied) and every tracked and untracked file that isn't ignored. Ignored files (build
//! outputs, node_modules) are left out: they can be huge, and rebuilding them is cheaper than
//! carrying them. The new bottle mounts the same repo at the same path, so the alternates file still
//! points at the objects it borrows.
//!
//! `claude.tar.gz` is ~/.claude, less what's only cache: the conversation, its subagents and
//! memory, /rewind's file history, the prompt history, and settings.json, with the choices made in
//! the thread (`/model` and the like). settings.json comes back whole, over the one the claude
//! feature wrote into the new image, so a change to that feature's options since reaches a thread
//! only when it's started afresh; the feature rarely changes, and threads don't live long. Leaving
//! out by name means a new kind of Claude Code state is kept without anyone having to notice it.
//!
//! Restoring them gets back the exact checkouts and conversation; a pause loses only running
//! processes. A restored checkout is then pointed at the new bottle's objects and remotes
//! (repoint), which bottle says where they are: a snapshot from an older bottle had them elsewhere.
//! Archives travel over `bottle exec`, never a mount. The scripts are POSIX sh, and `tar` either
//! GNU's or BSD's.
//!
//! An archive is the agent's, made in a bottle it controls, so on this machine it's only ever
//! copied, and read through to check it's whole: never unpacked, and never run git in. A repo's own
//! config can make git run commands (`core.fsmonitor`, a filter driver), so git run on the agent's
//! `.git` here would be running the agent's commands outside the sandbox. What folio needs to know
//! about a paused thread's workspace -- what in it was never pushed -- it records in the bottle at
//! pause, as `work.txt`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use crate::shell::quote as q;

pub const WORKSPACE_ARCHIVE: &str = "workspace.tar.gz";
pub const NOTES_ARCHIVE: &str = "notes.tar.gz";
pub const CLAUDE_ARCHIVE: &str = "claude.tar.gz";
/// The workspace's branches and whether it had changes, as the bottle saw them at pause.
pub const WORK: &str = "work.txt";

/// ~/.claude's files that are only cache, and CLAUDE.md: a link to the ~/BOTTLE.md that every
/// bottle writes afresh.
const CLAUDE_LEFT_OUT: [&str; 7] = [
    "CLAUDE.md",
    "plugins",
    "cache",
    "telemetry",
    "policy-limits*",
    "remote-settings.json",
    "backups",
];

/// Writes `workspace`'s archive to stdout. Tracked files since deleted are skipped.
pub fn workspace_out(workspace: &str) -> String {
    format!(
        "cd {workspace} && {{ printf '.git\\0'; git ls-files -z -co --exclude-standard | \
         xargs -0 sh -c 'for f do if [ -e \"$f\" ] || [ -L \"$f\" ]; then printf \"%s\\0\" \"$f\"; fi; done' sh; }} \
         | tar -czf - --null -T -"
    )
}

/// Replaces `workspace` with the archive on stdin.
pub fn workspace_in(workspace: &str) -> String {
    format!("cd {workspace} && find . -mindepth 1 -maxdepth 1 -exec rm -rf {{}} + && tar -xzf -")
}

/// Points the checkout at `path` at its bottle's objects and remotes' URLs, as bottle's status
/// gives them.
pub fn repoint(path: &str, objects: &str, urls: &BTreeMap<String, String>) -> String {
    let mut lines = vec![
        "set -e".to_string(),
        format!(
            "echo {} > {}/.git/objects/info/alternates",
            q(objects),
            q(path)
        ),
    ];
    for (name, url) in urls {
        lines.push(format!(
            "git -C {p} remote set-url {n} {u} 2>/dev/null || git -C {p} remote add {n} {u}",
            p = q(path),
            n = q(name),
            u = q(url)
        ));
    }
    lines.join("\n")
}

pub fn claude_out() -> String {
    let excludes: Vec<String> = CLAUDE_LEFT_OUT
        .iter()
        .map(|n| format!("--exclude='./{n}'"))
        .collect();
    format!(
        "mkdir -p \"$HOME/.claude\" && cd \"$HOME/.claude\" && tar -czf - {} .",
        excludes.join(" ")
    )
}

pub const CLAUDE_IN: &str = "mkdir -p \"$HOME/.claude\" && cd \"$HOME/.claude\" && tar -xzf -";

/// Fail unless `path` is a whole gzipped tar: read to the end, every member.
pub fn check(path: &Path) -> Result<(), String> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    read_through(path)
        .map_err(|e| format!("{name} came back incomplete ({e}); nothing was changed"))
}

fn read_through(path: &Path) -> std::io::Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(File::open(path)?));
    let mut sink = [0u8; 64 * 1024];
    let mut members = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        while entry.read(&mut sink)? > 0 {}
        members += 1;
    }
    // An empty or cut-short gzip stream reads as a tar with nothing in it: it's taken as broken.
    if members == 0 {
        return Err(std::io::Error::other("empty file"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn the_scripts_are_as_they_were() {
        assert_eq!(
            workspace_out("/workspace"),
            "cd /workspace && { printf '.git\\0'; git ls-files -z -co --exclude-standard | xargs -0 sh -c 'for f do if [ -e \"$f\" ] || [ -L \"$f\" ]; then printf \"%s\\0\" \"$f\"; fi; done' sh; } | tar -czf - --null -T -"
        );
        assert_eq!(
            claude_out(),
            "mkdir -p \"$HOME/.claude\" && cd \"$HOME/.claude\" && tar -czf - --exclude='./CLAUDE.md' --exclude='./plugins' --exclude='./cache' --exclude='./telemetry' --exclude='./policy-limits*' --exclude='./remote-settings.json' --exclude='./backups' ."
        );
        let urls = BTreeMap::from([(
            "origin".to_string(),
            "http://bottle.host/git/workspace/origin".to_string(),
        )]);
        assert_eq!(
            repoint("/workspace", "/mnt/objects/workspace", &urls),
            "set -e\necho /mnt/objects/workspace > /workspace/.git/objects/info/alternates\ngit -C /workspace remote set-url origin http://bottle.host/git/workspace/origin 2>/dev/null || git -C /workspace remote add origin http://bottle.host/git/workspace/origin"
        );
    }

    #[test]
    fn a_whole_archive_passes_and_a_cut_one_doesnt() {
        let tmp = crate::tempdir::Dir::new();
        let whole = tmp.path().join("claude.tar.gz");
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            File::create(&whole).unwrap(),
            Default::default(),
        ));
        let data = vec![7u8; 100_000];
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_cksum();
        builder.append_data(&mut header, "big", &data[..]).unwrap();
        builder.into_inner().unwrap().finish().unwrap();
        check(&whole).unwrap();
        let bytes = std::fs::read(&whole).unwrap();
        let cut = tmp.path().join("cut.tar.gz");
        File::create(&cut)
            .unwrap()
            .write_all(&bytes[..bytes.len() / 2])
            .unwrap();
        assert!(
            check(&cut)
                .unwrap_err()
                .starts_with("cut.tar.gz came back incomplete (")
        );
        let empty = tmp.path().join("claude.tar.gz.empty");
        File::create(&empty).unwrap();
        assert!(check(&empty).is_err());
    }
}
