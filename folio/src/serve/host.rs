//! bottle.host's services: what a bottle can ask of folio serve, at http://bottle.host/, which its
//! machine routes here. Its repos' remotes, at /git/<mount>/<remote>, over git's smart HTTP
//! protocol (git.rs); messages to other bottles, a POST to /messages of {"to": ..., "text": ...}
//! (an address it was given, or, with the message-any permission, a running bottle's name),
//! handed to that bottle's session as an event from the sender (messages.rs); and, with the
//! list-bottles permission, GET /bottles, the other running bottles: each one's name, repo and
//! labels.

use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use super::http::{self, Refused};
use super::{Bottle, Server, git, messages};

const GIT_PATH: &str = "/git";
const MESSAGES_PATH: &str = "/messages";
const BOTTLES_PATH: &str = "/bottles";
/// The most of a request's body read here: a push's pack.
const MAX_BODY: usize = 64 * 1024 * 1024;
/// The most of a message.
const MAX_MESSAGE: usize = 1024 * 1024;

/// What a remote's fetch can see: the upstream (served as origin is), the repo's own branches (as
/// host is), or one namespace, NS + its name.
pub const UPSTREAM: &str = "upstream";
pub const HEADS: &str = "heads";
pub const NS: &str = "ns/";

/// A remote and namespace name: one path segment.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

/// One remote a bottle is served: http://bottle.host/git/<mount>/<name>.
#[derive(Debug, Clone, PartialEq)]
pub struct Remote {
    pub name: String,
    /// UPSTREAM, HEADS, or NS + a namespace.
    pub fetch: String,
    /// NS + the namespace pushes go into; None: read-only.
    pub push: Option<String>,
}

impl Remote {
    /// A remote from its spec: NAME:fetch=VIEW[,push=ns/NAMESPACE].
    pub fn parse(spec: &str) -> Result<Remote, String> {
        let usage = format!(
            "invalid remote '{spec}': use NAME:fetch=VIEW[,push=ns/NAMESPACE], VIEW being {UPSTREAM}, {HEADS} or ns/NAMESPACE"
        );
        let (name, rest) = spec.split_once(':').ok_or(&usage)?;
        if !is_name(name) {
            return Err(usage);
        }
        let (mut fetch, mut push) = (None, None);
        for item in rest.split(',') {
            match item.split_once('=') {
                Some(("fetch", v)) if fetch.is_none() => fetch = Some(v.to_string()),
                Some(("push", v)) if push.is_none() => push = Some(v.to_string()),
                _ => return Err(usage),
            }
        }
        let namespace = |v: &str| v.strip_prefix(NS).is_some_and(is_name);
        let fetch = fetch
            .filter(|f| f == UPSTREAM || f == HEADS || namespace(f))
            .ok_or(&usage)?;
        if push.as_deref().is_some_and(|p| !namespace(p)) {
            return Err(format!(
                "invalid remote '{spec}': push takes ns/NAMESPACE, the namespace pushes go into"
            ));
        }
        Ok(Remote {
            name: name.into(),
            fetch,
            push,
        })
    }

    pub fn fetch_namespace(&self) -> Option<&str> {
        self.fetch.strip_prefix(NS)
    }

    pub fn push_namespace(&self) -> Option<&str> {
        self.push.as_deref().and_then(|p| p.strip_prefix(NS))
    }
}

/// One repo a bottle is served (a mount's), with its remotes, at http://bottle.host/git/<at>/<remote>.
#[derive(Debug, Clone)]
pub struct Served {
    /// The repo its remotes are served from; None: it's gone.
    pub git_dir: Option<PathBuf>,
    pub remotes: Vec<Remote>,
    /// Where under /git its remotes are: the mount's path without its leading /; "" for /git/<remote>.
    pub at: String,
}

impl Served {
    pub fn from_json(repo: &Value) -> Result<Served, String> {
        let remotes = repo["remotes"]
            .as_array()
            .map(|r| {
                r.iter()
                    .filter_map(Value::as_str)
                    .map(Remote::parse)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Served {
            git_dir: repo["git_dir"].as_str().map(PathBuf::from),
            remotes,
            at: repo["at"].as_str().unwrap_or_default().to_string(),
        })
    }
}

/// The repo, remote and rest of path a /git/... request is for; None if none is served there.
fn route<'a>(repos: &'a [Served], path: &str) -> Option<(&'a Served, &'a Remote, String)> {
    let rest = path.strip_prefix(GIT_PATH)?.strip_prefix('/')?;
    // The longest `at` first, so a mount at /a/b isn't taken for one at /a.
    let mut by_length: Vec<&Served> = repos.iter().collect();
    by_length.sort_by_key(|r| std::cmp::Reverse(r.at.len()));
    for repo in by_length {
        let prefix = if repo.at.is_empty() {
            String::new()
        } else {
            format!("{}/", repo.at)
        };
        let Some(after) = rest.strip_prefix(&prefix) else {
            continue;
        };
        if repo.git_dir.is_none() {
            continue;
        }
        let (name, path_info) = after.split_once('/').unwrap_or((after, ""));
        if let Some(remote) = repo.remotes.iter().find(|r| r.name == name) {
            return Some((repo, remote, format!("/{path_info}")));
        }
    }
    None
}

/// Refuses `stream`'s request, in plain http.
pub fn refuse(mut stream: UnixStream, status: u16, reason: &str) {
    http::respond_refused(&mut stream, &Refused::new(status, reason, "denied"));
}

/// Answers a bottle's request to bottle.host: how it went, and the status of a failure.
pub fn serve(
    server: &Arc<Server>,
    bottle: &Arc<Bottle>,
    stream: UnixStream,
) -> (String, Option<String>) {
    let _ = stream.set_read_timeout(Some(http::HEADER_TIMEOUT));
    let Ok(read) = stream.try_clone() else {
        return ("error".into(), None);
    };
    let mut reader = BufReader::new(read);
    let mut writer = stream;
    let request = (|| -> Result<(http::Head, Vec<u8>), Refused> {
        let head = http::read_head(&mut reader)?;
        if !head.target.starts_with('/') {
            return Err(Refused::bad_request());
        }
        let body = http::read_body(&mut reader, &head.headers(), MAX_BODY)
            .map_err(|_| Refused::bad_request())?
            .ok_or_else(Refused::bad_request)?;
        Ok((head, body))
    })();
    let (head, body) = match request {
        Ok(request) => request,
        Err(refused) => {
            http::respond_refused(&mut writer, &refused);
            return (refused.outcome.into(), None);
        }
    };
    let _ = writer.set_read_timeout(None);
    let (path, query) = head.path_and_query();
    let outcome = if let Some((repo, remote, path_info)) = route(&bottle.repos, path) {
        git::serve(repo, remote, &path_info, &head, query, &body, &mut writer)
    } else if path == MESSAGES_PATH {
        message(server, bottle, &head, &body, &mut writer)
    } else if path == BOTTLES_PATH {
        bottles(server, bottle, &head, &mut writer)
    } else {
        http::respond(
            &mut writer,
            404,
            "Not Found",
            b"no such bottle.host service\n",
            "text/plain",
        );
        "not-found".into()
    };
    (outcome, None)
}

fn message(
    server: &Arc<Server>,
    bottle: &Bottle,
    head: &http::Head,
    body: &[u8],
    writer: &mut impl Write,
) -> String {
    if head.method != "POST" {
        http::respond(
            writer,
            405,
            "Method Not Allowed",
            b"POST a message: {\"to\": ..., \"text\": ...}\n",
            "text/plain",
        );
        return "bad-message".into();
    }
    let message = match parse_message(body) {
        Ok(message) => message,
        Err(problem) => {
            http::respond(
                writer,
                400,
                "Bad Request",
                format!("{problem}\n").as_bytes(),
                "text/plain",
            );
            return "bad-message".into();
        }
    };
    match messages::deliver(server, bottle, &message.to, &message.text) {
        Ok(()) => {
            http::respond(writer, 200, "OK", b"delivered\n", "text/plain");
            "message".into()
        }
        Err(refused) => {
            http::respond(
                writer,
                refused.0,
                refused.1,
                format!("{}\n", refused.2).as_bytes(),
                "text/plain",
            );
            "undelivered".into()
        }
    }
}

/// A message a bottle sends: to a destination, its text.
struct Message {
    to: String,
    text: String,
}

/// The message a body is, {"to": a destination, "text": the message}; else what's wrong with it.
fn parse_message(body: &[u8]) -> Result<Message, String> {
    if body.len() > MAX_MESSAGE {
        return Err(format!("a message is at most {MAX_MESSAGE} bytes"));
    }
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return Err("a message is JSON: {\"to\": ..., \"text\": ...}".into());
    };
    let Some(fields) = message.as_object() else {
        return Err("a message is a JSON object: {\"to\": ..., \"text\": ...}".into());
    };
    let extra: Vec<&str> = fields
        .keys()
        .map(String::as_str)
        .filter(|k| !["to", "text"].contains(k))
        .collect();
    if !extra.is_empty() {
        return Err(format!(
            "a message has only to and text, not {}",
            extra.join(", ")
        ));
    }
    let Some(to) = message["to"].as_str().filter(|t| !t.trim().is_empty()) else {
        return Err("a message needs to: the address or bottle it's for".into());
    };
    let Some(text) = message["text"].as_str() else {
        return Err("a message needs text".into());
    };
    Ok(Message {
        to: to.into(),
        text: text.into(),
    })
}

/// The running bottles but the asker, for a bottle with list-bottles: each one's name, repo and labels.
fn bottles(
    server: &Arc<Server>,
    bottle: &Bottle,
    head: &http::Head,
    writer: &mut impl Write,
) -> String {
    if head.method != "GET" {
        http::respond(
            writer,
            405,
            "Method Not Allowed",
            b"GET the running bottles\n",
            "text/plain",
        );
        return "bad-request".into();
    }
    if !bottle.permissions.iter().any(|p| p == "list-bottles") {
        http::respond(
            writer,
            403,
            "Forbidden",
            b"listing bottles needs the list-bottles permission\n",
            "text/plain",
        );
        return "refused".into();
    }
    let peers: Vec<Value> = server
        .all()
        .iter()
        .filter(|b| b.name != bottle.name)
        .map(|b| json!({"name": b.name, "repo": b.repo, "labels": b.labels}))
        .collect();
    http::respond(
        writer,
        200,
        "OK",
        format!("{}\n", Value::Array(peers)).as_bytes(),
        "application/json",
    );
    "bottles".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_is_a_name_a_view_and_maybe_a_namespace_to_push_into() {
        let r = Remote::parse("agent:fetch=ns/t-out,push=ns/t-out").unwrap();
        assert_eq!(
            (r.fetch_namespace(), r.push_namespace()),
            (Some("t-out"), Some("t-out"))
        );
        assert_eq!(Remote::parse("origin:fetch=upstream").unwrap().push, None);
        for bad in [
            "x",
            "x:fetch=nope",
            "x:push=ns/a",
            "x:fetch=heads,push=heads",
            "-x:fetch=heads",
        ] {
            assert!(Remote::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_remote_parses_from_its_spec_and_refuses_what_isnt_one() {
        let remote = |name: &str, fetch: &str, push: Option<&str>| Remote {
            name: name.into(),
            fetch: fetch.into(),
            push: push.map(String::from),
        };
        for (spec, expected) in [
            ("origin:fetch=upstream", remote("origin", UPSTREAM, None)),
            ("host:fetch=heads", remote("host", HEADS, None)),
            (
                "work:fetch=ns/t-in,push=ns/t-out",
                remote("work", "ns/t-in", Some("ns/t-out")),
            ),
            (
                "mine:push=ns/x,fetch=ns/x",
                remote("mine", "ns/x", Some("ns/x")),
            ),
            (
                "up:fetch=upstream,push=ns/x",
                remote("up", UPSTREAM, Some("ns/x")),
            ),
        ] {
            assert_eq!(Remote::parse(spec).unwrap(), expected, "{spec}");
        }
        let origin = Remote::parse("origin:fetch=upstream").unwrap();
        assert_eq!(
            (origin.fetch_namespace(), origin.push_namespace()),
            (None, None)
        );
        for spec in [
            "origin",
            "origin:",
            ":fetch=heads",
            "a/b:fetch=heads",
            "x:push=ns/y",
            "x:fetch=everything",
            "x:fetch=ns/",
            "x:fetch=ns/a/b",
            "x:fetch=heads,push=heads",
            "x:fetch=heads,fetch=heads",
            "x:fetch=heads,pull=ns/y",
            "x:fetch=heads,push=ns/-y",
            "a:fetch=ns/x,push=ns/x,advance=main",
        ] {
            assert!(Remote::parse(spec).is_err(), "{spec}");
        }
    }

    #[test]
    fn a_request_is_routed_to_the_longest_mount_it_names() {
        let served = |at: &str| Served {
            git_dir: Some("/r".into()),
            remotes: vec![Remote::parse("origin:fetch=upstream").unwrap()],
            at: at.into(),
        };
        let repos = [served("a"), served("a/b")];
        let (repo, remote, rest) = route(&repos, "/git/a/b/origin/info/refs").unwrap();
        assert_eq!(
            (repo.at.as_str(), remote.name.as_str(), rest.as_str()),
            ("a/b", "origin", "/info/refs")
        );
        assert!(route(&repos, "/git/c/origin").is_none());
        assert!(route(&repos, "/other").is_none());
    }
}
