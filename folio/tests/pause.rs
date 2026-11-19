//! Pausing a thread (its agent asked to get ready, its bottle snapshotted, then deleted) and
//! resuming it (a new bottle, the snapshot restored), and deleting a paused one, on the stand-in
//! bottle-cli, whose machines' workspaces are real checkouts.

mod harness;

use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use harness::{
    CLAUDE_ARCHIVE, Folio, NOTES_ARCHIVE, SNAPSHOT, WORKSPACE_ARCHIVE, argv, current_slot,
    read_json, workspace_out, write_json,
};
use serde_json::json;

fn started() -> Folio {
    let f = Folio::new();
    f.ok(&["new", "fix-it"]);
    f
}

fn ws(f: &Folio, args: &[&str]) -> String {
    let workspace = f.workspace("fix-it").display().to_string();
    let mut all = vec!["-C", workspace.as_str()];
    all.extend(args);
    f.git(&all)
}

fn claude(f: &Folio) -> PathBuf {
    f.there("fix-it").join("home/.claude")
}

fn slot(f: &Folio) -> PathBuf {
    current_slot(&f.thread_dir("fix-it")).unwrap()
}

fn state(f: &Folio) -> String {
    f.rows()["fix-it"][0].clone()
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Every kind of state a workspace has, at once.
fn make_a_mess(f: &Folio) {
    let workspace = f.workspace("fix-it");
    fs::write(workspace.join("tracked.txt"), "v1\n").unwrap();
    fs::write(workspace.join("gone.txt"), "doomed\n").unwrap();
    fs::write(workspace.join(".gitignore"), "build/\n").unwrap();
    ws(f, &["add", "."]);
    ws(f, &["switch", "-q", "-c", "genie/fix-it-1"]);
    ws(f, &["commit", "-q", "-m", "never pushed"]);
    fs::write(workspace.join("tracked.txt"), "stashed\n").unwrap();
    ws(f, &["stash", "-q"]);
    fs::write(workspace.join("tracked.txt"), "staged\n").unwrap();
    ws(f, &["add", "tracked.txt"]);
    fs::write(workspace.join("tracked.txt"), "unstaged\n").unwrap();
    fs::remove_file(workspace.join("gone.txt")).unwrap();
    fs::write(workspace.join("untracked.txt"), "new\n").unwrap();
    fs::create_dir_all(workspace.join("build")).unwrap();
    fs::write(workspace.join("build/out.bin"), "ignored").unwrap();
    let claude = claude(f);
    let transcript = claude.join("projects/-workspace");
    fs::create_dir_all(transcript.join("memory")).unwrap();
    fs::write(
        transcript.join("session.jsonl"),
        "{\"the\": \"conversation\"}\n",
    )
    .unwrap();
    fs::write(transcript.join("memory/fact.md"), "remembered").unwrap();
    fs::create_dir_all(claude.join("plugins")).unwrap();
    fs::write(claude.join("plugins/cache.bin"), "regenerable").unwrap();
    fs::write(
        claude.join("settings.json"),
        r#"{"from": "the old bottle"}"#,
    )
    .unwrap();
}

#[test]
fn a_pause_and_resume_keep_everything_but_processes() {
    let f = started();
    make_a_mess(&f);
    let head = ws(&f, &["rev-parse", "HEAD"]);
    f.ok(&["pause", "fix-it"]);
    assert!(f.machine("fix-it").is_null());
    assert_eq!(state(&f), "paused");

    f.ok(&["claude", "fix-it"]);
    assert_eq!(state(&f), "running");
    assert_eq!(
        ws(&f, &["symbolic-ref", "--short", "HEAD"]),
        "genie/fix-it-1"
    );
    assert_eq!(ws(&f, &["rev-parse", "HEAD"]), head);
    assert_eq!(ws(&f, &["stash", "list"]).lines().count(), 1);
    assert_eq!(ws(&f, &["show", ":tracked.txt"]), "staged");
    let workspace = f.workspace("fix-it");
    assert_eq!(
        fs::read_to_string(workspace.join("tracked.txt")).unwrap(),
        "unstaged\n"
    );
    // Unstripped: its first column counts.
    let status = Command::new("git")
        .arg("-C")
        .arg(&workspace)
        .args(["status", "--porcelain"])
        .envs(f.env.iter().cloned())
        .output()
        .unwrap();
    let status = String::from_utf8(status.stdout).unwrap();
    assert_eq!(
        status.lines().collect::<Vec<_>>(),
        [" D gone.txt", "MM tracked.txt", "?? untracked.txt"]
    );
    // Ignored: rebuilt, not carried.
    assert!(!workspace.join("build").exists());
    let claude = claude(&f);
    assert_eq!(
        fs::read_to_string(claude.join("projects/-workspace/session.jsonl")).unwrap(),
        "{\"the\": \"conversation\"}\n"
    );
    assert_eq!(
        fs::read_to_string(claude.join("projects/-workspace/memory/fact.md")).unwrap(),
        "remembered"
    );
    assert!(!claude.join("plugins").exists());
    // /model and all.
    assert_eq!(
        fs::read_to_string(claude.join("settings.json")).unwrap(),
        r#"{"from": "the old bottle"}"#
    );
    let ran = f.ran("fix-it");
    let last = ran.last().unwrap();
    assert_eq!(
        last[last.len() - 4..last.len() - 2],
        argv(&["attach", "claude"])
    );
}

/// `git status --porcelain` in `dir`, unstripped: its first column counts.
fn status(f: &Folio, dir: &std::path::Path) -> Vec<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["status", "--porcelain", "-uall"])
        .envs(f.env.iter().cloned())
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The names in the snapshot's workspace archive.
fn archived(f: &Folio) -> Vec<String> {
    let out = Command::new("tar")
        .arg("-tzf")
        .arg(slot(f).join(WORKSPACE_ARCHIVE))
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_snapshot_carries_only_what_git_cant_give_back_worktrees_and_all() {
    let f = Folio::new();
    fs::write(f.repo.join("clean.txt"), "as committed\n").unwrap();
    fs::write(f.repo.join(".gitignore"), "build/\n").unwrap();
    let repo = f.repo.display().to_string();
    f.git(&["-C", &repo, "add", "."]);
    f.commit(&f.repo, "files");
    f.ok(&["new", "fix-it"]);
    let workspace = f.workspace("fix-it");
    // A worktree in the checkout, and one out of it.
    let inside = workspace.join(".worktrees/inside");
    let elsewhere = f.there("fix-it").join("elsewhere");
    for (tree, branch) in [(&inside, "inside"), (&elsewhere, "elsewhere")] {
        let path = tree.display().to_string();
        ws(&f, &["worktree", "add", "-q", "-b", branch, &path]);
        fs::write(tree.join("clean.txt"), "changed\n").unwrap();
        fs::write(tree.join("new.txt"), "untracked\n").unwrap();
        fs::create_dir_all(tree.join("build")).unwrap();
        fs::write(tree.join("build/out.bin"), "ignored").unwrap();
    }
    // Another's repo in the checkout: untracked files of the checkout's, its .git and all.
    let other = workspace.join("vendor/other");
    f.git(&["init", "-q", &other.display().to_string()]);
    fs::write(other.join("theirs.txt"), "theirs\n").unwrap();
    let roots = [workspace.clone(), inside.clone(), elsewhere.clone()];
    let before: Vec<Vec<String>> = roots.iter().map(|r| status(&f, r)).collect();
    f.ok(&["pause", "fix-it"]);

    let names = archived(&f);
    let has = |n: &str| names.iter().any(|m| m == n);
    // The index has it as it is: the index gives it back.
    assert!(!has("clean.txt"), "{names:?}");
    for tree in [
        ".worktrees/inside/".to_string(),
        format!("{}/", elsewhere.display()),
    ] {
        assert!(has(&format!("{tree}.git")), "{names:?}");
        assert!(has(&format!("{tree}clean.txt")), "{names:?}");
        assert!(has(&format!("{tree}new.txt")), "{names:?}");
    }
    assert!(names.iter().all(|n| !n.contains("build/")), "{names:?}");
    assert!(has("vendor/other/theirs.txt"), "{names:?}");
    assert!(has("vendor/other/.git/HEAD"), "{names:?}");

    let _ = fs::remove_dir_all(&elsewhere);
    f.ok(&["shell", "fix-it"]);
    let after: Vec<Vec<String>> = roots.iter().map(|r| status(&f, r)).collect();
    assert_eq!(after, before);
    assert_eq!(
        fs::read_to_string(workspace.join("clean.txt")).unwrap(),
        "as committed\n"
    );
    for tree in [&inside, &elsewhere] {
        assert_eq!(
            fs::read_to_string(tree.join("clean.txt")).unwrap(),
            "changed\n"
        );
        assert!(tree.join(".gitignore").exists());
        assert!(!tree.join("build").exists());
    }
    assert!(!workspace.join(".git/folio-deleted").exists());
    assert!(
        !workspace
            .join(".git/worktrees/inside/folio-deleted")
            .exists()
    );
}

#[test]
fn a_snapshot_from_before_resumes_as_it_was() {
    // Every file in its archive, and no list of the deleted ones: none comes back.
    let f = started();
    make_a_mess(&f);
    let workspace = f.workspace("fix-it");
    let old = f.tmp.join("old.tar.gz");
    let made = Command::new("sh")
        .arg("-c")
        .arg(format!(
            r#"{{ printf '.git\0'; git ls-files -z -co --exclude-standard | xargs -0 sh -c 'for f do if [ -e "$f" ] || [ -L "$f" ]; then printf "%s\0" "$f"; fi; done' sh; }} | tar -czf {} --null -T -"#,
            old.display()
        ))
        .current_dir(&workspace)
        .envs(f.env.iter().cloned())
        .status()
        .unwrap();
    assert!(made.success());
    let before = status(&f, &workspace);
    f.ok(&["pause", "fix-it"]);
    let archive = slot(&f).join(WORKSPACE_ARCHIVE);
    fs::remove_file(&archive).unwrap();
    fs::copy(&old, &archive).unwrap();
    f.ok(&["shell", "fix-it"]);
    assert_eq!(status(&f, &workspace), before);
    assert!(!workspace.join("gone.txt").exists());
}

#[test]
fn a_resumed_thread_keeps_its_snapshot_till_the_next() {
    let f = started();
    fs::write(f.workspace("fix-it").join("first.txt"), "first\n").unwrap();
    f.ok(&["pause", "fix-it"]);
    let first = fs::read(slot(&f).join(WORKSPACE_ARCHIVE)).unwrap();
    f.ok(&["shell", "fix-it"]);
    assert_eq!(state(&f), "running");
    assert_eq!(names(&slot(&f)), names_of(&SNAPSHOT, "thread.json"));
    assert_eq!(fs::read(slot(&f).join(WORKSPACE_ARCHIVE)).unwrap(), first);
    // A record saved meanwhile carries it into its slot.
    f.ok(&["configure", "fix-it", "--memory", "4G"]);
    assert_eq!(fs::read(slot(&f).join(WORKSPACE_ARCHIVE)).unwrap(), first);
    // The next replaces it, whole.
    fs::write(f.workspace("fix-it").join("second.txt"), "second\n").unwrap();
    f.ok(&["pause", "fix-it"]);
    assert!(archived(&f).contains(&"second.txt".to_string()));
    assert!(
        names(slot(&f).parent().unwrap())
            .iter()
            .all(|n| !n.starts_with('.'))
    );
}

/// `names`, and `more`, sorted: a slot's files.
fn names_of(names: &[&str], more: &str) -> Vec<String> {
    let mut all: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    all.push(more.to_string());
    all.sort();
    all
}

#[test]
fn the_sessions_are_stopped_before_the_snapshot() {
    let f = started();
    f.ok(&["claude", "fix-it"]);
    f.ok(&["pause", "fix-it"]);
    let ran = f.ran("fix-it");
    let kill = ran
        .iter()
        .position(|r| r.get(..2) == Some(&argv(&["kill", "-HUP"])[..]))
        .unwrap();
    let archive = ran
        .iter()
        .position(|r| {
            r.last()
                .is_some_and(|s| s.starts_with(&workspace_out("/workspace")))
        })
        .unwrap();
    assert!(kill < archive);
}

#[test]
fn a_resume_gets_the_repos_spec_as_it_is_now() {
    let f = started();
    f.ok(&["pause", "fix-it"]);
    f.ok(&["configure", "--feature", "claude"]);
    f.ok(&["claude", "fix-it"]);
    assert_eq!(
        f.record("fix-it")["spec"]["features"],
        json!(["claude", "tools"])
    );
}

#[test]
fn a_resume_gets_the_advice_as_it_is_now() {
    // Written afresh for the new bottle, not carried over from the old one.
    let mut f = started();
    f.add_runner("box");
    f.ok(&["pause", "fix-it"]);
    // What it's told of the sidecar it's given meanwhile, say.
    f.ok(&["sidecar", "configure", "fix-it", "--runner", "box"]);
    f.ok(&["claude", "fix-it"]);
    assert!(
        f.record("fix-it")["context"]
            .as_str()
            .unwrap()
            .contains("## Your sidecar")
    );
}

#[test]
fn list_shows_each_threads_snapshot_its_size_and_age() {
    let f = started();
    f.ok(&["new", "other"]);
    // Untracked: in the snapshot.
    fs::write(f.workspace("fix-it").join("big.bin"), vec![b'x'; 300_000]).unwrap();
    f.ok(&["pause", "fix-it"]);
    let size: u64 = SNAPSHOT
        .iter()
        .map(|n| fs::metadata(slot(&f).join(n)).unwrap().len())
        .sum();
    let size = format!("{:.1}K", size as f64 / 1024.0);
    let rows = f.rows();
    assert_eq!(
        rows["fix-it"][..6],
        ["paused", "local", "-", &size, "just", "now"]
    );
    assert_eq!(rows["other"][..4], ["running", "local", "-", "-"]);
    // Resumed, it still has the one it came from.
    f.ok(&["shell", "fix-it"]);
    assert_eq!(
        f.rows()["fix-it"][..6],
        ["running", "local", "-", &size, "just", "now"]
    );
}

#[test]
fn pausing_twice_or_with_no_bottle() {
    let f = started();
    f.ok(&["pause", "fix-it"]);
    assert!(
        f.folio(&["pause", "fix-it"])
            .stderr
            .contains("already paused")
    );
    f.ok(&["new", "lost"]);
    let mut cli = f.cli();
    cli["machines"]
        .as_object_mut()
        .unwrap()
        .remove(&f.bottle("lost"));
    f.set_cli(&cli);
    let out = f.folio(&["pause", "lost"]);
    assert!(
        out.stderr.contains("there's nothing to pause"),
        "{}",
        out.stderr
    );
}

#[test]
fn a_stopped_bottle_is_started_to_pause_it() {
    let f = started();
    f.set_machine("fix-it", json!({"state": "stopped"}));
    let before = f.calls("start").len();
    f.ok(&["pause", "fix-it"]);
    assert_eq!(
        f.calls("start")[before..],
        [argv(&["start", &f.bottle("fix-it")])]
    );
}

#[test]
fn a_bottle_from_before_notes_pauses_without_them_and_resumes_with_them() {
    let f = started();
    let path = f
        .thread_dir("fix-it")
        .join("bottles")
        .join(format!("{}.json", f.bottle("fix-it")));
    let mut record = read_json(&path).unwrap();
    record["mounts"]
        .as_array_mut()
        .unwrap()
        .retain(|m| m["path"] != "/notes");
    write_json(&path, &record);
    f.ok(&["pause", "fix-it"]);
    assert!(!slot(&f).join(NOTES_ARCHIVE).exists());
    f.ok(&["shell", "fix-it"]);
    let paths: Vec<String> = f.record("fix-it")["mounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["path"].as_str().unwrap().to_string())
        .collect();
    assert!(paths.contains(&"/notes".to_string()), "{paths:?}");
}

// --- asking the agent first -----------------------------------------------------------

/// The events sent to the thread's agent: each one's argv and text.
fn events(f: &Folio) -> Vec<(Vec<String>, String)> {
    f.execs("fix-it")
        .iter()
        .filter(|e| e["argv"][0] == "/usr/local/bin/bottle-event")
        .map(|e| {
            let argv = e["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect();
            (argv, e["stdin"].as_str().unwrap().to_string())
        })
        .collect()
}

#[test]
fn a_pause_asks_the_agent_to_get_ready_and_waits_for_it() {
    let f = started();
    f.ok(&["claude", "fix-it"]);
    let out = f.with(
        &["pause", "fix-it"],
        &[("FAKE_CLI_EVENT", "a minute, pushing|snapshot")],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stderr
            .contains("fix-it's agent says: a minute, pushing"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("fix-it's agent is ready"),
        "{}",
        out.stderr
    );
    assert_eq!(state(&f), "paused");
    let sent = events(&f);
    assert_eq!(sent.len(), 1);
    let (asked, text) = &sent[0];
    assert_eq!(asked[1..5], argv(&["send", "-t", "10", "-f"]));
    // An address of the snapshot ask's, for this bottle to answer.
    assert!(asked[5].starts_with("snapshot~"), "{asked:?}");
    assert!(text.starts_with("Snapshot interrupt: the user is pausing this thread."));
    assert!(text.contains("This thread has no snapshot yet."), "{text}");
    // Asked while its sessions ran, and only then were they stopped.
    let ran = f.ran("fix-it");
    let event = ran
        .iter()
        .position(|r| r[0] == "/usr/local/bin/bottle-event")
        .unwrap();
    let kill = ran
        .iter()
        .position(|r| r.get(..2) == Some(&argv(&["kill", "-HUP"])[..]))
        .unwrap();
    assert!(event < kill);

    // Resumed and paused again: told how big the last one was.
    f.ok(&["shell", "fix-it"]);
    f.ok(&["pause", "fix-it"]);
    let sent = events(&f);
    assert!(
        sent.last()
            .unwrap()
            .1
            .contains("The last snapshot of this thread took ")
    );
}

#[test]
fn with_no_session_to_ask_or_forced_it_snapshots_as_it_is() {
    let f = started();
    // The stand-in's machine has no session to take it.
    let out = f.with(&["pause", "fix-it"], &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stderr.contains(
            "fix-it's agent wasn't asked to get ready (no Claude session took the message)"
        ),
        "{}",
        out.stderr
    );
    assert_eq!(state(&f), "paused");
    f.ok(&["shell", "fix-it"]);
    let before = events(&f).len();
    f.ok(&["pause", "fix-it", "--force"]);
    assert_eq!(events(&f).len(), before);
    assert_eq!(state(&f), "paused");
}

#[test]
fn ctrl_c_while_waiting_leaves_it_running() {
    let f = started();
    let mut command = Command::new(harness::FOLIO);
    command
        .args(["pause", "fix-it"])
        .current_dir(&f.repo)
        .envs(f.env.iter().cloned())
        // Taken, and never answered.
        .env("FAKE_CLI_EVENT", "taken")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut folio = command.spawn().unwrap();
    let mut stderr = folio.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).unwrap();
        text
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    while events(&f).is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    harness::signal_group(folio.id(), rustix::process::Signal::INT);
    let status = folio.wait().unwrap();
    let err = reader.join().unwrap();
    assert_eq!(status.code(), Some(130), "{err}");
    assert!(err.contains("waiting for it to reply"), "{err}");
    assert_eq!(state(&f), "running");
}

// --- idle snapshots: folio serve's, of an agent at rest -------------------------------------

/// A thread whose folio serve takes an agent quiet for 2s as at rest, its agent answering events
/// as `answers` says (the stand-in's $FAKE_CLI_EVENT).
fn idling(answers: &str) -> Folio {
    let mut f = Folio::new();
    f.set("FOLIO_IDLE_SECONDS", "2");
    f.set("FAKE_CLI_EVENT", answers);
    f.ok(&["new", "fix-it"]);
    f
}

/// Its agent at work, now, as its hooks say.
fn at_work(f: &Folio) {
    let dir = f.home.join("activity");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(f.bottle("fix-it")), "").unwrap();
}

/// When its snapshot was taken, if it has one.
fn taken(f: &Folio) -> Option<std::time::SystemTime> {
    fs::metadata(slot(f).join(CLAUDE_ARCHIVE))
        .and_then(|m| m.modified())
        .ok()
}

fn within(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn an_agent_at_rest_is_snapshotted_once_ready_and_again_only_after_more_work() {
    let f = idling("snapshot");
    // Never said it was at work: never asked.
    std::thread::sleep(Duration::from_secs(4));
    assert!(events(&f).is_empty());
    assert_eq!(taken(&f), None);

    at_work(&f);
    assert!(within(30, || taken(&f).is_some()), "no snapshot");
    assert_eq!(state(&f), "running");
    let sent = events(&f);
    assert_eq!(sent.len(), 1);
    assert!(
        sent[0].1.starts_with("Automated snapshot interrupt."),
        "{}",
        sent[0].1
    );
    // Nothing done since but in the IDLE after it (the rest of the agent's turn): no other.
    let first = taken(&f).unwrap();
    at_work(&f);
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(events(&f).len(), 1);
    assert_eq!(taken(&f), Some(first));
    // Nor once folio serve's started again: that's told from what's on disk.
    f.ok(&["serve", "stop"]);
    harness::eventually("folio serve gone", || !f.home.join("serve.sock").exists());
    f.ok(&["serve", "start"]);
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(events(&f).len(), 1);
    assert_eq!(taken(&f), Some(first));
    // More work, and at rest again: another.
    at_work(&f);
    assert!(within(30, || taken(&f) > Some(first)), "no second snapshot");
    assert_eq!(events(&f).len(), 2);
}

#[test]
fn one_put_off_is_asked_again_later_and_one_with_no_session_is_taken() {
    // Taken, and never answered.
    let f = idling("taken");
    at_work(&f);
    assert!(within(30, || events(&f).len() >= 2), "asked once at most");
    assert_eq!(taken(&f), None);
    assert_eq!(state(&f), "running");
    // No session to take it: snapshotted as it is.
    let f = idling("");
    at_work(&f);
    assert!(within(30, || taken(&f).is_some()), "no snapshot");
}

#[test]
fn a_pause_doesnt_ask_an_agent_quiet_since_its_last_snapshot() {
    let f = idling("snapshot");
    at_work(&f);
    assert!(within(30, || taken(&f).is_some()), "no snapshot");
    // The rest of its turn, then at rest.
    at_work(&f);
    std::thread::sleep(Duration::from_secs(3));
    let out = f.folio(&["pause", "fix-it"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!out.stderr.contains("agent"), "{}", out.stderr);
    assert_eq!(events(&f).len(), 1);
    assert_eq!(state(&f), "paused");
}

#[test]
fn a_pause_asks_one_at_work_since_or_still_at_it() {
    let f = idling("snapshot");
    at_work(&f);
    assert!(within(30, || taken(&f).is_some()), "no snapshot");
    // At work after the IDLE that follows it.
    std::thread::sleep(Duration::from_secs(3));
    at_work(&f);
    f.ok(&["pause", "fix-it"]);
    assert_eq!(events(&f).len(), 2);
    // At work within the IDLE after one, and just now: maybe at it still.
    f.ok(&["shell", "fix-it"]);
    at_work(&f);
    f.ok(&["pause", "fix-it"]);
    assert_eq!(events(&f).len(), 3);
}

// --- failures ------------------------------------------------------------------------

#[test]
fn a_snapshot_that_doesnt_read_back_whole_deletes_nothing() {
    let f = started();
    let out = f.with(
        &["pause", "fix-it"],
        &[("FAKE_CLI_GARBLE", "cd \"$HOME/.claude\" && tar -czf")],
    );
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("claude.tar.gz came back incomplete"),
        "{}",
        out.stderr
    );
    assert!(!f.machine("fix-it").is_null());
    assert_eq!(names(&slot(&f)), ["thread.json"]);
    assert_eq!(state(&f), "running");
    assert!(
        names(slot(&f).parent().unwrap())
            .iter()
            .all(|n| !n.starts_with(".staging"))
    );
}

#[test]
fn a_resume_that_fails_leaves_it_paused() {
    let f = started();
    f.ok(&["pause", "fix-it"]);
    let out = f.with(
        &["claude", "fix-it"],
        &[(
            "FAKE_CLI_FAIL_EXEC",
            "find . -mindepth 1 -maxdepth 1 -exec rm -rf",
        )],
    );
    assert_eq!(out.code, 1);
    assert!(out.stderr.contains("failed"), "{}", out.stderr);
    assert!(f.machine("fix-it").is_null());
    assert_eq!(state(&f), "paused");
    // And it resumes once whatever it was is fixed.
    f.ok(&["claude", "fix-it"]);
    assert_eq!(state(&f), "running");
}

// --- deleting a paused thread ---------------------------------------------------------

#[test]
fn deleting_a_paused_thread_never_runs_git_on_the_snapshot() {
    // A snapshot's .git is the agent's, and its config can make git run commands (core.fsmonitor
    // runs on any git status): reading it here would run them outside the sandbox.
    let f = started();
    let marker = f.tmp.join("agent-ran-this");
    ws(
        &f,
        &[
            "config",
            "core.fsmonitor",
            &format!("touch {}; false", marker.display()),
        ],
    );
    f.ok(&["pause", "fix-it"]);
    // The stand-in's machine runs on this machine; in a real one this was in the VM.
    let _ = fs::remove_file(&marker);
    f.on_terminal(
        &["delete", "fix-it"],
        &[(b"Type fix-it to delete it: ", b"no\r")],
    );
    assert!(!marker.exists());
}

#[test]
fn deleting_a_paused_thread_reads_what_would_be_lost_from_the_snapshot() {
    let f = started();
    ws(&f, &["switch", "-q", "-c", "genie/fix-it-1"]);
    ws(&f, &["commit", "-q", "--allow-empty", "-m", "never pushed"]);
    fs::write(f.workspace("fix-it").join("draft.txt"), "draft").unwrap();
    f.ok(&["pause", "fix-it"]);
    let (code, out) = f.on_terminal(
        &["delete", "fix-it"],
        &[(b"Type fix-it to delete it: ", b"fix-it\r")],
    );
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("genie/fix-it-1: in the snapshot, never pushed"),
        "{out}"
    );
    assert!(out.contains("uncommitted changes in the snapshot"), "{out}");
    // The pause's; none for the delete.
    assert_eq!(
        f.calls("delete"),
        [argv(&["delete", &f.bottle("fix-it"), "-f"])]
    );
    assert_eq!(names(&f.thread_dir("fix-it")), ["notes"]);
}
