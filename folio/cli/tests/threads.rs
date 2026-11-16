//! Threads: started, attached to, listed, deleted and configured, as a person does each, on the
//! stand-in bottle-cli.

mod harness;

use std::fs;

use harness::{ATTACH, Folio, argv, current_slot, git_in, session, session_command};
use serde_json::{Value, json};

/// A record's mount's remotes, as specs: NAME:fetch=...[,push=...].
fn remote_specs(mount: &Value) -> Vec<String> {
    mount["remotes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let mut spec = format!(
                "{}:fetch={}",
                r["name"].as_str().unwrap(),
                r["fetch"].as_str().unwrap()
            );
            if let Some(push) = r["push"].as_str() {
                spec += &format!(",push={push}");
            }
            spec
        })
        .collect()
}

fn started() -> Folio {
    let f = Folio::new();
    f.ok(&["new", "fix-it"]);
    f
}

// --- new ---------------------------------------------------------------------------

#[test]
fn new_makes_the_bottle_and_lane_and_runs_nothing_in_it() {
    let f = Folio::new();
    let out = f.ok(&["new", "fix-it"]);
    assert!(out.contains("Started thread fix-it"), "{out}");
    assert!(
        out.contains("Work in it with: folio claude fix-it"),
        "{out}"
    );
    let record = f.record("fix-it");
    let mounts = record["mounts"].as_array().unwrap();
    let (workspace, notes) = (&mounts[0], &mounts[1]);
    assert_eq!(notes["path"], "/notes");
    assert_eq!(notes["repo"], json!(f.thread_dir("fix-it").join("notes")));
    assert_eq!(
        (&workspace["path"], &workspace["repo"]),
        (&json!("/workspace"), &json!(f.repo))
    );
    assert_eq!(
        remote_specs(workspace),
        [
            "origin:fetch=upstream",
            "host:fetch=heads",
            "agent:fetch=ns/fix-it-out,push=ns/fix-it-out",
            "user:fetch=ns/fix-it-in"
        ]
    );
    assert_eq!(workspace["branch"], "main");
    assert_eq!(f.machine("fix-it")["sessions"], json!({}));
    // Nothing but what makes the bottle.
    assert!(
        f.execs("fix-it")
            .iter()
            .all(|e| !e["step"].as_str().unwrap().is_empty()),
        "{:?}",
        f.ran("fix-it")
    );
    let context = record["context"].as_str().unwrap();
    for told in [
        "# Thread `fix-it`",
        "genie/fix-it-1",
        "git fetch bottle/fix-it && git switch genie/<feature>-N",
        "`git push -u agent <branch>`",
        "**Fetch `user`** when you get a prompt",
        "this thread's first is\n`genie/fix-it-1`",
        "**Push as you go**, without being asked",
        "`folio cp fix-it:<path> .`",
    ] {
        assert!(context.contains(told), "{told} in {context}");
    }
    assert!(!context.contains("work`"));
    assert_eq!(
        git_in(&f.repo, &["config", "remote.bottle/fix-it.uploadpack"]),
        "git --namespace=fix-it-out upload-pack"
    );
}

#[test]
fn new_from_a_branch() {
    let f = Folio::new();
    git_in(&f.repo, &["branch", "release"]);
    f.ok(&["new", "fix-it", "--from", "release"]);
    assert_eq!(f.record("fix-it")["mounts"][0]["branch"], "release");
}

#[test]
fn the_repos_spec_is_checked_and_saved_first() {
    let f = Folio::new();
    f.ok(&["new", "fix-it"]);
    assert_eq!(
        f.repo_spec(),
        Some(json!({"features": ["tools"], "memory": "2G"}))
    );
    assert_eq!(
        f.record("fix-it")["spec"],
        json!({"features": ["tools"], "memory": "2G", "labels": {"thread": "fix-it"}})
    );
}

#[test]
fn new_refuses_a_bad_name_or_a_taken_one() {
    let f = Folio::new();
    for name in ["Fix", "fix_it", "fix-", "fix--it", "fix/it"] {
        let out = f.folio(&["new", name]);
        assert_eq!(out.code, 1, "{name}");
        assert!(
            out.stderr.contains("invalid thread name"),
            "{name}: {}",
            out.stderr
        );
    }
    f.ok(&["new", "fix-it"]);
    assert!(
        f.folio(&["new", "fix-it"])
            .stderr
            .contains("thread fix-it already exists")
    );
}

#[test]
fn a_failed_bottle_leaves_no_thread() {
    let f = Folio::new();
    let out = f.with(&["new", "fix-it"], &[("FAKE_CLI_FAIL", "create")]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("creating failed"), "{}", out.stderr);
    assert_eq!(git_in(&f.repo, &["remote"]), "");
    assert!(current_slot(&f.thread_dir("fix-it")).is_none());
}

#[test]
fn the_same_thread_name_in_two_repos_gets_two_bottles() {
    let f = Folio::new();
    let other = f.make_repo("elsewhere/project");
    f.ok(&["new", "fix-it"]);
    f.ok_in(&other, &["new", "fix-it"]);
    assert_eq!(f.cli()["machines"].as_object().unwrap().len(), 2);
}

// --- attached to -------------------------------------------------------------------

/// Whether `ran` started a session in the background.
fn starts_a_session(ran: &[String]) -> bool {
    ran.get(3..5) == Some(&argv(&["attach", "--background"])[..])
}

fn attach_argv(session: &str) -> Vec<String> {
    argv(&[
        "sh",
        "-c",
        ATTACH,
        "attach",
        session,
        &session_command(session),
        "/workspace",
    ])
}

#[test]
fn claude_starts_the_session_once_and_attaches() {
    let f = started();
    f.ok(&["claude", "fix-it"]);
    assert_eq!(
        f.machine("fix-it")["sessions"],
        json!({"claude": session("claude")})
    );
    assert_eq!(f.ran("fix-it").last().unwrap(), &attach_argv("claude"));
    f.ok(&["claude", "fix-it"]);
    // Reattached, not started again.
    assert_eq!(
        f.ran("fix-it")
            .iter()
            .filter(|r| starts_a_session(r))
            .count(),
        1
    );
}

#[test]
fn the_attach_can_start_its_session_again() {
    // With the command the session was started with, for a session shpool can no longer show.
    let f = started();
    f.ok(&["claude", "fix-it"]);
    let ran = f.ran("fix-it");
    let last = ran.last().unwrap();
    assert_eq!(last[last.len() - 2], session_command("claude"));
    let started = ran.iter().find(|r| starts_a_session(r)).unwrap();
    assert_eq!(
        &started[started.len() - 3..],
        argv(&["--cmd", &last[last.len() - 2], "claude"])
    );
}

#[test]
fn a_shell_is_a_session_of_its_own() {
    let f = started();
    f.ok(&["shell", "fix-it"]);
    assert_eq!(f.machine("fix-it")["sessions"]["shell"], "exec bash -l");
    let ran = f.ran("fix-it");
    let last = ran.last().unwrap();
    assert_eq!(last[last.len() - 3], "shell");
}

#[test]
fn a_session_whose_program_exited_is_started_again() {
    // shpool still lists it, and attaching would get a plain shell instead.
    let f = started();
    f.set_machine(
        "fix-it",
        json!({"sessions": {"claude": session("claude")}, "exited": ["claude"]}),
    );
    f.ok(&["claude", "fix-it"]);
    assert!(
        f.ran("fix-it")
            .contains(&argv(&["shpool", "kill", "claude"]))
    );
    assert_eq!(
        f.ran("fix-it")
            .iter()
            .filter(|r| starts_a_session(r))
            .count(),
        1
    );
    assert_eq!(f.machine("fix-it")["exited"], json!([]));
}

#[test]
fn a_stopped_bottle_is_started_and_claude_started_again() {
    let f = started();
    // The VM stopped, and its processes with it.
    f.set_machine("fix-it", json!({"state": "stopped", "sessions": {}}));
    let before = f.calls("start").len();
    f.ok(&["claude", "fix-it"]);
    assert_eq!(
        f.calls("start")[before..],
        [argv(&["start", &f.bottle("fix-it")])]
    );
    assert_eq!(
        f.machine("fix-it")["sessions"],
        json!({"claude": session("claude")})
    );
}

#[test]
fn a_missing_bottle_says_where_the_branches_are() {
    let f = started();
    let mut state = f.cli();
    state["machines"]
        .as_object_mut()
        .unwrap()
        .remove(&f.bottle("fix-it"));
    f.set_cli(&state);
    let out = f.folio(&["claude", "fix-it"]);
    assert_eq!(out.code, 1);
    for said in [
        "is gone, and with it everything that wasn't pushed",
        "What was pushed is in your repo: `git fetch bottle/fix-it`",
        "`folio delete fix-it` removes what's left of the thread",
    ] {
        assert!(out.stderr.contains(said), "{said} in {}", out.stderr);
    }
}

#[test]
fn an_unknown_thread_is_started_and_attached_to() {
    let f = started();
    for session in ["claude", "shell"] {
        let name = format!("new-{session}");
        assert!(
            f.ok(&[session, &name])
                .contains(&format!("Started thread {name}"))
        );
        assert_eq!(f.ran(&name).last().unwrap(), &attach_argv(session));
    }
    assert!(git_in(&f.repo, &["remote"]).contains("new-claude"));
}

#[test]
fn an_unknown_thread_with_a_bad_name_starts_nothing() {
    let f = started();
    let out = f.folio(&["claude", "Nope"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("invalid thread name 'Nope'"),
        "{}",
        out.stderr
    );
    assert_eq!(f.cli()["machines"].as_object().unwrap().len(), 1);
}

#[test]
fn an_unknown_thread_elsewhere() {
    let f = started();
    let out = f.folio(&["pause", "nope"]);
    assert!(
        out.stderr.contains("no thread named 'nope'"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("(threads: fix-it)"), "{}", out.stderr);
}

#[test]
fn attaching_makes_it_most_recently_active_and_leaves_its_configuration() {
    let f = started();
    f.ok(&["new", "other"]);
    let recent = |f: &Folio| -> Vec<String> {
        f.ok(&["list"])
            .lines()
            .skip(1)
            .map(|l| l.split_whitespace().next().unwrap().to_string())
            .collect()
    };
    assert_eq!(recent(&f), ["other", "fix-it"]);
    let slot = current_slot(&f.thread_dir("fix-it"));
    f.ok(&["claude", "fix-it"]);
    assert_eq!(recent(&f), ["fix-it", "other"]);
    // thread.json not written.
    assert_eq!(current_slot(&f.thread_dir("fix-it")), slot);
}

#[test]
fn list() {
    let f = started();
    git_in(
        &f.repo,
        &[
            "update-ref",
            "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1",
            "HEAD",
        ],
    );
    let out = f.ok(&["list"]);
    let lines: Vec<Vec<&str>> = out
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(
        lines[0],
        [
            "THREAD", "STATE", "RUNNER", "SIDECAR", "SNAPSHOT", "ACTIVE", "FEATURES"
        ]
    );
    // Its branches aren't listed: a thread can have a great many.
    assert_eq!(
        lines[1],
        [
            "fix-it", "running", "local", "-", "-", "just", "now", "tools"
        ]
    );
    assert_eq!(f.ok(&["ls"]), out);
    assert_eq!(f.ok(&["status"]), out);
}

// --- deleted -----------------------------------------------------------------------

/// `folio delete fix-it` on a terminal, `typed` typed when it asks for the name.
fn delete_typing(f: &Folio, typed: &[u8]) -> (i32, String) {
    delete_typing_with(f, typed, &[])
}

fn delete_typing_with(f: &Folio, typed: &[u8], env: &[(&str, &str)]) -> (i32, String) {
    f.on_terminal_in(
        None,
        &["delete", "fix-it"],
        &[(b"Type fix-it to delete it: ", typed)],
        env,
    )
}

#[test]
fn yes_deletes_the_bottle_lane_remote_and_record() {
    let f = started();
    git_in(
        &f.repo,
        &[
            "update-ref",
            "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1",
            "HEAD",
        ],
    );
    f.ok(&["delete", "fix-it", "--yes"]);
    assert_eq!(f.cli()["machines"], json!({}));
    assert_eq!(git_in(&f.repo, &["for-each-ref", "refs/namespaces"]), "");
    assert_eq!(git_in(&f.repo, &["remote"]), "");
    // The record and snapshot are gone; the notes stay, for a thread of the same name.
    let left: Vec<String> = fs::read_dir(f.thread_dir("fix-it"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["notes"]);
}

#[test]
fn delete_without_a_terminal_needs_yes() {
    let f = started();
    let out = f.folio(&["delete", "fix-it"]);
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("needs --yes"), "{}", out.stderr);
    assert!(f.cli()["machines"].get(f.bottle("fix-it")).is_some());
}

#[test]
fn delete_says_what_would_be_lost_and_asks_for_the_name() {
    let f = started();
    let head = git_in(&f.repo, &["rev-parse", "HEAD"]);
    let tree = git_in(&f.repo, &["rev-parse", "HEAD^{tree}"]);
    let pushed = f.git(&[
        "-C",
        &f.repo.display().to_string(),
        "commit-tree",
        &tree,
        "-p",
        &head,
        "-m",
        "pushed",
    ]);
    git_in(
        &f.repo,
        &[
            "update-ref",
            "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-2",
            &pushed,
        ],
    );
    let workspace = f.workspace("fix-it");
    git_in(&workspace, &["switch", "-q", "-c", "genie/fix-it-3"]);
    f.commit(&workspace, "never pushed");
    fs::write(workspace.join("notes.txt"), "draft").unwrap();
    let (code, out) = delete_typing(&f, b"fix-it-not\r");
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("genie/fix-it-2: pushed, but not on your branches or origin's"),
        "{out}"
    );
    assert!(
        out.contains("genie/fix-it-3: in the bottle, never pushed"),
        "{out}"
    );
    assert!(out.contains("uncommitted changes in the bottle"), "{out}");
    assert!(!out.contains("main:"), "{out}");
    assert!(out.contains("Not deleted."), "{out}");
    assert!(f.cli()["machines"].get(f.bottle("fix-it")).is_some());
    let (code, out) = delete_typing(&f, b"fix-it\r");
    assert_eq!(code, 0, "{out}");
    assert_eq!(f.cli()["machines"], json!({}));
}

#[test]
fn no_answer_deletes_nothing() {
    let f = started();
    // The end of input; Ctrl-C.
    for answer in [b"\x04", b"\x03"] {
        let (code, out) = delete_typing(&f, answer);
        assert_eq!(code, 1, "{answer:?}: {out}");
        assert!(out.contains("Not deleted."), "{out}");
        assert!(f.cli()["machines"].get(f.bottle("fix-it")).is_some());
    }
}

#[test]
fn nothing_to_lose() {
    let f = started();
    let (code, out) = delete_typing(&f, b"fix-it\r");
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("loses nothing"), "{out}");
}

#[test]
fn a_stopped_bottle_is_started_to_look() {
    let f = started();
    f.set_machine("fix-it", json!({"state": "stopped"}));
    let before = f.calls("start").len();
    delete_typing(&f, b"fix-it\r");
    assert_eq!(
        f.calls("start")[before..],
        [argv(&["start", &f.bottle("fix-it")])]
    );
}

#[test]
fn a_bottle_that_cant_be_asked_is_said_to_lose_whatever_is_in_it() {
    // One broken from inside (its /etc/passwd gone, say), so nothing runs in it.
    let f = started();
    let (code, out) =
        delete_typing_with(&f, b"fix-it\r", &[("FAKE_CLI_FAIL_EXEC", "for-each-ref")]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("whatever is in the bottle, which couldn't be asked what it has"),
        "{out}"
    );
    assert_eq!(f.cli()["machines"], json!({}));
}

#[test]
fn a_thread_whose_bottle_is_gone() {
    let f = started();
    let mut state = f.cli();
    state["machines"]
        .as_object_mut()
        .unwrap()
        .remove(&f.bottle("fix-it"));
    f.set_cli(&state);
    let before = f.calls("delete").len();
    f.ok(&["delete", "fix-it", "--yes"]);
    assert_eq!(f.calls("delete").len(), before);
    assert_eq!(git_in(&f.repo, &["remote"]), "");
}

// --- the repo's spec ---------------------------------------------------------------

#[test]
fn configure_without_options_shows_the_default_until_set() {
    let f = Folio::new();
    assert_eq!(
        f.ok(&["configure"]),
        "features [tools], memory 2G (the default; nothing set yet)\n"
    );
}

#[test]
fn features_added_given_options_and_taken_away_memory_and_disk_set() {
    let f = Folio::new();
    assert_eq!(
        f.ok(&["configure", "--feature", "jvm:version=17", "--memory", "8G"]),
        "Set project's spec: features [jvm:version=17, tools], memory 8G\n"
    );
    f.ok(&[
        "configure",
        "--feature",
        "jvm:version=21",
        "--feature",
        "claude",
    ]);
    assert_eq!(
        f.repo_spec(),
        Some(json!({"features": ["claude", "jvm:version=21", "tools"], "memory": "8G"}))
    );
    f.ok(&["configure", "--no-feature", "jvm", "--feature", "python"]);
    assert_eq!(
        f.repo_spec(),
        Some(json!({"features": ["claude", "python", "tools"], "memory": "8G"}))
    );
    assert_eq!(
        f.ok(&["configure", "--disk", "50G"]),
        "Set project's spec: features [claude, python, tools], memory 8G, disk 50G\n"
    );
    assert_eq!(
        f.ok(&["configure"]),
        "features [claude, python, tools], memory 8G, disk 50G\n"
    );
    f.ok(&["configure", "--no-disk"]);
    assert!(f.repo_spec().unwrap().get("disk").is_none());
}

#[test]
fn a_feature_taken_away_is_named_alone_and_must_be_there() {
    let f = Folio::new();
    for (args, error) in [
        (
            &["--no-feature", "tools:x=1"][..],
            "takes a feature's name alone, e.g. tools",
        ),
        (
            &["--no-feature", "tools", "--feature", "tools"],
            "both name tools",
        ),
        // A usage error names what's wrong, in the argument parser's own words.
        (&["--disk", "50G", "--no-disk"], "--no-disk"),
        // A thread comes first: a word after a flag that takes none isn't one.
        (
            &["--no-disk", "50G"],
            "unexpected argument '50G': the thread comes first",
        ),
        (
            &["--memory", "8G", "fix-it"],
            "unexpected argument 'fix-it'",
        ),
    ] {
        let mut all = vec!["configure"];
        all.extend(args);
        let out = f.folio(&all);
        assert_eq!(out.code, 2, "{args:?}");
        assert!(out.stderr.contains(error), "{args:?}: {}", out.stderr);
    }
    let out = f.folio(&["sidecar", "configure", "--memory", "8G", "fix-it"]);
    assert_eq!(out.code, 2);
    assert!(
        out.stderr
            .contains("the thread comes first, `folio sidecar configure THREAD [OPTIONS]`"),
        "{}",
        out.stderr
    );
    let out = f.folio(&["configure", "--no-feature", "jvm"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("there's no feature jvm to take away (features: tools)"),
        "{}",
        out.stderr
    );
    assert_eq!(f.repo_spec(), None);
}

#[test]
fn a_spec_is_checked_before_its_saved() {
    let f = Folio::new();
    f.ok(&["configure", "--memory", "2G"]);
    let out = f.folio(&["configure", "--feature", "nope"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("no feature named 'nope'"),
        "{}",
        out.stderr
    );
    assert_eq!(
        f.repo_spec(),
        Some(json!({"features": ["tools"], "memory": "2G"}))
    );
}

#[test]
fn what_folio_needs_cant_be_taken_away_and_a_spec_saved_without_it_has_it() {
    let f = Folio::new();
    let out = f.folio(&["configure", "--no-feature", "tools"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains(
            "folio's threads need the tools feature: shpool, which every thread's sessions run in"
        ),
        "{}",
        out.stderr
    );
    assert_eq!(f.repo_spec(), None);
    f.save_spec(&json!({"features": ["claude"], "memory": "4G"}));
    assert_eq!(
        f.repo_spec(),
        Some(json!({"features": ["claude", "tools"], "memory": "4G"}))
    );
    f.ok(&["new", "fix-it"]);
    assert_eq!(
        f.record("fix-it")["spec"]["features"],
        json!(["claude", "tools"])
    );
}

#[test]
fn a_new_thread_gets_the_spec_and_its_name() {
    let f = Folio::new();
    f.ok(&["configure", "--feature", "claude"]);
    f.ok(&["new", "fix-it"]);
    assert_eq!(
        f.record("fix-it")["spec"],
        json!({"features": ["claude", "tools"], "memory": "2G", "labels": {"thread": "fix-it"}})
    );
    assert!(f.repo_spec().unwrap().get("labels").is_none());
}

#[test]
fn a_threads_context_says_what_a_pause_keeps() {
    let f = started();
    let record = f.record("fix-it");
    let context = record["context"].as_str().unwrap();
    for told in [
        "## What survives",
        "A pause keeps `/workspace`, `/notes` and your conversation",
        "## Your notes",
        "git -C /notes push agent main",
        "could resume this thread from them alone",
    ] {
        assert!(context.contains(told), "{told}");
    }
}

#[test]
fn a_repos_spec_cant_give_permissions_or_labels() {
    let f = Folio::new();
    for (key, value) in [
        ("permissions", json!(["message-any"])),
        ("labels", json!({"thread": "other"})),
    ] {
        let mut spec = json!({"features": ["tools"], "memory": "2G"});
        spec[key] = value;
        f.save_spec(&spec);
        let thread = format!("t-{key}");
        for args in [
            &["new", thread.as_str()][..],
            &["configure", "--memory", "4G"],
        ] {
            let out = f.folio(args);
            assert_ne!(out.code, 0, "{args:?}");
            assert!(
                out.stderr.contains(&format!("can't give {key}")),
                "{args:?}: {}",
                out.stderr
            );
        }
    }
    assert_eq!(f.cli()["machines"], json!({}));
}

#[test]
fn every_repo_is_listed_wherever_its_run() {
    let f = Folio::new();
    let other = f.make_repo("other");
    f.ok(&["new", "a"]);
    f.ok_in(&other, &["configure", "--memory", "2G"]);
    let out = f.ok_in(&f.tmp, &["repo", "list"]);
    let mut rows: Vec<Vec<&str>> = out
        .lines()
        .skip(1)
        .map(|l| l.split_whitespace().skip(1).collect())
        .collect();
    rows.sort();
    let (other, repo) = (other.display().to_string(), f.repo.display().to_string());
    assert_eq!(rows, [vec![other.as_str(), "0"], vec![repo.as_str(), "1"]]);
}

// --- a thread's own settings, over the repo's ------------------------------------------

/// A thread's settings, as its record keeps them.
fn settings(f: &Folio) -> Value {
    f.thread("fix-it")["settings"].clone()
}

#[test]
fn a_threads_configure_without_options_shows_what_its_next_bottle_gets_and_whats_its_own() {
    let mut f = started();
    assert_eq!(
        f.ok(&["configure", "fix-it"]),
        "features [tools], memory 2G, runner this machine; thread fix-it, running\n"
    );
    f.add_runner("desktop");
    f.ok(&["configure", "--runner", "desktop"]);
    assert!(f.ok(&["configure", "fix-it"]).contains("runner desktop;"));
}

#[test]
fn one_threads_memory_and_features_its_own_and_the_repos_changes_still_reach_it() {
    let f = started();
    f.ok(&["new", "other"]);
    f.ok(&["pause", "fix-it"]);
    assert_eq!(
        f.ok(&[
            "configure",
            "fix-it",
            "--memory",
            "4G",
            "--feature",
            "jvm:version=21"
        ]),
        "Set thread fix-it: features [tools, jvm:version=21 (overridden)], memory 4G (overridden), runner this machine\n"
    );
    assert_eq!(
        settings(&f),
        json!({"features": ["jvm:version=21"], "memory": "4G"})
    );
    f.ok(&["claude", "fix-it"]);
    let spec = f.record("fix-it")["spec"].clone();
    assert_eq!(
        (&spec["memory"], &spec["features"]),
        (&json!("4G"), &json!(["jvm:version=21", "tools"]))
    );
    assert_eq!(f.record("other")["spec"]["memory"], "2G");
    // The repo's changes reach it, but for what's its own: its jvm's options are its own.
    f.ok(&[
        "configure",
        "--feature",
        "python",
        "--feature",
        "jvm:version=17",
        "--disk",
        "50G",
    ]);
    assert_eq!(
        f.ok(&["configure", "fix-it"]).split(';').next().unwrap(),
        "features [jvm:version=21 (overridden), python, tools], memory 4G (overridden), disk 50G, runner this machine"
    );
}

#[test]
fn no_takes_its_own_away_and_it_has_the_repos_again() {
    let f = started();
    f.ok(&["configure", "--feature", "jvm:version=17", "--disk", "50G"]);
    f.ok(&[
        "configure",
        "fix-it",
        "--feature",
        "jvm:version=21",
        "--feature",
        "python",
        "--memory",
        "8G",
        "--disk",
        "100G",
        "--runner",
        "local",
    ]);
    f.ok(&[
        "configure",
        "fix-it",
        "--no-feature",
        "jvm",
        "--no-memory",
        "--no-disk",
        "--no-runner",
    ]);
    assert_eq!(settings(&f), json!({"features": ["python"]}));
    assert_eq!(
        f.ok(&["configure", "fix-it"]).split(';').next().unwrap(),
        "features [jvm:version=17, tools, python (overridden)], memory 2G, disk 50G, runner this machine"
    );
}

#[test]
fn a_feature_the_repo_gives_cant_be_taken_away() {
    let f = started();
    let out = f.folio(&["configure", "fix-it", "--no-feature", "tools"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains(
            "thread fix-it doesn't override feature tools (overridden: none); one the repo gives can't be taken away"
        ),
        "{}",
        out.stderr
    );
}

#[test]
fn a_change_to_a_thread_with_a_bottle_comes_with_its_next() {
    let f = started();
    assert!(f.ok(&["configure", "fix-it", "--memory", "8G"]).contains(
        "It comes with the thread's next bottle: `folio pause fix-it`, then `folio claude fix-it`"
    ));
    assert_eq!(f.record("fix-it")["spec"]["memory"], "2G");
    // Nothing made.
    assert_eq!(f.calls("create").len(), 1);
}

#[test]
fn a_threads_spec_is_checked_over_the_repos() {
    let f = started();
    let out = f.folio(&["configure", "fix-it", "--feature", "nope"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("no feature named 'nope'"),
        "{}",
        out.stderr
    );
    assert_eq!(settings(&f), json!({}));
}

#[test]
fn a_paused_threads_runner_is_set_without_starting_it() {
    let mut f = started();
    let desktop = f.add_runner("desktop");
    f.ok(&["pause", "fix-it"]);
    assert_eq!(
        f.ok(&["configure", "fix-it", "--runner", "desktop"]),
        "Set thread fix-it: features [tools], memory 2G, runner desktop (overridden)\n"
    );
    assert!(desktop.machine("fix-it").is_null());
    assert!(f.machine("fix-it").is_null());
    let rows = f.rows();
    assert_eq!(rows["fix-it"][..2], ["paused", "desktop"]);
    f.ok(&["claude", "fix-it"]);
    assert_eq!(f.record("fix-it")["runner"], "desktop");
    // `local`: this machine, whatever the repo's; --no-runner: the repo's again.
    f.ok(&["pause", "fix-it"]);
    f.ok(&["configure", "--runner", "desktop"]);
    f.ok(&["configure", "fix-it", "--runner", "local"]);
    assert!(
        f.ok(&["configure", "fix-it"])
            .contains("runner this machine (overridden);")
    );
    f.ok(&["configure", "fix-it", "--no-runner"]);
    assert!(f.ok(&["configure", "fix-it"]).contains("runner desktop;"));
}

#[test]
fn a_thread_with_a_bottle_elsewhere_is_paused_to_move() {
    let mut f = started();
    f.add_runner("desktop");
    let out = f.folio(&["configure", "fix-it", "--runner", "desktop"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("thread fix-it's bottle is on local; pause it first (`folio pause fix-it`)"),
        "{}",
        out.stderr
    );
    assert_eq!(settings(&f), json!({}));
    // Having the repo's again, which is elsewhere, is a move too.
    f.ok(&["configure", "fix-it", "--runner", "local"]);
    f.ok(&["configure", "--runner", "desktop"]);
    assert!(
        f.folio(&["configure", "fix-it", "--no-runner"])
            .stderr
            .contains("pause it first")
    );
}

// --- where a thread's bottles are made: the repo's runner, else the thread's own -------------

#[test]
fn a_thread_is_made_here_by_default() {
    let f = Folio::new();
    f.ok(&["new", "fix-it"]);
    assert_eq!(f.record("fix-it")["runner"], "local");
    assert_eq!(f.machine("fix-it")["state"], "running");
}

#[test]
fn a_thread_can_be_made_on_a_runner() {
    let mut f = Folio::new();
    let desktop = f.add_runner("desktop");
    f.ok(&["new", "fix-it", "--runner", "desktop"]);
    assert_eq!(f.record("fix-it")["runner"], "desktop");
    assert_eq!(desktop.machine("fix-it")["state"], "running");
    assert_eq!(f.rows()["fix-it"][1], "desktop");
}

#[test]
fn the_repos_runner_is_its_threads_unless_one_has_its_own() {
    let mut f = Folio::new();
    f.add_runner("desktop");
    assert_eq!(
        f.ok(&["configure", "--runner", "desktop"]),
        "Set project's runner: desktop\n"
    );
    assert!(f.ok(&["configure"]).contains("runner desktop"));
    f.ok(&["new", "one"]);
    f.ok(&["new", "two", "--runner", "local"]);
    assert_eq!(
        (
            f.record("one")["runner"].clone(),
            f.record("two")["runner"].clone()
        ),
        (json!("desktop"), json!("local"))
    );
}

#[test]
fn no_runner_is_this_machine_again() {
    let mut f = Folio::new();
    f.add_runner("desktop");
    f.ok(&["configure", "--runner", "desktop"]);
    assert_eq!(
        f.ok(&["configure", "--no-runner"]),
        "Set project's runner: this machine\n"
    );
    assert!(!f.ok(&["configure"]).contains("runner"));
    assert_eq!(
        f.folio(&["configure", "--runner", "desktop", "--no-runner"])
            .code,
        2
    );
}

#[test]
fn a_paused_thread_resumes_on_the_runner_its_given() {
    let mut f = Folio::new();
    f.add_runner("desktop");
    f.ok(&["new", "fix-it"]);
    f.ok(&["pause", "fix-it"]);
    f.ok(&["claude", "fix-it", "--runner", "desktop"]);
    assert_eq!(f.record("fix-it")["runner"], "desktop");
    // And stays there: its next bottle is made there too.
    f.ok(&["pause", "fix-it"]);
    f.ok(&["shell", "fix-it"]);
    assert_eq!(f.record("fix-it")["runner"], "desktop");
}

#[test]
fn a_thread_with_a_bottle_elsewhere_is_paused_first() {
    let mut f = Folio::new();
    f.add_runner("desktop");
    f.ok(&["new", "fix-it"]);
    let out = f.folio(&["claude", "fix-it", "--runner", "desktop"]);
    assert_ne!(out.code, 0);
    assert!(
        out.stderr
            .contains("thread fix-it's bottle is on local; pause it first (`folio pause fix-it`)"),
        "{}",
        out.stderr
    );
    assert_eq!(f.record("fix-it")["runner"], "local");
}
