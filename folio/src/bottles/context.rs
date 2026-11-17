//! What a bottle is told of itself: its machine's environment, and its context (BOTTLE.md).

use super::*;

/// What a bottle's machine is created with: the stand-ins its features asked for, never
/// credentials; its permissions, as BOTTLE_PERMISSIONS, so the tools that use them know to offer
/// them; and where a login starts, as BOTTLE_WORKDIR (the base image's profile goes there).
///
/// BOTTLE_PERMISSIONS only says what folio serve would allow: folio serve checks the bottle's spec,
/// not this, so a bottle that sets it itself gains nothing.
///
/// Every exec sees these. SSH sessions only see /etc/environment, so the base image's entrypoint
/// mirrors there the ones BOTTLE_MIRROR_ENV names.
pub(super) fn machine_env(
    features: &[String],
    permissions: &[String],
    workdir: &str,
) -> Result<BTreeMap<String, String>, String> {
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
        let name = Path::new(&mount.repo)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        lines.push(format!(
            "- `{}` is a checkout of `{name}`, {at}.",
            mount.path
        ));
        if mount.remotes.is_empty() {
            lines.push("  It has no remotes.".into());
        } else {
            lines.push("  Its remotes are served by the host:".into());
            lines.extend(
                mount
                    .remotes
                    .iter()
                    .map(|r| format!("  - `{}`: {}", r.name, describe_remote(r))),
            );
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
        lines.extend(
            told.iter()
                .map(|(id, text)| format!("\n### Feature '{id}'\n\n{text}")),
        );
    }
    let text = lines.join("\n") + "\n";
    Ok(if bottle.context.is_empty() {
        text
    } else {
        format!("{text}\n{}", bottle.context)
    })
}

pub(super) fn describe_remote(remote: &Remote) -> String {
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
            format!(
                "{what} You can push to it: pushes can't rewrite or delete what's already there.{echo}"
            )
        }
    }
}
