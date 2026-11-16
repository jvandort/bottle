//! bottle.host as a bottle reaches it: folio serve, told of a bottle and the repos it's served, and
//! real git in a bottle-style workspace talking to it through a stand-in for the bottle's machine
//! (harness::bridge). Its remotes' views and rules, its lanes, and the host's branches following a
//! bottle's push.

mod harness;

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use harness::{Folio, git_in};
use serde_json::{Value, json};

const MACHINE: &str = "t-m";

/// One repo a bottle is served, as folio serve is told of it: its git dir, its remotes' specs, and
/// where under bottle.host/git they are.
fn served(git_dir: &Path, remotes: &[&str]) -> Value {
    json!({"git_dir": git_dir, "at": "", "remotes": remotes})
}

fn bind_message(repos: Vec<Value>) -> Value {
    json!({"op": "bind", "bottle": "t", "machine": MACHINE, "runner": "local", "id": "", "features": [],
           "permissions": [], "labels": {}, "repo": null, "ssh": [], "forwards": [], "repos": repos})
}

/// One request to folio serve, on its control socket, as folio makes one: its answer.
fn control(f: &Folio, message: &Value) -> Value {
    let mut s = UnixStream::connect(f.home.join("serve.sock")).unwrap();
    s.write_all(format!("{message}\n").as_bytes()).unwrap();
    s.shutdown(std::net::Shutdown::Write).unwrap();
    let mut answer = String::new();
    s.read_to_string(&mut answer).unwrap();
    serde_json::from_str(answer.lines().next().unwrap()).unwrap()
}

/// folio serve, given a bottle served `repos`, and git's http://bottle.host/ pointed at a stand-in
/// for its machine: its port, and the environment git runs with.
fn serve_bottle_host(f: &Folio, repos: Vec<Value>) -> (u16, Vec<(String, String)>) {
    f.ok(&["serve", "start"]);
    let reply = control(f, &bind_message(repos));
    assert_eq!(reply["ok"], true, "{reply}");
    let port = harness::bridge(f.home.join("services/host.sock"), MACHINE.into());
    let mut env = f.env.clone();
    env.extend([
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        (
            "GIT_CONFIG_KEY_0".into(),
            format!("url.http://127.0.0.1:{port}/.insteadOf"),
        ),
        ("GIT_CONFIG_VALUE_0".into(), "http://bottle.host/".into()),
    ]);
    (port, env)
}

fn served_log(f: &Folio) -> String {
    fs::read_to_string(f.home.join("logs/serve.log")).unwrap_or_default()
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

/// A workspace, as a bottle's is, and git run there with bottle.host's environment.
struct Workspace {
    f: Folio,
    dir: PathBuf,
    env: Vec<(String, String)>,
    port: u16,
}

impl Workspace {
    fn git(&self, args: &[&str]) -> Output {
        Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .envs(self.env.iter().cloned())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.git(args);
        assert!(out.status.success(), "git {args:?}: {}", stderr(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn ls_remote(&self, remote: &str) -> Vec<String> {
        let out = self.git(&["ls-remote", &format!("http://bottle.host/git/{remote}")]);
        assert!(out.status.success(), "{}", stderr(&out));
        let mut refs: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.split('\t').nth(1).unwrap().to_string())
            .collect();
        refs.sort();
        refs
    }

    fn branches(&self) -> Vec<String> {
        git_in(&self.dir, &["branch", "-r", "--format=%(refname:short)"])
            .split_whitespace()
            .map(String::from)
            .collect()
    }

    /// A raw request to bottle.host: what came back.
    fn request(&self, raw: &[u8]) -> Vec<u8> {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        s.write_all(raw).unwrap();
        let mut answer = Vec::new();
        let _ = s.read_to_end(&mut answer);
        answer
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

// --- a bottle's remotes: origin, host, and its lanes ----------------------------------------

/// The lane this bottle writes and the host reads, and the one the host writes and it reads.
const OUT: &str = "refs/namespaces/bottle-b-out";
const IN: &str = "refs/namespaces/bottle-b-in";

/// upstream stands in for the repo's real remote (GitHub, say); repo is the host's, a clone of it
/// with local branches of its own; the workspace is set up as a bottle's /workspace.
struct Remotes {
    w: Workspace,
    upstream: PathBuf,
    repo: PathBuf,
    served: Value,
}

impl Remotes {
    fn new() -> Remotes {
        let f = Folio::new();
        let upstream = f.make_repo("upstream");
        git_in(&upstream, &["branch", "release"]);
        git_in(&upstream, &["tag", "v1"]);
        let repo = f.tmp.join("repo");
        f.git(&["clone", "-q", &s(&upstream), &s(&repo)]);
        git_in(&repo, &["branch", "mine"]);
        // What a bottle made from a registered repo is served.
        let served = served(
            &repo.join(".git"),
            &[
                "origin:fetch=upstream",
                "host:fetch=heads",
                "work:fetch=ns/bottle-b-in,push=ns/bottle-b-out",
            ],
        );
        let (port, env) = serve_bottle_host(&f, vec![served.clone()]);
        let dir = f.tmp.join("workspace");
        f.git(&["init", "-q", "-b", "main", &s(&dir)]);
        for remote in ["origin", "host", "work"] {
            git_in(
                &dir,
                &[
                    "remote",
                    "add",
                    remote,
                    &format!("http://bottle.host/git/{remote}"),
                ],
            );
        }
        git_in(&dir, &["config", "checkout.defaultRemote", "origin"]);
        Remotes {
            w: Workspace { f, dir, env, port },
            upstream,
            repo,
            served,
        }
    }

    fn fetch(&self) {
        self.w.ok(&["fetch", "-q", "--multiple", "origin", "host"]);
    }

    fn push(&self, args: &[&str]) -> Output {
        let mut all = vec!["push"];
        all.extend(args);
        self.w.git(&all)
    }

    fn work(&self, message: &str) -> String {
        self.fetch();
        self.w.ok(&["checkout", "-q", "-b", "work", "host/main"]);
        self.w.f.commit(&self.w.dir, message)
    }

    fn pushed(&self, branch: &str) -> String {
        git_in(
            &self.repo,
            &["rev-parse", &format!("{OUT}/refs/heads/{branch}")],
        )
    }

    /// What the host's own remote does: write its lane, which the bottle reads.
    fn host_writes(&self, branch: &str, commit: &str) {
        git_in(
            &self.repo,
            &["update-ref", &format!("{IN}/refs/heads/{branch}"), commit],
        );
    }

    fn host_refs(&self) -> Vec<String> {
        git_in(&self.repo, &["for-each-ref", "--format=%(refname)"])
            .split_whitespace()
            .map(String::from)
            .collect()
    }
}

#[test]
fn origin_is_the_repos_upstream() {
    let r = Remotes::new();
    assert_eq!(
        r.w.ls_remote("origin"),
        [
            "HEAD",
            "refs/heads/main",
            "refs/heads/release",
            "refs/tags/v1"
        ]
    );
}

#[test]
fn an_upstream_branch_is_checked_out_by_name() {
    let r = Remotes::new();
    r.fetch();
    r.w.ok(&["checkout", "-q", "release"]);
    assert_eq!(
        git_in(&r.w.dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/release"
    );
}

#[test]
fn pull_through_gets_upstream_commits_the_host_never_fetched() {
    let r = Remotes::new();
    let new = r.w.f.commit(&r.upstream, "pushed upstream");
    r.fetch();
    assert_eq!(git_in(&r.w.dir, &["rev-parse", "origin/main"]), new);
    // The host repo was fetched.
    assert_eq!(git_in(&r.repo, &["rev-parse", "origin/main"]), new);
}

#[test]
fn every_fetch_pulls_through() {
    let r = Remotes::new();
    r.fetch();
    let later = r.w.f.commit(&r.upstream, "just after");
    r.fetch();
    assert_eq!(git_in(&r.w.dir, &["rev-parse", "origin/main"]), later);
}

#[test]
fn a_failed_pull_through_still_serves_what_the_repo_has() {
    let r = Remotes::new();
    git_in(
        &r.repo,
        &["remote", "set-url", "origin", &s(&r.w.f.tmp.join("gone"))],
    );
    r.fetch();
    assert!(r.w.branches().contains(&"origin/release".to_string()));
    assert!(served_log(&r.w.f).contains("pull-through: git fetch origin"));
}

#[test]
fn origin_is_read_only() {
    let r = Remotes::new();
    r.fetch();
    r.w.ok(&["checkout", "-q", "release"]);
    assert!(stderr(&r.push(&["origin", "HEAD:refs/heads/x"])).contains("403"));
}

#[test]
fn host_is_the_repos_local_branches() {
    let r = Remotes::new();
    r.fetch();
    let branches = r.w.branches();
    assert!(
        branches.contains(&"host/main".to_string()) && branches.contains(&"host/mine".to_string()),
        "{branches:?}"
    );
}

#[test]
fn host_is_read_only() {
    let r = Remotes::new();
    r.work("agent work");
    let out = r.push(&["host", "work"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("403"));
}

#[test]
fn a_push_writes_the_bottles_own_lane() {
    let r = Remotes::new();
    r.work("agent work");
    let out = r.push(&["work", "work"]);
    assert!(out.status.success(), "{}", stderr(&out));
    // An ordinary `git push work work` lands in the lane, with no refspec to configure and nothing
    // for the agent to know.
    assert_eq!(r.pushed("work"), git_in(&r.w.dir, &["rev-parse", "work"]));
}

#[test]
fn a_fetch_reads_the_lane_the_host_writes() {
    let r = Remotes::new();
    r.work("agent work");
    let mine = git_in(&r.repo, &["rev-parse", "mine"]);
    r.host_writes("review", &mine);
    r.w.ok(&["fetch", "-q", "work"]);
    assert_eq!(git_in(&r.w.dir, &["rev-parse", "work/review"]), mine);
}

#[test]
fn the_lanes_are_separate() {
    // What the bottle pushes is not what it reads back, or the loop is a mirror.
    let r = Remotes::new();
    r.work("agent work");
    r.push(&["work", "work"]);
    assert!(
        r.host_refs()
            .iter()
            .any(|r| r.starts_with(&format!("{OUT}/")))
    );
    assert!(
        !r.host_refs()
            .iter()
            .any(|r| r.starts_with(&format!("{IN}/")))
    );
    // `git push` optimistically updates the remote-tracking ref, which is read from the other
    // lane, so it disagrees until a pruning fetch.
    r.w.ok(&["fetch", "-q", "--prune", "work"]);
    assert!(!r.w.branches().contains(&"work/work".to_string()));
}

#[test]
fn the_hosts_refs_are_out_of_the_bottles_reach() {
    // Not refused, unreachable: every ref receive-pack resolves is inside the lane, so naming main
    // writes the lane's main, not the host's.
    let r = Remotes::new();
    let before = r.host_refs();
    r.work("agent work");
    for target in [
        "HEAD:main",
        "HEAD:mine",
        "HEAD:refs/tags/v1",
        "HEAD:refs/remotes/origin/main",
    ] {
        let out = r.push(&["work", target]);
        assert!(out.status.success(), "{target}: {}", stderr(&out));
    }
    assert_eq!(
        git_in(&r.repo, &["rev-parse", "main"]),
        git_in(&r.upstream, &["rev-parse", "main"])
    );
    for new in r.host_refs().iter().filter(|r| !before.contains(r)) {
        assert!(new.starts_with(&format!("{OUT}/")), "{new}");
    }
}

#[test]
fn a_push_refuses_to_rewrite_what_it_has_pushed() {
    // The user's view of the branch stays append-only: every fetch of it fast-forwards, and
    // nothing they have read changes underneath them.
    let r = Remotes::new();
    r.work("first");
    r.push(&["work", "work"]);
    let first = r.pushed("work");
    git_in(&r.w.dir, &["reset", "-q", "--hard", "HEAD~1"]);
    r.w.f.commit(&r.w.dir, "rewritten");
    for extra in [&[][..], &["--force"]] {
        let mut args = extra.to_vec();
        args.extend(["work", "work"]);
        assert!(!r.push(&args).status.success(), "{extra:?}");
        assert_eq!(r.pushed("work"), first);
    }
    assert!(
        stderr(&r.push(&["--force", "work", "work"]))
            .to_lowercase()
            .contains("non-fast-forward")
    );
}

#[test]
fn a_push_refuses_deletes() {
    // Only the host retires a name, since a delete takes the ref's reflog with it.
    let r = Remotes::new();
    r.work("agent work");
    r.push(&["work", "work"]);
    let pushed = r.pushed("work");
    let out = r.push(&["work", ":work"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("deletion prohibited"),
        "{}",
        stderr(&out)
    );
    assert_eq!(r.pushed("work"), pushed);
}

#[test]
fn a_remote_that_would_push_to_no_namespace_isnt_served() {
    // Fail closed: with no namespace a push would write the host's own refs. folio serve takes
    // remotes only as specs, and refuses one that would push to no namespace.
    let r = Remotes::new();
    r.work("agent work");
    let mut message = bind_message(vec![r.served.clone()]);
    message["repos"] = json!([{"git_dir": r.repo.join(".git"), "at": "", "remotes": ["work:fetch=heads,push=ns/"]}]);
    let reply = control(&r.w.f, &message);
    assert_eq!(reply["ok"], false);
    assert!(
        reply["error"].as_str().unwrap().contains("invalid remote"),
        "{reply}"
    );
    assert!(!r.host_refs().contains(&"refs/heads/work".to_string()));
}

#[test]
fn the_repos_own_hooks_never_run() {
    let r = Remotes::new();
    let hook = r.repo.join(".git/hooks/pre-receive");
    fs::write(&hook, "#!/bin/sh\necho repo hook ran >&2\nexit 1\n").unwrap();
    Command::new("chmod")
        .arg("755")
        .arg(&hook)
        .status()
        .unwrap();
    r.work("agent work");
    let out = r.push(&["work", "work"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(!stderr(&out).contains("repo hook ran"));
}

#[test]
fn other_paths_are_not_found() {
    let r = Remotes::new();
    for path in ["/nope", "/git", "/git/other/info/refs"] {
        let answer =
            r.w.request(format!("GET {path} HTTP/1.1\r\nHost: bottle.host\r\n\r\n").as_bytes());
        assert!(
            answer.starts_with(b"HTTP/1.1 404"),
            "{path}: {}",
            String::from_utf8_lossy(&answer[..40.min(answer.len())])
        );
    }
}

// --- the host's branches follow a bottle's push -------------------------------------------

/// The host repo set up as folio sets one up: a remote, bottle/t, reading the bottle's lane, and a
/// branch genie/x tracking the lane's genie/x, checked out.
struct Follow {
    w: Workspace,
    repo: PathBuf,
}

impl Follow {
    fn new() -> Follow {
        let f = Folio::new();
        let repo = f.make_repo("app");
        let (port, env) = serve_bottle_host(
            &f,
            vec![served(
                &repo.join(".git"),
                &["agent:fetch=ns/t-out,push=ns/t-out"],
            )],
        );
        for (key, value) in [
            ("url", s(&repo)),
            ("fetch", "+refs/heads/*:refs/remotes/bottle/t/*".into()),
            ("uploadpack", "git --namespace=t-out upload-pack".into()),
        ] {
            git_in(
                &repo,
                &["config", &format!("remote.bottle/t.{key}"), &value],
            );
        }
        let dir = f.tmp.join("workspace");
        f.git(&["clone", "-q", &s(&repo), &s(&dir)]);
        git_in(
            &dir,
            &["remote", "add", "agent", "http://bottle.host/git/agent"],
        );
        git_in(&dir, &["checkout", "-q", "-b", "genie/x"]);
        let follow = Follow {
            w: Workspace { f, dir, env, port },
            repo,
        };
        follow.write("a.txt", "first\n");
        follow.push();
        // As `git fetch bottle/t && git switch genie/x` does: the user's copy, tracking the bottle's.
        follow
            .w
            .f
            .git(&["-C", &s(&follow.repo), "fetch", "-q", "bottle/t"]);
        git_in(&follow.repo, &["switch", "-q", "genie/x"]);
        assert_eq!(
            git_in(
                &follow.repo,
                &["rev-parse", "--symbolic-full-name", "genie/x@{upstream}"]
            ),
            "refs/remotes/bottle/t/genie/x"
        );
        follow
    }

    /// Push genie/x from the bottle: what the push's output said about following.
    fn push(&self) -> String {
        let out = self.w.git(&["push", "agent", "genie/x"]);
        assert!(out.status.success(), "{}", stderr(&out));
        stderr(&out)
            .lines()
            .filter(|l| l.starts_with("remote: folio:"))
            .map(|l| l.trim_start_matches("remote: ").trim().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn write(&self, path: &str, text: &str) -> String {
        fs::write(self.w.dir.join(path), text).unwrap();
        git_in(&self.w.dir, &["add", path]);
        self.w.f.git(&[
            "-C",
            &s(&self.w.dir),
            "commit",
            "-q",
            "-m",
            &format!("write {path}"),
        ]);
        git_in(&self.w.dir, &["rev-parse", "HEAD"])
    }

    fn users(&self, branch: &str) -> String {
        git_in(&self.repo, &["rev-parse", branch])
    }
}

#[test]
fn a_tracking_branch_follows_files_and_all_and_the_bottle_is_told() {
    let f = Follow::new();
    let pushed = f.write("a.txt", "second\n");
    let said = f.push();
    assert_eq!(f.users("genie/x"), pushed);
    assert_eq!(
        fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "second\n"
    );
    // As if the user had fetched.
    assert_eq!(f.users("bottle/t/genie/x"), pushed);
    assert_eq!(
        said,
        format!(
            "folio: moved the user's genie/x to {}, files and all",
            &pushed[..12]
        )
    );
    assert!(served_log(&f.w.f).contains("moved the user's genie/x"));
}

#[test]
fn uncommitted_changes_elsewhere_come_along() {
    let f = Follow::new();
    fs::write(f.repo.join("mine.txt"), "not committed\n").unwrap();
    let pushed = f.write("a.txt", "second\n");
    f.push();
    assert_eq!(f.users("genie/x"), pushed);
    assert_eq!(
        fs::read_to_string(f.repo.join("mine.txt")).unwrap(),
        "not committed\n"
    );
}

#[test]
fn not_over_a_change_in_the_way() {
    let f = Follow::new();
    fs::write(f.repo.join("a.txt"), "the user's edit\n").unwrap();
    let before = f.users("genie/x");
    f.write("a.txt", "second\n");
    let said = f.push();
    assert_eq!(f.users("genie/x"), before);
    assert_eq!(
        fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "the user's edit\n"
    );
    assert_eq!(
        said,
        "folio: didn't move the user's genie/x: Your local changes to the following files would be overwritten by merge: a.txt"
    );
}

#[test]
fn not_over_commits_of_the_users() {
    let f = Follow::new();
    let mine = f.w.f.commit(&f.repo, "the user's own");
    f.write("a.txt", "second\n");
    let said = f.push();
    assert_eq!(f.users("genie/x"), mine);
    assert_eq!(
        said,
        "folio: didn't move the user's genie/x: it has commits yours doesn't"
    );
}

#[test]
fn not_in_the_middle_of_a_git_operation() {
    let f = Follow::new();
    let before = f.users("genie/x");
    fs::write(f.repo.join(".git/MERGE_HEAD"), format!("{before}\n")).unwrap();
    f.write("a.txt", "second\n");
    let said = f.push();
    assert_eq!(f.users("genie/x"), before);
    assert_eq!(
        said,
        "folio: didn't move the user's genie/x: it's in the middle of a merge"
    );
}

#[test]
fn not_when_the_branch_isnt_checked_out() {
    // In curate's read mode, HEAD is curate/genie/x: the branch isn't checked out, so nothing moves.
    let f = Follow::new();
    git_in(&f.repo, &["switch", "-q", "-c", "curate/genie/x"]);
    let before = f.users("genie/x");
    let pushed = f.write("a.txt", "second\n");
    assert_eq!(f.push(), "");
    assert_eq!(f.users("genie/x"), before);
    assert_eq!(fs::read_to_string(f.repo.join("a.txt")).unwrap(), "first\n");
    // Fetched all the same.
    assert_eq!(f.users("bottle/t/genie/x"), pushed);
}

#[test]
fn a_branch_youre_reading_in_curate_comes_along_by_curates_own_hook() {
    let f = Follow::new();
    let curate = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../bin/curate"));
    for args in [&["start", "--from", "main"][..], &["read"]] {
        let out = Command::new(&curate)
            .args(args)
            .current_dir(&f.repo)
            .envs(f.w.f.env.iter().cloned())
            .output()
            .unwrap();
        assert!(out.status.success(), "curate {args:?}: {}", stderr(&out));
    }
    assert_eq!(
        git_in(&f.repo, &["symbolic-ref", "HEAD"]),
        "refs/heads/curate/genie/x"
    );
    let pushed = f.write("a.txt", "second\n");
    let said = f.push();
    assert_eq!(f.users("genie/x"), pushed);
    assert_eq!(
        fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "second\n"
    );
    assert_eq!(
        git_in(&f.repo, &["symbolic-ref", "HEAD"]),
        "refs/heads/curate/genie/x"
    );
    assert!(
        said.contains("curate: 'genie/x' followed bottle/t/genie/x"),
        "{said}"
    );
}

#[test]
fn in_whichever_worktree_has_it() {
    let f = Follow::new();
    git_in(&f.repo, &["switch", "-q", "main"]);
    let elsewhere = f.w.f.tmp.join("review");
    git_in(
        &f.repo,
        &["worktree", "add", "-q", &s(&elsewhere), "genie/x"],
    );
    let pushed = f.write("a.txt", "second\n");
    let said = f.push();
    assert_eq!(f.users("genie/x"), pushed);
    assert_eq!(
        fs::read_to_string(elsewhere.join("a.txt")).unwrap(),
        "second\n"
    );
    assert_eq!(
        said,
        format!(
            "folio: moved the user's genie/x (in worktree review) to {}, files and all",
            &pushed[..12]
        )
    );
    // No host paths for the bottle.
    assert!(!said.contains(&s(&f.w.f.tmp)));
}

#[test]
fn branches_tracking_anything_else_stay_put() {
    let f = Follow::new();
    git_in(&f.repo, &["branch", "--unset-upstream", "genie/x"]);
    let before = f.users("genie/x");
    f.write("a.txt", "second\n");
    assert_eq!(f.push(), "");
    assert_eq!(f.users("genie/x"), before);
}

#[test]
fn the_repo_can_say_no() {
    let f = Follow::new();
    git_in(&f.repo, &["config", "bottle.follow", "false"]);
    let before = f.users("genie/x");
    f.write("a.txt", "second\n");
    assert_eq!(f.push(), "");
    assert_eq!(f.users("genie/x"), before);
}

#[test]
fn ignored_files_dont_stop_it() {
    let f = Follow::new();
    f.write(".gitignore", ".obsidian/\n");
    f.push();
    fs::create_dir_all(f.repo.join(".obsidian")).unwrap();
    fs::write(f.repo.join(".obsidian/workspace.json"), "{}").unwrap();
    let pushed = f.write("Now.md", "x\n");
    f.push();
    assert_eq!(f.users("genie/x"), pushed);
}

// --- remotes a caller named itself: the same views and rules, under its names ---------------

struct Named {
    w: Workspace,
    repo: PathBuf,
}

impl Named {
    fn new() -> Named {
        let f = Folio::new();
        let upstream = f.make_repo("upstream");
        git_in(&upstream, &["branch", "release"]);
        let repo = f.tmp.join("repo");
        f.git(&["clone", "-q", &s(&upstream), &s(&repo)]);
        git_in(&repo, &["branch", "mine"]);
        let remotes = [
            "up:fetch=upstream",
            "theirs:fetch=heads",
            "lane:fetch=ns/t-in,push=ns/t-out",
            "own:fetch=ns/o,push=ns/o",
        ];
        let (port, env) = serve_bottle_host(&f, vec![served(&repo.join(".git"), &remotes)]);
        let dir = f.tmp.join("workspace");
        f.git(&["init", "-q", "-b", "main", &s(&dir)]);
        for remote in ["up", "theirs", "lane", "own", "origin", "work"] {
            git_in(
                &dir,
                &[
                    "remote",
                    "add",
                    remote,
                    &format!("http://bottle.host/git/{remote}"),
                ],
            );
        }
        Named {
            w: Workspace { f, dir, env, port },
            repo,
        }
    }

    /// Working on a branch of theirs: one commit on it.
    fn agent(&self) -> String {
        self.w.ok(&["fetch", "-q", "theirs"]);
        self.w.ok(&["checkout", "-q", "-b", "agent", "theirs/main"]);
        self.w.f.commit(&self.w.dir, "agent work")
    }
}

#[test]
fn each_view() {
    let n = Named::new();
    assert_eq!(
        n.w.ls_remote("up"),
        ["HEAD", "refs/heads/main", "refs/heads/release"]
    );
    let theirs = n.w.ls_remote("theirs");
    assert!(
        theirs.contains(&"refs/heads/main".to_string())
            && theirs.contains(&"refs/heads/mine".to_string())
    );
    // Nothing in t-in yet.
    assert!(n.w.ls_remote("lane").is_empty());
    git_in(
        &n.repo,
        &[
            "update-ref",
            "refs/namespaces/t-in/refs/heads/handed",
            "HEAD",
        ],
    );
    assert_eq!(n.w.ls_remote("lane"), ["refs/heads/handed"]);
}

#[test]
fn names_it_wasnt_given_arent_served() {
    let n = Named::new();
    for remote in ["origin", "work"] {
        let out =
            n.w.git(&["ls-remote", &format!("http://bottle.host/git/{remote}")]);
        assert!(!out.status.success(), "{remote}");
        assert!(
            stderr(&out).contains("no such bottle.host service"),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn a_push_lands_in_its_namespace_append_only() {
    let n = Named::new();
    let first = n.agent();
    let out = n.w.git(&["push", "lane", "agent"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        git_in(
            &n.repo,
            &["rev-parse", "refs/namespaces/t-out/refs/heads/agent"]
        ),
        first
    );
    assert!(
        !git_in(&n.repo, &["for-each-ref", "--format=%(refname)"])
            .split_whitespace()
            .any(|r| r == "refs/heads/agent")
    );
    n.w.ok(&["reset", "-q", "--hard", "HEAD~"]);
    n.w.f.commit(&n.w.dir, "a rewrite");
    assert!(
        !n.w.git(&["push", "--force", "lane", "agent"])
            .status
            .success()
    );
    assert_eq!(
        git_in(
            &n.repo,
            &["rev-parse", "refs/namespaces/t-out/refs/heads/agent"]
        ),
        first
    );
    assert!(stderr(&n.w.git(&["push", "lane", ":agent"])).contains("deletion prohibited"));
}

#[test]
fn a_remote_that_fetches_what_it_pushes_is_ordinary_git() {
    // Pushing, then seeing the push, tracking it, and git status saying where you are.
    let n = Named::new();
    let first = n.agent();
    n.w.ok(&["push", "-q", "-u", "own", "agent"]);
    assert_eq!(git_in(&n.w.dir, &["rev-parse", "own/agent"]), first);
    assert_eq!(
        git_in(&n.w.dir, &["rev-parse", "--abbrev-ref", "@{u}"]),
        "own/agent"
    );
    n.w.f.commit(&n.w.dir, "more");
    let status = n.w.ok(&["status", "-sb"]);
    assert_eq!(
        status.lines().next().unwrap(),
        "## agent...own/agent [ahead 1]"
    );
    n.w.ok(&["push", "-q"]);
    // Nothing new, and nothing refused.
    n.w.ok(&["pull", "-q"]);
}

#[test]
fn read_only_remotes_say_where_to_push() {
    let n = Named::new();
    n.agent();
    for remote in ["up", "theirs"] {
        let out = n.w.git(&["push", remote, "agent"]);
        assert!(!out.status.success(), "{remote}");
        assert!(stderr(&out).contains("403"), "{remote}: {}", stderr(&out));
    }
}
