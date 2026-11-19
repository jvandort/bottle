//! A thread's record: its configuration (Thread, Settings), kept in its slot (see store.rs) with
//! its snapshot while it has one, and found again by name.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::repos::{self, Repo, default_spec};
use crate::snapshots;
use crate::specs::Spec;
use crate::store::{self, Content};

const RECORD: &str = "thread.json";

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

pub(super) fn directory(repo: &Repo, name: &str) -> PathBuf {
    repo.directory().join(name)
}

pub fn exists(repo: &Repo, name: &str) -> bool {
    is_name(name) && store::current_slot(&directory(repo, name)).is_some()
}

pub(super) fn read(slot: &Path) -> Result<Thread, String> {
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

pub(super) fn save(
    repo: &Repo,
    thread: &Thread,
    files: Vec<(&str, Content)>,
) -> Result<(), String> {
    let record = store::json(thread).into_bytes();
    let mut all = vec![(RECORD, Content::Bytes(record))];
    all.extend(files);
    store::write_slot(&directory(repo, &thread.name), all).map(|_| ())
}

/// What a thread's snapshot is, in its slot: each mount's archive, Claude's, and the work found
/// in it as it was paused.
pub(super) const SNAPSHOT: [&str; 4] = [
    snapshots::WORKSPACE_ARCHIVE,
    snapshots::NOTES_ARCHIVE,
    snapshots::CLAUDE_ARCHIVE,
    snapshots::WORK,
];

/// The bytes the thread's snapshot takes up; None if it has none.
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

/// When the thread's snapshot was taken (its Claude archive written, the last of it); None if it
/// has none.
pub fn snapshot_taken(repo: &Repo, thread: &Thread) -> Option<f64> {
    let written = fs::metadata(snapshot(repo, thread)?.join(snapshots::CLAUDE_ARCHIVE))
        .and_then(|m| m.modified())
        .ok()?;
    Some(
        written
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64(),
    )
}

/// The slot holding the thread's snapshot, if it has one: every snapshot has Claude's archive.
pub(super) fn snapshot(repo: &Repo, thread: &Thread) -> Option<PathBuf> {
    store::current_slot(&directory(repo, &thread.name))
        .filter(|slot| slot.join(snapshots::CLAUDE_ARCHIVE).exists())
}

/// Where the thread's notes are kept: in its directory, so a rename takes them along.
pub fn notes_dir(repo: &Repo, name: &str) -> PathBuf {
    directory(repo, name).join("notes")
}

#[cfg(test)]
mod tests {
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
