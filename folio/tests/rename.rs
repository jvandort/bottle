//! Renaming a paused thread: fix-it, with a branch the agent pushed, fetched and checked out by
//! you; then renamed to sort-it.

mod harness;

use harness::{Folio, git_in};
use serde_json::json;

fn renaming() -> Folio {
    let f = Folio::new();
    f.ok(&["new", "fix-it"]);
    let tree = git_in(&f.repo, &["rev-parse", "HEAD^{tree}"]);
    let pushed = f.git(&[
        "-C",
        &f.repo.display().to_string(),
        "commit-tree",
        &tree,
        "-p",
        "HEAD",
        "-m",
        "agent work",
    ]);
    git_in(
        &f.repo,
        &[
            "update-ref",
            "refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1",
            &pushed,
        ],
    );
    git_in(
        &f.repo,
        &[
            "update-ref",
            "refs/namespaces/fix-it-in/refs/heads/handed",
            &pushed,
        ],
    );
    f.git(&[
        "-C",
        &f.repo.display().to_string(),
        "fetch",
        "-q",
        "bottle/fix-it",
    ]);
    git_in(&f.repo, &["switch", "-q", "genie/fix-it-1"]);
    f
}

fn refs(f: &Folio, prefix: &str) -> Vec<String> {
    git_in(&f.repo, &["for-each-ref", "--format=%(refname)", prefix])
        .lines()
        .map(String::from)
        .collect()
}

#[test]
fn a_thread_with_a_bottle_is_paused_first() {
    let f = renaming();
    let out = f.folio(&["rename", "fix-it", "sort-it"]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("pause it first"), "{}", out.stderr);
    assert!(out.stderr.contains("folio pause fix-it"), "{}", out.stderr);
    assert_eq!(
        refs(&f, "refs/namespaces/fix-it-out/"),
        ["refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1"]
    );
    assert!(f.ok(&["list"]).contains("fix-it"));
}

#[test]
fn everything_moves_and_the_branches_keep_their_names() {
    let f = renaming();
    f.ok(&["pause", "fix-it"]);
    assert!(
        f.ok(&["rename", "fix-it", "sort-it"])
            .contains("Renamed thread fix-it to sort-it")
    );
    assert_eq!(
        refs(&f, "refs/namespaces/sort-it-out/"),
        ["refs/namespaces/sort-it-out/refs/heads/genie/fix-it-1"]
    );
    assert_eq!(
        refs(&f, "refs/namespaces/sort-it-in/"),
        ["refs/namespaces/sort-it-in/refs/heads/handed"]
    );
    assert!(
        refs(&f, "refs/namespaces/fix-it-out/").is_empty()
            && refs(&f, "refs/namespaces/fix-it-in/").is_empty()
    );
    // The remote, and your branch that tracked it, move to the new name.
    assert_eq!(
        refs(&f, "refs/remotes/bottle/"),
        ["refs/remotes/bottle/sort-it/genie/fix-it-1"]
    );
    assert_eq!(
        git_in(
            &f.repo,
            &["rev-parse", "--abbrev-ref", "genie/fix-it-1@{upstream}"]
        ),
        "bottle/sort-it/genie/fix-it-1"
    );
    assert_eq!(
        git_in(&f.repo, &["config", "remote.bottle/sort-it.uploadpack"]),
        "git --namespace=sort-it-out upload-pack"
    );
    // And still reads the lane.
    f.git(&[
        "-C",
        &f.repo.display().to_string(),
        "fetch",
        "-q",
        "bottle/sort-it",
    ]);
    let listed = f.ok(&["list"]);
    assert!(listed.contains("sort-it"));
    assert!(!listed.contains("fix-it "));
}

#[test]
fn the_resume_makes_a_bottle_by_the_new_name() {
    let f = renaming();
    f.ok(&["pause", "fix-it"]);
    f.ok(&["rename", "fix-it", "sort-it"]);
    f.ok(&["shell", "sort-it"]);
    let record = f.record("sort-it");
    assert!(
        record["context"]
            .as_str()
            .unwrap()
            .contains("# Thread `sort-it`")
    );
    assert!(
        record["mounts"][0]["remotes"].as_array().unwrap().contains(
            &json!({"name": "agent", "fetch": "ns/sort-it-out", "push": "ns/sort-it-out"})
        )
    );
    assert_eq!(record["spec"]["labels"]["thread"], "sort-it");
    assert_eq!(f.machine("sort-it")["state"], "running");
}

#[test]
fn a_name_thats_taken_or_invalid_is_refused() {
    let f = renaming();
    f.ok(&["new", "other"]);
    f.ok(&["pause", "fix-it"]);
    for (new, why) in [
        ("other", "already exists"),
        ("Sort It", "invalid thread name"),
    ] {
        let out = f.folio(&["rename", "fix-it", new]);
        assert_ne!(out.code, 0, "{new}");
        assert!(out.stderr.contains(why), "{new}: {}", out.stderr);
    }
    assert_eq!(
        refs(&f, "refs/namespaces/fix-it-out/"),
        ["refs/namespaces/fix-it-out/refs/heads/genie/fix-it-1"]
    );
}
