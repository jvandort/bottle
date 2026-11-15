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
//!     some to folio serve: bottle.host, which serves each mount's git remotes, ssh to bottles, and
//!     its credentials' hosts (see services.rs; serve/)
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

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

use crate::bottle_cli::{Create, Exec};
use crate::daemon::{self, Binding, Served};
use crate::git::bottle_git;
use crate::lanes::shell_quote as q;
use crate::repos::{self, Repo};
use crate::runners::{self, Runner};
use crate::serve::host::{HEADS, NS, Remote, UPSTREAM};
use crate::services::{self, HOST, HOST_ADDRESS, MAX_SSH};
use crate::{auth, builds, features, images, specs, store};

/// A mount at /x has its objects at /mnt/objects/x.
const OBJECTS_ROOT: &str = "/mnt/objects";
pub const USER: &str = "genie";
const HOME: &str = "/home/genie";
/// In genie's home.
const CONTEXT_FILE: &str = "BOTTLE.md";
/// The remotes a bottle's repo is served when it's given none, read-only.
const ORIGIN: &str = "origin";
const HOST_REMOTE: &str = "host";
/// Where a thread's directory has its bottles' records.
const RECORDS: &str = "bottles";
/// The version of the policy network() makes: 1, its routes to folio serve's one socket; 2, folio
/// serve's, by service; 3, on a runner, to folio serve's forwarded there (services_sockets), not
/// bottle-cli's.
const NETWORK: u32 = 3;
/// What a flow to a bottle it may ssh to is joined to, there: sshd, for that one connection.
pub const SSHD: &str = "mkdir -p /run/sshd && exec /usr/sbin/sshd -i -e";
/// The state of a bottle on a runner that can't be reached; and on one detached, which isn't asked.
pub const UNREACHABLE: &str = "unreachable";
pub const DETACHED: &str = "detached";

/// What bottle relies on inside every bottle, checked (as genie) whenever one starts. Documented in
/// folio/features/base/install.sh; keep them in sync.
const CONTRACT: [(&str, &str); 6] = [
    ("bottle-entrypoint is installed", "test -x /usr/local/bin/bottle-entrypoint"),
    ("the genie user has UID 1000", "[ \"$(id -un)\" = genie ] && [ \"$(id -u)\" = 1000 ]"),
    ("genie has passwordless sudo", "sudo -n true"),
    ("git is installed", "command -v git"),
    ("sshd is installed", "test -x /usr/sbin/sshd"),
    ("update-ca-certificates is installed", "test -x /usr/sbin/update-ca-certificates"),
];

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

/// A remote, as a record keeps it: {name, fetch, push}, options since removed dropped.
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

fn remote_spec(r: &Remote) -> String {
    format!("{}:fetch={}", r.name, r.fetch) + &r.push.as_ref().map(|p| format!(",push={p}")).unwrap_or_default()
}

fn push_namespace(r: &Remote) -> Option<&str> {
    r.push.as_deref().map(|p| p.strip_prefix(NS).unwrap_or(p))
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
    #[serde(default, deserialize_with = "remotes_from", serialize_with = "remotes_to")]
    pub remotes: Vec<Remote>,
    /// Where its objects are mounted in the bottle (default: under OBJECTS_ROOT).
    #[serde(default)]
    pub objects: String,
    /// Where its remotes are under bottle.host/git (default: its path, less the /).
    #[serde(default)]
    pub at: Option<String>,
}

impl Mount {
    fn new(path: &str, repo: &str, branch: Option<String>, commit: String, remotes: Vec<Remote>) -> Mount {
        let mut mount = Mount {
            path: path.into(),
            repo: repo.into(),
            branch,
            commit,
            remotes,
            objects: String::new(),
            at: None,
        };
        mount.fill();
        mount
    }

    /// Its defaults, where a record left them out.
    fn fill(&mut self) {
        if self.objects.is_empty() {
            self.objects = format!("{OBJECTS_ROOT}{}", self.path);
        }
        if self.at.is_none() {
            self.at = Some(self.path.trim_start_matches('/').to_string());
        }
    }

    fn at(&self) -> &str {
        self.at.as_deref().unwrap_or_default()
    }

    pub fn url(&self, remote: &str) -> String {
        format!("http://{HOST}{}/{}/{remote}", services::GIT_PATH, self.at())
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

/// A mount as create is asked for one: the repo, where, from what, served what.
pub struct MountRequest {
    pub path: String,
    pub repo: PathBuf,
    /// A branch or commit (default: origin's default branch, else what's checked out).
    pub checkout: Option<String>,
    /// Remote specs (default: origin and host, read-only).
    pub remotes: Option<Vec<String>>,
}

fn default_status() -> String {
    "creating".into()
}

fn local() -> String {
    runners::LOCAL.into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bottle {
    pub name: String,
    pub id: String,
    pub image: String,
    pub created: f64,
    /// "creating" until setup finishes, "ready" after; "broken" if cleanup failed.
    #[serde(default = "default_status")]
    pub status: String,
    /// Features installed on top of the image, as specs (e.g. jvm:version=17), including
    /// dependencies.
    #[serde(default)]
    pub features: Vec<String>,
    /// The VM's memory, a size in M or G (e.g. 8G), from its spec. None only in records from before
    /// specs had to say.
    #[serde(default)]
    pub memory: Option<String>,
    /// The spec it was made from, canonical.
    #[serde(default = "empty_object")]
    pub spec: Value,
    /// Its repos, each checked out at its path; the first is where a shell starts.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Its creator's addition to ~/BOTTLE.md.
    #[serde(default)]
    pub context: String,
    /// The runner its machine is on; records from before runners were the local one's.
    #[serde(default = "local")]
    pub runner: String,
    /// The bottles it may ssh to.
    #[serde(default)]
    pub ssh: Vec<SshGrant>,
    /// Its machine's name, as bottle-cli where it is has it.
    #[serde(default)]
    pub machine_name: String,
    /// Which of network()'s policies its machine has (NETWORK the current one); 0 for one from
    /// before this was recorded. One behind is given the current one as it's next started.
    #[serde(default)]
    pub network_version: u32,
    /// When it was last used (touch): made, or attached to.
    #[serde(default)]
    pub last_active: f64,
}

fn empty_object() -> Value {
    json!({})
}

impl Bottle {
    /// The image and its features, for display: base+claude+jvm.
    fn environment(&self) -> String {
        let mut features = self.features.clone();
        features.sort();
        std::iter::once(self.image.clone()).chain(features).collect::<Vec<_>>().join("+")
    }

    /// Where a shell or a command starts: the first mount, else genie's home.
    pub fn workdir(&self) -> &str {
        self.mounts.first().map_or(HOME, |m| m.path.as_str())
    }

    /// Its machine's name, as bottle-cli there lists it: the bottle's own.
    pub fn machine(&self) -> &str {
        &self.machine_name
    }

    /// Its machine and where it is, as folio shows it: RUNNER/NAME for one on a runner.
    fn qualified(&self) -> String {
        if self.runner == runners::LOCAL {
            self.machine().to_string()
        } else {
            format!("{}/{}", self.runner, self.machine())
        }
    }

    fn permissions(&self) -> Vec<String> {
        self.spec
            .get("permissions")
            .and_then(Value::as_array)
            .map(|p| p.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    }
}

/// Each bottle's record, by name: in its thread's directory, <repo key>/<thread>/bottles/.
fn records() -> BTreeMap<String, PathBuf> {
    let mut paths = Vec::new();
    for repo in fs::read_dir(crate::home()).into_iter().flatten().flatten() {
        for thread in fs::read_dir(repo.path()).into_iter().flatten().flatten() {
            for record in fs::read_dir(thread.path().join(RECORDS)).into_iter().flatten().flatten() {
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
        // Keys a record from an older bottle may have (its driver, say) are left out.
        if let Some(entry) = store::read_json(&path)? {
            let mut bottle: Bottle =
                serde_json::from_value(entry).map_err(|e| format!("{}: {e}", path.display()))?;
            for mount in &mut bottle.mounts {
                mount.fill();
            }
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
            crate::repr(name),
            if known.is_empty() { "none".to_string() } else { known.join(", ") }
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
            None => return Err(format!("bottle {} has no record, and no thread to keep one", bottle.name)),
        },
    };
    store::write_json_sorted(&path, bottle)
}

fn forget(name: &str) {
    if let Some(path) = records().remove(name) {
        let _ = fs::remove_file(&path);
        // A thread's bottles/, with none left.
        let _ = fs::remove_dir(path.parent().unwrap());
    }
}

/// Create a bottle from a spec: its features and memory, from a file its creator owns.
///
/// `name` is the creator's choice, and the bottle's identity: there's no default, so a creator
/// never has to find out what it got, and a taken name is refused rather than numbered. Each mount
/// is a repo whose objects are mounted and whose remotes are served, checked out at its path;
/// without any, the bottle has no repo. Nothing is registered in a repo: the bottle's creator reads
/// what it pushes, from the namespaces it chose. Its machine is on `runner`. It may ssh to each
/// bottle `ssh` gives, by the name given: {name: bottle}. Its record is kept in `owner`, its
/// thread's directory.
pub fn create(
    owner: &Path,
    spec: &Value,
    name: &str,
    mounts: &[MountRequest],
    context: &str,
    runner: &str,
    ssh: &[(String, String)],
) -> Result<Bottle, String> {
    let on = runners::get(runner)?;
    let place = if runner == runners::LOCAL {
        "this machine".to_string()
    } else {
        format!("runner {runner}")
    };
    let spec = specs::canonical(spec, Some(on.memory_mib()?), &place)?;
    let installed: Vec<String> = features::resolve(&repos::features_of(&spec))?.iter().map(|f| f.spec()).collect();
    let existing = load()?;
    if !is_name(name) {
        return Err(format!("invalid name {}: use letters, digits, '.', '_' or '-'", crate::repr(name)));
    }
    if existing.contains_key(name) {
        return Err(format!("a bottle named {} already exists", crate::repr(name)));
    }
    let grants = plan_ssh(ssh, &existing)?;
    // A bottle it may ssh to is one it works with: their lanes may be shared.
    let granted: BTreeSet<&str> = grants.iter().map(|g| g.id.as_str()).collect();
    let others: Vec<&Bottle> = existing.values().filter(|b| !granted.contains(b.id.as_str())).collect();
    let planned = plan_mounts(mounts, &others)?;
    let tag = builds::ensure_built(&installed, &on)?;
    auth::warn_unset(&installed)?;

    let now = now();
    let bottle = Bottle {
        name: name.into(),
        id: uuid(),
        image: features::BASE.into(),
        created: now,
        status: default_status(),
        features: installed,
        memory: spec.get("memory").and_then(Value::as_str).map(String::from),
        spec,
        mounts: planned.iter().map(|(m, _)| m.clone()).collect(),
        context: context.into(),
        runner: runner.into(),
        ssh: grants,
        machine_name: name.into(),
        network_version: 0,
        last_active: now,
    };
    let repos: Vec<Repo> = planned.into_iter().map(|(_, r)| r).collect();
    bring_up(bottle, &repos, &tag, owner)
}

fn uuid() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut random) = fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut random, &mut bytes);
    }
    // A version 4 UUID, as Python's uuid4 makes, as hex.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// A mount's path: absolute, of name-like segments, so it's safe in a URL and a shell.
fn is_mount_path(path: &str) -> bool {
    path.starts_with('/') && path[1..].split('/').all(is_name)
}

/// A name a grant gives: a host name, lower case.
fn is_ssh_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|label| {
            let b = label.as_bytes();
            !b.is_empty()
                && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
                && b[0] != b'-'
                && b[b.len() - 1] != b'-'
        })
}

/// Each requested mount, checked and worked out, with its repo; nothing written.
fn plan_mounts(requests: &[MountRequest], others: &[&Bottle]) -> Result<Vec<(Mount, Repo)>, String> {
    let mut planned: Vec<(Mount, Repo)> = Vec::new();
    for request in requests {
        let trimmed = request.path.trim_end_matches('/');
        let path = if trimmed.is_empty() { request.path.as_str() } else { trimmed };
        if !is_mount_path(path) {
            return Err(format!(
                "invalid mount path {}: an absolute path of names, e.g. /workspace",
                crate::repr(&request.path)
            ));
        }
        for (earlier, _) in &planned {
            let e = earlier.path.as_str();
            if path == e || path.starts_with(&format!("{e}/")) || e.starts_with(&format!("{path}/")) {
                return Err(format!("mounts {e} and {path} overlap"));
            }
        }
        let toplevel = repos::toplevel(&request.repo)?;
        let repo = Repo::new(repos::derive_name(&toplevel), toplevel);
        let start = start_at(&repo, request.checkout.as_deref())?;
        let served = parse_remotes(request.remotes.as_deref())?;
        refuse_a_second_writer(&repo, &served, others)?;
        let mount = Mount::new(path, &repo.path.to_string_lossy(), start.branch, start.commit, served);
        planned.push((mount, repo));
    }
    Ok(planned)
}

/// Each bottle the new one may ssh to, checked: a bottle that exists and is ready, under a host
/// name.
fn plan_ssh(ssh: &[(String, String)], existing: &BTreeMap<String, Bottle>) -> Result<Vec<SshGrant>, String> {
    if ssh.len() > MAX_SSH {
        return Err(format!("a bottle may ssh to at most {MAX_SSH} others"));
    }
    let mut grants = Vec::new();
    for (index, (name, target)) in ssh.iter().enumerate() {
        if !is_ssh_name(name) || name == HOST {
            return Err(format!("invalid ssh name {}: a host name, e.g. sidecar.host", crate::repr(name)));
        }
        let Some(there) = existing.get(target) else {
            return Err(format!("ssh {name}={target}: no bottle named {}", crate::repr(target)));
        };
        if there.status != "ready" {
            return Err(format!("ssh {name}={target}: {target} is {}", there.status));
        }
        grants.push(SshGrant {
            name: name.clone(),
            bottle: target.clone(),
            id: there.id.clone(),
            address: services::ssh_address(index),
        });
    }
    Ok(grants)
}

/// Create the bottle's machine from `tag`, and set it up, its record in `owner`'s; roll it all back
/// on failure.
fn bring_up(bottle: Bottle, mounted: &[Repo], tag: &str, owner: &Path) -> Result<Bottle, String> {
    // Never build on a name something else is using: rollback would then be cleaning up after it.
    if runner_of(&bottle)?.state(bottle.machine())?.is_some() {
        return Err(format!("a machine named {} already exists", bottle.machine()));
    }
    save(&bottle, Some(owner))?;
    let mut bottle = match run_machine(bottle.clone(), mounted, tag) {
        Ok(bottle) => bottle,
        Err(failure) => {
            eprintln!("{}: creating failed, rolling back: {failure}", bottle.name);
            if let Err(cleanup) = delete(&bottle.name) {
                return Err(format!("{failure}; cleanup also failed: {cleanup}"));
            }
            return Err(failure);
        }
    };
    bottle.status = "ready".into();
    save(&bottle, None)?;
    Ok(bottle)
}

/// Where a checkout starts: a branch (the repo's, else origin's), else any commit; by default
/// origin's default branch, else what the repo has checked out.
fn start_at(repo: &Repo, checkout: Option<&str>) -> Result<repos::Start, String> {
    let Some(checkout) = checkout else {
        return repos::default_start(repo);
    };
    if let Ok(commit) = repos::resolve_branch(repo, checkout) {
        return Ok(repos::Start {
            branch: Some(checkout.into()),
            commit,
        });
    }
    match bottle_git(&["rev-parse", "--verify", "--quiet", &format!("{checkout}^{{commit}}")], Some(&repo.path)) {
        Ok(commit) => Ok(repos::Start { branch: None, commit }),
        Err(_) => Err(format!(
            "{} has no branch or commit {}",
            repo.path.display(),
            crate::repr(checkout)
        )),
    }
}

/// The remotes a bottle is served: those given, or origin and host, read-only.
fn parse_remotes(given: Option<&[String]>) -> Result<Vec<Remote>, String> {
    let Some(given) = given else {
        return Ok(vec![
            Remote {
                name: ORIGIN.into(),
                fetch: UPSTREAM.into(),
                push: None,
            },
            Remote {
                name: HOST_REMOTE.into(),
                fetch: HEADS.into(),
                push: None,
            },
        ]);
    };
    let remotes = given.iter().map(|s| Remote::parse(s)).collect::<Result<Vec<_>, _>>()?;
    let mut names: Vec<&str> = remotes.iter().map(|r| r.name.as_str()).collect();
    names.sort();
    for pair in names.windows(2) {
        if pair[0] == pair[1] {
            return Err(format!("remote {} given twice", pair[0]));
        }
    }
    let mut pushed: Vec<&str> = remotes.iter().filter_map(push_namespace).collect();
    pushed.sort();
    for pair in pushed.windows(2) {
        if pair[0] == pair[1] {
            return Err(format!(
                "two remotes push into namespace {}; each namespace has one writer",
                pair[0]
            ));
        }
    }
    Ok(remotes)
}

/// Refuse a namespace another bottle already pushes into, in the same repo: each has one writer,
/// but for the bottles a new one may ssh to, which `others` leaves out: they work together, and as
/// pushes can't rewrite, a racing one is refused rather than lost.
fn refuse_a_second_writer(repo: &Repo, remotes: &[Remote], others: &[&Bottle]) -> Result<(), String> {
    let ours: BTreeSet<&str> = remotes.iter().filter_map(push_namespace).collect();
    if ours.is_empty() {
        return Ok(());
    }
    let mine = repos::objects_dir(repo)?;
    for other in others {
        for mount in &other.mounts {
            let theirs: BTreeSet<&str> = mount.remotes.iter().filter_map(push_namespace).collect();
            let shared: Vec<&&str> = ours.intersection(&theirs).collect();
            if let (Some(first), Some(directory)) = (shared.first(), git_dir(mount))
                && directory.join("objects") == mine
            {
                return Err(format!(
                    "bottle {} already pushes into namespace {first} of {}; each namespace has one writer",
                    other.name,
                    repo.path.display()
                ));
            }
        }
    }
    Ok(())
}

/// Create and start a fresh machine for the bottle from image `tag`, and set it up from scratch.
fn run_machine(mut bottle: Bottle, mounted: &[Repo], tag: &str) -> Result<Bottle, String> {
    let runner = runner_of(&bottle)?;
    // The bottle gets stand-ins, never credentials (see auth.rs), and no proxy: its flows are
    // decided as they open, by the policy its machine is made with.
    let mut mounts = Vec::new();
    for (repo, mount) in mounted.iter().zip(&bottle.mounts) {
        mounts.push(runner.mount(repo, &mount.objects)?);
    }
    runner.create(Create {
        name: bottle.machine(),
        image: tag,
        mounts,
        env: machine_env(&bottle.features, &bottle.permissions(), bottle.workdir())?,
        memory_mib: Some(memory_mib(bottle.memory.as_deref())?),
        cpus: 0,
        disk_gib: specs::disk_gib(&bottle.spec),
        // What bottle-cli's exec and shell use unless told, and what its ls shows.
        user: Some(USER),
        workdir: Some(bottle.workdir()),
        labels: specs::labels(&bottle.spec),
        network: Some(network(&bottle)?),
    })?;
    bottle.network_version = NETWORK;
    save(&bottle, None)?;
    start_machine(&bottle)?;
    verify_contract(&bottle)?;
    point_at_host(&bottle)?;
    init_mounts(&bottle)?;
    for grant in &bottle.ssh {
        let_in(&bottle, grant)?;
    }
    Ok(bottle)
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

/// Let the bottle ssh to the one it's granted, as grant.name: its key let in there, that one's host
/// key known here.
fn let_in(bottle: &Bottle, grant: &SshGrant) -> Result<(), String> {
    let there = ensure_running(&grant.bottle)?;
    let key = in_bottle(
        bottle,
        &sh(&format!(
            "install -d -m 700 ~/.ssh && {{ test -f ~/.ssh/id_ed25519 || ssh-keygen -q -t ed25519 -N '' -f \
             ~/.ssh/id_ed25519 -C {USER}@{}; }} && cat ~/.ssh/id_ed25519.pub",
            bottle.name
        )),
    )?;
    let host_key = exec(
        &there,
        &sh("{ test -f /etc/ssh/ssh_host_ed25519_key || ssh-keygen -q -t ed25519 -N '' -f \
             /etc/ssh/ssh_host_ed25519_key; } && cut -d' ' -f1,2 /etc/ssh/ssh_host_ed25519_key.pub"),
        Some("root"),
        None,
    )?
    .trim()
    .to_string();
    // Modes set, not left to a umask: commands run in a machine get 0000, and sshd refuses keys
    // others can write.
    exec(
        &there,
        &sh("install -d -m 700 ~/.ssh && read -r key && touch ~/.ssh/authorized_keys && \
             chmod 600 ~/.ssh/authorized_keys && \
             { grep -qxF \"$key\" ~/.ssh/authorized_keys || echo \"$key\" >> ~/.ssh/authorized_keys; }"),
        Some(USER),
        Some(&format!("{key}\n")),
    )?;
    exec(
        bottle,
        &sh(&format!(
            "install -d -m 700 ~/.ssh && touch ~/.ssh/known_hosts && chmod 600 ~/.ssh/known_hosts && \
             sed -i '/^{} /d' ~/.ssh/known_hosts && cat >> ~/.ssh/known_hosts",
            regex_escape(&grant.name)
        )),
        Some(USER),
        Some(&format!("{} {host_key}\n", grant.name)),
    )?;
    Ok(())
}

/// `s` as Python's re.escape has it: the characters a regular expression gives a meaning escaped.
fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "()[]{}?*+-|^$\\.&~# \t\n\r\x0b\x0c".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Start the bottle's machine, folio serve answering what it routes there as this bottle's, its
/// credentials read now. bottle-cli makes the machine's caching CA as it starts, for its policy's
/// hosts, and gives it to the bottle to trust (the base image's entrypoint does).
fn start_machine(bottle: &Bottle) -> Result<(), String> {
    daemon::bind(&binding(bottle))?;
    runner_of(bottle)?.start(bottle.machine())
}

/// The policy the bottle's machine decides its connections by (services::network): what its
/// features cache, and what it routes to folio serve: bottle.host, ssh to bottles, and its
/// credentials' hosts, logged in or not.
fn network(bottle: &Bottle) -> Result<Value, String> {
    let resolved = features::resolve(&bottle.features)?;
    Ok(services::network(
        &auth::credential_destinations(&bottle.features)?,
        &images::cached_for(&resolved),
    ))
}

/// A machine's memory in MiB, from its spec's size (8G, 512M). It takes it as the guest touches it.
fn memory_mib(memory: Option<&str>) -> Result<u64, String> {
    let given = memory.unwrap_or_default().trim().to_uppercase();
    let refused = || {
        format!(
            "a bottle needs its memory, a size in M or G, e.g. \"8G\" (not {})",
            memory.map_or("None".to_string(), crate::repr)
        )
    };
    let (number, unit) = given.split_at(given.len().saturating_sub(1));
    if !(unit == "M" || unit == "G")
        || !number.starts_with(|c: char| ('1'..='9').contains(&c))
        || !number.chars().all(|c| c.is_ascii_digit())
    {
        return Err(refused());
    }
    let amount: u64 = number.parse().map_err(|_| refused())?;
    Ok(amount * if unit == "G" { 1024 } else { 1 })
}

/// Give the bottle bottle.host, at the address whose flows its machine routes to folio serve's git
/// service, and the name of each bottle it may ssh to, at its address.
fn point_at_host(bottle: &Bottle) -> Result<(), String> {
    let mut entries = vec![format!("{HOST_ADDRESS} {HOST}")];
    entries.extend(bottle.ssh.iter().map(|g| format!("{} {}", g.address, g.name)));
    let script: Vec<String> = entries
        .iter()
        .map(|e| format!("grep -qsx {e} /etc/hosts || echo {e} >> /etc/hosts", e = q(e)))
        .collect();
    exec(bottle, &sh(&script.join("; ")), Some("root"), None)
        .map(|_| ())
        .map_err(|e| format!("giving {} bottle.host failed: {e}", bottle.machine()))
}

/// Run a command in the bottle's running machine and return its output.
fn exec(bottle: &Bottle, argv: &[String], user: Option<&str>, input: Option<&str>) -> Result<String, String> {
    runner_of(bottle)?.exec(bottle.machine(), argv, user, input)
}

fn in_bottle(bottle: &Bottle, argv: &[String]) -> Result<String, String> {
    Ok(exec(bottle, argv, Some(USER), None)?.trim().to_string())
}

/// The git dir of a mount's repo, which its remotes are served from; None if it's gone.
pub fn git_dir(mount: &Mount) -> Option<PathBuf> {
    repos::objects_dir(&Repo::new("", &mount.repo))
        .ok()
        .and_then(|o| o.parent().map(Path::to_path_buf))
}

/// What folio serve is told of the bottle: all it needs of it.
fn binding(bottle: &Bottle) -> Binding<'_> {
    Binding {
        bottle: &bottle.name,
        machine: bottle.machine(),
        runner: &bottle.runner,
        id: &bottle.id,
        repos: bottle
            .mounts
            .iter()
            .map(|m| Served {
                git_dir: git_dir(m),
                remotes: m.remotes.iter().map(remote_spec).collect(),
                at: m.at().to_string(),
            })
            .collect(),
        features: bottle.features.clone(),
        permissions: bottle.permissions(),
        labels: specs::labels(&bottle.spec),
        repo: bottle
            .mounts
            .first()
            .map(|m| Path::new(&m.repo).file_name().unwrap_or_default().to_string_lossy().into_owned()),
        ssh: bottle
            .ssh
            .iter()
            .map(|g| json!({"name": g.name, "bottle": g.bottle, "id": g.id, "address": g.address}))
            .collect(),
    }
}

/// What folio serve is told of every ready bottle as it starts, to answer them before anyone else:
/// wherever they run, but on a runner detached (turned off). One it can't be told of (its runner
/// unreachable) is left out, as it can't be reached either.
pub fn bind_messages() -> Result<Vec<Value>, String> {
    let mut messages = Vec::new();
    let detached = runners::registered()?;
    for bottle in load()?.values() {
        let off = bottle.runner != runners::LOCAL && detached.get(&bottle.runner).copied().unwrap_or(false);
        if bottle.status != "ready" || off {
            continue;
        }
        match daemon::bind_message(&binding(bottle)) {
            Ok(message) => messages.push(message),
            Err(e) => eprintln!("{}: folio serve won't answer it: {e}", bottle.name),
        }
    }
    Ok(messages)
}

/// What a bottle's machine is created with: the stand-ins its features asked for, never
/// credentials; its permissions, as BOTTLE_PERMISSIONS, so the tools that use them know to offer
/// them; and where a login starts, as BOTTLE_WORKDIR (the base image's profile goes there).
///
/// BOTTLE_PERMISSIONS only says what folio serve would allow: folio serve checks the bottle's spec,
/// not this, so a bottle that sets it itself gains nothing.
///
/// Every exec sees these. SSH sessions only see /etc/environment, so the base image's entrypoint
/// mirrors there the ones BOTTLE_MIRROR_ENV names.
fn machine_env(features: &[String], permissions: &[String], workdir: &str) -> Result<BTreeMap<String, String>, String> {
    let mut env = auth::standins_for(features)?;
    if !permissions.is_empty() {
        env.insert("BOTTLE_PERMISSIONS".into(), permissions.join(","));
    }
    env.insert("BOTTLE_WORKDIR".into(), workdir.into());
    let mirrored: Vec<String> = env.keys().cloned().collect();
    env.insert("BOTTLE_MIRROR_ENV".into(), mirrored.join(" "));
    Ok(env)
}

/// What the agent should know about its bottle, written to ~/BOTTLE.md when its workspace is set
/// up: what the bottle is, what its features give it, and nothing about how to work in it.
///
/// Agent-neutral: agent features point their own instruction files at it (the claude feature links
/// ~/.claude/CLAUDE.md to it).
///
/// How to work -- branches, pushing, merging -- is its creator's to say, in the context it adds;
/// this says what's there.
pub fn context(bottle: &Bottle) -> Result<String, String> {
    let mut lines = vec![format!(
        "# Bottle\n\nYou're `{USER}`, running in bottle `{}`: a sandboxed Linux VM with passwordless sudo.\n",
        bottle.name
    )];
    for mount in &bottle.mounts {
        let at = match &mount.branch {
            Some(branch) => format!("on the `{branch}` branch"),
            None => "at a detached commit".into(),
        };
        let name = Path::new(&mount.repo).file_name().unwrap_or_default().to_string_lossy();
        lines.push(format!("- `{}` is a checkout of `{name}`, {at}.", mount.path));
        if mount.remotes.is_empty() {
            lines.push("  It has no remotes.".into());
        } else {
            lines.push("  Its remotes are served by the host:".into());
            lines.extend(mount.remotes.iter().map(|r| format!("  - `{}`: {}", r.name, describe_remote(r))));
        }
    }
    if bottle.mounts.is_empty() {
        lines.push("- This bottle has no repo.".into());
    }
    for grant in &bottle.ssh {
        lines.push(format!(
            "- `ssh {}` reaches bottle `{}`, as `{USER}`: a shell, a command, scp, git over ssh.",
            grant.name, grant.bottle
        ));
    }
    lines.push(
        "- The network works as usual, DNS included. Every connection is decided by\n  the host: private \
         addresses are blocked, and a refused one is reset. `bottle.host` is the host."
            .into(),
    );
    let resolved = features::resolve(&bottle.features)?;
    if resolved.iter().any(|f| !f.credentials.is_empty()) {
        lines.push(
            "- A credential the host attaches goes only where its feature allows: a request it refuses\n  \
             comes back 403, saying why. That's policy, not a fault: if you need it, tell the user."
                .into(),
        );
    }
    // What the spec permits it beyond every bottle, by specs::PERMISSIONS' own word on each.
    let permissions = bottle.permissions();
    if !permissions.is_empty() {
        lines.push("\n## Permissions\n\nThe host lets this bottle do more than most:\n".into());
        for p in &permissions {
            if let Some((_, said)) = specs::PERMISSIONS.iter().find(|(name, _)| name == p) {
                lines.push(format!("- {said}"));
            }
        }
    }
    // Each feature's own word on what it gives the agent (customizations.bottle.context).
    let told: Vec<(String, String)> = resolved
        .iter()
        .map(|f| (f.id.clone(), f.agent_context()))
        .filter(|(_, text)| !text.is_empty())
        .collect();
    if !told.is_empty() {
        lines.push("\n## Features".into());
        lines.extend(told.iter().map(|(id, text)| format!("\n### Feature '{id}'\n\n{text}")));
    }
    let text = lines.join("\n") + "\n";
    Ok(if bottle.context.is_empty() {
        text
    } else {
        format!("{text}\n{}", bottle.context)
    })
}

fn describe_remote(remote: &Remote) -> String {
    let what = if remote.fetch == UPSTREAM {
        "the repo's upstream, up to date whenever you fetch."
    } else if remote.fetch == HEADS {
        "the user's own branches, as they stand in their repo."
    } else {
        "refs the host keeps for this bottle."
    };
    match &remote.push {
        None => format!("{what} Read-only."),
        // Fetching reads other refs than pushing writes (a two-lane exchange, say), so say so, or
        // an agent fetching to check its push finds nothing and pushes again.
        Some(push) => {
            let echo = if *push == remote.fetch {
                ""
            } else {
                " Fetching it doesn't show your own pushes."
            };
            format!("{what} You can push to it: pushes can't rewrite or delete what's already there.{echo}")
        }
    }
}

/// Check out each mount's branch or commit at its path, borrowing its mounted objects, with its
/// remotes; then write the context.
///
/// Each path is made for genie (as root: it may be anywhere), and marked a safe directory for git,
/// whoever ends up owning it. No branch tracks anything: which remote a branch pulls from is its
/// creator's to decide.
fn init_mounts(bottle: &Bottle) -> Result<(), String> {
    let paths: Vec<String> = bottle.mounts.iter().map(|m| q(&m.path)).collect();
    let paths = paths.join(" ");
    if !paths.is_empty() {
        exec(bottle, &sh(&format!("install -d -o {USER} -g {USER} {paths}")), Some("root"), None)
            .map_err(|e| format!("making {paths} failed: {e}"))?;
    }
    let mut script = vec!["set -e".to_string()];
    for mount in &bottle.mounts {
        let (path, commit) = (q(&mount.path), q(&mount.commit));
        script.push(format!("git config --global --add safe.directory {path}"));
        let point_head = match &mount.branch {
            Some(branch) => {
                let branch = q(branch);
                script.push(format!("git init -q -b {branch} {path}"));
                format!("update-ref refs/heads/{branch} {commit}")
            }
            None => {
                script.push(format!("git init -q {path}"));
                format!("update-ref --no-deref HEAD {commit}")
            }
        };
        script.push(format!("echo {} > {path}/.git/objects/info/alternates", q(&mount.objects)));
        script.push(format!("git -C {path} {point_head}"));
        script.push(format!("git -C {path} reset -q --hard"));
        let names: Vec<&str> = mount.remotes.iter().map(|r| r.name.as_str()).collect();
        for name in &names {
            script.push(format!("git -C {path} remote add {name} {}", q(&mount.url(name))));
        }
        if names.contains(&ORIGIN) {
            script.push(format!("git -C {path} config checkout.defaultRemote {ORIGIN}"));
        }
        if !names.is_empty() {
            script.push(format!("git -C {path} fetch -q --multiple {}", names.join(" ")));
        }
    }
    script.push(format!("printf '%s' {} > \"$HOME/{CONTEXT_FILE}\"", q(&context(bottle)?)));
    let checked_out = if paths.is_empty() { "nothing".to_string() } else { paths.clone() };
    exec(bottle, &sh(&script.join("\n")), Some(USER), None)
        .map(|_| ())
        .map_err(|e| format!("checking out {checked_out} failed: {e}"))
}

pub fn start(name: &str) -> Result<Bottle, String> {
    auth::warn_unset(&get(name)?.features)?;
    ensure_running(name)
}

/// Start the bottle's machine if it isn't running, folio serve answering it.
pub fn ensure_running(name: &str) -> Result<Bottle, String> {
    let mut bottle = get(name)?;
    if bottle.status != "ready" {
        return Err(format!(
            "{name} is {}; delete its thread to remove it (`folio delete`)",
            bottle.status
        ));
    }
    let runner = runner_of(&bottle)?;
    let Some(state) = runner.state(bottle.machine())? else {
        return Err(format!("{}'s machine is gone; delete its thread (`folio delete`)", bottle.name));
    };
    if state != "running" {
        let given = bottle.network_version < NETWORK;
        if given {
            // Made with another policy than this one: given it now.
            runner.configure(bottle.machine(), &network(&bottle)?)?;
            bottle.network_version = NETWORK;
            save(&bottle, None)?;
        }
        start_machine(&bottle)?;
        if given {
            // What's set up in it for its policy, again; one made with this policy has all it needs
            // on its disk, as bottle-cli's start alone would find it.
            verify_contract(&bottle)?;
            point_at_host(&bottle)?;
        }
    } else {
        // Running: folio serve, if it had stopped, binds it again as it starts.
        daemon::start()?;
    }
    Ok(bottle)
}

/// Fail loudly unless the running bottle provides everything bottle relies on (CONTRACT).
fn verify_contract(bottle: &Bottle) -> Result<(), String> {
    let checks: Vec<String> = CONTRACT
        .iter()
        .map(|(what, test)| format!("sh -c {} >/dev/null 2>&1 || echo {}", q(test), q(what)))
        .collect();
    let missing = in_bottle(bottle, &sh(&format!("# bottle contract\n{}", checks.join("\n"))))
        .unwrap_or_else(|e| format!("couldn't run the checks as {USER} ({e})"));
    if !missing.is_empty() {
        let problems: Vec<&str> = missing.lines().collect();
        return Err(format!(
            "{} doesn't meet the bottle contract: {}",
            bottle.environment(),
            problems.join("; ")
        ));
    }
    Ok(())
}

/// Record that the bottle is being used now (its last_active).
pub fn touch(name: &str) -> Result<(), String> {
    let mut bottle = get(name)?;
    bottle.last_active = now();
    save(&bottle, None)
}

/// The command line, for this machine, that runs `argv` in the bottle as genie, at its first mount:
/// started if it's stopped, logged in first to what its features need.
pub fn exec_command(name: &str, argv: &[String], tty: bool) -> Result<Vec<String>, String> {
    auth::warn_unset(&get(name)?.features)?;
    let bottle = ensure_running(name)?;
    let how = Exec {
        user: Some(USER),
        workdir: Some(bottle.workdir()),
        tty,
        ..Default::default()
    };
    Ok(runner_of(&bottle)?.exec_command(bottle.machine(), argv, &how))
}

/// Every bottle with its current state, or just those `names`: its machine's, unless setup never
/// finished, or its runner can't be reached or is detached. Only the runners these bottles are on
/// are asked, each once, all at once: a runner that's off costs what it takes to say so
/// (runners::LIST_TIMEOUT), however many there are.
pub fn list_all(names: Option<&[String]>) -> Result<Vec<(Bottle, String)>, String> {
    let mut chosen: Vec<Bottle> = load()?
        .into_values()
        .filter(|b| names.is_none_or(|n| n.contains(&b.name)))
        .collect();
    chosen.sort_by(|a, b| a.created.total_cmp(&b.created));
    let asked: BTreeSet<String> = chosen.iter().filter(|b| b.status == "ready").map(|b| b.runner.clone()).collect();
    let remote = if asked.iter().any(|r| r != runners::LOCAL) {
        runners::registered()?
    } else {
        BTreeMap::new()
    };
    let detached: BTreeSet<String> = asked
        .iter()
        .filter(|r| *r != runners::LOCAL && remote.get(*r).copied().unwrap_or(false))
        .cloned()
        .collect();
    let to_ask: Vec<String> = asked.difference(&detached).cloned().collect();
    let found: Vec<(String, Result<Option<BTreeMap<String, String>>, String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = to_ask
            .iter()
            .map(|name| {
                let name = name.clone();
                scope.spawn(move || {
                    // The runner's machines' states; None if it can't be reached (this machine
                    // always can).
                    let states = runners::get(&name).and_then(|r| r.states());
                    let result = match states {
                        Ok(states) => Ok(Some(states)),
                        Err(e) if name == runners::LOCAL => Err(e),
                        Err(_) => Ok(None),
                    };
                    (name, result)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut states: BTreeMap<String, Option<BTreeMap<String, String>>> = BTreeMap::new();
    for (name, result) in found {
        states.insert(name, result?);
    }
    Ok(chosen
        .into_iter()
        .map(|bottle| {
            let state = if bottle.status != "ready" {
                bottle.status.clone()
            } else if detached.contains(&bottle.runner) {
                DETACHED.into()
            } else {
                match &states[&bottle.runner] {
                    None => UNREACHABLE.into(),
                    Some(machines) => machines.get(bottle.machine()).cloned().unwrap_or_else(|| "missing".into()),
                }
            };
            (bottle, state)
        })
        .collect())
}

/// Every bottle's state and what it was made from, or just those `names`. A name with no bottle
/// gets a row with state "missing", so a caller can tell a bottle that's gone from a command that
/// failed.
pub fn status(names: &[String]) -> Result<Vec<Value>, String> {
    let listed = list_all(if names.is_empty() { None } else { Some(names) })?;
    let order: Vec<String> = if names.is_empty() {
        listed.iter().map(|(b, _)| b.name.clone()).collect()
    } else {
        names.to_vec()
    };
    let states: BTreeMap<String, (Bottle, String)> =
        listed.into_iter().map(|(b, s)| (b.name.clone(), (b, s))).collect();
    Ok(order
        .iter()
        .map(|name| {
            let Some((bottle, state)) = states.get(name) else {
                return json!({"name": name, "state": "missing"});
            };
            let mut features = bottle.features.clone();
            features.sort();
            let ssh: serde_json::Map<String, Value> =
                bottle.ssh.iter().map(|g| (g.name.clone(), g.bottle.clone().into())).collect();
            let mounts: Vec<Value> = bottle
                .mounts
                .iter()
                .map(|m| {
                    let urls: serde_json::Map<String, Value> =
                        m.remotes.iter().map(|r| (r.name.clone(), m.url(&r.name).into())).collect();
                    json!({"path": m.path, "repo": m.repo, "branch": m.branch, "commit": m.commit,
                           "remotes": m.remotes.iter().map(remote_spec).collect::<Vec<_>>(),
                           "objects": m.objects, "urls": urls})
                })
                .collect();
            json!({
                "name": bottle.name, "state": state, "created": bottle.created, "last_active": bottle.last_active,
                "runner": bottle.runner,
                // Its machine, as bottle-cli here names it (`bottle shell MACHINE`).
                "machine": bottle.qualified(),
                "features": features, "memory": bottle.memory, "spec": bottle.spec,
                "ssh": ssh,
                // Where its objects and remotes are, as a checkout restored into it has to point.
                "mounts": mounts,
            })
        })
        .collect())
}

/// Remove every part of the bottle that exists, then its record, and any images gone stale.
///
/// Each step tolerates its part being absent, so this also finishes off a bottle left half-made or
/// half-deleted. If a step fails, the record stays (marked broken) so the next delete can retry.
pub fn delete(name: &str) -> Result<(), String> {
    let mut bottle = get(name)?;
    // The registry says the machine is the bottle's: a machine has no labels to ask.
    let runner = runner_of(&bottle)?;
    let mut failures = Vec::new();
    if let Err(e) = daemon::release(&bottle.name) {
        failures.push(format!("egress: {e}"));
    }
    if let Err(e) = runner.delete(bottle.machine()) {
        failures.push(format!("machine: {e}"));
    }
    if !failures.is_empty() {
        eprintln!("{name}: deleting left parts behind: {}", failures.join("; "));
        bottle.status = "broken".into();
        save(&bottle, None)?;
        return Err(format!(
            "couldn't fully delete {name} ({}); try again, and what's left is deleted",
            failures.join("; ")
        ));
    }
    forget(name);
    // Images whose features or base have changed since they were built, if no bottle uses one. An
    // image that's still current is kept, even with nothing using it: it's a cache, and the next
    // bottle with those features starts without a build. Best effort: the bottle is already gone.
    if let Err(e) = builds::remove_stale(&runner) {
        eprintln!("bottle: couldn't remove stale images: {e}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_paths() {
        assert!(is_name("example-2.x_y") && !is_name("-x") && !is_name("broker~abc"));
        assert!(is_mount_path("/workspace") && is_mount_path("/a/b.c") && !is_mount_path("/") && !is_mount_path("x"));
        assert!(is_ssh_name("sidecar") && is_ssh_name("a.b-c") && !is_ssh_name("Sidecar") && !is_ssh_name("-a"));
    }

    #[test]
    fn memory_in_mib() {
        assert_eq!(memory_mib(Some("8G")).unwrap(), 8192);
        assert_eq!(memory_mib(Some("512m")).unwrap(), 512);
        assert_eq!(
            memory_mib(None).unwrap_err(),
            "a bottle needs its memory, a size in M or G, e.g. \"8G\" (not None)"
        );
    }

    #[test]
    fn a_record_from_an_older_folio_reads() {
        let record = json!({"name": "b", "id": "i", "image": "base", "created": 1.5, "driver": "smolvm",
                            "mounts": [{"path": "/workspace", "repo": "/r", "branch": null, "commit": "c",
                                        "remotes": [{"name": "origin", "fetch": "upstream", "gone": 1}]}]});
        let mut bottle: Bottle = serde_json::from_value(record).unwrap();
        for m in &mut bottle.mounts {
            m.fill();
        }
        assert_eq!((bottle.status.as_str(), bottle.runner.as_str(), bottle.network_version), ("creating", "local", 0));
        assert_eq!(bottle.mounts[0].objects, "/mnt/objects/workspace");
        assert_eq!(bottle.mounts[0].url("origin"), "http://bottle.host/git/workspace/origin");
    }

    #[test]
    fn re_escaped_as_python_escapes() {
        assert_eq!(regex_escape("side-car.host"), "side\\-car\\.host");
    }
}
