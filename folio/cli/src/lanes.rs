//! Lanes: how a thread's commits travel between its bottle and the user's repo.
//!
//! A thread's lane is two git namespaces in the user's repo: `<thread>-out`, which the agent writes
//! and the user reads, and `<thread>-in`, the reverse. Each has one writer, so neither side's push is
//! ever refused for what the other did, and conflicts are always the agent's to merge.
//!
//! The bottle is served them as two remotes, each fetching what it pushes, so git behaves as git:
//! `agent`, the out lane, which it pushes to and sees its own pushes in; and `user`, the in lane,
//! read-only, where what the user hands over arrives. The user's repo gets them as one remote,
//! `bottle/<thread>`, which folio owns: fetching reads the agent's lane, pushing hands over into the
//! user's. So a thread's branches can be fetched whether or not it has a bottle right now.
//!
//! A branch the user deletes from the remote (`git push bottle/<thread> :refs/heads/<branch>`, as
//! `curate integrate` does, `remote.<name>.curateDelete` saying to) is deleted from the agent's lane
//! too, by the remote's receive-pack hook (this program's `hook lane-pre-receive`, serve/hook.rs):
//! if it's still what the user last fetched of it, and the push is refused if not.
//! Nothing else deletes from the out lane.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::git::{git, git_input, succeeds};

pub const OUT: &str = "out";
pub const IN: &str = "in";
const REMOTE_PREFIX: &str = "bottle/";

pub fn namespace(thread: &str, direction: &str) -> String {
    format!("{thread}-{direction}")
}

pub fn remote_name(thread: &str) -> String {
    format!("{REMOTE_PREFIX}{thread}")
}

/// The remotes a thread's bottle is served, as host.Remote specs.
pub fn bottle_remotes(thread: &str) -> Vec<String> {
    let (out, in_) = (namespace(thread, OUT), namespace(thread, IN));
    vec![
        "origin:fetch=upstream".into(),
        "host:fetch=heads".into(),
        format!("agent:fetch=ns/{out},push=ns/{out}"),
        format!("user:fetch=ns/{in_}"),
    ]
}

/// The lane remote's hooks: in folio's home, scripts that run this program (`hook
/// lane-pre-receive`), written once a process (folio may have moved since), and again if they've
/// gone.
fn hooks() -> Result<PathBuf, String> {
    static WRITTEN: Mutex<Option<PathBuf>> = Mutex::new(None);
    let dir = crate::home().join("hooks").join("lane");
    let mut written = WRITTEN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if written.as_ref() == Some(&dir) && dir.join("pre-receive").exists() {
        return Ok(dir);
    }
    crate::serve::hook::write(&dir, &[("pre-receive", "lane-pre-receive")])
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    *written = Some(dir.clone());
    Ok(dir)
}

/// Make the lane remote `bottle/<thread>` of the repo: fetch the out lane, push to the in lane.
///
/// One remote, two lanes. `uploadpack` and `receivepack` name the command git runs on the remote
/// side, so pointing them at different namespaces means `git fetch` reads what the bottle wrote and
/// `git push` writes what it reads. Every setting is written with --replace-all, so registering
/// again repairs a remote that was edited.
pub fn register(repo: &Path, thread: &str) -> Result<(), String> {
    register_remote(
        repo,
        &remote_name(thread),
        &namespace(thread, OUT),
        &namespace(thread, IN),
    )
}

/// Make `name` a lane remote of the repo: fetch reads namespace `out`, push writes `in_`.
pub fn register_remote(repo: &Path, name: &str, out: &str, in_: &str) -> Result<(), String> {
    let hooks = hooks()?;
    let settings = [
        ("url", repo.to_string_lossy().into_owned()),
        ("fetch", format!("+refs/heads/*:refs/remotes/{name}/*")),
        ("uploadpack", format!("git --namespace={out} upload-pack")),
        // The user's own lane, so no restrictions: rewriting what they haven't handed over yet is
        // theirs to do. Its hook takes a delete through to the agent's lane, which is why curate
        // deletes there; the -c settings reach it, saying where that is.
        (
            "receivepack",
            format!(
                "git -c core.hooksPath={} -c folio.remote={name} -c folio.out={out} \
                 -c receive.denyCurrentBranch=ignore --namespace={in_} receive-pack",
                shell_quote(&hooks.to_string_lossy())
            ),
        ),
        ("curateDelete", "true".into()),
    ];
    for (key, value) in settings {
        git(
            &[
                "config",
                "--replace-all",
                &format!("remote.{name}.{key}"),
                &value,
            ],
            Some(repo),
        )?;
    }
    Ok(())
}

/// `s` as Python's shlex.quote has it.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r#"'"'"'"#))
}

/// Remove the lane: both namespaces' refs, and the remote with what it fetched. Gone already is
/// fine.
pub fn forget(repo: &Path, thread: &str) -> Result<(), String> {
    let mut refs: Vec<String> = branches(repo, thread, OUT, true)?
        .into_keys()
        .chain(branches(repo, thread, IN, true)?.into_keys())
        .collect();
    refs.sort();
    if !refs.is_empty() {
        let input: String = refs.iter().map(|r| format!("delete {r}\n")).collect();
        git_input(&["update-ref", "--stdin"], repo, &input)?;
    }
    let remote = remote_name(thread);
    if succeeds(
        &["config", "--get", &format!("remote.{remote}.url")],
        Some(repo),
    ) {
        git(&["remote", "remove", &remote], Some(repo))?;
    }
    Ok(())
}

/// Move the lane: both namespaces' refs to the new thread's, and the remote to `bottle/<new>`.
///
/// One update-ref transaction moves every lane ref, so a failure moves none. `git remote rename`
/// takes the remote's fetched branches along, and the upstream of every branch of yours that tracked
/// one; registering again then points its uploadpack and receivepack at the new namespaces.
pub fn rename(repo: &Path, old: &str, new: &str) -> Result<(), String> {
    let mut moves = String::new();
    for direction in [OUT, IN] {
        let old_prefix = format!("refs/namespaces/{}/", namespace(old, direction));
        let new_prefix = format!("refs/namespaces/{}/", namespace(new, direction));
        for (r, commit) in branches(repo, old, direction, true)? {
            let rest = r.strip_prefix(&old_prefix).unwrap_or(&r);
            moves += &format!("create {new_prefix}{rest} {commit}\ndelete {r} {commit}\n");
        }
    }
    if !moves.is_empty() {
        git_input(
            &["update-ref", "--stdin"],
            repo,
            &format!("start\n{moves}commit\n"),
        )?;
    }
    if succeeds(
        &[
            "config",
            "--get",
            &format!("remote.{}.url", remote_name(old)),
        ],
        Some(repo),
    ) {
        git(
            &["remote", "rename", &remote_name(old), &remote_name(new)],
            Some(repo),
        )?;
    }
    register(repo, new)
}

/// The lane's refs: branch name (or, with `full`, the whole ref) to commit.
pub fn branches(
    repo: &Path,
    thread: &str,
    direction: &str,
    full: bool,
) -> Result<BTreeMap<String, String>, String> {
    let prefix = format!("refs/namespaces/{}/", namespace(thread, direction));
    let out = git(
        &["for-each-ref", "--format=%(objectname) %(refname)", &prefix],
        Some(repo),
    )?;
    let heads = format!("{prefix}refs/heads/");
    Ok(out
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(commit, r)| {
            let name = if full {
                r.to_string()
            } else {
                r.strip_prefix(&heads).unwrap_or(r).to_string()
            };
            (name, commit.to_string())
        })
        .collect())
}

/// The out lane's branches with commits that aren't on the user's branches or origin's.
/// Reachability, not existence: the lane's objects are in the repo either way, and go at its next gc
/// once the lane is deleted.
pub fn unintegrated(repo: &Path, thread: &str) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    for (branch, commit) in branches(repo, thread, OUT, false)? {
        if !git(
            &[
                "rev-list",
                "-n1",
                &commit,
                "--not",
                "--branches",
                "--remotes=origin",
            ],
            Some(repo),
        )?
        .is_empty()
        {
            result.push(branch);
        }
    }
    Ok(result)
}

/// Of a bottle's own branch tips (name to commit), those with commits the lane and the user don't
/// have.
pub fn unsaved(
    repo: &Path,
    thread: &str,
    tips: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    let mut lane: Vec<String> = branches(repo, thread, OUT, false)?.into_values().collect();
    lane.sort();
    lane.dedup();
    let mut result = Vec::new();
    for (name, commit) in tips {
        let missing = !succeeds(
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
            Some(repo),
        );
        let unreached = || -> Result<bool, String> {
            let mut args = vec![
                "rev-list",
                "-n1",
                commit.as_str(),
                "--not",
                "--branches",
                "--remotes=origin",
            ];
            args.extend(lane.iter().map(String::as_str));
            Ok(!git(&args, Some(repo))?.is_empty())
        };
        if missing || unreached()? {
            result.push(name.clone());
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::{self, git as g};
    use std::process::Command;

    /// Real git: the lane's namespaces in a repo, and the remote that reads and writes them.
    struct Lane {
        _env: tempdir::Isolated,
        _tmp: tempdir::Dir,
        repo: PathBuf,
        head: String,
    }

    impl Lane {
        fn new() -> Lane {
            let env = tempdir::isolated();
            // What the lane's hook runs.
            tempdir::set(&env, "FOLIO_CLI", tempdir::folio().to_str().unwrap());
            let tmp = tempdir::Dir::new();
            let repo = tempdir::repo(&tmp.path().join("project"));
            register(&repo, "t").unwrap();
            let head = g(&["-C", repo.to_str().unwrap(), "rev-parse", "HEAD"]);
            Lane {
                _env: env,
                _tmp: tmp,
                repo,
                head,
            }
        }

        fn git(&self, args: &[&str]) -> String {
            let mut all = vec!["-C", self.repo.to_str().unwrap()];
            all.extend(args);
            g(&all)
        }

        /// What the agent's `git push agent` lands as: a ref in the out lane.
        fn bottle_pushes(&self, branch: &str, commit: &str) {
            self.git(&[
                "update-ref",
                &format!("refs/namespaces/t-out/refs/heads/{branch}"),
                commit,
            ]);
        }

        /// A commit in the repo's object store that no branch reaches, as a bottle's push leaves.
        fn new_commit(&self, message: &str) -> String {
            let tree = self.git(&["rev-parse", "HEAD^{tree}"]);
            self.git(&["commit-tree", &tree, "-p", &self.head, "-m", message])
        }

        fn push(&self, args: &[&str]) -> std::process::Output {
            Command::new("git")
                .arg("push")
                .args(args)
                .current_dir(&self.repo)
                .output()
                .unwrap()
        }

        fn lane(&self) -> BTreeMap<String, String> {
            branches(&self.repo, "t", OUT, false).unwrap()
        }
    }

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_bottle_is_served_each_lane_as_a_remote_of_its_own() {
        assert_eq!(
            bottle_remotes("t"),
            [
                "origin:fetch=upstream",
                "host:fetch=heads",
                "agent:fetch=ns/t-out,push=ns/t-out",
                "user:fetch=ns/t-in"
            ]
        );
    }

    #[test]
    fn fetching_the_remote_reads_the_out_lane() {
        let lane = Lane::new();
        let commit = lane.new_commit("agent work");
        lane.bottle_pushes("genie/t-1", &commit);
        lane.git(&["fetch", "-q", "bottle/t"]);
        assert_eq!(lane.git(&["rev-parse", "bottle/t/genie/t-1"]), commit);
        lane.git(&["switch", "-q", "genie/t-1"]); // tracks it, with no more setup
        assert_eq!(
            lane.git(&["rev-parse", "--abbrev-ref", "@{u}"]),
            "bottle/t/genie/t-1"
        );
    }

    #[test]
    fn pushing_to_the_remote_writes_the_in_lane() {
        let lane = Lane::new();
        lane.git(&["push", "-q", "bottle/t", "main:refs/heads/handed"]);
        assert_eq!(
            branches(&lane.repo, "t", IN, false).unwrap(),
            map(&[("handed", &lane.head)])
        );
        assert_eq!(lane.lane(), map(&[]));
    }

    #[test]
    fn registering_twice_leaves_one_of_each() {
        let lane = Lane::new();
        register(&lane.repo, "t").unwrap();
        for key in ["url", "fetch", "uploadpack", "receivepack"] {
            let values = lane.git(&["config", "--get-all", &format!("remote.bottle/t.{key}")]);
            assert_eq!(values.lines().count(), 1, "{key}");
        }
    }

    #[test]
    fn unintegrated_is_by_reachability() {
        let lane = Lane::new();
        lane.bottle_pushes("genie/t-1", &lane.new_commit("agent work"));
        lane.bottle_pushes("merged", &lane.head);
        assert_eq!(unintegrated(&lane.repo, "t").unwrap(), ["genie/t-1"]);
        lane.git(&["fetch", "-q", "bottle/t"]);
        assert_eq!(unintegrated(&lane.repo, "t").unwrap(), ["genie/t-1"]); // the remote's copy goes with the lane
        lane.git(&["branch", "kept", "bottle/t/genie/t-1"]);
        assert!(unintegrated(&lane.repo, "t").unwrap().is_empty());
    }

    #[test]
    fn unsaved_is_whats_neither_in_the_lane_nor_the_repo() {
        let lane = Lane::new();
        let pushed = lane.new_commit("pushed");
        lane.bottle_pushes("genie/t-1", &pushed);
        let missing = "1".repeat(40);
        let tips = map(&[
            ("genie/t-1", &pushed),
            ("main", &lane.head),
            ("local", &missing),
        ]);
        assert_eq!(unsaved(&lane.repo, "t", &tips).unwrap(), ["local"]);
    }

    #[test]
    fn forget_removes_both_lanes_and_the_remote() {
        let lane = Lane::new();
        lane.bottle_pushes("genie/t-1", &lane.new_commit("agent work"));
        lane.git(&["push", "-q", "bottle/t", "main:refs/heads/handed"]);
        lane.git(&["fetch", "-q", "bottle/t"]);
        forget(&lane.repo, "t").unwrap();
        let refs = lane.git(&["for-each-ref", "--format=%(refname)"]);
        assert!(
            !refs.contains("namespaces") && !refs.contains("bottle/t"),
            "{refs}"
        );
        assert_eq!(lane.git(&["remote"]), "");
        forget(&lane.repo, "t").unwrap(); // and again, with nothing left: fine
    }

    #[test]
    fn another_threads_lane_is_left_alone() {
        let lane = Lane::new();
        register(&lane.repo, "t-2").unwrap();
        lane.git(&[
            "update-ref",
            "refs/namespaces/t-2-out/refs/heads/x",
            &lane.head,
        ]);
        forget(&lane.repo, "t").unwrap();
        assert_eq!(
            branches(&lane.repo, "t-2", OUT, false).unwrap(),
            map(&[("x", &lane.head)])
        );
        assert_eq!(lane.git(&["remote"]), "bottle/t-2");
    }

    #[test]
    fn renaming_moves_both_lanes_and_the_remote() {
        let lane = Lane::new();
        let work = lane.new_commit("agent work");
        lane.bottle_pushes("genie/t-1", &work);
        lane.git(&["push", "-q", "bottle/t", "main:refs/heads/handed"]);
        rename(&lane.repo, "t", "u").unwrap();
        assert_eq!(
            branches(&lane.repo, "u", OUT, false).unwrap(),
            map(&[("genie/t-1", &work)])
        );
        assert_eq!(
            branches(&lane.repo, "u", IN, false).unwrap(),
            map(&[("handed", &lane.head)])
        );
        assert_eq!(lane.lane(), map(&[]));
        assert_eq!(lane.git(&["remote"]), "bottle/u");
        let pack = lane.git(&["config", "remote.bottle/u.uploadpack"]);
        assert_eq!(pack, "git --namespace=u-out upload-pack");
    }

    /// A delete pushed to the lane remote: the agent's branch goes too, if it's still what was
    /// fetched.
    fn fetched() -> (Lane, String) {
        let lane = Lane::new();
        let work = lane.new_commit("agent work");
        lane.bottle_pushes("genie/t-1", &work);
        lane.git(&["fetch", "-q", "bottle/t"]);
        (lane, work)
    }

    #[test]
    fn a_deleted_branch_goes_from_the_agents_lane_with_its_tracking_ref() {
        let (lane, _) = fetched();
        let result = lane.push(&["bottle/t", ":refs/heads/genie/t-1"]);
        let said = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{said}");
        assert!(
            said.contains("folio: deleted genie/t-1 from the agent's lane"),
            "{said}"
        );
        assert_eq!(lane.lane(), map(&[]));
        assert_eq!(
            lane.git(&[
                "for-each-ref",
                "--format=%(refname)",
                "refs/remotes/bottle/t/"
            ]),
            ""
        );
    }

    #[test]
    fn one_the_agent_pushed_to_since_is_kept_and_the_push_refused() {
        let (lane, _) = fetched();
        let later = lane.new_commit("more");
        lane.bottle_pushes("genie/t-1", &later);
        let result = lane.push(&["bottle/t", ":refs/heads/genie/t-1"]);
        let said = String::from_utf8_lossy(&result.stderr);
        assert!(!result.status.success());
        assert!(
            said.contains("folio: kept genie/t-1: the agent has pushed to it since you fetched it"),
            "{said}"
        );
        assert_eq!(lane.lane(), map(&[("genie/t-1", &later)]));
    }

    #[test]
    fn one_alongside_a_handover_goes_too() {
        let (lane, _) = fetched();
        let result = lane.push(&["bottle/t", "main", ":refs/heads/genie/t-1"]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(lane.lane(), map(&[]));
        assert!(
            branches(&lane.repo, "t", IN, false)
                .unwrap()
                .contains_key("main")
        );
    }

    #[test]
    fn curate_integrating_it_deletes_it() {
        let (lane, _) = fetched();
        let curate = concat!(env!("CARGO_MANIFEST_DIR"), "/../../bin/curate");
        let run = |args: &[&str]| {
            Command::new(curate)
                .args(args)
                .current_dir(&lane.repo)
                .output()
                .unwrap()
        };
        lane.git(&["switch", "-q", "genie/t-1"]);
        for args in [&["start", "--from", lane.head.as_str()][..], &["read"]] {
            let result = run(args);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
        lane.git(&["add", "-A"]);
        let result = run(&["integrate", "--into", "main"]);
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            String::from_utf8_lossy(&result.stdout).contains("Deleted genie/t-1 from bottle/t.")
        );
        assert_eq!(lane.lane(), map(&[]));
    }

    #[test]
    fn quoted_as_shlex_quotes() {
        assert_eq!(shell_quote("/a/b-c_d.e"), "/a/b-c_d.e");
        assert_eq!(shell_quote("it's here"), r#"'it'"'"'s here'"#);
        assert_eq!(shell_quote(""), "''");
    }
}
