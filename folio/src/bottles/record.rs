//! A bottle's record: what a bottle is (Bottle, its mounts and grants), kept as JSON in its creator's
//! directory, and found again by name.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};

use crate::control::Remote;
use crate::services::GIT_HOST;
use crate::specs::Spec;
use crate::store;

/// How far a bottle's making got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Made {
    /// Its setup hasn't finished.
    Creating,
    Ready,
    /// Deleting it left parts behind.
    Broken,
}

impl std::fmt::Display for Made {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Made::Creating => "creating",
            Made::Ready => "ready",
            Made::Broken => "broken",
        })
    }
}

/// A mount at /x has its objects at /mnt/objects/x.
pub(super) const OBJECTS_ROOT: &str = "/mnt/objects";
pub(super) const HOME: &str = "/home/genie";
/// Where a thread's directory has its bottles' records.
pub(super) const RECORDS: &str = "bottles";
pub(super) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// A remote, as a record keeps it: {name, fetch, push}.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct RemoteRecord {
    pub(super) name: String,
    pub(super) fetch: String,
    #[serde(default)]
    pub(super) push: Option<String>,
}

pub(super) fn remotes_from<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Remote>, D::Error> {
    let records: Vec<RemoteRecord> = Vec::deserialize(d)?;
    Ok(records
        .into_iter()
        .map(|r| Remote {
            name: r.name,
            fetch: r.fetch,
            push: r.push,
        })
        .collect())
}

pub(super) fn remotes_to<S: serde::Serializer>(
    remotes: &[Remote],
    s: S,
) -> Result<S::Ok, S::Error> {
    let records: Vec<RemoteRecord> = remotes
        .iter()
        .map(|r| RemoteRecord {
            name: r.name.clone(),
            fetch: r.fetch.clone(),
            push: r.push.clone(),
        })
        .collect();
    records.serialize(s)
}

/// A repo in the bottle: its objects mounted read-only, checked out at `path` borrowing them, and
/// served the remotes it was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mount {
    /// Where it's checked out, absolute.
    pub path: String,
    /// Its toplevel on the controller: the hub, whose objects are mounted.
    pub repo: String,
    /// None: a detached commit.
    pub branch: Option<String>,
    pub commit: String,
    #[serde(deserialize_with = "remotes_from", serialize_with = "remotes_to")]
    pub remotes: Vec<Remote>,
    /// Where its objects are mounted in the bottle: under OBJECTS_ROOT, at its path.
    pub objects: String,
    /// Where its remotes are under git.bottle.internal: its path, less the /.
    pub at: String,
}

impl Mount {
    pub(super) fn new(
        path: &str,
        repo: &str,
        branch: Option<String>,
        commit: String,
        remotes: Vec<Remote>,
    ) -> Mount {
        Mount {
            path: path.into(),
            repo: repo.into(),
            branch,
            commit,
            remotes,
            objects: format!("{OBJECTS_ROOT}{path}"),
            at: path.trim_start_matches('/').into(),
        }
    }

    /// Where its remote `remote` is: http://git.bottle.internal/<at>/<remote>.
    pub fn url(&self, remote: &str) -> String {
        match self.at.as_str() {
            "" => format!("http://{GIT_HOST}/{remote}"),
            at => format!("http://{GIT_HOST}/{at}/{remote}"),
        }
    }
}

/// A bottle this one may ssh to: as `name`, at `address`, which is bottle `bottle` with id `id` and
/// no other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SshGrant {
    pub name: String,
    pub bottle: String,
    pub id: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bottle {
    pub name: String,
    pub id: String,
    pub image: String,
    pub created: f64,
    /// Creating until setup finishes, Ready after; Broken if cleanup failed.
    pub status: Made,
    /// Features installed on top of the image, as specs (e.g. jvm:version=17), including
    /// dependencies.
    pub features: Vec<String>,
    /// The spec it was made from, canonical: its memory, say.
    pub spec: Spec,
    /// Its repos, each checked out at its path; the first is where a shell starts.
    pub mounts: Vec<Mount>,
    /// Its creator's addition to ~/BOTTLE.md.
    pub context: String,
    /// The runner its machine is on.
    pub runner: String,
    /// The bottles it may ssh to.
    pub ssh: Vec<SshGrant>,
    /// Which of network()'s policies its machine has (NETWORK the current one). One behind is
    /// given the current one as it's next started.
    pub network_version: u32,
    /// When it was last used (touch): made, or attached to.
    pub last_active: f64,
}

impl Bottle {
    /// The image and its features, for display: base+claude+jvm.
    pub(super) fn environment(&self) -> String {
        let mut features = self.features.clone();
        features.sort();
        std::iter::once(self.image.clone())
            .chain(features)
            .collect::<Vec<_>>()
            .join("+")
    }

    /// Where a shell or a command starts: the first mount, else genie's home.
    pub fn workdir(&self) -> &str {
        self.mounts.first().map_or(HOME, |m| m.path.as_str())
    }

    /// Its machine's name, as bottle-cli there lists it: the bottle's own.
    pub fn machine(&self) -> &str {
        &self.name
    }

    /// Its memory, as its spec says.
    pub(super) fn memory(&self) -> Option<&str> {
        Some(&self.spec.memory)
    }

    pub(super) fn permissions(&self) -> Vec<String> {
        self.spec.permissions.clone()
    }
}

/// Each bottle's record, by name: in its thread's directory, <repo key>/<thread>/bottles/.
pub(super) fn records() -> BTreeMap<String, PathBuf> {
    let mut paths = Vec::new();
    for repo in fs::read_dir(crate::home()).into_iter().flatten().flatten() {
        for thread in fs::read_dir(repo.path()).into_iter().flatten().flatten() {
            for record in fs::read_dir(thread.path().join(RECORDS))
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = record.path();
                if path.extension().is_some_and(|e| e == "json") {
                    paths.push(path);
                }
            }
        }
    }
    paths.sort();
    paths
        .into_iter()
        .map(|p| (p.file_stem().unwrap().to_string_lossy().into_owned(), p))
        .collect()
}

pub fn load() -> Result<BTreeMap<String, Bottle>, String> {
    let mut bottles = BTreeMap::new();
    for (name, path) in records() {
        if let Some(entry) = store::read_json(&path)? {
            let bottle: Bottle =
                serde_json::from_value(entry).map_err(|e| format!("{}: {e}", path.display()))?;
            bottles.insert(name, bottle);
        }
    }
    Ok(bottles)
}

pub fn get(name: &str) -> Result<Bottle, String> {
    let mut bottles = load()?;
    bottles.remove(name).ok_or_else(|| {
        let known: Vec<&str> = bottles.keys().map(String::as_str).collect();
        format!(
            "no bottle named {} (bottles: {})",
            crate::quoted(name),
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        )
    })
}

/// Write the bottle's record where it is, else in thread directory `owner`'s.
pub(super) fn save(bottle: &Bottle, owner: Option<&Path>) -> Result<(), String> {
    let path = match records().remove(&bottle.name) {
        Some(path) => path,
        None => match owner {
            Some(owner) => owner.join(RECORDS).join(format!("{}.json", bottle.name)),
            None => {
                return Err(format!(
                    "bottle {} has no record, and no thread to keep one",
                    bottle.name
                ));
            }
        },
    };
    store::write_json(&path, bottle)
}

pub(super) fn forget(name: &str) {
    if let Some(path) = records().remove(name) {
        let _ = fs::remove_file(&path);
        // A thread's bottles/, with none left.
        let _ = fs::remove_dir(path.parent().unwrap());
    }
}

/// Record that the bottle is being used now (its last_active).
pub fn touch(name: &str) -> Result<(), String> {
    let mut bottle = get(name)?;
    bottle.last_active = now();
    save(&bottle, None)
}
