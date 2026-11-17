//! A bottle's repos' remotes, over git's smart HTTP protocol, with `git http-backend`. What a fetch
//! sees is one of three views: the upstream (the hub's refs/remotes/origin/*, served as branches,
//! plus tags, from a view folio serve writes beside the hub, the hub fetching origin first), the
//! hub's own refs (heads), or the refs in one git namespace of the hub (ns/<namespace>).
//!
//! A remote that takes pushes writes one namespace, which confines the push: every ref
//! receive-pack resolves is inside it, and the hub's own refs aren't advertised. Pushes are
//! append-only, refusing force-pushes and deletes; bottle's hooks run (hook.rs: pre-receive refuses
//! a push with no namespace; post-receive has the hub's checked-out branches follow it), and the
//! repo's own hooks never do.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::http::{self, Head};
use crate::control::{Remote, Served, UPSTREAM};
use crate::log::log;

/// How long the hub's fetch of origin may take before a fetch goes on without it.
const PULL_THROUGH_TIMEOUT: Duration = Duration::from_secs(120);
/// Framing headers this sets itself, so a copy from the CGI program would duplicate them; Status is
/// the CGI way of setting the status line.
const CGI_DROP: [&str; 4] = [
    "status",
    "content-length",
    "connection",
    "transfer-encoding",
];

/// Answers a request for `remote` of `repo`; an outcome for the log.
/// A request for a remote: the path under it, its head, query and body.
pub struct Asked<'a> {
    pub path_info: &'a str,
    pub head: &'a Head,
    pub query: &'a str,
    pub body: &'a [u8],
}

pub fn serve(
    pulls: &PullThroughs,
    repo: &Served,
    remote: &Remote,
    asked: &Asked,
    writer: &mut impl Write,
) -> String {
    let Asked {
        path_info,
        head,
        query,
        body,
    } = *asked;
    let git_dir = repo.git_dir.as_ref().expect("routed only with a repo");
    let push = path_info.contains("git-receive-pack") || query.contains("service=git-receive-pack");
    if push && remote.push.is_none() {
        let writable: Vec<&str> = repo
            .remotes
            .iter()
            .filter(|r| r.push.is_some())
            .map(|r| r.name.as_str())
            .collect();
        let hint = if writable.is_empty() {
            String::new()
        } else {
            format!("; push to {} instead", writable.join(" or "))
        };
        http::respond(
            writer,
            403,
            "Forbidden",
            format!("read-only{hint}\n").as_bytes(),
            "text/plain",
        );
        return "denied".into();
    }
    let out = if push {
        receive(git_dir, remote, path_info, head, query, body)
    } else {
        if path_info.contains("info/refs") && remote.fetch == UPSTREAM {
            // Once per fetch, at its start.
            pull_through(pulls, git_dir);
        }
        if remote.fetch == UPSTREAM {
            match write_view(git_dir, &default_view_dir(git_dir)) {
                Ok(view) => http_backend(&view, path_info, head, query, body, None, &[], &[]),
                Err(e) => {
                    log!(
                        "bottle.egress",
                        "writing {}'s view failed: {e}",
                        git_dir.display()
                    );
                    None
                }
            }
        } else {
            http_backend(
                git_dir,
                path_info,
                head,
                query,
                body,
                remote.fetch_namespace(),
                &[],
                &[],
            )
        }
    };
    let Some(out) = out else {
        http::respond(
            writer,
            500,
            "Internal Server Error",
            b"git http-backend failed\n",
            "text/plain",
        );
        return "failed".into();
    };
    relay_cgi(&out, writer);
    if push { "pushed".into() } else { "ok".into() }
}

/// A push into `remote`'s namespace of the hub at `git_dir`, under bottle's rules, its branches
/// following.
fn receive(
    git_dir: &Path,
    remote: &Remote,
    path_info: &str,
    head: &Head,
    query: &str,
    body: &[u8],
) -> Option<Vec<u8>> {
    let hooks = match super::hook::hooks_dir() {
        Ok(dir) => dir.display().to_string(),
        Err(e) => {
            log!("bottle.egress", "can't write bottle's hooks: {e}");
            return None;
        }
    };
    let config = [
        ("http.receivepack", "true"),
        // bottle's rules; the repo's own hooks never run.
        ("core.hooksPath", hooks.as_str()),
        ("receive.fsckObjects", "true"),
        // Nothing a reviewer has read may change underneath them, so the out lane is append-only.
        ("receive.denyNonFastForwards", "true"),
        ("receive.denyDeletes", "true"),
        // Refs outside refs/heads get no reflog by default.
        ("core.logAllRefUpdates", "always"),
        // A bottle pushing `main` writes its lane's main, but git compares the unnamespaced name
        // against the hub's checked-out branch and would refuse it.
        ("receive.denyCurrentBranch", "ignore"),
    ];
    // What hooks/post-receive needs to have the hub's branches follow a push, and where it says
    // what it did.
    let hook_log = (head.method == "POST").then(|| {
        std::env::temp_dir().join(format!(
            "bottle-follow-{}-{}.log",
            std::process::id(),
            unique()
        ))
    });
    let extra: Vec<(String, String)> = match &hook_log {
        Some(path) => vec![("FOLIO_HOOK_LOG".into(), path.display().to_string())],
        None => vec![],
    };
    let out = http_backend(
        git_dir,
        path_info,
        head,
        query,
        body,
        remote.push_namespace(),
        &config,
        &extra,
    );
    if let Some(path) = hook_log {
        for line in fs::read_to_string(&path).unwrap_or_default().lines() {
            log!("bottle.egress", "{}: {line}", git_dir.display());
        }
        let _ = fs::remove_file(path);
    }
    out
}

fn unique() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// git http-backend on the request, against `root`; its output, or None if it failed without any.
#[allow(clippy::too_many_arguments)]
fn http_backend(
    root: &Path,
    path_info: &str,
    head: &Head,
    query: &str,
    body: &[u8],
    namespace: Option<&str>,
    config: &[(&str, &str)],
    extra: &[(String, String)],
) -> Option<Vec<u8>> {
    let headers = head.headers();
    let body = if headers.get("content-encoding").map(String::as_str) == Some("gzip") {
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(body)
            .read_to_end(&mut plain)
            .ok()?;
        plain
    } else {
        body.to_vec()
    };
    let mut command = Command::new("git");
    command
        .arg("http-backend")
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .env("HOME", std::env::var("HOME").unwrap_or_else(|_| "/".into()))
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", path_info)
        .env("REQUEST_METHOD", &head.method)
        .env("QUERY_STRING", query)
        .env(
            "CONTENT_TYPE",
            headers.get("content-type").map_or("", String::as_str),
        )
        .env("CONTENT_LENGTH", body.len().to_string())
        .env("REMOTE_ADDR", "bottle")
        .env("REMOTE_USER", "bottle");
    // What confines a push: every ref receive-pack resolves is under refs/namespaces/<this>/.
    // hooks/pre-receive refuses a push without one, so an empty one is never passed.
    if let Some(namespace) = namespace.filter(|n| !n.is_empty()) {
        command.env("GIT_NAMESPACE", namespace);
    }
    if let Some(protocol) = headers.get("git-protocol") {
        command.env("GIT_PROTOCOL", protocol);
    }
    if !config.is_empty() {
        command.env("GIT_CONFIG_COUNT", config.len().to_string());
        for (i, (key, value)) in config.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{i}"), key)
                .env(format!("GIT_CONFIG_VALUE_{i}"), value);
        }
    }
    command.envs(extra.iter().map(|(k, v)| (k, v)));
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take().unwrap();
    let feeding = std::thread::spawn(move || {
        let _ = stdin.write_all(&body);
    });
    let output = child.wait_with_output().ok()?;
    let _ = feeding.join();
    if !output.status.success() && output.stdout.is_empty() {
        log!(
            "bottle.egress",
            "git http-backend failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        return None;
    }
    Some(output.stdout)
}

/// A CGI response (headers, an empty line, the body) as an HTTP one.
fn relay_cgi(output: &[u8], writer: &mut impl Write) {
    let mut at = 0;
    let mut status = "200 OK".to_string();
    let mut headers = String::new();
    while at < output.len() {
        let end = output[at..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(output.len(), |i| at + i);
        let line: String = output[at..end].iter().map(|&b| b as char).collect();
        let line = line.trim_end_matches('\r');
        at = end + 1;
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        let (name, value) = (name.trim(), value.trim());
        if name.eq_ignore_ascii_case("status") {
            status = value.to_string();
        } else if !CGI_DROP.contains(&name.to_lowercase().as_str()) {
            headers.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    let body = &output[at.min(output.len())..];
    let head = format!(
        "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = writer.write_all(head.as_bytes());
    let _ = writer.write_all(body);
    let _ = writer.flush();
}

/// Where the upstream view of `git_dir` is: in folio's home, by a hash of where the hub is.
pub fn default_view_dir(git_dir: &Path) -> PathBuf {
    let digest: String = Sha256::digest(git_dir.display().to_string().as_bytes())
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect();
    crate::home().join("views").join(format!("{digest}.git"))
}

/// (Re)writes the upstream view of `git_dir` at `view`, from the hub's refs right now: a tiny bare
/// repo whose objects are the hub's own (objects/info/alternates), so nothing is copied; only
/// packed-refs (refs/remotes/origin/*, as branches, and tags) really changes.
pub fn write_view(git_dir: &Path, view: &Path) -> std::io::Result<PathBuf> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args([
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/remotes/origin",
            "refs/tags",
        ])
        .output()?;
    if !out.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    let listed = String::from_utf8_lossy(&out.stdout).into_owned();
    let refs: Vec<(&str, &str)> = listed.lines().filter_map(|l| l.split_once(' ')).collect();
    let mut view_refs: Vec<(String, String)> = refs
        .iter()
        .filter(|(_, r)| r.starts_with("refs/remotes/origin/") && *r != "refs/remotes/origin/HEAD")
        .map(|(sha, r)| {
            (
                sha.to_string(),
                format!("refs/heads/{}", &r["refs/remotes/origin/".len()..]),
            )
        })
        .collect();
    view_refs.extend(
        refs.iter()
            .filter(|(_, r)| r.starts_with("refs/tags/"))
            .map(|(s, r)| (s.to_string(), r.to_string())),
    );
    let head = ["refs/heads/main", "refs/heads/master"]
        .into_iter()
        .find(|h| view_refs.iter().any(|(_, r)| r == h))
        .unwrap_or("refs/heads/main");
    fs::create_dir_all(view.join("objects/info"))?;
    fs::create_dir_all(view.join("refs"))?;
    write(
        &view.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = true\n",
    )?;
    write(
        &view.join("objects/info/alternates"),
        &format!("{}\n", git_dir.join("objects").display()),
    )?;
    write(&view.join("HEAD"), &format!("ref: {head}\n"))?;
    view_refs.sort_by(|a, b| a.1.cmp(&b.1));
    let packed: String = view_refs
        .iter()
        .map(|(sha, r)| format!("{sha} {r}\n"))
        .collect();
    write(
        &view.join("packed-refs"),
        &format!("# pack-refs with: sorted \n{packed}"),
    )?;
    Ok(view.to_path_buf())
}

/// Replaces `path` whole: a concurrent request reads all of the old file or all of the new.
fn write(path: &Path, text: &str) -> std::io::Result<()> {
    let staging = path.with_file_name(format!(
        ".{}.{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        std::process::id(),
        unique()
    ));
    fs::write(&staging, text)?;
    fs::rename(staging, path)
}

/// A repo's pull-through fetches: whether one's running, how many have started and finished.
#[derive(Default)]
struct Pulls {
    state: Mutex<(bool, u64, u64)>,
    changed: Condvar,
}

/// Every repo's pull-through fetches, by its git dir: folio serve's (Server's).
#[derive(Default)]
pub struct PullThroughs(Mutex<HashMap<PathBuf, Arc<Pulls>>>);

/// `git fetch origin` in the hub, one that starts after this was asked for: fetches of one repo
/// share, so each sees origin as it was when it asked, and a repo has at most one fetch running and
/// one waiting, however many bottles fetch at once.
pub fn pull_through(pulls: &PullThroughs, git_dir: &Path) {
    let pulls = pulls
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(git_dir.to_path_buf())
        .or_default()
        .clone();
    let mut state = pulls.state.lock().unwrap_or_else(|e| e.into_inner());
    let wanted = state.1 + 1;
    while state.2 < wanted {
        if state.0 {
            state = pulls.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            continue;
        }
        state.0 = true;
        state.1 += 1;
        drop(state);
        fetch_origin(git_dir);
        state = pulls.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 = false;
        state.2 = state.1;
        pulls.changed.notify_all();
    }
}

/// With the controller's network and credentials, never prompting, and never failing the bottle's
/// request: if it can't fetch, the bottle gets what the hub has.
fn fetch_origin(git_dir: &Path) {
    let child = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["fetch", "--quiet", "origin"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else {
        log!(
            "bottle.egress",
            "pull-through: git fetch origin in {} failed to start",
            git_dir.display()
        );
        return;
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let mut said = String::new();
                    if let Some(mut stderr) = child.stderr.take() {
                        let _ = stderr.read_to_string(&mut said);
                    }
                    log!(
                        "bottle.egress",
                        "pull-through: git fetch origin in {} failed: {}",
                        git_dir.display(),
                        said.trim()
                    );
                }
                return;
            }
            Ok(None) if started.elapsed() > PULL_THROUGH_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                log!(
                    "bottle.egress",
                    "pull-through: git fetch origin in {} timed out",
                    git_dir.display()
                );
                return;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cgi_response_is_an_http_one() {
        let mut out = Vec::new();
        relay_cgi(b"Status: 404 Not Found\r\nContent-Type: text/plain\nContent-Length: 9\r\n\r\nnot here\n", &mut out);
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.starts_with(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 9\r\n"
            ),
            "{out}"
        );
        assert!(
            out.ends_with("Connection: close\r\n\r\nnot here\n"),
            "{out}"
        );
    }
}
