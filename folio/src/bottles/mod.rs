//! Bottles: a sandbox, in its own VM, made from a spec and served the git remotes it's given.
//!
//! A bottle's record is written before anything is created, so a failure at any point leaves a
//! record of what may exist. It's kept with the thread it's for, in its directory in folio's home:
//! <repo key>/<thread>/bottles/<name>.json. Creation rolls back on failure; delete() removes
//! whatever parts exist, so it also cleans up a bottle that was left half-made.
//!
//! Each bottle has:
//!   - a machine, <name> (see bottle_cli.rs), with each of its mounts' repos' objects mounted
//!     read-only, and the network policy it decides its connections by (network), which routes
//!     some to folio serve: its services (git: each mount's git remotes, at git.bottle.internal;
//!     messages; the bottles running), ssh to bottles, and its credentials' hosts (see
//!     services.rs; serve/)
//!   - for each mount, a checkout at its path that borrows those objects via alternates. Which paths
//!     is the bottle's creator's to say; none is special, but the first is where a shell or a
//!     command starts (genie's home without one).
//!
//! What it installs, and its memory, come from a spec its creator owns (see specs.rs). Its remotes
//! are the ones it was created with, and nothing is registered in the repo: what the bottle pushes
//! is for its creator to read. What's worth keeping before a bottle is deleted is its creator's call
//! too.
//!
//! A bottle may ssh to other bottles its creator names (ssh: {NAME: BOTTLE}), and to nothing else
//! that way: NAME, in its /etc/hosts, is an address whose port 22 folio serve joins to sshd in that
//! bottle, the one that existed when the grant was made. Its key is let in there and that bottle's
//! host key is known here, when it's made: so that bottle must exist first, and be made again with
//! it, or the grant reaches nothing. The two may push into the same namespaces, which no other two
//! bottles may: they work together.
//!
//! Objects the bottle uses from the repo aren't protected from `git gc` in the hub: if the user
//! deletes a branch a bottle checked out and gc later prunes its commits (after git's grace
//! periods, weeks to months), that checkout breaks, and the bottle has to be made again.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};

use crate::control::Remote;
use crate::runners::{self, Runner};
use crate::services::GIT_HOST;
use crate::specs::Spec;
use crate::store;

mod bind;
mod context;
mod create;
mod machine;
mod state;

pub use bind::bindings;
pub use create::{MountRequest, Order, create};
pub use machine::{delete, ensure_running, exec_command, start};
pub use state::{Known, Made, State, Status, status};

/// A mount at /x has its objects at /mnt/objects/x.
const OBJECTS_ROOT: &str = "/mnt/objects";
pub const USER: &str = "genie";
const HOME: &str = "/home/genie";
/// In genie's home.
const CONTEXT_FILE: &str = "BOTTLE.md";
/// The remote a checkout's branches default to (git's checkout.defaultRemote), if it has one.
const ORIGIN: &str = "origin";
/// Where a thread's directory has its bottles' records.
const RECORDS: &str = "bottles";
/// The version of the policy network() makes: 1, its routes to folio serve's one socket; 2, folio
/// serve's, by service; 3, on a runner, to folio serve's forwarded there (services_sockets), not
/// bottle-cli's; 4, a socket for each service, git, messages and the bottles routed by their names
/// under bottle.internal (bottle.host's paths before). A bottle given 4 has its names and remotes
/// pointed there again (machine.rs's ensure_running).
const NETWORK: u32 = 4;
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// A remote, as a record keeps it: {name, fetch, push}.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct RemoteRecord {
    name: String,
    fetch: String,
    #[serde(default)]
    push: Option<String>,
}

fn remotes_from<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Remote>, D::Error> {
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

fn remotes_to<S: serde::Serializer>(remotes: &[Remote], s: S) -> Result<S::Ok, S::Error> {
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
    fn new(
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
    fn environment(&self) -> String {
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
    fn memory(&self) -> Option<&str> {
        Some(&self.spec.memory)
    }

    fn permissions(&self) -> Vec<String> {
        self.spec.permissions.clone()
    }
}

/// Each bottle's record, by name: in its thread's directory, <repo key>/<thread>/bottles/.
fn records() -> BTreeMap<String, PathBuf> {
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

pub fn runner_of(bottle: &Bottle) -> Result<Runner, String> {
    runners::get(&bottle.runner)
}

/// Write the bottle's record where it is, else in thread directory `owner`'s.
fn save(bottle: &Bottle, owner: Option<&Path>) -> Result<(), String> {
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

fn forget(name: &str) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::quote as q;
    use crate::specs;

    use super::context::context;
    use super::context::machine_env;
    use super::machine::{CONTRACT, memory_mib};
    use crate::tempdir;
    use serde_json::{Value, json};

    #[test]
    fn the_contract_checks_run_and_name_what_failed() {
        // Run the real checks here: the script itself must be well-formed. Which fail depends on the
        // machine, so only require that whatever it reports is contract names and nothing else.
        let checks: Vec<String> = CONTRACT
            .iter()
            .map(|(what, test)| format!("sh -c {} >/dev/null 2>&1 || echo {}", q(test), q(what)))
            .collect();
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("# bottle contract\n{}", checks.join("\n"))])
            .output()
            .unwrap();
        assert!(out.status.success());
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            assert!(CONTRACT.iter().any(|(what, _)| *what == line), "{line}");
        }
    }

    #[test]
    fn a_bottle_gets_stand_ins_and_its_permissions_and_where_a_login_starts() {
        let _env = tempdir::isolated();
        let env = machine_env(&["claude".into()], &[], "/workspace").unwrap();
        assert_eq!(env["CLAUDE_CODE_OAUTH_TOKEN"], crate::auth::STANDIN);
        assert_eq!(
            env["BOTTLE_MIRROR_ENV"],
            "BOTTLE_WORKDIR CLAUDE_CODE_OAUTH_TOKEN"
        );
        let env = machine_env(&["tools".into()], &[], "/w").unwrap();
        assert_eq!(
            env,
            BTreeMap::from([
                ("BOTTLE_WORKDIR".into(), "/w".into()),
                ("BOTTLE_MIRROR_ENV".into(), "BOTTLE_WORKDIR".into())
            ])
        );
        let env = machine_env(
            &["tools".into()],
            &["list-bottles".into(), "message-any".into()],
            "/w",
        )
        .unwrap();
        assert_eq!(env["BOTTLE_PERMISSIONS"], "list-bottles,message-any");
        assert_eq!(
            env["BOTTLE_MIRROR_ENV"],
            "BOTTLE_PERMISSIONS BOTTLE_WORKDIR"
        );
    }

    #[test]
    fn its_context_says_what_its_features_and_permissions_give_it() {
        let _env = tempdir::isolated();
        let bottle = |features: &[&str], spec: Value| Bottle {
            name: "b".into(),
            id: "id".into(),
            image: "base".into(),
            created: 0.0,
            status: Made::Ready,
            features: features.iter().map(|f| f.to_string()).collect(),
            spec: Spec::of(
                json!({"memory": "2G", "permissions": spec["permissions"].clone()
                .as_array().cloned().unwrap_or_default()}),
            ),
            mounts: vec![],
            context: String::new(),
            runner: "local".into(),
            ssh: vec![],
            network_version: 0,
            last_active: 0.0,
        };
        let context_of = |b: &Bottle| context(b).unwrap();
        let text = context_of(&bottle(&["jvm:version=21", "tools"], json!({})));
        assert!(
            text.contains(
                "\n## Features\n\n### Feature 'jvm'\n\nTemurin JDK 21 is the default `java`"
            ),
            "{text}"
        );
        assert!(text.contains("\n### Feature 'tools'\n\n"));
        assert!(!context_of(&bottle(&[], json!({}))).contains("## Features"));
        let text = context_of(&bottle(&[], json!({"permissions": ["message-any"]})));
        assert!(
            text.contains("## Permissions")
                && text.contains(specs::PERMISSIONS[1].1)
                && !text.contains(specs::PERMISSIONS[0].1)
        );
        let refused = "comes back 403, saying why. That's policy, not a fault: if you need it, tell the user.";
        assert!(context_of(&bottle(&["github"], json!({}))).contains(refused));
        assert!(!context_of(&bottle(&["tools"], json!({}))).contains(refused));
        let detached = Bottle {
            mounts: vec![Mount::new(
                "/workspace",
                "/src/r",
                None,
                "c".repeat(40),
                vec![],
            )],
            ..bottle(&[], json!({}))
        };
        assert!(context_of(&detached).contains("checkout of `r`, at a detached commit."));
        assert_eq!(bottle(&[], json!({})).workdir(), "/home/genie");
    }

    #[test]
    fn memory_in_mib() {
        assert_eq!(memory_mib(Some("8G")).unwrap(), 8192);
        assert_eq!(memory_mib(Some("512m")).unwrap(), 512);
        assert_eq!(
            memory_mib(None).unwrap_err(),
            "a bottle needs its memory, a size in M or G, e.g. \"8G\" (not none)"
        );
    }
}
