//! A bottle's machine, once made: started (with its contract checked), commands run in it, and deleted.

use super::*;

/// What bottle relies on inside every bottle, checked (as genie) whenever one starts. Documented in
/// folio/features/base/install.sh; keep them in sync.
pub(super) const CONTRACT: [(&str, &str); 6] = [
    (
        "bottle-entrypoint is installed",
        "test -x /usr/local/bin/bottle-entrypoint",
    ),
    (
        "the genie user has UID 1000",
        "[ \"$(id -un)\" = genie ] && [ \"$(id -u)\" = 1000 ]",
    ),
    ("genie has passwordless sudo", "sudo -n true"),
    ("git is installed", "command -v git"),
    ("sshd is installed", "test -x /usr/sbin/sshd"),
    (
        "update-ca-certificates is installed",
        "test -x /usr/sbin/update-ca-certificates",
    ),
];

pub(super) fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

/// Start the bottle's machine, folio serve answering what it routes there as this bottle's, its
/// credentials read now. bottle-cli makes the machine's caching CA as it starts, for its policy's
/// hosts, and gives it to the bottle to trust (the base image's entrypoint does).
pub(super) fn start_machine(known: &Known, bottle: &Bottle) -> Result<(), String> {
    known.serve.bind(binding(bottle)?)?;
    runner_of(bottle)?.start(bottle.machine())
}

/// The policy the bottle's machine decides its connections by (services::network): what its
/// features cache, and what it routes to folio serve: its services, ssh to bottles, and its
/// credentials' hosts, logged in or not.
pub(super) fn network(bottle: &Bottle) -> Result<Network, String> {
    let resolved = features::resolve(&bottle.features)?;
    Ok(services::network(
        &auth::credential_destinations(&bottle.features)?,
        images::cached_for(&resolved),
    ))
}

/// A machine's memory in MiB, from its spec's size (8G, 512M). It takes it as the guest touches it.
pub(super) fn memory_mib(memory: Option<&str>) -> Result<u64, String> {
    let given = memory.unwrap_or_default().trim().to_uppercase();
    let refused = || {
        format!(
            "a bottle needs its memory, a size in M or G, e.g. \"8G\" (not {})",
            memory.map_or("none".to_string(), crate::quoted)
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

/// Give the bottle the names of folio serve's services, at the address whose flows its machine
/// routes there by name, and the name of each bottle it may ssh to, at its address.
pub(super) fn point_at_host(bottle: &Bottle) -> Result<(), String> {
    let mut entries: Vec<String> = services::NAMES
        .iter()
        .map(|name| format!("{HOST_ADDRESS} {name}"))
        .collect();
    entries.extend(
        bottle
            .ssh
            .iter()
            .map(|g| format!("{} {}", g.address, g.name)),
    );
    let script: Vec<String> = entries
        .iter()
        .map(|e| {
            format!(
                "grep -qsx {e} /etc/hosts || echo {e} >> /etc/hosts",
                e = q(e)
            )
        })
        .collect();
    exec(bottle, &sh(&script.join("; ")), Some("root"), None)
        .map(|_| ())
        .map_err(|e| {
            format!(
                "giving {} folio serve's names failed: {e}",
                bottle.machine()
            )
        })
}

/// Point each of the bottle's repos' remotes where they are now (Mount::url): for one made when
/// they were elsewhere.
pub(super) fn point_remotes(bottle: &Bottle) -> Result<(), String> {
    let script: Vec<String> = bottle
        .mounts
        .iter()
        .flat_map(|m| {
            m.remotes.iter().map(move |r| {
                format!(
                    "git -C {} remote set-url {} {}",
                    q(&m.path),
                    q(&r.name),
                    q(&m.url(&r.name))
                )
            })
        })
        .collect();
    if script.is_empty() {
        return Ok(());
    }
    in_bottle(bottle, &sh(&script.join(" && ")))
        .map(|_| ())
        .map_err(|e| format!("pointing {}'s remotes failed: {e}", bottle.name))
}

/// Run a command in the bottle's running machine and return its output.
pub(super) fn exec(
    bottle: &Bottle,
    argv: &[String],
    user: Option<&str>,
    input: Option<&str>,
) -> Result<String, String> {
    runner_of(bottle)?.exec(bottle.machine(), argv, user, input)
}

pub(super) fn in_bottle(bottle: &Bottle, argv: &[String]) -> Result<String, String> {
    Ok(exec(bottle, argv, Some(USER), None)?.trim().to_string())
}

pub fn start(known: &Known, name: &str) -> Result<Bottle, String> {
    auth::warn_unset(&get(name)?.features)?;
    ensure_running(known, name)
}

/// Start the bottle's machine if it isn't running, folio serve answering it; one `known` running
/// isn't asked again.
pub fn ensure_running(known: &Known, name: &str) -> Result<Bottle, String> {
    let mut bottle = get(name)?;
    if known.running(name) {
        // Running: folio serve, if it had stopped, binds it again as it starts.
        known.serve.start()?;
        return Ok(bottle);
    }
    if bottle.status != Made::Ready {
        return Err(format!(
            "{name} is {}; delete its thread to remove it (`folio delete`)",
            bottle.status
        ));
    }
    let runner = runner_of(&bottle)?;
    let Some(state) = runner.state(bottle.machine())? else {
        return Err(format!(
            "{}'s machine is gone; delete its thread (`folio delete`)",
            bottle.name
        ));
    };
    if state != listing::State::Running {
        let given = bottle.network_version < NETWORK;
        if given {
            // Made with another policy than this one: given it now.
            runner.configure(bottle.machine(), &network(&bottle)?)?;
            bottle.network_version = NETWORK;
            save(&bottle, None)?;
        }
        start_machine(known, &bottle)?;
        crate::log::log!("bottles", "{}: started", bottle.name);
        if given {
            // What's set up in it for its policy, again; one made with this policy has all it needs
            // on its disk, as bottle-cli's start alone would find it.
            verify_contract(&bottle)?;
            point_at_host(&bottle)?;
            point_remotes(&bottle)?;
        }
    } else {
        // Running: folio serve, if it had stopped, binds it again as it starts.
        known.serve.start()?;
    }
    known.started(name);
    Ok(bottle)
}

/// Fail loudly unless the running bottle provides everything bottle relies on (CONTRACT).
pub(super) fn verify_contract(bottle: &Bottle) -> Result<(), String> {
    let checks: Vec<String> = CONTRACT
        .iter()
        .map(|(what, test)| format!("sh -c {} >/dev/null 2>&1 || echo {}", q(test), q(what)))
        .collect();
    let missing = in_bottle(
        bottle,
        &sh(&format!("# bottle contract\n{}", checks.join("\n"))),
    )
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

/// The command line, for this machine, that runs `argv` in the bottle as genie, at its first mount:
/// started if it's stopped, logged in first to what its features need.
pub fn exec_command(
    known: &Known,
    name: &str,
    argv: &[String],
    tty: bool,
) -> Result<Vec<String>, String> {
    auth::warn_unset(&get(name)?.features)?;
    let bottle = ensure_running(known, name)?;
    let how = Exec {
        user: Some(USER),
        workdir: Some(bottle.workdir()),
        tty,
        ..Default::default()
    };
    Ok(runner_of(&bottle)?.exec_command(bottle.machine(), argv, &how))
}

/// Remove every part of the bottle that exists, then its record, and any images gone stale.
///
/// Each step tolerates its part being absent, so this also finishes off a bottle left half-made or
/// half-deleted. If a step fails, the record stays (marked broken) so the next delete can retry.
pub fn delete(known: &Known, name: &str) -> Result<(), String> {
    known.forget(name);
    let mut bottle = get(name)?;
    // The registry says the machine is the bottle's: a machine has no labels to ask.
    let runner = runner_of(&bottle)?;
    let mut failures = Vec::new();
    if let Err(e) = known.serve.release(&bottle.name) {
        failures.push(format!("egress: {e}"));
    }
    if let Err(e) = runner.delete(bottle.machine()) {
        failures.push(format!("machine: {e}"));
    }
    if !failures.is_empty() {
        crate::log::say!(
            "bottles",
            "{name}: deleting left parts behind: {}",
            failures.join("; ")
        );
        bottle.status = Made::Broken;
        save(&bottle, None)?;
        return Err(format!(
            "couldn't fully delete {name} ({}); try again, and what's left is deleted",
            failures.join("; ")
        ));
    }
    forget(name);
    crate::log::log!("bottles", "{name}: deleted");
    // Images whose features or base have changed since they were built, if no bottle uses one. An
    // image that's still current is kept, even with nothing using it: it's a cache, and the next
    // bottle with those features starts without a build. Best effort: the bottle is already gone.
    if let Err(e) = builds::remove_stale(&runner) {
        crate::log::say!("bottles", "folio: couldn't remove stale images: {e}");
    }
    Ok(())
}
