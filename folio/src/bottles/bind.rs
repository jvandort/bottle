//! What folio serve is told of a bottle, to answer it.

use std::path::{Path, PathBuf};

use crate::control::{self, Bind, Served};
use crate::repos::{self, Repo};
use crate::runners;

use super::record::Made;
use super::{Bottle, Mount, load};

/// The git dir of a mount's repo, which its remotes are served from; None if it's gone.
pub fn git_dir(mount: &Mount) -> Option<PathBuf> {
    repos::objects_dir(&Repo::new("", &mount.repo))
        .ok()
        .and_then(|o| o.parent().map(Path::to_path_buf))
}

/// What folio serve is told of the bottle: all it needs of it, and on a runner, what to forward
/// there with it.
pub(super) fn binding(bottle: &Bottle) -> Result<Bind, String> {
    let forwards = if bottle.runner == runners::LOCAL {
        vec![]
    } else {
        runners::get(&bottle.runner)?.forwards()?
    };
    Ok(Bind {
        bottle: bottle.name.clone(),
        machine: bottle.machine().into(),
        runner: bottle.runner.clone(),
        id: bottle.id.clone(),
        repos: bottle
            .mounts
            .iter()
            .map(|m| Served {
                git_dir: git_dir(m),
                remotes: m.remotes.clone(),
                at: m.at.clone(),
            })
            .collect(),
        features: bottle.features.clone(),
        permissions: bottle.permissions(),
        labels: bottle.spec.labels.clone(),
        repo: bottle.mounts.first().map(|m| {
            Path::new(&m.repo)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        }),
        ssh: bottle
            .ssh
            .iter()
            .map(|g| control::Grant {
                name: g.name.clone(),
                bottle: g.bottle.clone(),
                id: g.id.clone(),
                address: g.address.clone(),
            })
            .collect(),
        forwards,
    })
}

/// What folio serve is told of every ready bottle as it starts (daemon::Restore), to answer them
/// before anyone else: wherever they run, but on a runner detached (turned off).
pub fn bindings() -> Result<Vec<Bind>, String> {
    let detached = runners::registered()?;
    Ok(load()?
        .values()
        .filter(|bottle| {
            let off = bottle.runner != runners::LOCAL
                && detached.get(&bottle.runner).copied().unwrap_or(false);
            bottle.status == Made::Ready && !off
        })
        .filter_map(|bottle| {
            binding(bottle)
                .inspect_err(|e| {
                    crate::log::say!(
                        "bottles",
                        "{}: folio serve won't answer it: {e}",
                        bottle.name
                    )
                })
                .ok()
        })
        .collect())
}
