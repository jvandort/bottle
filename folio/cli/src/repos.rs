//! Repos: what folio knows about each repo it has been used in, and what a bottle needs to know about
//! a repo it's made with (a mount's).
//!
//! There's no registering a repo: the first folio command in one gives it a directory,
//! ~/.folio/<key>/. The key is the repo's name and a hash of its path, `<name>-<hash>`: readable,
//! and two repos with the same name never collide. The path is the toplevel as `git rev-parse
//! --show-toplevel` gives it, which on macOS resolves symlinks and has the casing that's on disk, so
//! however the repo's directory is typed, the key is the same. Moving a repo orphans what folio
//! knows about it; that's for later.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::git::{bottle_git, git};
use crate::store;

/// What folio needs in every thread's bottle, by feature, and what for: every repo's spec has them,
/// one saved before they were needed too, and none can be taken away.
pub const REQUIRED_FEATURES: [(&str, &str); 1] =
    [("tools", "shpool, which every thread's sessions run in")];
/// What a new repo's bottles get until `folio configure` says otherwise.
pub const DEFAULT_MEMORY: &str = "2G";
/// Under folio's home, the repos folio owns (owned_path).
const OWNED: &str = "repos";
/// What a repo's spec can't say: a thread's bottle never has permissions, and folio labels each
/// with its thread itself.
const NOT_IN_A_REPOS_SPEC: [&str; 3] = ["permissions", "labels", "thread"];

/// What a new repo's bottles get: what folio needs, and memory, which every spec gives.
pub fn default_spec() -> Value {
    json!({"features": REQUIRED_FEATURES.iter().map(|(f, _)| *f).collect::<Vec<_>>(), "memory": DEFAULT_MEMORY})
}

/// `spec`, if it's one a repo's threads (or, with `sidecar`, their sidecars, which run no
/// sessions) can be made from.
pub fn check_repo_spec(spec: Value, sidecar: bool) -> Result<Value, String> {
    let given: Vec<&str> = NOT_IN_A_REPOS_SPEC
        .into_iter()
        .filter(|key| spec.get(key).is_some())
        .collect();
    if !given.is_empty() {
        return Err(format!(
            "a repo's spec can't give {}: its threads' bottles never have permissions, and each one's labels \
             are folio's, saying which thread it is",
            given.join(" or ")
        ));
    }
    let there = feature_names(&spec);
    for (feature, why) in REQUIRED_FEATURES {
        if !there.iter().any(|f| f == feature) && !sidecar {
            return Err(format!("folio's threads need the {feature} feature: {why}"));
        }
    }
    Ok(spec)
}

fn feature_names(spec: &Value) -> Vec<String> {
    features_of(spec)
        .iter()
        .map(|f| name_of(f).to_string())
        .collect()
}

/// A spec's features, as given.
pub fn features_of(spec: &Value) -> Vec<String> {
    spec.get("features")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|f| f.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// A feature spec's name: what's before its options.
pub fn name_of(feature: &str) -> &str {
    feature.split(':').next().unwrap_or(feature)
}

/// `features`, and any of folio's REQUIRED_FEATURES they lack.
pub fn with_required(features: Vec<String>) -> Vec<String> {
    let mut result = features;
    for (feature, _) in REQUIRED_FEATURES {
        if !result.iter().any(|f| name_of(f) == feature) {
            result.push(feature.to_string());
        }
    }
    result
}

/// `current` with `given` added: a feature given again replaces its options, where it was.
pub fn merge_features(current: &[String], given: &[String]) -> Vec<String> {
    let mut merged: Vec<(String, String)> = Vec::new();
    for spec in current.iter().chain(given) {
        let name = name_of(spec).to_string();
        match merged.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = spec.clone(),
            None => merged.push((name, spec.clone())),
        }
    }
    merged.into_iter().map(|(_, spec)| spec).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Repo {
    pub name: String,
    pub path: PathBuf,
}

impl Repo {
    pub fn new(name: impl Into<String>, path: impl Into<PathBuf>) -> Repo {
        Repo {
            name: name.into(),
            path: path.into(),
        }
    }

    pub fn key(&self) -> String {
        format!("{}-{}", self.name, path_hash(&self.path))
    }

    pub fn directory(&self) -> PathBuf {
        crate::home().join(self.key())
    }

    fn spec_path(&self) -> PathBuf {
        self.directory().join("spec.json")
    }

    fn record_path(&self) -> PathBuf {
        self.directory().join("repo.json")
    }

    /// The repo's bottle spec, as last saved; None until it has one. One saved before every spec had
    /// to give its memory (none, or "all") gets the default, and one saved before folio needed a
    /// feature (REQUIRED_FEATURES) has it.
    pub fn spec(&self) -> Result<Option<Value>, String> {
        let Some(mut spec) = store::read_json(&self.spec_path())? else {
            return Ok(None);
        };
        let Some(map) = spec.as_object_mut() else {
            return Ok(Some(spec));
        };
        if map.get("memory").and_then(Value::as_str).unwrap_or("all") == "all" {
            map.insert("memory".into(), DEFAULT_MEMORY.into());
        }
        let features = with_required(features_of(&Value::Object(map.clone())));
        map.insert("features".into(), json!(features));
        Ok(Some(spec))
    }

    pub fn save_spec(&self, spec: &Value) -> Result<(), String> {
        self.remember()?;
        store::write_json(&self.spec_path(), spec)
    }

    /// Give the repo its directory, if it hasn't one yet.
    pub fn remember(&self) -> Result<(), String> {
        if !self.record_path().exists() {
            store::write_json(
                &self.record_path(),
                &Record {
                    name: &self.name,
                    path: self.path.to_string_lossy().into_owned(),
                    runner: None,
                },
            )?;
        }
        Ok(())
    }

    /// The runner its threads' bottles are made on, unless a thread says otherwise; None: bottle's
    /// default.
    pub fn runner(&self) -> Result<Option<String>, String> {
        Ok(store::read_json(&self.record_path())?
            .and_then(|r| r.get("runner").and_then(Value::as_str).map(String::from)))
    }

    pub fn save_runner(&self, runner: Option<&str>) -> Result<(), String> {
        self.remember()?;
        let record = Record {
            name: &self.name,
            path: self.path.to_string_lossy().into_owned(),
            runner: runner.filter(|r| !r.is_empty()),
        };
        store::write_json(&self.record_path(), &record)
    }
}

#[derive(serde::Serialize)]
struct Record<'a> {
    name: &'a str,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    runner: Option<&'a str>,
}

pub fn path_hash(path: &Path) -> String {
    let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// A path folio names a repo of its own by, rather than one of yours: the broker's identity (where
/// its vault once was).
pub fn owned_path(name: &str) -> PathBuf {
    crate::home().join(OWNED).join(name)
}

/// The repo the working directory (or `cwd`) is in.
pub fn current(cwd: Option<&Path>) -> Result<Repo, String> {
    let toplevel = git(&["rev-parse", "--show-toplevel"], cwd)
        .map(PathBuf::from)
        .map_err(|_| "not in a git repo; folio works on the repo you're in".to_string())?;
    Ok(Repo::new(name(&toplevel), toplevel))
}

/// Every repo folio has a directory for, wherever it's run.
pub fn known() -> Vec<Repo> {
    let home = crate::home();
    let mut records: Vec<PathBuf> = fs::read_dir(&home)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("repo.json"))
        .filter(|p| p.is_file())
        .collect();
    records.sort();
    records
        .iter()
        .filter_map(|p| store::read_json(p).ok().flatten())
        .filter_map(|data| {
            Some(Repo::new(
                data.get("name")?.as_str()?,
                data.get("path")?.as_str()?,
            ))
        })
        .collect()
}

/// The origin remote's repo name, else the directory's, as something a bottle may be named after.
fn name(toplevel: &Path) -> String {
    let from_origin = git(&["remote", "get-url", "origin"], Some(toplevel))
        .map(|url| name_from_url(&url))
        .unwrap_or_default();
    let raw = if from_origin.is_empty() {
        toplevel
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    } else {
        from_origin
    };
    // Runs of anything else, a '-'; then no '-', '.' or '_' at either end.
    let mut name = String::new();
    let mut in_run = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || "._-".contains(c) {
            name.push(c);
            in_run = false;
        } else if !in_run {
            name.push('-');
            in_run = true;
        }
    }
    let name = name.trim_matches(|c| "-._".contains(c));
    if name.is_empty() {
        "repo".into()
    } else {
        name.into()
    }
}

/// The last path segment of a git URL, minus .git: https://host/org/name.git, git@host:org/name.git
/// and plain paths.
pub fn name_from_url(url: &str) -> String {
    let tail = url
        .trim_end_matches('/')
        .rsplit(['/', ':'])
        .next()
        .unwrap_or_default();
    tail.strip_suffix(".git").unwrap_or(tail).to_string()
}

// --- what a bottle needs of a repo ------------------------------------------------------

/// Where a bottle starts: a branch, or (branch None) a detached commit.
#[derive(Debug, Clone, PartialEq)]
pub struct Start {
    pub branch: Option<String>,
    pub commit: String,
}

/// origin's default branch; without one, whatever the checkout has checked out.
pub fn default_start(repo: &Repo) -> Result<Start, String> {
    if !has_ref(repo, "HEAD") {
        return Err(format!("{} has no commits", repo.name));
    }
    for r in ["refs/remotes/origin/HEAD", "HEAD"] {
        let Ok(branch) = bottle_git(&["symbolic-ref", "--quiet", "--short", r], Some(&repo.path))
        else {
            continue;
        };
        let branch = branch
            .strip_prefix("origin/")
            .unwrap_or(&branch)
            .to_string();
        let commit = resolve_branch(repo, &branch)?;
        return Ok(Start {
            branch: Some(branch),
            commit,
        });
    }
    Ok(Start {
        branch: None,
        commit: bottle_git(
            &["rev-parse", "--verify", "HEAD^{commit}"],
            Some(&repo.path),
        )?,
    })
}

/// The commit `branch` points at: the local branch, else origin's.
pub fn resolve_branch(repo: &Repo, branch: &str) -> Result<String, String> {
    for r in [
        format!("refs/heads/{branch}"),
        format!("refs/remotes/origin/{branch}"),
    ] {
        if has_ref(repo, &r) {
            return bottle_git(
                &["rev-parse", "--verify", &format!("{r}^{{commit}}")],
                Some(&repo.path),
            );
        }
    }
    Err(format!(
        "{} has no branch {}",
        repo.name,
        crate::repr(branch)
    ))
}

/// Where the repo's objects live; for a linked worktree, the main repo's.
pub fn objects_dir(repo: &Repo) -> Result<PathBuf, String> {
    let common = bottle_git(&["rev-parse", "--git-common-dir"], Some(&repo.path))?;
    let dir = repo.path.join(common);
    Ok(dir.canonicalize().unwrap_or(dir).join("objects"))
}

fn has_ref(repo: &Repo, r: &str) -> bool {
    bottle_git(&["rev-parse", "--verify", "--quiet", r], Some(&repo.path)).is_ok()
}

/// Name a repo after its origin remote, falling back to its directory name.
pub fn derive_name(repo: &Path) -> String {
    let dir = || {
        repo.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    match bottle_git(&["remote", "get-url", "origin"], Some(repo)) {
        Ok(url) => Some(name_from_url(&url))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(dir),
        Err(_) => dir(),
    }
}

/// The repo root, which `path` must be (not a subdirectory of it).
pub fn toplevel(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let toplevel = bottle_git(&["rev-parse", "--show-toplevel"], Some(path))
        .map(PathBuf::from)
        .map_err(|_| format!("{} is not a git repo", path.display()))?;
    // git reports the resolved path, so resolve ours (symlinks, /var -> /private/var) to compare.
    if path.canonicalize().ok().as_deref() != Some(toplevel.as_path()) {
        return Err(format!(
            "{} is inside a git repo; give its root instead: {}",
            path.display(),
            toplevel.display()
        ));
    }
    Ok(toplevel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::{self, git as g};

    fn rev(repo: &Path, what: &str) -> String {
        g(&["-C", repo.to_str().unwrap(), "rev-parse", what])
    }

    #[test]
    fn a_checkout_starts_at_origins_default_branch() {
        let _env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        let origin = tempdir::repo(&tmp.path().join("origin"));
        g(&[
            "-C",
            origin.to_str().unwrap(),
            "branch",
            "-m",
            "main",
            "trunk",
        ]);
        let clone = tmp.path().join("clone");
        g(&[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ]);
        g(&[
            "-C",
            clone.to_str().unwrap(),
            "switch",
            "-q",
            "-c",
            "elsewhere",
        ]);
        let start = default_start(&Repo::new("clone", &clone)).unwrap();
        assert_eq!(
            start,
            Start {
                branch: Some("trunk".into()),
                commit: rev(&origin, "trunk")
            }
        );
    }

    #[test]
    fn without_origin_its_the_checked_out_branch_or_commit() {
        let _env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        let path = tempdir::repo(&tmp.path().join("r"));
        let p = path.to_str().unwrap();
        let first = rev(&path, "HEAD");
        g(&["-C", p, "switch", "-q", "-c", "feature"]);
        g(&["-C", p, "commit", "-q", "--allow-empty", "-m", "on feature"]);
        let head = rev(&path, "HEAD");
        assert_eq!(
            default_start(&Repo::new("r", &path)).unwrap(),
            Start {
                branch: Some("feature".into()),
                commit: head
            }
        );
        g(&["-C", p, "checkout", "-q", "--detach", &first]);
        assert_eq!(
            default_start(&Repo::new("r", &path)).unwrap(),
            Start {
                branch: None,
                commit: first
            }
        );
        let empty = tmp.path().join("empty");
        g(&["init", "-q", empty.to_str().unwrap()]);
        assert_eq!(
            default_start(&Repo::new("empty", &empty)).unwrap_err(),
            "empty has no commits"
        );
    }

    #[test]
    fn a_branch_is_the_repos_own_else_origins() {
        let _env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        let origin = tempdir::repo(&tmp.path().join("origin"));
        g(&["-C", origin.to_str().unwrap(), "branch", "remote-only"]);
        let clone = tmp.path().join("clone");
        g(&[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            clone.to_str().unwrap(),
        ]);
        let repo = Repo::new("clone", &clone);
        assert_eq!(
            resolve_branch(&repo, "remote-only").unwrap(),
            rev(&origin, "remote-only")
        );
        g(&[
            "-C",
            clone.to_str().unwrap(),
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "local work",
        ]);
        assert_eq!(resolve_branch(&repo, "main").unwrap(), rev(&clone, "HEAD"));
        assert_eq!(
            resolve_branch(&repo, "nope").unwrap_err(),
            "clone has no branch 'nope'"
        );
    }

    #[test]
    fn a_linked_worktrees_objects_are_the_main_repos() {
        let _env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        let path = tempdir::repo(&tmp.path().join("main-repo"));
        let worktree = tmp.path().join("wt");
        g(&[
            "-C",
            path.to_str().unwrap(),
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
        ]);
        assert_eq!(
            objects_dir(&Repo::new("wt", &worktree)).unwrap(),
            path.join(".git/objects")
        );
    }

    #[test]
    fn a_repo_is_its_toplevel_named_after_origin_keyed_by_its_path() {
        let env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        let path = tempdir::repo(&tmp.path().join("project"));
        std::fs::create_dir(path.join("sub")).unwrap();
        let repo = current(Some(&path.join("sub"))).unwrap();
        assert_eq!(
            (repo.name.as_str(), repo.path.as_path()),
            ("project", path.as_path())
        );
        assert_eq!(repo.key(), format!("project-{}", path_hash(&path)));
        assert_eq!(repo.directory(), env.home().join(repo.key()));
        g(&[
            "-C",
            path.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "/srv/repos/odd name!.git",
        ]);
        assert_eq!(current(Some(&path)).unwrap().name, "odd-name");
        assert_eq!(
            current(Some(tmp.path())).unwrap_err(),
            "not in a git repo; folio works on the repo you're in"
        );
        assert!(known().is_empty());
        repo.save_spec(&json!({"features": ["tools"], "memory": "all"}))
            .unwrap();
        assert_eq!(known(), std::slice::from_ref(&repo));
        assert_eq!(
            repo.spec().unwrap().unwrap(),
            json!({"features": ["tools"], "memory": "2G"})
        );
        assert_eq!(
            toplevel(&path.join("sub")).unwrap_err(),
            format!(
                "{}/sub is inside a git repo; give its root instead: {}",
                path.display(),
                path.display()
            )
        );
    }

    #[test]
    fn named_after_a_urls_last_segment() {
        for (url, name) in [
            ("https://github.com/example/project.git", "project"),
            ("https://github.com/example/project", "project"),
            ("git@github.com:example/project.git", "project"),
            ("host:project.git", "project"),
            ("https://example.com/org/repo/", "repo"),
            ("/srv/git/tools.git", "tools"),
        ] {
            assert_eq!(name_from_url(url), name, "{url}");
        }
    }

    #[test]
    fn a_bottles_repo_is_named_after_origin_else_its_directory() {
        let _env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        let repo = tempdir::repo(&tmp.path().join("checkout"));
        g(&[
            "-C",
            repo.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            "git@github.com:example/project.git",
        ]);
        assert_eq!(derive_name(&repo), "project");
        assert_eq!(
            derive_name(&tempdir::repo(&tmp.path().join("my-tool"))),
            "my-tool"
        );
    }

    #[test]
    fn a_bottles_repo_is_a_repos_root_through_a_symlink_and_nothing_else() {
        let _env = tempdir::isolated();
        let tmp = tempdir::Dir::new();
        let repo = tempdir::repo(&tmp.path().join("project"));
        assert_eq!(toplevel(&repo).unwrap(), repo);
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        assert_eq!(toplevel(&link).unwrap(), repo);
        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(
            toplevel(&tmp.path().join("nope")).unwrap_err(),
            format!("{}/nope is not a directory", tmp.path().display())
        );
        assert_eq!(
            toplevel(&plain).unwrap_err(),
            format!("{} is not a git repo", plain.display())
        );
    }

    #[test]
    fn merging_features_replaces_options_in_place() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            merge_features(
                &s(&["tools", "jvm:version=17"]),
                &s(&["jvm:version=21", "claude"])
            ),
            s(&["tools", "jvm:version=21", "claude"])
        );
    }

    #[test]
    fn a_repos_spec_has_what_folio_needs_and_nothing_of_a_bottles_own() {
        assert!(check_repo_spec(json!({"features": ["tools"], "memory": "2G"}), false).is_ok());
        assert_eq!(
            check_repo_spec(json!({"features": [], "memory": "2G"}), false).unwrap_err(),
            "folio's threads need the tools feature: shpool, which every thread's sessions run in"
        );
        assert!(check_repo_spec(json!({"features": [], "memory": "2G"}), true).is_ok());
        assert!(
            check_repo_spec(json!({"features": ["tools"], "permissions": []}), false)
                .unwrap_err()
                .starts_with("a repo's spec can't give permissions:")
        );
    }
}
