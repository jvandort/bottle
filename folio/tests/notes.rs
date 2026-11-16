//! A thread's notes: a repo of its own in the thread's folio directory, at /notes in its bottle.

mod harness;

use std::fs;
use std::path::PathBuf;

use harness::{Folio, NOTES_ARCHIVE, current_slot, git_in};
use serde_json::json;

const GITIGNORE: &str = ".obsidian/\n.trash/\n";

fn started() -> Folio {
    let f = Folio::new();
    f.ok(&["new", "fix-it"]);
    f
}

fn notes(f: &Folio, thread: &str) -> PathBuf {
    f.thread_dir(thread).join("notes")
}

/// The machine's checkout of /notes.
fn in_bottle(f: &Folio) -> PathBuf {
    f.there("fix-it").join("notes")
}

#[test]
fn a_thread_starts_with_notes_of_its_own() {
    let f = started();
    let notes = notes(&f, "fix-it");
    assert_eq!(git_in(&notes, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(
        fs::read_to_string(notes.join(".gitignore")).unwrap(),
        GITIGNORE
    );
    assert!(notes.join(".obsidian").is_dir());
    let mount = f.record("fix-it")["mounts"][1].clone();
    assert_eq!(
        (&mount["path"], &mount["repo"]),
        (&json!("/notes"), &json!(notes))
    );
    assert_eq!(
        mount["remotes"],
        json!([{"name": "host", "fetch": "heads", "push": null},
               {"name": "agent", "fetch": "ns/notes-out", "push": "ns/notes-out"},
               {"name": "user", "fetch": "ns/notes-in", "push": null}])
    );
}

#[test]
fn your_copy_follows_the_agents_main() {
    // The agent pushes main to the notes repo's lane; your main tracks it, so bottle's follow
    // moves it.
    let f = started();
    let notes = notes(&f, "fix-it");
    assert_eq!(
        git_in(
            &notes,
            &["for-each-ref", "--format=%(upstream)", "refs/heads/main"]
        ),
        "refs/remotes/bottle/main"
    );
    assert_eq!(
        git_in(&notes, &["config", "remote.bottle.uploadpack"]),
        "git --namespace=notes-out upload-pack"
    );
}

#[test]
fn a_pause_keeps_whats_not_pushed_and_the_resume_brings_it_back() {
    let f = started();
    fs::write(in_bottle(&f).join("Lead.md"), "look at the cache\n").unwrap();
    f.ok(&["pause", "fix-it"]);
    assert!(
        current_slot(&f.thread_dir("fix-it"))
            .unwrap()
            .join(NOTES_ARCHIVE)
            .exists()
    );
    f.ok(&["shell", "fix-it"]);
    assert_eq!(
        fs::read_to_string(in_bottle(&f).join("Lead.md")).unwrap(),
        "look at the cache\n"
    );
    // Pointed at the new bottle's objects and remotes, wherever the old one had them.
    assert!(f.ran("fix-it").iter().any(|r| {
        let last = r.last().unwrap();
        last.contains("remote set-url") && last.contains("/notes")
    }));
}

#[test]
fn a_resume_of_a_snapshot_from_before_notes() {
    let f = started();
    f.ok(&["pause", "fix-it"]);
    fs::remove_file(
        current_slot(&f.thread_dir("fix-it"))
            .unwrap()
            .join(NOTES_ARCHIVE),
    )
    .unwrap();
    f.ok(&["shell", "fix-it"]);
    // Checked out afresh.
    assert!(in_bottle(&f).join(".gitignore").exists());
}

#[test]
fn delete_keeps_them_and_a_new_thread_of_the_name_finds_them() {
    let f = started();
    let notes = notes(&f, "fix-it");
    f.commit(&notes, "what I learnt");
    let kept = git_in(&notes, &["rev-parse", "HEAD"]);
    f.ok(&["delete", "fix-it", "--yes"]);
    assert_eq!(git_in(&notes, &["rev-parse", "HEAD"]), kept);
    f.ok(&["new", "fix-it"]);
    assert_eq!(git_in(&notes, &["rev-parse", "HEAD"]), kept);
    assert_eq!(f.record("fix-it")["mounts"][1]["repo"], json!(notes));
}

#[test]
fn a_rename_takes_them_along() {
    let f = started();
    f.ok(&["pause", "fix-it"]);
    f.ok(&["rename", "fix-it", "sort-it"]);
    assert!(!notes(&f, "fix-it").exists());
    assert_eq!(
        git_in(&notes(&f, "sort-it"), &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
}

#[test]
fn folio_notes_opens_them() {
    let mut f = started();
    let (opener, log) = (f.tmp.join("open"), f.tmp.join("opened"));
    fs::write(
        &opener,
        format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n", log.display()),
    )
    .unwrap();
    std::process::Command::new("chmod")
        .arg("+x")
        .arg(&opener)
        .status()
        .unwrap();
    f.set("FOLIO_OPEN", opener.display().to_string());
    let out = f.ok(&["notes", "fix-it"]);
    assert!(out.contains("Opened"), "{out}");
    let vaults = fs::read_to_string(f.tmp.join("obsidian.json")).unwrap();
    assert!(
        vaults.contains(
            &notes(&f, "fix-it")
                .canonicalize()
                .unwrap()
                .display()
                .to_string()
        ),
        "{vaults}"
    );
}

#[test]
fn no_notes_yet() {
    let f = started();
    let out = f.folio(&["notes", "other"]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("has no notes yet"), "{}", out.stderr);
}
