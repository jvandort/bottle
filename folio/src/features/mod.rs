//! Features: installable additions to an image, in the Dev Container feature format.
//!
//! Each feature is a directory, folio/features/<id>/ ($FOLIO_FEATURES, else the checkout's),
//! holding a devcontainer-feature.json and an install.sh
//! (https://containers.dev/implementors/features/).
//! bottle composes an image from features with builds that run each feature's
//! install.sh in turn, the way the Dev Container tooling does, but without it: no
//! Node, no Docker, local features only. The base feature, the bottle contract,
//! is in every bottle: installed on Debian on its own, so every bottle shares it,
//! and a bottle's features on that. It isn't one to ask for.
//!
//! Only a subset of the spec is supported. Anything else in a definition is an
//! error, so a feature never silently loses behavior it asked for:
//!
//!   id, version (required); name, description, documentationURL, licenseURL,
//!   keywords (metadata); options (string and boolean, with defaults);
//!   containerEnv; dependsOn (local feature ids, without options);
//!   installsAfter (local feature ids); customizations.bottle (below).
//!
//! customizations.bottle declares what a feature needs from bottle itself:
//! credentials, downloads to cache, what it installs with apt, options a user has
//! to set, and what to tell the agent.
//!
//! A credential is a secret bottle keeps on the controller and a bottle's tools need
//! (auth.rs). It never enters a bottle: a credential says which hosts it belongs to, and
//! folio serve attaches it to the bottle's requests to those hosts (serve/credentials.rs).
//!
//!   "customizations": {"bottle": {"credentials": {"teamcity": {
//!       "description": "TeamCity access token",
//!       "inject": {"hosts": ["${server}"], "header": "Authorization",
//!                  "value": "Bearer ${credential}"}}}}}
//!
//!     hosts    Where the credential is sent: `host` or `host:port`, always
//!              reached over https -- a credential is never put on a request the
//!              proxy sends in the clear. `${option}` is the value of one of this
//!              feature's options, so the server a user configures is the server
//!              the credential is attached to; a host that is only `${option}`
//!              is each of the option's comma-separated values, for a user with
//!              several servers; and a host may be a pattern
//!              (`*.example.com`). A request matching several patterns gets one
//!              header, from the first credential that claims it, and two
//!              credentials claiming the same host is an error.
//!     header   The header it goes in; Authorization by default.
//!     value    The whole header value, with ${credential} where the real one goes.
//!     standin  An environment variable a bottle gets a fake token in, for a tool
//!              that won't work until it thinks it's logged in (see below).
//!     forbid   Routes never sent to the hosts, with the credential or without:
//!              `METHOD[|METHOD] /path`, `*` for any method, and in the path `*`
//!              for part of a segment, `**` for any number of them, `{a,b}` for
//!              either (routes.rs). For the requests whose answer is a
//!              credential (a token minted from this one, a key provisioned, a
//!              runner registered), or that let something outside the bottle
//!              act with it later: the proxy would otherwise spend the
//!              credential on them like any other.
//!     allow    Routes the credential is attached to, when there are any; a
//!              request on another that presents the stand-in is refused. None
//!              may reach a forbidden route: forbid is what allow must never
//!              grow to cover, checked when the feature loads.
//!     rewrite  Where else a tool presents the stand-in, by route: a header whose
//!              whole value is the stand-in, or a top-level field of a JSON body,
//!              either raw or base64 (`"encoding": "base64"`, unpadded):
//!
//!                {"route": "POST /scans/publish/*/*/token", "json": "accessKey"}
//!                {"route": "GET /distribution-broker/**", "header": "X-User-Access-Key",
//!                 "encoding": "base64"}
//!
//!              The credential goes in that place, in the same encoding, when
//!              what's there is exactly the stand-in, and nowhere else.
//!
//!   A tool in the bottle talks to the host over https as it always would: its machine routes the
//!   connection to folio serve, bottle-cli taking the TLS off with a certificate from the machine's
//!   CA, which the bottle trusts for its credentials' and cached hosts and no others, so folio
//!   serve can read the request and put the credential in place of the stand-in, on the requests
//!   that present it (see below). A host is a name, an IP address, or `*.` and a name, since the
//!   CA's name constraint can say nothing else. Anything else the tool needs in order to believe
//!   it's configured is the feature's own business, in its install.sh.
//!
//!   A CLI doesn't know someone is attaching its token for it, and usually
//!   refuses to make a request until it has one. What it gets is a stand-in, and
//!   never a secret, and sending it is how a request asks for the credential: in
//!   the header, as the token (`Bearer <stand-in>`) or as the password of Basic
//!   credentials. A request without it goes to the host as the bottle sent it.
//!   The feature either writes one itself (the teamcity feature puts it in the
//!   CLI's config file, as STANDIN) or names an environment variable in
//!   inject.standin, and bottle sets that variable, to a value of its own, in
//!   every bottle with the feature.
//!
//! properties is optional: a file on the machine where a tool already keeps its
//! credentials, as Java properties of `host=credential` lines (Gradle's
//! Develocity keys.properties is one), with `${option}` for an option's value and
//! `~` for the home directory. folio serve reads it each time a bottle starts, and
//! attaches to each of inject.hosts the credential on that host's line; a host
//! the file has no line for gets the one stored by `folio auth set`, if any.
//! Only the line for a host the feature names ever leaves the file.
//!
//! cache names hosts whose downloads bottle-cli keeps on the machine, so the next
//! bottle gets them from there (images.rs): as with a credential's
//! host, the bottle's TLS to them is terminated with its machine's CA. exclude is
//! fnmatch patterns over a request's path, for files a host changes after
//! they're published, which are never kept.
//!
//!   "customizations": {"bottle": {"cache": {
//!       "hosts": ["files.example.com"], "exclude": ["*/index.json"]}}}
//!
//! apt is what the feature installs with apt (images.rs): packages, from
//! Debian's repositories and any the feature declares. An image's build installs
//! every repository its features declare, then all their packages, in one step
//! before any install.sh, which is left with what's the feature's own to do. A
//! repository stays installed in the image, its key (a file in the feature's
//! directory) in /etc/apt/keyrings, so a bottle can install more from it. A
//! package is a name, with ${option} for an option's value: one holding a
//! comma-separated list is a package for each value, and none for an empty one.
//! bottle-cli keeps every repository's packages for builds and bottles alike, so a
//! feature's cache needn't name its repositories' hosts.
//!
//!   "customizations": {"bottle": {"apt": {
//!       "repositories": [{"uri": "https://apt.example.com/stable", "suite": "stable",
//!                         "components": ["main"], "key": "example.asc"}],
//!       "packages": ["example-cli", "jq"]}}}
//!
//! requiredOptions names options a user has to set, for the ones with no sensible
//! default (the teamcity feature's server). The format has no way to say this --
//! an option must declare a default, of its own type, so there is no null to mean
//! "none" -- and bottle doesn't read the placeholder default those options
//! declare: it refuses the feature unless the user set the option.
//!
//!   "customizations": {"bottle": {"requiredOptions": ["server"]}}
//!
//! context is what the agent in a bottle with the feature should know about it,
//! added to its ~/BOTTLE.md under a heading of its own: Markdown, a sentence or
//! two, saying what it can now do, not how the feature was installed. `${option}`
//! is the value of one of the feature's options.
//!
//!   "customizations": {"bottle": {"context": "The `teamcity` command talks to ${server}."}}
//!
//! Features are requested as specs: an id, optionally with option values, e.g.
//! `jvm:version=17` or `name:a=1,b=true`. Unset options take their defaults.
//!
//! A JSON object's members are read in key order: what a feature writes is the same feature
//! however its members are ordered.
//!
//!     folio-cli features list
//!     folio-cli features show SPEC
//!     folio-cli features canonical SPEC...
//!     folio-cli features resolve [SPEC...]
//!     folio-cli features image [SPEC...]
//!     folio-cli features cached [--build] [SPEC...]
//!     folio-cli features route SPEC METHOD PATH
//!
//! Each writes JSON: the features a bottle can ask for (all but base, which every bottle has); one,
//! as its definition says, with the options SPEC sets, and what they make of it as far as they can
//! (what can't be, for an option that needs setting, is said instead: `"unresolved": why`); each
//! SPEC in its canonical form (`jvm`, or `jvm:version=17` with options set); the features SPECs ask
//! for and all they depend on, in install order, their options filled in (or `"unresolved": why`,
//! as for show); what builds the base image, or, given SPECs, the image with those features
//! (images.rs); and the hosts a bottle with those features has its downloads kept for (`--build`:
//! what every build keeps).

#[cfg(test)]
use resolve::parse_spec;
#[cfg(test)]
use serde_json::Value;

#[cfg(test)]
use crate::routes::Route;

use std::env;
use std::path::PathBuf;

#[cfg(test)]
use serde_json::json;

mod feature;
mod load;
mod names;
mod resolve;
mod sections;

pub use feature::{Credential, Feature, Repository, Rewrite};
pub use load::{available, load};
pub use resolve::{canonical, resolve};

pub const BASE: &str = "base";
pub const CREDENTIAL_PLACEHOLDER: &str = "${credential}";
/// RAW and BASE64: where a rewrite finds the stand-in.
pub const RAW: &str = "raw";
pub const BASE64: &str = "base64";

const METADATA_KEYS: [&str; 5] = [
    "name",
    "description",
    "documentationURL",
    "licenseURL",
    "keywords",
];
const SUPPORTED_KEYS: [&str; 7] = [
    "id",
    "version",
    "options",
    "containerEnv",
    "dependsOn",
    "installsAfter",
    "customizations",
];
const BOTTLE_KEYS: [&str; 5] = ["credentials", "cache", "requiredOptions", "context", "apt"];
const CACHE_KEYS: [&str; 2] = ["hosts", "exclude"];
const APT_KEYS: [&str; 2] = ["repositories", "packages"];
const REPOSITORY_KEYS: [&str; 4] = ["uri", "suite", "components", "key"];
const CREDENTIAL_KEYS: [&str; 3] = ["description", "inject", "properties"];
const INJECT_KEYS: [&str; 7] = [
    "hosts", "header", "value", "standin", "forbid", "allow", "rewrite",
];
const REWRITE_KEYS: [&str; 4] = ["route", "header", "json", "encoding"];
const OPTION_KEYS: [&str; 5] = ["type", "default", "description", "proposals", "enum"];

type Result<T> = std::result::Result<T, String>;

// Where a unit test has its features, on its own thread (tests run in parallel; the environment is
// the process's).
#[cfg(test)]
thread_local! {
    pub static TEST_DIR: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Where features are: $FOLIO_FEATURES, else this checkout's folio/features.
pub fn dir() -> PathBuf {
    #[cfg(test)]
    if let Some(dir) = TEST_DIR.with(|d| d.borrow().clone()) {
        return dir;
    }
    env::var_os("FOLIO_FEATURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/features")))
}

#[cfg(test)]
/// What folio serve does with a request for `path` by `method` to the hosts of SPEC's
/// credentials: the path as a server routes it (null: refused as no route), and for each credential
/// whether the request is forbidden (never sent), and whether the credential would go on it, if it
/// presents the stand-in.
fn route(spec: &str, method: &str, path: &str) -> Result<Value> {
    let (id, settings) = parse_spec(spec)?;
    let feature = load(&id)?.configured(&settings)?;
    let canonical = crate::routes::canonical(path);
    let credentials: Vec<Value> = feature
        .credentials
        .iter()
        .map(|c| {
            let rewritten: Vec<Route> = c.inject.rewrites.iter().map(|r| r.route.clone()).collect();
            let (forbidden, allowed) = match &canonical {
                Some(p) => (
                    crate::routes::forbids(&c.inject.forbid, method, p),
                    crate::routes::allows(&c.inject.allow, &rewritten, method, p),
                ),
                None => (true, false),
            };
            json!({"name": c.name, "forbidden": forbidden, "allowed": allowed && !forbidden})
        })
        .collect();
    Ok(json!({"path": canonical, "credentials": credentials}))
}

#[cfg(test)]
#[path = "format_tests.rs"]
mod format;

#[cfg(test)]
mod tests {
    use super::names::{is_package, references, substitute, whole_reference};
    use super::resolve::parse_spec;

    fn settings(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_spec_is_an_id_and_its_options_a_segment_without_equals_continuing_a_value() {
        assert_eq!(parse_spec("f").unwrap(), ("f".into(), vec![]));
        assert_eq!(
            parse_spec("f:version=17,docs=true").unwrap(),
            ("f".into(), settings(&[("version", "17"), ("docs", "true")]))
        );
        assert_eq!(
            parse_spec("f:list=1,2,3,x=y").unwrap(),
            ("f".into(), settings(&[("list", "1,2,3"), ("x", "y")]))
        );
        assert_eq!(
            parse_spec("f:url=a=b").unwrap(),
            ("f".into(), settings(&[("url", "a=b")]))
        );
        for bad in ["f:", "f:version", "f:=1", "f:,x=1", "f:version=1,version=2"] {
            assert!(parse_spec(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_reference_is_an_options_name_in_braces() {
        assert_eq!(references("${a}-x-${b}"), ["a", "b"]);
        assert_eq!(substitute("${a}:${b}", |n| n.to_uppercase()), "A:B");
        assert_eq!(whole_reference("${server}"), Some("server"));
        assert_eq!(whole_reference("${a}.${b}"), None);
    }

    #[test]
    fn a_package_is_a_name_with_references_and_an_architecture() {
        for good in [
            "jq",
            "openjdk-${version}-jdk",
            "musl-dev:amd64",
            "g++",
            "lib${name}1.2",
        ] {
            assert!(is_package(good), "{good}");
        }
        for bad in ["Jq", "a b", "x:", "${}", "a:B"] {
            assert!(!is_package(bad), "{bad}");
        }
    }
}
