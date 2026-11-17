//! Making a bottle: its mounts and sidecar planned, its machine made and set up, rolled back if that fails.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::bottle_cli::Create;
use crate::control::Remote;
use crate::git::bottle_git;
use crate::repos::{self, Repo};
use crate::shell::quote as q;
use crate::specs::Spec;
use crate::{auth, builds, features, runners, services};

use super::context::{context, machine_env};
use super::machine::runner_of;
use super::machine::{
    delete, ensure_running, exec, in_bottle, memory_mib, network, point_at_host, sh, start_machine,
    verify_contract,
};
use super::record::Made;
use super::record::{now, save};
use super::state::Known;
use super::{Bottle, CONTEXT_FILE, Mount, NETWORK, ORIGIN, SshGrant, USER, load};

/// A bottle to make: named `name`, from `spec`, with `mounts`, its creator's `context` for
/// ~/BOTTLE.md, on `runner` (None: this machine), that may ssh to its `sidecar`, if it's given one;
/// its record kept in `owner`, its creator's directory.
#[derive(Clone, Copy)]
pub struct Order<'a> {
    pub owner: &'a Path,
    pub name: &'a str,
    pub spec: &'a Spec,
    pub mounts: &'a [MountRequest],
    pub context: &'a str,
    pub runner: Option<&'a str>,
    pub sidecar: Option<&'a str>,
}

/// A mount as create is asked for one: the repo, checked out at `path`, from `checkout`, served
/// `remotes`.
pub struct MountRequest {
    pub path: String,
    pub repo: PathBuf,
    /// A branch or commit (default: origin's default branch, else what's checked out).
    pub checkout: Option<String>,
    /// Remote specs.
    pub remotes: Vec<String>,
}

/// Create a bottle from a spec: its features and memory, from the thread's (threads.rs).
///
/// `name` is the bottle's identity (its thread's): a taken name is refused rather than numbered.
/// Each mount is a repo whose objects are mounted and whose remotes are served, checked out at its
/// path (a thread's /workspace and /notes; the broker's /notes alone). Nothing is registered in a
/// repo: the thread reads what it pushes from its lane. Its machine is on `runner`. With `sidecar`,
/// it may ssh to that bottle, as `sidecar`. Its record is kept in `owner`, its thread's directory.
pub fn create(known: &Known, order: &Order) -> Result<Bottle, String> {
    let Order {
        owner,
        name,
        spec,
        mounts,
        context,
        runner,
        sidecar,
    } = *order;
    let runner = runner.unwrap_or(runners::LOCAL);
    let on = runners::get(runner)?;
    let place = if runner == runners::LOCAL {
        "this machine".to_string()
    } else {
        format!("runner {runner}")
    };
    let spec = spec.canonical(Some(on.memory_mib()?), &place)?;
    let installed: Vec<String> = features::resolve(&spec.features)?
        .iter()
        .map(|f| f.spec())
        .collect();
    let existing = load()?;
    if existing.contains_key(name) {
        return Err(format!(
            "a bottle named {} already exists",
            crate::quoted(name)
        ));
    }
    let grants = plan_sidecar(sidecar, &existing)?;
    let planned = plan_mounts(mounts)?;
    let tag = builds::ensure_built(&installed, &on)?;
    auth::warn_unset(&installed)?;

    let now = now();
    let bottle = Bottle {
        name: name.into(),
        id: uuid(),
        image: features::BASE.into(),
        created: now,
        status: Made::Creating,
        features: installed,
        spec,
        mounts: planned.iter().map(|(m, _)| m.clone()).collect(),
        context: context.into(),
        runner: runner.into(),
        ssh: grants,
        network_version: 0,
        last_active: now,
    };
    let repos: Vec<Repo> = planned.into_iter().map(|(_, r)| r).collect();
    let bottle = bring_up(known, bottle, &repos, &tag, owner)?;
    let mounts: Vec<String> = bottle
        .mounts
        .iter()
        .map(|m| {
            format!(
                "{} at {} on {}",
                m.repo,
                m.branch.clone().unwrap_or_else(|| m.commit.clone()),
                m.path
            )
        })
        .collect();
    crate::log::log!(
        "bottles",
        "{}: created on {}, {}, features {}",
        bottle.name,
        bottle.runner,
        if mounts.is_empty() {
            "no repo".to_string()
        } else {
            mounts.join(", ")
        },
        if bottle.features.is_empty() {
            "none".to_string()
        } else {
            bottle.features.join(", ")
        }
    );
    Ok(bottle)
}

pub(super) fn uuid() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut random) = fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut random, &mut bytes);
    }
    // A version 4 UUID, as hex.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Each requested mount worked out, with its repo; nothing written.
pub(super) fn plan_mounts(requests: &[MountRequest]) -> Result<Vec<(Mount, Repo)>, String> {
    let mut planned: Vec<(Mount, Repo)> = Vec::new();
    for request in requests {
        let path = request.path.as_str();
        let toplevel = repos::toplevel(&request.repo)?;
        let repo = Repo::new(repos::derive_name(&toplevel), toplevel);
        let start = start_at(&repo, request.checkout.as_deref())?;
        let served = request
            .remotes
            .iter()
            .map(|r| Remote::parse(r))
            .collect::<Result<Vec<_>, _>>()?;
        let mount = Mount::new(
            path,
            &repo.path.to_string_lossy(),
            start.branch,
            start.commit,
            served,
        );
        planned.push((mount, repo));
    }
    Ok(planned)
}

/// The name a bottle has its sidecar by, in its /etc/hosts.
pub const SIDECAR: &str = "sidecar";

/// The bottle may ssh to its sidecar, if it has one: a bottle that exists and is ready.
pub(super) fn plan_sidecar(
    sidecar: Option<&str>,
    existing: &BTreeMap<String, Bottle>,
) -> Result<Vec<SshGrant>, String> {
    let Some(target) = sidecar else {
        return Ok(Vec::new());
    };
    let Some(there) = existing.get(target) else {
        return Err(format!("no sidecar named {}", crate::quoted(target)));
    };
    if there.status != Made::Ready {
        return Err(format!("its sidecar, {target}, is {}", there.status));
    }
    Ok(vec![SshGrant {
        name: SIDECAR.into(),
        bottle: target.into(),
        id: there.id.clone(),
        address: services::ssh_address(0),
    }])
}

/// Create the bottle's machine from `tag`, and set it up, its record in `owner`'s; roll it all back
/// on failure.
pub(super) fn bring_up(
    known: &Known,
    bottle: Bottle,
    mounted: &[Repo],
    tag: &str,
    owner: &Path,
) -> Result<Bottle, String> {
    // Never build on a name something else is using: rollback would then be cleaning up after it.
    if runner_of(&bottle)?.state(bottle.machine())?.is_some() {
        return Err(format!(
            "a machine named {} already exists",
            bottle.machine()
        ));
    }
    save(&bottle, Some(owner))?;
    let mut bottle = match run_machine(known, bottle.clone(), mounted, tag) {
        Ok(bottle) => bottle,
        Err(failure) => {
            crate::log::say!(
                "bottles",
                "{}: creating failed, rolling back: {failure}",
                bottle.name
            );
            if let Err(cleanup) = crate::interrupt::shielded(|| delete(known, &bottle.name)) {
                return Err(format!("{failure}; cleanup also failed: {cleanup}"));
            }
            return Err(failure);
        }
    };
    bottle.status = Made::Ready;
    save(&bottle, None)?;
    Ok(bottle)
}

/// Where a checkout starts: a branch (the repo's, else origin's), else any commit; by default
/// origin's default branch, else what the repo has checked out.
pub(super) fn start_at(repo: &Repo, checkout: Option<&str>) -> Result<repos::Start, String> {
    let Some(checkout) = checkout else {
        return repos::default_start(repo);
    };
    if let Ok(commit) = repos::resolve_branch(repo, checkout) {
        return Ok(repos::Start {
            branch: Some(checkout.into()),
            commit,
        });
    }
    match bottle_git(
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{checkout}^{{commit}}"),
        ],
        Some(&repo.path),
    ) {
        Ok(commit) => Ok(repos::Start {
            branch: None,
            commit,
        }),
        Err(_) => Err(format!(
            "{} has no branch or commit {}",
            repo.path.display(),
            crate::quoted(checkout)
        )),
    }
}

/// Create and start a fresh machine for the bottle from image `tag`, and set it up from scratch.
pub(super) fn run_machine(
    known: &Known,
    mut bottle: Bottle,
    mounted: &[Repo],
    tag: &str,
) -> Result<Bottle, String> {
    let runner = runner_of(&bottle)?;
    // The bottle gets stand-ins, never credentials (see auth.rs), and no proxy: its flows are
    // decided as they open, by the policy its machine is made with.
    let mut mounts = Vec::new();
    for (repo, mount) in mounted.iter().zip(&bottle.mounts) {
        mounts.push(runner.mount(repo, &mount.objects)?);
    }
    runner.create(
        Create {
            name: bottle.machine(),
            image: tag,
            mounts,
            env: machine_env(&bottle.features, &bottle.permissions(), bottle.workdir())?,
            memory_mib: Some(memory_mib(bottle.memory())?),
            cpus: 0,
            disk_gib: bottle.spec.disk_gib(),
            // What bottle-cli's exec and shell use unless told, and what its ls shows.
            user: Some(USER),
            workdir: Some(bottle.workdir()),
            labels: bottle.spec.labels.clone(),
        },
        Some(&network(&bottle)?),
    )?;
    bottle.network_version = NETWORK;
    save(&bottle, None)?;
    start_machine(known, &bottle)?;
    verify_contract(&bottle)?;
    point_at_host(&bottle)?;
    init_mounts(&bottle)?;
    for grant in &bottle.ssh {
        let_in(known, &bottle, grant)?;
    }
    Ok(bottle)
}

/// Let the bottle ssh to the one it's granted, as grant.name: its key let in there, that one's host
/// key known here.
pub(super) fn let_in(known: &Known, bottle: &Bottle, grant: &SshGrant) -> Result<(), String> {
    let there = ensure_running(known, &grant.bottle)?;
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
        &sh(
            "{ test -f /etc/ssh/ssh_host_ed25519_key || ssh-keygen -q -t ed25519 -N '' -f \
             /etc/ssh/ssh_host_ed25519_key; } && cut -d' ' -f1,2 /etc/ssh/ssh_host_ed25519_key.pub",
        ),
        Some("root"),
        None,
    )?
    .trim()
    .to_string();
    // Modes set, not left to a umask: commands run in a machine get 0000, and sshd refuses keys
    // others can write.
    exec(
        &there,
        &sh(
            "install -d -m 700 ~/.ssh && read -r key && touch ~/.ssh/authorized_keys && \
             chmod 600 ~/.ssh/authorized_keys && \
             { grep -qxF \"$key\" ~/.ssh/authorized_keys || echo \"$key\" >> ~/.ssh/authorized_keys; }",
        ),
        Some(USER),
        Some(&format!("{key}\n")),
    )?;
    exec(
        bottle,
        // Its line for the name, whatever it said before: the others kept, as they are.
        &sh(&format!(
            "install -d -m 700 ~/.ssh && touch ~/.ssh/known_hosts && \
             {{ awk -v name={} '$1 != name' ~/.ssh/known_hosts; cat; }} > ~/.ssh/known_hosts.new && \
             chmod 600 ~/.ssh/known_hosts.new && mv ~/.ssh/known_hosts.new ~/.ssh/known_hosts",
            q(&grant.name)
        )),
        Some(USER),
        Some(&format!("{} {host_key}\n", grant.name)),
    )?;
    Ok(())
}

/// Check out each mount's branch or commit at its path, borrowing its mounted objects, with its
/// remotes; then write the context.
///
/// Each path is made for genie (as root: it may be anywhere), and marked a safe directory for git,
/// whoever ends up owning it. No branch tracks anything: which remote a branch pulls from is its
/// creator's to decide.
pub(super) fn init_mounts(bottle: &Bottle) -> Result<(), String> {
    let paths: Vec<String> = bottle.mounts.iter().map(|m| q(&m.path)).collect();
    let paths = paths.join(" ");
    if !paths.is_empty() {
        exec(
            bottle,
            &sh(&format!("install -d -o {USER} -g {USER} {paths}")),
            Some("root"),
            None,
        )
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
        script.push(format!(
            "echo {} > {path}/.git/objects/info/alternates",
            q(&mount.objects)
        ));
        script.push(format!("git -C {path} {point_head}"));
        script.push(format!("git -C {path} reset -q --hard"));
        let names: Vec<&str> = mount.remotes.iter().map(|r| r.name.as_str()).collect();
        for name in &names {
            script.push(format!(
                "git -C {path} remote add {name} {}",
                q(&mount.url(name))
            ));
        }
        if names.contains(&ORIGIN) {
            script.push(format!(
                "git -C {path} config checkout.defaultRemote {ORIGIN}"
            ));
        }
        if !names.is_empty() {
            script.push(format!(
                "git -C {path} fetch -q --multiple {}",
                names.join(" ")
            ));
        }
    }
    script.push(format!(
        "printf '%s' {} > \"$HOME/{CONTEXT_FILE}\"",
        q(&context(bottle)?)
    ));
    let checked_out = if paths.is_empty() {
        "nothing".to_string()
    } else {
        paths.clone()
    };
    exec(bottle, &sh(&script.join("\n")), Some(USER), None)
        .map(|_| ())
        .map_err(|e| format!("checking out {checked_out} failed: {e}"))
}
