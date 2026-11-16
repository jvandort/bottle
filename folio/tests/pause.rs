//! Pausing a thread (its bottle snapshotted, then deleted) and resuming it (a new bottle, the
//! snapshot restored), and deleting a paused one, on the stand-in bottle-cli, whose machines'
//! workspaces are real checkouts.

mod harness;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use harness::{
    Folio, NOTES_ARCHIVE, SNAPSHOT, WORKSPACE_ARCHIVE, argv, current_slot, read_json,
    workspace_out, write_json,
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

#[test]
fn a_resumed_thread_has_no_snapshot_left_to_go_stale() {
    let f = started();
    f.ok(&["pause", "fix-it"]);
    assert!(slot(&f).join(WORKSPACE_ARCHIVE).exists());
    f.ok(&["shell", "fix-it"]);
    assert_eq!(names(&slot(&f)), ["thread.json"]);
    assert!(
        names(slot(&f).parent().unwrap())
            .iter()
            .all(|n| !n.starts_with('.'))
    );
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
        .position(|r| r.last() == Some(&workspace_out("/workspace")))
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
fn list_shows_what_a_paused_thread_takes_up() {
    let f = started();
    f.ok(&["new", "other"]);
    // Untracked: in the snapshot.
    fs::write(f.workspace("fix-it").join("big.bin"), vec![b'x'; 300_000]).unwrap();
    f.ok(&["pause", "fix-it"]);
    let size: u64 = SNAPSHOT
        .iter()
        .map(|n| fs::metadata(slot(&f).join(n)).unwrap().len())
        .sum();
    let rows = f.rows();
    assert_eq!(
        rows["fix-it"][..4],
        [
            "paused",
            "local",
            "-",
            &format!("{:.1}K", size as f64 / 1024.0)
        ]
    );
    assert_eq!(rows["other"][..4], ["running", "local", "-", "-"]);
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
