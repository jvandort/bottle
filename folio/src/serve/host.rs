//! bottle.host's services: what a bottle can ask of folio serve, at http://bottle.host/, which its
//! machine routes here. Its repos' remotes, at /git/<mount>/<remote>, over git's smart HTTP
//! protocol (git.rs); messages to other bottles, a POST to /messages of {"to": ..., "text": ...}
//! (an address it was given, or, with the message-any permission, a running bottle's name),
//! handed to that bottle's session as an event from the sender (messages.rs); and, with the
//! list-bottles permission, GET /bottles, the other running bottles: each one's name, repo and
//! labels.

use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use serde::Serialize;
use serde_json::Value;

use super::http::{self, Refused};
use super::{Bottle, Server, git, messages};
use crate::control::{Remote, Served};

const GIT_PATH: &str = "/git";
const MESSAGES_PATH: &str = "/messages";
const BOTTLES_PATH: &str = "/bottles";
/// The most of a request's body read here: a push's pack.
const MAX_BODY: usize = 64 * 1024 * 1024;
/// The most of a message.
const MAX_MESSAGE: usize = 1024 * 1024;

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
        let asked = git::Asked {
            path_info: &path_info,
            head: &head,
            query,
            body: &body,
        };
        git::serve(&server.pulls, repo, remote, &asked, &mut writer)
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
    /// A running bottle, as another with list-bottles sees it.
    #[derive(Serialize)]
    struct Peer<'a> {
        labels: &'a BTreeMap<String, String>,
        name: &'a str,
        repo: &'a Option<String>,
    }
    let all = server.all();
    let peers: Vec<Peer> = all
        .iter()
        .filter(|b| b.name != bottle.name)
        .map(|b| Peer {
            name: &b.name,
            repo: &b.repo,
            labels: &b.labels,
        })
        .collect();
    let peers = serde_json::to_string(&peers).expect("bottles are JSON");
    http::respond(
        writer,
        200,
        "OK",
        format!("{peers}\n").as_bytes(),
        "application/json",
    );
    "bottles".into()
}

#[cfg(test)]
mod tests {
    use super::*;

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
