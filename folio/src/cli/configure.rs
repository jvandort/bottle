//! configure and sidecar: a repo's spec and runner, a thread's settings, its sidecar.

use super::*;

pub(super) fn changes_spec(matches: &ArgMatches) -> bool {
    !args(matches, "feature").is_empty()
        || !args(matches, "no-feature").is_empty()
        || arg(matches, "memory").is_some()
        || flag(matches, "no-memory")
        || arg(matches, "disk").is_some()
        || flag(matches, "no-disk")
}

/// Whether a configure command was given anything to change: else it shows.
pub(super) fn changes(matches: &ArgMatches) -> bool {
    changes_spec(matches) || arg(matches, "runner").is_some() || flag(matches, "no-runner")
}

/// `spec` as a configure command changes it: features added (one there already gets the options
/// given) or taken away, memory or disk set, or the disk the default again.
pub(super) fn changed(spec: &Spec, matches: &ArgMatches, command: &str) -> Result<Spec, String> {
    let mut spec = spec.clone();
    let (given, taken) = feature_args(matches, command);
    let features = repos::merge_features(&spec.features, &given);
    let there: Vec<&str> = features.iter().map(|f| repos::name_of(f)).collect();
    for name in &taken {
        if !there.contains(&name.as_str()) {
            return Err(format!(
                "there's no feature {name} to take away (features: {})",
                listed(&there)
            ));
        }
    }
    let kept: Vec<String> = features
        .iter()
        .filter(|f| !taken.iter().any(|t| t == repos::name_of(f)))
        .cloned()
        .collect();
    spec.features = kept;
    if let Some(memory) = arg(matches, "memory") {
        spec.memory = memory.into();
    }
    if flag(matches, "no-memory") {
        spec.memory = repos::DEFAULT_MEMORY.into();
    }
    if let Some(disk) = arg(matches, "disk") {
        spec.disk = Some(disk.into());
    }
    if flag(matches, "no-disk") {
        spec.disk = None;
    }
    Ok(spec)
}

/// `names`, as an error lists them.
pub(super) fn listed(names: &[&str]) -> String {
    if names.is_empty() {
        "none".to_string()
    } else {
        names.join(", ")
    }
}

/// A configure command's features to add, and names to take away, refused as a usage error if one
/// to take away has options, or one's both.
pub(super) fn feature_args(matches: &ArgMatches, command: &str) -> (Vec<String>, Vec<String>) {
    let (given, taken) = (args(matches, "feature"), args(matches, "no-feature"));
    for name in &taken {
        if name.contains(':') || name.contains('=') {
            usage_error(
                command,
                &format!(
                    "--no-feature takes a feature's name alone, e.g. {}",
                    repos::name_of(name)
                ),
            );
        }
    }
    let mut both: Vec<&str> = given
        .iter()
        .map(|g| repos::name_of(g))
        .filter(|n| taken.iter().any(|t| t == n))
        .collect();
    both.sort();
    both.dedup();
    if !both.is_empty() {
        usage_error(
            command,
            &format!("--feature and --no-feature both name {}", both.join(", ")),
        );
    }
    (given, taken)
}

/// A configure command's thread, refused unless it comes first, before any option: a word after a
/// flag that takes none (`--no-disk 50G`) isn't taken for one.
pub(super) fn thread_first(matches: &ArgMatches, command: &str) {
    let Some(at) = matches.index_of("thread") else {
        return;
    };
    let options = [
        "feature",
        "no-feature",
        "memory",
        "no-memory",
        "disk",
        "no-disk",
        "runner",
        "no-runner",
    ];
    let first = options
        .iter()
        .filter(|id| matches.value_source(id) == Some(clap::parser::ValueSource::CommandLine))
        .filter_map(|id| matches.index_of(id))
        .min();
    if first.is_some_and(|first| first < at) {
        usage_error(
            command,
            &format!(
                "unexpected argument '{}': the thread comes first, `folio {command} THREAD [OPTIONS]`",
                arg(matches, "thread").unwrap_or_default()
            ),
        );
    }
}

pub(super) fn configure(engine: &Engine, matches: &ArgMatches) -> Outcome {
    thread_first(matches, "configure");
    let repo = current()?;
    if let Some(name) = arg(matches, "thread") {
        return thread_configure(engine, matches, &repo, &threads::load(&repo, name)?);
    }
    if !changes(matches) {
        let shown = match repo.spec()? {
            Some(spec) => spec_text(&spec),
            None => format!(
                "{} (the default; nothing set yet)",
                spec_text(&repos::default_spec())
            ),
        };
        println!("{shown}{}", runner_text(&repo)?);
        return Ok(0);
    }
    let mut spec = Spec::default();
    if changes_spec(matches) {
        // Checked before anything's saved, the runner included.
        let current = repo.spec()?.unwrap_or_else(repos::default_spec);
        spec = engine::validate_spec(&repos::check_repo_spec(
            changed(&current, matches, "configure")?,
            false,
        )?)?;
    }
    if arg(matches, "runner").is_some() || flag(matches, "no-runner") {
        repo.save_runner(arg(matches, "runner"))?;
    }
    if changes_spec(matches) {
        repo.save_spec(&spec)?;
        println!(
            "Set {}'s spec: {}{}",
            repo.name,
            spec_text(&spec),
            runner_text(&repo)?
        );
    } else {
        println!(
            "Set {}'s runner: {}",
            repo.name,
            repo.runner()?.unwrap_or_else(|| "this machine".into())
        );
    }
    Ok(0)
}

/// `current`, a thread's settings or its sidecar's, as a configure command changes them: features
/// added or given other options, or ones of its own taken away; its own memory, disk or runner
/// given, or taken away.
pub(super) fn settings_changed(
    current: &Settings,
    matches: &ArgMatches,
    command: &str,
    whose: &str,
) -> Result<Settings, String> {
    let (given, taken) = feature_args(matches, command);
    let own: Vec<&str> = current.features.iter().map(|f| repos::name_of(f)).collect();
    for name in &taken {
        if !own.contains(&name.as_str()) {
            return Err(if command == "configure" {
                format!(
                    "{whose} doesn't override feature {name} (overridden: {}); one the repo gives can't be taken away",
                    listed(&own)
                )
            } else {
                format!(
                    "{whose} has no feature {name} to take away (features: {})",
                    listed(&own)
                )
            });
        }
    }
    let kept: Vec<String> = current
        .features
        .iter()
        .filter(|f| !taken.iter().any(|t| t == repos::name_of(f)))
        .cloned()
        .collect();
    let given_or = |value: &str, none: &str, now: &Option<String>| match arg(matches, value) {
        Some(v) => Some(v.to_string()),
        None if flag(matches, none) => None,
        None => now.clone(),
    };
    Ok(Settings {
        features: repos::merge_features(&kept, &given),
        memory: given_or("memory", "no-memory", &current.memory),
        disk: given_or("disk", "no-disk", &current.disk),
        runner: given_or("runner", "no-runner", &current.runner),
    })
}

/// A thread's settings as configure shows them: the bottle it gets next, and what of that it
/// overrides of the repo's.
pub(super) fn thread_text(repo: &Repo, settings: &Settings) -> Result<String, String> {
    let spec = settings.spec(repo)?;
    let own = |is: bool| if is { " (overridden)" } else { "" };
    let features: Vec<String> = spec
        .features
        .iter()
        .map(|f| {
            let mine = own(settings.features.contains(f));
            format!("{f}{mine}")
        })
        .collect();
    let disk = spec
        .disk
        .as_ref()
        .map(|d| format!(", disk {d}{}", own(settings.disk.is_some())))
        .unwrap_or_default();
    let runner = match settings.runner(repo)? {
        Some(runner) if runner != runners::LOCAL => runner,
        _ => "this machine".into(),
    };
    Ok(format!(
        "features [{}], memory {}{}{disk}, runner {runner}{}",
        features.join(", "),
        spec.memory,
        own(settings.memory.is_some()),
        own(settings.runner.is_some())
    ))
}

/// A sidecar as configure shows it.
pub(super) fn sidecar_text(sidecar: &Settings) -> String {
    format!(
        "{}, runner {}",
        spec_text(&sidecar.over(&threads::sidecar_base())),
        sidecar.runner.as_deref().unwrap_or_default()
    )
}

pub(super) fn thread_configure(
    engine: &Engine,
    matches: &ArgMatches,
    repo: &Repo,
    thread: &Thread,
) -> Outcome {
    if !changes(matches) {
        let state = threads::states(engine, repo, std::slice::from_ref(thread))?[&thread.name]
            .status
            .state;
        println!(
            "{}; thread {}, {state}",
            thread_text(repo, &thread.settings)?,
            thread.name
        );
        return Ok(0);
    }
    let whose = format!("thread {}", thread.name);
    let settings = settings_changed(&thread.settings, matches, "configure", &whose)?;
    let thread = threads::set_settings(engine, repo, thread, settings)?;
    println!(
        "Set thread {}: {}{}",
        thread.name,
        thread_text(repo, &thread.settings)?,
        when(engine, repo, &thread)?
    );
    Ok(0)
}

pub(super) fn sidecar_configure(engine: &Engine, matches: &ArgMatches) -> Outcome {
    thread_first(matches, "sidecar configure");
    let repo = current()?;
    let Some(thread) = thread_of(engine, matches, &repo, "sidecar configure", false)? else {
        return Ok(0);
    };
    if !changes(matches) {
        let Some(sidecar) = &thread.sidecar else {
            println!("Thread {} has no sidecar", thread.name);
            return Ok(0);
        };
        let name = thread.sidecar_bottle();
        let state = engine.status(std::slice::from_ref(&name))?[&name].state;
        println!("{}; bottle {name}, {state}", sidecar_text(sidecar));
        return Ok(0);
    }
    if flag(matches, "no-runner") {
        return Err(format!(
            "a sidecar is on a runner of its own; `folio sidecar remove {}` takes it away",
            thread.name
        ));
    }
    if thread.sidecar.is_none() && arg(matches, "runner").is_none() {
        return Err(format!(
            "thread {name} has no sidecar; give it one with `folio sidecar configure {name} --runner RUNNER ...`",
            name = thread.name
        ));
    }
    let current = thread.sidecar.clone().unwrap_or_default();
    let sidecar = settings_changed(&current, matches, "sidecar configure", "the sidecar")?;
    let thread = threads::set_sidecar(&repo, &thread, Some(sidecar))?;
    println!(
        "Set thread {}'s sidecar: {}{}",
        thread.name,
        sidecar_text(thread.sidecar.as_ref().unwrap()),
        when(engine, &repo, &thread)?
    );
    Ok(0)
}

pub(super) fn sidecar_remove(engine: &Engine, matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let Some(thread) = thread_of(engine, matches, &repo, "sidecar remove", false)? else {
        return Ok(0);
    };
    if thread.sidecar.is_none() {
        println!("Thread {} has no sidecar", thread.name);
        return Ok(0);
    }
    let thread = threads::set_sidecar(&repo, &thread, None)?;
    println!(
        "Took thread {}'s sidecar away{}",
        thread.name,
        when(engine, &repo, &thread)?
    );
    Ok(0)
}

pub(super) fn sidecar_shell(engine: &Engine, matches: &ArgMatches) -> Outcome {
    let repo = current()?;
    let Some(thread) = thread_of(engine, matches, &repo, "sidecar shell", false)? else {
        return Ok(0);
    };
    match threads::sidecar_shell(engine, &repo, &thread)? {}
}

/// When a change to it or its sidecar reaches the thread: at once, for one with no bottle.
pub(super) fn when(engine: &Engine, repo: &Repo, thread: &Thread) -> Result<String, String> {
    let state = threads::states(engine, repo, std::slice::from_ref(thread))?[&thread.name]
        .status
        .state;
    if matches!(state, State::Paused | State::Missing) {
        return Ok(String::new());
    }
    Ok(format!(
        ". It comes with the thread's next bottle: `folio pause {name}`, then `folio claude {name}`",
        name = thread.name
    ))
}

pub(super) fn runner_text(repo: &Repo) -> Result<String, String> {
    Ok(repo
        .runner()?
        .map(|r| format!(", runner {r}"))
        .unwrap_or_default())
}

pub(super) fn spec_text(spec: &Spec) -> String {
    let disk = spec
        .disk
        .as_ref()
        .map(|d| format!(", disk {d}"))
        .unwrap_or_default();
    format!(
        "features [{}], memory {}{disk}",
        spec.features.join(", "),
        spec.memory
    )
}
