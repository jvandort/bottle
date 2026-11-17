//! folio serve's control protocol: what folio (daemon.rs) asks of folio serve (serve/mod.rs) on its
//! control socket, a JSON line each way, and what it's told of a bottle to answer. Types only:
//! both ends, and what builds a bind (bottles.rs), depend on this, and it on nothing of theirs.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// A request on folio serve's control socket, a JSON line: {"op": ..., ...}.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum Request {
    /// What code it runs: Reply's code.
    Ping,
    /// Answer a bottle's machine.
    Bind(Box<Bind>),
    /// Stop answering a bottle's machine: with none left, folio serve stops.
    Release {
        bottle: String,
    },
    Shutdown,
}

/// What folio serve answers a request: whether it did, why not, and to a ping, the code it runs.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

impl Reply {
    pub fn ok() -> Reply {
        Reply {
            ok: true,
            ..Reply::default()
        }
    }

    pub fn refused(error: impl Into<String>) -> Reply {
        Reply {
            ok: false,
            error: Some(error.into()),
            ..Reply::default()
        }
    }
}

/// What folio serve is told of a bottle: its machine, on its runner, and what that routes to folio
/// serve to answer: its repos, features (whose credentials it attaches), permissions, labels and
/// first repo's name (as the list of bottles has them), the bottles it may ssh to, and, on a runner,
/// what to forward there (THERE=HERE).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bind {
    pub bottle: String,
    pub machine: String,
    pub runner: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub repos: Vec<Served>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub ssh: Vec<Grant>,
    #[serde(default)]
    pub forwards: Vec<String>,
}

/// One repo a bottle is served -- a mount's -- with its remotes (specs), under git.bottle.internal/<at>.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Served {
    /// The repo its remotes are served from; None: it's gone.
    pub git_dir: Option<PathBuf>,
    pub remotes: Vec<Remote>,
    /// Where under /git its remotes are: the mount's path without its leading /; "" for
    /// /git/<remote>.
    pub at: String,
}

/// A bottle another may ssh to: as `name`, at `address`, which is bottle `bottle` with id `id` and
/// no other.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    pub name: String,
    pub bottle: String,
    pub id: String,
    pub address: String,
}

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

/// One remote a bottle is served: http://git.bottle.internal/<mount>/<name>.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
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

impl TryFrom<String> for Remote {
    type Error = String;

    fn try_from(spec: String) -> Result<Remote, String> {
        Remote::parse(&spec)
    }
}

impl From<Remote> for String {
    /// Its spec, as parse takes it.
    fn from(r: Remote) -> String {
        format!("{}:fetch={}", r.name, r.fetch)
            + &r.push.map(|p| format!(",push={p}")).unwrap_or_default()
    }
}

/// This program's code, as a ping's reply says it: a hash of the program itself. A folio serve
/// running other code than the folio asking is restarted.
pub fn code() -> String {
    use sha2::{Digest, Sha256};
    let exe = std::env::current_exe()
        .and_then(std::fs::read)
        .unwrap_or_default();
    Sha256::digest(&exe)
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
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
    fn a_remote_is_its_spec_in_a_bind() {
        let remote = Remote::parse("work:fetch=ns/t-in,push=ns/t-out").unwrap();
        let json = serde_json::to_string(&remote).unwrap();
        assert_eq!(json, "\"work:fetch=ns/t-in,push=ns/t-out\"");
        assert_eq!(serde_json::from_str::<Remote>(&json).unwrap(), remote);
        assert!(serde_json::from_str::<Remote>("\"x\"").is_err());
    }
}
