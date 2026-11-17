//! Threads: one line of work in one repo, and the bottle it's being worked in.
//!
//! A thread has a kebab-case name, unique within its repo; its bottle is named `<repo
//! key>-<thread>`, unique across repos. Its record is in its slot (see store.rs), and its state is
//! read live from bottle, never trusted from the record:
//!
//! ```text
//! running   the bottle is up: attach to it
//! stopped   the VM stopped, its disk intact (a crash, a restart, `bottle stop`): start it, and the
//!           work is all there
//! paused    no bottle, and a snapshot of it (see snapshots.rs): attaching resumes it in a new
//!           bottle, made from the repo's spec as it is now
//! missing   no bottle, and no snapshot: only what's in the lane is left. Folio never leaves a
//!           thread so (a snapshot exists exactly while there's no bottle); it's a bottle deleted
//!           from under it.
//! ```
//!
//! A thread has a snapshot only while it has no bottle: attaching to one drops it, since the bottle
//! is where the work is from then on.
//!
//! A thread can have a sidecar (`folio sidecar configure`): a bottle of a spec of its own, on a
//! runner of its own, `<thread's bottle>-sidecar`, with the thread's repos, fetch-only, which the
//! thread's bottle reaches as `ssh sidecar`. It's scratch: made just before the thread's bottle is,
//! started with it, and deleted with it, a pause included. So a sidecar given to a thread with a
//! bottle comes with the next one: pause and resume it.
//!
//! Inside the bottle, Claude and a shell each run in a shpool session of their own, started by one
//! exec and attached to by another, so closing a terminal only detaches it, and whatever the session
//! started keeps running. shpool passes the session's output straight through, so it's the
//! terminal's own scrollback that has it.

use pause::SNAPSHOT;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::repos::{Repo, default_spec};
use crate::specs::Spec;
use crate::store::{self, Content};
use crate::{notes, repos, snapshots};

mod lifecycle;
mod pause;
mod sessions;
mod sidecar;
mod state;

pub use lifecycle::{create, delete, rename, set_settings};
pub use pause::{losses, pause};
pub use sessions::{attach, copy, exec};
pub use sidecar::{set_sidecar, sidecar_shell};
pub use state::{last_active, recent, states};

pub const RECORD: &str = "thread.json";
pub const WORKSPACE: &str = "/workspace";
pub const CLAUDE: &str = "claude";
pub const SHELL: &str = "shell";
/// What a session's programs are told their terminal is. Not the one attached, whose TERM the
/// bottle may have no terminfo for, and which a session outlives.
const TERM: &str = "xterm-256color";
/// What a thread has that the repo's settings don't, or a sidecar has that a new one doesn't (its
/// base): features it adds or changes the options of (by name: one can't be taken away), and its
/// own memory, disk and runner. What it doesn't say, it has as its base has it, as that is when a
/// bottle's made.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner: Option<String>,
}

impl Settings {
    /// `base`, a spec, with these over it.
    pub fn over(&self, base: &Spec) -> Spec {
        let mut spec = base.clone();
        spec.features = repos::merge_features(&base.features, &self.features);
        if let Some(memory) = &self.memory {
            spec.memory = memory.clone();
        }
        if let Some(disk) = &self.disk {
            spec.disk = Some(disk.clone());
        }
        spec
    }

    /// The spec a thread's bottle is made from: the repo's, with these over it.
    pub fn spec(&self, repo: &Repo) -> Result<Spec, String> {
        Ok(self.over(&repo.spec()?.unwrap_or_else(default_spec)))
    }

    /// Where a thread's bottle is made: its own runner, else the repo's; None: this machine.
    pub fn runner(&self, repo: &Repo) -> Result<Option<String>, String> {
        match &self.runner {
            Some(runner) => Ok(Some(runner.clone())),
            None => repo.runner(),
        }
    }

    /// Whether it says anything of a spec: else its spec is its base's.
    pub fn changes_spec(&self) -> bool {
        !self.features.is_empty() || self.memory.is_some() || self.disk.is_some()
    }
}

/// What a sidecar has when it's given nothing: it runs no sessions, so no features.
pub fn sidecar_base() -> Spec {
    Spec {
        memory: repos::DEFAULT_MEMORY.into(),
        ..Spec::default()
    }
}

/// A thread's configuration: written only by what changes it (new, configure, rename, a runner
/// given), never by using it. What its bottle is, and when it was last used, is the bottle's
/// record's (bottles.rs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Thread {
    pub name: String,
    pub bottle: String,
    pub created: f64,
    /// The branch it started from, if one was asked for; None: the repo's default.
    pub base: Option<String>,
    /// The broker (see broker.rs), whose bottle is made folio's way, not the repo's.
    pub broker: bool,
    /// What it has that the repo's settings don't, as a sidecar's are kept.
    pub settings: Settings,
    /// Its sidecar, if it has one: a runner of its own, always.
    pub sidecar: Option<Settings>,
}

impl Thread {
    pub fn sidecar_bottle(&self) -> String {
        format!("{}-sidecar", self.bottle)
    }
}

/// A thread's name: lowercase letters and digits, in words joined by '-'.
pub fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|w| {
            !w.is_empty()
                && w.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

fn directory(repo: &Repo, name: &str) -> PathBuf {
    repo.directory().join(name)
}

pub fn exists(repo: &Repo, name: &str) -> bool {
    is_name(name) && store::current_slot(&directory(repo, name)).is_some()
}

fn read(slot: &Path) -> Result<Thread, String> {
    let path = slot.join(RECORD);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub fn load(repo: &Repo, name: &str) -> Result<Thread, String> {
    let slot = if is_name(name) {
        store::current_slot(&directory(repo, name))
    } else {
        None
    };
    let Some(slot) = slot else {
        let known: Vec<String> = all(repo)?.into_iter().map(|t| t.name).collect();
        return Err(format!(
            "no thread named {} in {} (threads: {})",
            crate::quoted(name),
            repo.path.display(),
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        ));
    };
    read(&slot)
}

/// The repo's threads, by name.
pub fn all(repo: &Repo) -> Result<Vec<Thread>, String> {
    let mut threads = Vec::new();
    for entry in fs::read_dir(repo.directory())
        .into_iter()
        .flatten()
        .flatten()
    {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if let Some(slot) = store::current_slot(&entry.path())
            && slot.join(RECORD).exists()
        {
            threads.push(read(&slot)?);
        }
    }
    threads.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(threads)
}

fn mtime(path: &Path) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn save(repo: &Repo, thread: &Thread, files: Vec<(&str, Content)>) -> Result<(), String> {
    let record = store::json(thread).into_bytes();
    let mut all = vec![(RECORD, Content::Bytes(record))];
    all.extend(files);
    store::write_slot(&directory(repo, &thread.name), all).map(|_| ())
}

/// The bytes a paused thread's snapshot takes up; None if it has none.
pub fn snapshot_size(repo: &Repo, thread: &Thread) -> Option<u64> {
    let slot = snapshot(repo, thread)?;
    Some(
        SNAPSHOT
            .iter()
            .filter_map(|name| fs::metadata(slot.join(name)).ok())
            .map(|m| m.len())
            .sum(),
    )
}

/// The slot holding the thread's snapshot, if it has one: every snapshot has Claude's archive.
fn snapshot(repo: &Repo, thread: &Thread) -> Option<PathBuf> {
    store::current_slot(&directory(repo, &thread.name))
        .filter(|slot| slot.join(snapshots::CLAUDE_ARCHIVE).exists())
}

/// Where the thread's notes are kept: in its directory, so a rename takes them along.
pub fn notes_dir(repo: &Repo, name: &str) -> PathBuf {
    directory(repo, name).join("notes")
}

/// The paths of the thread's bottle's mounts: its repo's checkout, and its notes; the broker, its
/// notes.
fn mounts(thread: &Thread) -> Vec<&'static str> {
    if thread.broker {
        vec![notes::MOUNT]
    } else {
        vec![WORKSPACE, notes::MOUNT]
    }
}

/// Where the thread's next bottle is made: its own runner, else the repo's; None for bottle's
/// default. The broker's is always here.
pub fn runner_of(repo: &Repo, thread: &Thread) -> Result<Option<String>, String> {
    if thread.broker {
        return Ok(None);
    }
    thread.settings.runner(repo)
}

#[cfg(test)]
mod tests {
    use super::sessions::session_command;
    use super::*;
    use serde_json::json;

    #[test]
    fn names_are_kebab_case() {
        assert!(is_name("fix-it") && is_name("a1") && is_name("x-2-y"));
        assert!(
            !is_name("")
                && !is_name("Fix")
                && !is_name("-x")
                && !is_name("x-")
                && !is_name("a--b")
                && !is_name("a_b")
        );
    }

    #[test]
    fn a_session_runs_as_shpool_takes_it() {
        assert_eq!(session_command(SHELL), "sh -c 'exec bash -l'");
        assert!(session_command(CLAUDE).starts_with("sh -c 'set --; if ls"));
    }

    #[test]
    fn a_record_reads_back() {
        let thread = Thread {
            name: "t".into(),
            bottle: "r-t".into(),
            created: 1.5,
            base: None,
            broker: false,
            settings: Settings {
                memory: Some("8G".into()),
                features: vec!["jvm:version=21".into()],
                ..Settings::default()
            },
            sidecar: Some(Settings {
                runner: Some("box".into()),
                memory: Some("32G".into()),
                ..Settings::default()
            }),
        };
        assert_eq!(
            serde_json::from_str::<Thread>(&store::json(&thread)).unwrap(),
            thread
        );
    }

    #[test]
    fn settings_go_over_their_base_feature_by_feature() {
        let base = Spec::of(
            json!({"features": ["tools", "jvm:version=17"], "memory": "2G", "disk": "50G"}),
        );
        let settings = Settings {
            features: vec!["jvm:version=21".into(), "python".into()],
            memory: Some("16G".into()),
            ..Settings::default()
        };
        assert_eq!(
            settings.over(&base),
            Spec::of(
                json!({"features": ["tools", "jvm:version=21", "python"], "memory": "16G", "disk": "50G"})
            )
        );
        assert_eq!(Settings::default().over(&base), base);
    }
}
