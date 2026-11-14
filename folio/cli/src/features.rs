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
//! (folio/folio/bottle/auth.py). It never enters a bottle: a credential says which hosts
//! it belongs to, and folio serve attaches it to the bottle's requests to
//! those hosts (folio/folio/bottle/egress.py).
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
//!
//! Each writes JSON: the features a bottle can ask for (all but base, which every bottle has); one,
//! as its definition says, with the options SPEC sets, and what they make of it as far as they can
//! (what can't be, for an option that needs setting, is said instead: `"unresolved": why`); each
//! SPEC in its canonical form (`jvm`, or `jvm:version=17` with options set); the features SPECs ask
//! for and all they depend on, in install order, their options filled in (or `"unresolved": why`,
//! as for show); what builds the base image, or, given SPECs, the image with those features
//! (images.rs); and the hosts a bottle with those features has its downloads kept for (`--build`:
//! what every build keeps).

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Map, Value, json};

use crate::routes::Route;
use crate::{FAILED, images, repr};

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

/// Where features are: $FOLIO_FEATURES, else this checkout's folio/features.
pub fn dir() -> PathBuf {
    env::var_os("FOLIO_FEATURES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../features")))
}

/// An option's value: a string or a boolean.
#[derive(Debug, Clone, PartialEq)]
pub enum OptionValue {
    String(String),
    Bool(bool),
}

impl OptionValue {
    /// As install.sh and a spec have it: a string as it is, a boolean as true or false.
    pub fn text(&self) -> String {
        match self {
            OptionValue::String(s) => s.clone(),
            OptionValue::Bool(b) => b.to_string(),
        }
    }

    fn json(&self) -> Value {
        match self {
            OptionValue::String(s) => s.clone().into(),
            OptionValue::Bool(b) => (*b).into(),
        }
    }

    fn from_json(v: &Value) -> Option<OptionValue> {
        match v {
            Value::String(s) => Some(OptionValue::String(s.clone())),
            Value::Bool(b) => Some(OptionValue::Bool(*b)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FeatureOption {
    /// "string" or "boolean".
    pub kind: String,
    pub default: OptionValue,
    pub allowed: Option<Vec<OptionValue>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rewrite {
    pub route: Route,
    pub header: String,
    pub json: String,
    pub encoding: String,
}

/// Where a credential belongs, so folio serve can attach it there.
#[derive(Debug, Clone, PartialEq)]
pub struct Inject {
    /// `host` or `host:port`; `${option}` is an option's value.
    pub hosts: Vec<String>,
    /// The header value, with ${credential} where the real one goes.
    pub value: String,
    pub header: String,
    pub standin: Option<String>,
    pub forbid: Vec<Route>,
    pub allow: Vec<Route>,
    pub rewrites: Vec<Rewrite>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Credential {
    pub name: String,
    pub description: String,
    pub inject: Inject,
    /// A properties file of host=credential lines, read before the Keychain.
    pub properties: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Cache {
    pub hosts: Vec<String>,
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Repository {
    /// http:// or https://, to the repository's root (where dists/ and pool/ are).
    pub uri: String,
    pub suite: String,
    pub components: Vec<String>,
    /// The file of the key it's signed with, in its feature's directory.
    pub key: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Apt {
    pub repositories: Vec<Repository>,
    /// Names, with ${option} where an option's value goes.
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Feature {
    pub id: String,
    pub version: String,
    pub path: PathBuf,
    pub options: BTreeMap<String, FeatureOption>,
    pub container_env: BTreeMap<String, String>,
    pub depends_on: Vec<String>,
    pub installs_after: Vec<String>,
    pub credentials: Vec<Credential>,
    pub cache: Option<Cache>,
    pub apt: Apt,
    pub required_options: Vec<String>,
    /// What to tell the agent about it, with ${option} where an option's value goes.
    pub context: String,
    /// Option values set explicitly (not defaulted), from the feature's spec, by name.
    pub overrides: BTreeMap<String, OptionValue>,
}

impl Feature {
    pub fn values(&self) -> BTreeMap<String, OptionValue> {
        let mut values: BTreeMap<String, OptionValue> = self
            .options
            .iter()
            .map(|(name, option)| (name.clone(), option.default.clone()))
            .collect();
        values.extend(self.overrides.clone());
        values
    }

    /// The canonical spec: `jvm`, or `jvm:version=17` when options are set.
    pub fn spec(&self) -> String {
        if self.overrides.is_empty() {
            return self.id.clone();
        }
        let set: Vec<String> = self
            .overrides
            .iter()
            .map(|(k, v)| format!("{k}={}", v.text()))
            .collect();
        format!("{}:{}", self.id, set.join(","))
    }

    /// Options as install.sh sees them, per the spec's naming rule (e.g. version -> VERSION).
    pub fn option_env(&self) -> BTreeMap<String, String> {
        self.values()
            .iter()
            .map(|(k, v)| (option_env_name(k), v.text()))
            .collect()
    }

    /// `text` with each ${option} replaced by what the option's set to.
    fn filled(&self, text: &str) -> String {
        let values = self.values();
        substitute(text, |name| {
            values.get(name).map(OptionValue::text).unwrap_or_default()
        })
    }

    /// Fails unless the user set every option the feature says they must.
    pub fn check_required(&self) -> Result<()> {
        for name in &self.required_options {
            match self.overrides.get(name) {
                None => {
                    return Err(format!(
                        "feature {} needs its {name} option: --feature {}:{name}=...",
                        self.id, self.id
                    ));
                }
                Some(v) if v.text().is_empty() => {
                    return Err(format!(
                        "feature {}: its {name} option can't be empty",
                        self.id
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// This feature's credentials, with ${option} in inject.hosts and properties replaced by what
    /// the options are set to: a host that is only `${option}` is each of its comma-separated
    /// values.
    pub fn resolved_credentials(&self) -> Result<Vec<Credential>> {
        let values = self.values();
        let mut resolved = Vec::new();
        for credential in &self.credentials {
            let mut hosts = Vec::new();
            for host in &credential.inject.hosts {
                hosts.extend(self.expand(host, credential, &values)?);
            }
            let properties = credential
                .properties
                .as_ref()
                .map(|p| self.filled(p))
                .filter(|p| !p.is_empty());
            let mut credential = credential.clone();
            credential.inject.hosts = hosts;
            credential.properties = properties;
            resolved.push(credential);
        }
        Ok(resolved)
    }

    fn expand(
        &self,
        host: &str,
        credential: &Credential,
        values: &BTreeMap<String, OptionValue>,
    ) -> Result<Vec<String>> {
        let Some(option) = whole_reference(host) else {
            return Ok(vec![self.substitute_host(host, credential, None)?]);
        };
        let text = values
            .get(option)
            .map(OptionValue::text)
            .unwrap_or_default();
        let given: Vec<&str> = text
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .collect();
        if given.is_empty() {
            return Ok(vec![self.substitute_host(host, credential, Some(""))?]);
        }
        given
            .into_iter()
            .map(|v| self.substitute_host(host, credential, Some(v)))
            .collect()
    }

    /// One of inject.hosts with its ${option} filled in, as a bare host or host:port: https://
    /// dropped, as an option holding a server is the URL a user has at hand.
    fn substitute_host(
        &self,
        host: &str,
        credential: &Credential,
        value: Option<&str>,
    ) -> Result<String> {
        let option = references(host).into_iter().next();
        let filled = match value {
            Some(v) => v.to_string(),
            None => self.filled(host),
        };
        let place = format!("{}: credential {}", self.id, repr(&credential.name));
        if let Some(option) = option
            && filled.is_empty()
        {
            return Err(format!(
                "{place} is attached to the {option} option's host, which isn't set: use --feature {}:{option}=...",
                self.id
            ));
        }
        let filled = filled
            .strip_prefix("https://")
            .unwrap_or(&filled)
            .trim_end_matches('/');
        if filled.contains('/') || filled.contains(',') || filled.chars().any(char::is_whitespace) {
            return Err(format!(
                "{place}: {} must be a host, reached over https, with no path",
                repr(filled)
            ));
        }
        Ok(filled.to_string())
    }

    /// The packages it installs, its options filled in: a name with an option holding a
    /// comma-separated list is a package for each of its values, and none for an empty one.
    pub fn apt_packages(&self) -> Result<Vec<String>> {
        let values = self.values();
        let mut packages = Vec::new();
        for package in &self.apt.packages {
            let mut names = vec![package.clone()];
            let mut seen = BTreeSet::new();
            for option in references(package) {
                if !seen.insert(option) {
                    continue;
                }
                let text = values
                    .get(option)
                    .map(OptionValue::text)
                    .unwrap_or_default();
                let given: Vec<&str> = text
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .collect();
                let reference = format!("${{{option}}}");
                names = names
                    .iter()
                    .flat_map(|name| {
                        given
                            .iter()
                            .map(|v| name.replace(&reference, v))
                            .collect::<Vec<_>>()
                    })
                    .collect();
            }
            for name in &names {
                if !is_package(name) || name.contains('$') {
                    return Err(format!(
                        "{}: {}, from its options, isn't an apt package's name",
                        self.id,
                        repr(name)
                    ));
                }
            }
            packages.extend(names);
        }
        Ok(packages)
    }

    /// What to tell the agent about this feature, with its options filled in; "" for nothing.
    pub fn agent_context(&self) -> String {
        self.filled(&self.context).trim().to_string()
    }

    /// This feature with options set from a spec's `name=value` strings.
    pub fn configured(&self, settings: &[(String, String)]) -> Result<Feature> {
        let mut overrides = BTreeMap::new();
        for (name, raw) in settings {
            let Some(option) = self.options.get(name) else {
                let known = self.options.keys().cloned().collect::<Vec<_>>();
                return Err(format!(
                    "feature {} has no option {} (options: {})",
                    self.id,
                    repr(name),
                    or_none(&known)
                ));
            };
            let value = if option.kind == "boolean" {
                match raw.as_str() {
                    "true" => OptionValue::Bool(true),
                    "false" => OptionValue::Bool(false),
                    _ => {
                        return Err(format!(
                            "{}: option {name} is true or false, not {}",
                            self.id,
                            repr(raw)
                        ));
                    }
                }
            } else {
                OptionValue::String(raw.clone())
            };
            if let Some(allowed) = &option.allowed
                && !allowed.contains(&value)
            {
                let texts: Vec<String> = allowed.iter().map(OptionValue::text).collect();
                return Err(format!(
                    "{}: option {name} must be one of {}, not {}",
                    self.id,
                    texts.join(", "),
                    repr(raw)
                ));
            }
            // A required option's default is a placeholder, so what the user set is kept even
            // when it matches: it's the value, not a default.
            if value != option.default || self.required_options.contains(name) {
                overrides.insert(name.clone(), value);
            }
        }
        let mut configured = self.clone();
        configured.overrides = overrides;
        Ok(configured)
    }

    /// As its definition says, as JSON.
    pub fn json(&self) -> Value {
        let options: Map<String, Value> = self
            .options
            .iter()
            .map(|(name, o)| {
                let mut option = json!({"type": o.kind, "default": o.default.json()});
                if let Some(allowed) = &o.allowed {
                    option["enum"] = allowed.iter().map(OptionValue::json).collect();
                }
                (name.clone(), option)
            })
            .collect();
        json!({
            "id": self.id,
            "version": self.version,
            "path": self.path,
            "options": options,
            "container_env": self.container_env,
            "depends_on": self.depends_on,
            "installs_after": self.installs_after,
            "credentials": self.credentials.iter().map(credential_json).collect::<Vec<_>>(),
            "cache": self.cache.as_ref().map(|c| json!({"hosts": c.hosts, "exclude": c.exclude})),
            "apt": {
                "repositories": self.apt.repositories.iter().map(|r| json!({
                    "uri": r.uri, "suite": r.suite, "components": r.components, "key": r.key,
                })).collect::<Vec<_>>(),
                "packages": self.apt.packages,
            },
            "required_options": self.required_options,
            "context": self.context,
            "overrides": self.overrides.iter().map(|(k, v)| (k.clone(), v.json())).collect::<Map<_, _>>(),
            "spec": self.spec(),
        })
    }

    /// As json, and what its options make of it: their values, as install.sh sees them, and its
    /// context; and its credentials and packages, filled in, unless what they need isn't set, which
    /// is said instead (`"unresolved": why`).
    pub fn derived_json(&self) -> Value {
        let mut feature = self.json();
        feature["values"] = self
            .values()
            .iter()
            .map(|(k, v)| (k.clone(), v.json()))
            .collect::<Map<_, _>>()
            .into();
        feature["option_env"] = json!(self.option_env());
        feature["agent_context"] = self.agent_context().into();
        let filled = self
            .resolved_credentials()
            .and_then(|c| Ok((c, self.apt_packages()?)));
        match filled {
            Ok((credentials, packages)) => {
                feature["resolved_credentials"] = credentials
                    .iter()
                    .map(credential_json)
                    .collect::<Vec<_>>()
                    .into();
                feature["apt_packages"] = json!(packages);
            }
            Err(why) => feature["unresolved"] = why.into(),
        }
        feature
    }
}

fn credential_json(c: &Credential) -> Value {
    let routes = |routes: &[Route]| routes.iter().map(|r| r.text.clone()).collect::<Vec<_>>();
    json!({
        "name": c.name,
        "description": c.description,
        "properties": c.properties,
        "inject": {
            "hosts": c.inject.hosts,
            "value": c.inject.value,
            "header": c.inject.header,
            "standin": c.inject.standin,
            "forbid": routes(&c.inject.forbid),
            "allow": routes(&c.inject.allow),
            "rewrite": c.inject.rewrites.iter().map(|r| json!({
                "route": r.route.text, "header": r.header, "json": r.json, "encoding": r.encoding,
            })).collect::<Vec<_>>(),
        },
    })
}

// --- names and references -------------------------------------------------------

/// `[a-z0-9][a-z0-9-]*`
fn is_id(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `[A-Za-z_][A-Za-z0-9_]*`
fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// An HTTP header's name: a token.
fn is_header_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+.^_`|~-".contains(c))
}

/// An apt package's name, with ${option} where an option's value goes, and an :architecture.
fn is_package(s: &str) -> bool {
    let (name, arch) = match s.rsplit_once(':') {
        // A colon inside a reference isn't an architecture's.
        Some((name, arch)) if !arch.contains('}') => (name, Some(arch)),
        _ => (s, None),
    };
    if let Some(arch) = arch
        && (arch.is_empty()
            || !arch
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
    {
        return false;
    }
    let mut rest = name;
    if rest.is_empty() {
        return false;
    }
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                return false;
            };
            let inner = &after[..end];
            if inner.is_empty() || !inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return false;
            }
            rest = &after[end + 1..];
        } else {
            let c = rest.chars().next().unwrap();
            if !(c.is_ascii_lowercase() || c.is_ascii_digit() || "+.-".contains(c)) {
                return false;
            }
            rest = &rest[c.len_utf8()..];
        }
    }
    true
}

/// The names of the `${option}` references in `text`, in order.
pub fn references(text: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else { break };
        names.push(&after[..end]);
        rest = &after[end + 1..];
    }
    names
}

/// The option `text` is wholly a reference to, `${option}`, if it is.
fn whole_reference(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("${")?.strip_suffix('}')?;
    (!inner.contains('}')).then_some(inner)
}

/// `text` with each `${name}` replaced by `value(name)`.
fn substitute(text: &str, value: impl Fn(&str) -> String) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else { break };
        out.push_str(&rest[..start]);
        out.push_str(&value(&after[..end]));
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

fn option_env_name(option: &str) -> String {
    option
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .to_uppercase()
}

fn or_none(names: &[String]) -> String {
    let mut sorted = names.to_vec();
    sorted.sort();
    if sorted.is_empty() {
        "none".into()
    } else {
        sorted.join(", ")
    }
}

/// A JSON object's keys outside `known`, sorted.
fn unsupported(object: &Map<String, Value>, known: &[&str]) -> Vec<String> {
    object
        .keys()
        .filter(|k| !known.contains(&k.as_str()))
        .cloned()
        .collect()
}

// --- loading --------------------------------------------------------------------

/// The features a bottle can ask for: all but base, which every bottle has.
pub fn available() -> Vec<String> {
    let mut ids: Vec<String> = fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("devcontainer-feature.json").is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|id| id != BASE)
        .collect();
    ids.sort();
    ids
}

pub fn load(id: &str) -> Result<Feature> {
    let path = dir().join(id);
    if !is_id(id) || !path.join("devcontainer-feature.json").is_file() {
        let available = available();
        return Err(format!(
            "no feature named {}; available: {}",
            repr(id),
            if available.is_empty() {
                "none".into()
            } else {
                available.join(", ")
            }
        ));
    }
    let place = format!("folio/features/{id}/devcontainer-feature.json");
    let fail = |message: String| format!("{place}: {message}");
    let text = fs::read_to_string(path.join("devcontainer-feature.json"))
        .map_err(|e| fail(e.to_string()))?;
    let spec: Value =
        serde_json::from_str(&text).map_err(|e| fail(format!("invalid JSON: {e}")))?;
    let Some(spec) = spec.as_object() else {
        return Err(fail("must be a JSON object".into()));
    };
    let known: Vec<&str> = SUPPORTED_KEYS
        .iter()
        .chain(&METADATA_KEYS)
        .copied()
        .collect();
    let extra = unsupported(spec, &known);
    if !extra.is_empty() {
        let mut supported: Vec<&str> = known.clone();
        supported.sort();
        return Err(fail(format!(
            "unsupported: {} (bottle supports: {})",
            extra.join(", "),
            supported.join(", ")
        )));
    }
    if spec.get("id").and_then(Value::as_str) != Some(id) {
        return Err(fail(format!("id must be {}, its directory name", repr(id))));
    }
    let version = match spec.get("version").and_then(Value::as_str) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => return Err(fail("version is required".into())),
    };
    if !path.join("install.sh").is_file() {
        return Err(fail("install.sh is missing".into()));
    }
    let empty = Value::Object(Map::new());
    let options = options(spec.get("options").unwrap_or(&empty)).map_err(fail)?;
    let mut feature = Feature {
        id: id.to_string(),
        version,
        path: path.clone(),
        container_env: container_env(spec.get("containerEnv").unwrap_or(&empty)).map_err(fail)?,
        depends_on: depends_on(spec.get("dependsOn").unwrap_or(&empty)).map_err(fail)?,
        installs_after: id_list(
            spec.get("installsAfter").unwrap_or(&Value::Array(vec![])),
            "installsAfter",
        )
        .map_err(fail)?,
        options,
        credentials: vec![],
        cache: None,
        apt: Apt::default(),
        required_options: vec![],
        context: String::new(),
        overrides: BTreeMap::new(),
    };
    customizations(&mut feature, spec.get("customizations").unwrap_or(&empty)).map_err(fail)?;
    for repository in &feature.apt.repositories {
        if !path.join(&repository.key).is_file() {
            return Err(fail(format!(
                "customizations.bottle.apt.repositories: {}'s key {} is missing",
                repository.uri,
                repr(&repository.key)
            )));
        }
    }
    Ok(feature)
}

fn options(options: &Value) -> Result<BTreeMap<String, FeatureOption>> {
    let Some(options) = options.as_object() else {
        return Err("options must be an object".into());
    };
    let mut parsed = BTreeMap::new();
    for (name, option) in options {
        let Some(option) = option.as_object() else {
            return Err(format!("option {} must be an object", repr(name)));
        };
        let extra = unsupported(option, &OPTION_KEYS);
        if !extra.is_empty() {
            return Err(format!(
                "option {}: unsupported: {}",
                repr(name),
                extra.join(", ")
            ));
        }
        let kind = match option.get("type").and_then(Value::as_str) {
            Some(k @ ("string" | "boolean")) => k.to_string(),
            _ => {
                return Err(format!(
                    "option {}: type must be one of string, boolean",
                    repr(name)
                ));
            }
        };
        let default = option
            .get("default")
            .and_then(OptionValue::from_json)
            .filter(|d| {
                matches!(
                    (d, kind.as_str()),
                    (OptionValue::String(_), "string") | (OptionValue::Bool(_), "boolean")
                )
            });
        let Some(default) = default else {
            return Err(format!("option {}: needs a {kind} default", repr(name)));
        };
        let allowed = match option.get("enum") {
            None => None,
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .filter_map(OptionValue::from_json)
                    .collect::<Vec<_>>(),
            ),
            Some(_) => return Err(format!("option {}: default isn't in its enum", repr(name))),
        };
        if let Some(allowed) = &allowed
            && !allowed.contains(&default)
        {
            return Err(format!("option {}: default isn't in its enum", repr(name)));
        }
        if !is_env_name(&option_env_name(name)) {
            return Err(format!(
                "option {}: doesn't make a valid environment variable name",
                repr(name)
            ));
        }
        parsed.insert(
            name.clone(),
            FeatureOption {
                kind,
                default,
                allowed,
            },
        );
    }
    Ok(parsed)
}

/// What the feature asks of bottle: its credentials, its cache, what it installs with apt, the
/// options a user has to set, and its context.
fn customizations(feature: &mut Feature, customizations: &Value) -> Result<()> {
    let Some(customizations) = customizations.as_object() else {
        return Err("customizations must be an object".into());
    };
    let extra = unsupported(customizations, &["bottle"]);
    if !extra.is_empty() {
        return Err(format!(
            "customizations: unsupported: {} (only bottle is supported)",
            extra.join(", ")
        ));
    }
    let empty = Value::Object(Map::new());
    let Some(bottle) = customizations.get("bottle").unwrap_or(&empty).as_object() else {
        return Err("customizations.bottle must be an object".into());
    };
    let extra = unsupported(bottle, &BOTTLE_KEYS);
    if !extra.is_empty() {
        return Err(format!(
            "customizations.bottle: unsupported: {}",
            extra.join(", ")
        ));
    }
    let Some(credentials) = bottle.get("credentials").unwrap_or(&empty).as_object() else {
        return Err("customizations.bottle.credentials must be an object".into());
    };
    let known: Vec<String> = feature.options.keys().cloned().collect();
    let required: Vec<String> = match bottle.get("requiredOptions") {
        None => vec![],
        Some(Value::Array(names)) if names.iter().all(Value::is_string) => names
            .iter()
            .filter_map(|n| n.as_str().map(String::from))
            .collect(),
        Some(_) => {
            return Err(
                "customizations.bottle.requiredOptions must be a list of option names".into(),
            );
        }
    };
    for name in &required {
        if !feature.options.contains_key(name) {
            return Err(format!(
                "customizations.bottle.requiredOptions: no option {} (options: {})",
                repr(name),
                or_none(&known)
            ));
        }
    }
    let context = match bottle.get("context") {
        None => String::new(),
        Some(Value::String(c)) => c.clone(),
        Some(_) => return Err("customizations.bottle.context must be a string".into()),
    };
    for name in references(&context) {
        if !feature.options.contains_key(name) {
            return Err(format!(
                "customizations.bottle.context: ${{{name}}} names no option of this feature (options: {})",
                or_none(&known)
            ));
        }
    }
    for (name, spec) in credentials {
        feature
            .credentials
            .push(credential(name, spec, &feature.options)?);
    }
    if let Some(c) = bottle.get("cache") {
        feature.cache = Some(cache(c)?);
    }
    if let Some(a) = bottle.get("apt") {
        feature.apt = apt(a, &feature.options)?;
    }
    feature.required_options = required;
    feature.context = context;
    Ok(())
}

fn credential(
    name: &str,
    spec: &Value,
    options: &BTreeMap<String, FeatureOption>,
) -> Result<Credential> {
    let place = format!("credential {}", repr(name));
    if !is_id(name) {
        return Err(format!(
            "{place}: names use lowercase letters, digits and '-'"
        ));
    }
    let Some(spec) = spec.as_object() else {
        return Err(format!("{place} must be an object"));
    };
    let extra = unsupported(spec, &CREDENTIAL_KEYS);
    if !extra.is_empty() {
        return Err(format!("{place}: unsupported: {}", extra.join(", ")));
    }
    let description = match spec.get("description").and_then(Value::as_str) {
        Some(d) if !d.is_empty() => d.to_string(),
        _ => return Err(format!("{place}: needs a description")),
    };
    let Some(inject) = spec.get("inject") else {
        return Err(format!(
            "{place}: needs inject, the hosts the proxy attaches it to; a bottle never holds a credential"
        ));
    };
    let properties = match spec.get("properties") {
        None => None,
        Some(Value::String(p)) if !p.is_empty() => {
            for option in references(p) {
                if options.get(option).is_none_or(|o| o.kind != "string") {
                    return Err(format!(
                        "{place}: properties: {} names no string option of this feature",
                        repr(p)
                    ));
                }
            }
            Some(p.clone())
        }
        Some(_) => {
            return Err(format!(
                "{place}: properties must be the path of a properties file"
            ));
        }
    };
    Ok(Credential {
        name: name.to_string(),
        description,
        inject: self::inject(inject, options, &place)?,
        properties,
    })
}

fn inject(
    inject: &Value,
    options: &BTreeMap<String, FeatureOption>,
    place: &str,
) -> Result<Inject> {
    let Some(inject) = inject.as_object() else {
        return Err(format!("{place}: inject must be an object"));
    };
    let extra = unsupported(inject, &INJECT_KEYS);
    if !extra.is_empty() {
        return Err(format!(
            "{place}: inject: unsupported: {}",
            extra.join(", ")
        ));
    }
    let hosts: Vec<String> = match inject.get("hosts") {
        Some(Value::Array(hosts))
            if !hosts.is_empty()
                && hosts
                    .iter()
                    .all(|h| h.as_str().is_some_and(|h| !h.is_empty())) =>
        {
            hosts
                .iter()
                .filter_map(|h| h.as_str().map(String::from))
                .collect()
        }
        _ => {
            return Err(format!(
                "{place}: inject.hosts must be a non-empty list of hosts"
            ));
        }
    };
    let known: Vec<String> = options.keys().cloned().collect();
    for host in &hosts {
        // A credential is only ever put on a request the proxy makes over TLS, so a host says
        // nothing about its scheme: there is one.
        if host.contains("//") {
            return Err(format!(
                "{place}: inject.hosts: {} is a host or host:port, always reached over https",
                repr(host)
            ));
        }
        for option in references(host) {
            match options.get(option) {
                None => {
                    return Err(format!(
                        "{place}: inject.hosts: {} names no option of this feature (options: {})",
                        repr(host),
                        or_none(&known)
                    ));
                }
                Some(o) if o.kind != "string" => {
                    return Err(format!(
                        "{place}: inject.hosts: option {} is a {}, not a host",
                        repr(option),
                        o.kind
                    ));
                }
                _ => {}
            }
        }
    }
    let header = match inject.get("header") {
        None => "Authorization".to_string(),
        Some(Value::String(h)) if is_header_name(h) => h.clone(),
        Some(_) => return Err(format!("{place}: inject.header must be a header name")),
    };
    let value = match inject.get("value").and_then(Value::as_str) {
        Some(v) if v.contains(CREDENTIAL_PLACEHOLDER) => v.to_string(),
        _ => {
            return Err(format!(
                "{place}: inject.value must be the header value, containing {CREDENTIAL_PLACEHOLDER}"
            ));
        }
    };
    let standin = match inject.get("standin") {
        None => None,
        Some(Value::String(s)) if is_env_name(s) => Some(s.clone()),
        Some(_) => {
            return Err(format!(
                "{place}: inject.standin must be an environment variable name"
            ));
        }
    };
    let none = Value::Array(vec![]);
    let forbid = routes(
        inject.get("forbid").unwrap_or(&none),
        &format!("{place}: inject.forbid"),
    )?;
    let allow = routes(
        inject.get("allow").unwrap_or(&none),
        &format!("{place}: inject.allow"),
    )?;
    let Some(rewrites) = inject.get("rewrite").unwrap_or(&none).as_array() else {
        return Err(format!("{place}: inject.rewrite must be a list"));
    };
    let rewrites = rewrites
        .iter()
        .map(|r| rewrite(r, &format!("{place}: inject.rewrite")))
        .collect::<Result<Vec<_>>>()?;
    // forbid asserts what allow must never reach: a route allowed (a rewrite's is) that a forbidden
    // one could also match is allowed too widely.
    for route in allow.iter().chain(rewrites.iter().map(|r| &r.route)) {
        for forbidden in &forbid {
            if route.overlaps_forbidden(forbidden) {
                return Err(format!(
                    "{place}: {} is allowed, but reaches {}, which is forbidden: narrow it",
                    repr(&route.text),
                    repr(&forbidden.text)
                ));
            }
        }
    }
    Ok(Inject {
        hosts,
        value,
        header,
        standin,
        forbid,
        allow,
        rewrites,
    })
}

fn routes(routes: &Value, place: &str) -> Result<Vec<Route>> {
    let Some(routes) = routes.as_array().filter(|r| r.iter().all(Value::is_string)) else {
        return Err(format!(
            "{place} must be a list of routes, e.g. \"POST /api/auth/token\""
        ));
    };
    routes
        .iter()
        .map(|r| Route::parse(r.as_str().unwrap()).map_err(|e| format!("{place}: {e}")))
        .collect()
}

fn rewrite(rewrite: &Value, place: &str) -> Result<Rewrite> {
    let Some(rewrite) = rewrite.as_object() else {
        return Err(format!("{place}: each is an object"));
    };
    let extra = unsupported(rewrite, &REWRITE_KEYS);
    if !extra.is_empty() {
        return Err(format!("{place}: unsupported: {}", extra.join(", ")));
    }
    let route = routes(
        &Value::Array(vec![rewrite.get("route").cloned().unwrap_or(Value::Null)]),
        place,
    )?
    .remove(0);
    let text = |key: &str| match rewrite.get(key) {
        None => Some(String::new()),
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => None,
    };
    let (Some(header), Some(field)) = (text("header"), text("json")) else {
        return Err(format!(
            "{place}: {}: needs a header or a json field, not both",
            route.text
        ));
    };
    if header.is_empty() == field.is_empty() {
        return Err(format!(
            "{place}: {}: needs a header or a json field, not both",
            route.text
        ));
    }
    if !header.is_empty() && !is_header_name(&header) {
        return Err(format!(
            "{place}: {}: {} isn't a header name",
            route.text,
            repr(&header)
        ));
    }
    let encoding = match rewrite.get("encoding") {
        None => RAW.to_string(),
        Some(Value::String(e)) if e == RAW || e == BASE64 => e.clone(),
        Some(_) => {
            return Err(format!(
                "{place}: {}: encoding is {RAW} or {BASE64}",
                route.text
            ));
        }
    };
    Ok(Rewrite {
        route,
        header,
        json: field,
        encoding,
    })
}

/// `(\*\.)?[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*`: a lowercase host
/// name, or `*.` and one.
pub fn is_cache_host(host: &str) -> bool {
    let name = host.strip_prefix("*.").unwrap_or(host);
    !name.is_empty()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

fn cache(cache: &Value) -> Result<Cache> {
    let Some(cache) = cache.as_object() else {
        return Err("customizations.bottle.cache must be an object".into());
    };
    let extra = unsupported(cache, &CACHE_KEYS);
    if !extra.is_empty() {
        return Err(format!(
            "customizations.bottle.cache: unsupported: {}",
            extra.join(", ")
        ));
    }
    let hosts: Vec<String> = match cache.get("hosts") {
        Some(Value::Array(h)) if !h.is_empty() && h.iter().all(Value::is_string) => h
            .iter()
            .filter_map(|h| h.as_str().map(String::from))
            .collect(),
        _ => {
            return Err(
                "customizations.bottle.cache.hosts must be a non-empty list of hosts".into(),
            );
        }
    };
    for host in &hosts {
        // Always https, on its own port: a host is only ever a name.
        if !is_cache_host(host) {
            return Err(format!(
                "customizations.bottle.cache.hosts: {} must be a lowercase host name, or *. and one",
                repr(host)
            ));
        }
    }
    let exclude: Vec<String> = match cache.get("exclude") {
        None => vec![],
        Some(Value::Array(e))
            if e.iter().all(|p| {
                p.as_str()
                    .is_some_and(|p| p.starts_with('/') || p.starts_with('*'))
            }) =>
        {
            e.iter()
                .filter_map(|p| p.as_str().map(String::from))
                .collect()
        }
        Some(_) => {
            return Err(
                "customizations.bottle.cache.exclude must be a list of path patterns, each starting with / or *"
                    .into(),
            );
        }
    };
    Ok(Cache { hosts, exclude })
}

fn apt(spec: &Value, options: &BTreeMap<String, FeatureOption>) -> Result<Apt> {
    let place = "customizations.bottle.apt";
    let Some(spec) = spec.as_object() else {
        return Err(format!("{place} must be an object"));
    };
    let extra = unsupported(spec, &APT_KEYS);
    if !extra.is_empty() {
        return Err(format!("{place}: unsupported: {}", extra.join(", ")));
    }
    let none = Value::Array(vec![]);
    let Some(repositories) = spec.get("repositories").unwrap_or(&none).as_array() else {
        return Err(format!("{place}.repositories must be a list"));
    };
    let name_chars = |s: &str, extra: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
    };
    let mut parsed = Vec::new();
    for repository in repositories {
        let fields = repository.as_object().filter(|r| {
            let mut keys: Vec<&str> = r.keys().map(String::as_str).collect();
            keys.sort();
            keys == REPOSITORY_KEYS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        });
        let Some(r) = fields else {
            let mut keys = REPOSITORY_KEYS.to_vec();
            keys.sort();
            return Err(format!(
                "{place}.repositories: each is an object of {}",
                keys.join(", ")
            ));
        };
        let uri = r["uri"]
            .as_str()
            .filter(|u| is_repository_uri(u))
            .ok_or_else(|| {
                format!(
                    "{place}.repositories: uri {} must be an http:// or https:// URL",
                    json_repr(&r["uri"])
                )
            })?;
        let suite = r["suite"]
            .as_str()
            .filter(|s| name_chars(s, "._/-"))
            .ok_or_else(|| {
                format!(
                    "{place}.repositories: suite {} must be a suite's name, e.g. stable",
                    json_repr(&r["suite"])
                )
            })?;
        let components: Vec<String> = match &r["components"] {
            Value::Array(c)
                if !c.is_empty()
                    && c.iter()
                        .all(|c| c.as_str().is_some_and(|c| name_chars(c, "._-"))) =>
            {
                c.iter()
                    .filter_map(|c| c.as_str().map(String::from))
                    .collect()
            }
            _ => {
                return Err(format!(
                    "{place}.repositories: components must be a non-empty list of names, e.g. [\"main\"]"
                ));
            }
        };
        let key = r["key"].as_str().filter(|k| name_chars(k, "._-")).ok_or_else(|| {
            format!(
                "{place}.repositories: key {} must be the name of a file in the feature's directory",
                json_repr(&r["key"])
            )
        })?;
        parsed.push(Repository {
            uri: uri.to_string(),
            suite: suite.to_string(),
            components,
            key: key.to_string(),
        });
    }
    let packages: Vec<String> = match spec.get("packages") {
        None => vec![],
        Some(Value::Array(p)) if p.iter().all(|p| p.as_str().is_some_and(is_package)) => p
            .iter()
            .filter_map(|p| p.as_str().map(String::from))
            .collect(),
        Some(_) => {
            return Err(format!(
                "{place}.packages must be a list of package names, with ${{option}} for an option's value"
            ));
        }
    };
    let known: Vec<String> = options.keys().cloned().collect();
    for package in &packages {
        for name in references(package) {
            if !options.contains_key(name) {
                return Err(format!(
                    "{place}.packages: ${{{name}}} names no option of this feature (options: {})",
                    or_none(&known)
                ));
            }
        }
    }
    Ok(Apt {
        repositories: parsed,
        packages,
    })
}

/// `https?://[a-z0-9.-]+(/[A-Za-z0-9._~/-]*)?`
fn is_repository_uri(uri: &str) -> bool {
    let Some(rest) = uri
        .strip_prefix("https://")
        .or_else(|| uri.strip_prefix("http://"))
    else {
        return false;
    };
    let (host, path) = rest
        .split_once('/')
        .map_or((rest, None), |(h, p)| (h, Some(p)));
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && path.is_none_or(|p| {
            p.chars()
                .all(|c| c.is_ascii_alphanumeric() || "._~/-".contains(c))
        })
}

fn json_repr(v: &Value) -> String {
    match v {
        Value::String(s) => repr(s),
        other => other.to_string(),
    }
}

fn container_env(env: &Value) -> Result<BTreeMap<String, String>> {
    let Some(env) = env.as_object().filter(|e| e.values().all(Value::is_string)) else {
        return Err("containerEnv must map names to strings".into());
    };
    let mut parsed = BTreeMap::new();
    for (name, value) in env {
        let value = value.as_str().unwrap();
        if !is_env_name(name) {
            return Err(format!(
                "containerEnv: invalid variable name {}",
                repr(name)
            ));
        }
        // Set as it is, as a build's env step sets it: there's no ${NAME} to expand.
        if value.contains('$') {
            return Err(format!(
                "containerEnv: {name}: references to other variables ($) aren't supported"
            ));
        }
        parsed.insert(name.clone(), value.to_string());
    }
    Ok(parsed)
}

fn depends_on(depends_on: &Value) -> Result<Vec<String>> {
    let Some(depends_on) = depends_on.as_object() else {
        return Err("dependsOn must be an object".into());
    };
    for (dependency, options) in depends_on {
        if options.as_object().is_none_or(|o| !o.is_empty()) {
            return Err(format!(
                "dependsOn {}: options for dependencies aren't supported; use {{}}",
                repr(dependency)
            ));
        }
    }
    id_list(
        &Value::Array(
            depends_on
                .keys()
                .map(|k| Value::String(k.clone()))
                .collect(),
        ),
        "dependsOn",
    )
}

fn id_list(ids: &Value, field: &str) -> Result<Vec<String>> {
    let Some(ids) = ids.as_array().filter(|i| i.iter().all(Value::is_string)) else {
        return Err(format!("{field} must be a list of feature ids"));
    };
    let mut parsed = Vec::new();
    for id in ids.iter().filter_map(Value::as_str) {
        if !is_id(id) {
            return Err(format!(
                "{field}: {} isn't a local feature id (remote features aren't supported)",
                repr(id)
            ));
        }
        if !dir().join(id).join("devcontainer-feature.json").is_file() {
            return Err(format!("{field}: no feature named {}", repr(id)));
        }
        if id == BASE {
            return Err(format!(
                "{field}: every feature is installed on {BASE} already"
            ));
        }
        parsed.push(id.to_string());
    }
    Ok(parsed)
}

// --- specs and resolution -------------------------------------------------------

/// `jvm:version=25,additionalVersions=17,21` -> ("jvm", [("version", "25"), ("additionalVersions",
/// "17,21")]): options are comma-separated NAME=VALUE pairs, and a segment without `=` continues
/// the previous value, so list values keep the Dev Container convention of commas.
pub fn parse_spec(spec: &str) -> Result<(String, Vec<(String, String)>)> {
    let (id, rest) = spec
        .split_once(':')
        .map_or((spec, None), |(i, r)| (i, Some(r)));
    let mut settings: Vec<(String, String)> = Vec::new();
    if let Some(rest) = rest {
        for item in rest.split(',') {
            match item.split_once('=') {
                Some((key, value)) if !key.is_empty() => {
                    if settings.iter().any(|(k, _)| k == key) {
                        return Err(format!(
                            "bad feature spec {}: option {} given twice",
                            repr(spec),
                            repr(key)
                        ));
                    }
                    settings.push((key.to_string(), value.to_string()));
                }
                None if !settings.is_empty() => {
                    let last = settings.last_mut().unwrap();
                    last.1 = format!("{},{item}", last.1);
                }
                _ => {
                    return Err(format!(
                        "bad feature spec {}: expected FEATURE:OPTION=VALUE[,OPTION=VALUE]",
                        repr(spec)
                    ));
                }
            }
        }
    }
    Ok((id.to_string(), settings))
}

/// A spec in its canonical form, its options checked: `jvm`, or `jvm:version=17`.
pub fn canonical(spec: &str) -> Result<String> {
    let (id, settings) = parse_spec(spec)?;
    Ok(load(&id)?.configured(&settings)?.spec())
}

/// The requested features (specs) plus everything they depend on, in install order: dependencies
/// take their option defaults unless requested themselves; dependsOn and installsAfter both order
/// features, and dependsOn also pulls the dependency in; ties are broken by id, so the same set
/// always installs in the same order.
pub fn resolve(requested: &[String]) -> Result<Vec<Feature>> {
    let mut settings: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for spec in requested {
        let (id, options) = parse_spec(spec)?;
        if id == BASE {
            return Err(format!(
                "{BASE} is in every bottle, under its features: it isn't one to ask for"
            ));
        }
        match settings.iter().find(|(i, _)| *i == id) {
            Some((_, before)) if *before != options => {
                return Err(format!(
                    "feature {id} requested twice with different options"
                ));
            }
            Some(_) => {}
            None => settings.push((id, options)),
        }
    }
    let mut loaded: BTreeMap<String, Feature> = BTreeMap::new();
    let mut pending: Vec<String> = settings.iter().map(|(id, _)| id.clone()).collect();
    while let Some(id) = pending.pop() {
        if loaded.contains_key(&id) {
            continue;
        }
        let given = settings
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, s)| s.clone())
            .unwrap_or_default();
        let feature = load(&id)?.configured(&given)?;
        feature.check_required()?;
        pending.extend(feature.depends_on.iter().cloned());
        loaded.insert(id, feature);
    }
    let mut before: BTreeMap<String, BTreeSet<String>> = loaded
        .values()
        .map(|f| {
            let deps = f
                .depends_on
                .iter()
                .chain(&f.installs_after)
                .filter(|d| loaded.contains_key(*d))
                .cloned()
                .collect();
            (f.id.clone(), deps)
        })
        .collect();
    let mut order = Vec::new();
    while !before.is_empty() {
        let Some(ready) = before
            .iter()
            .find(|(_, deps)| deps.is_empty())
            .map(|(id, _)| id.clone())
        else {
            let ids: Vec<String> = before.keys().cloned().collect();
            return Err(format!(
                "features depend on each other in a cycle: {}",
                ids.join(", ")
            ));
        };
        before.remove(&ready);
        for deps in before.values_mut() {
            deps.remove(&ready);
        }
        order.push(loaded.remove(&ready).unwrap());
    }
    Ok(order)
}

// --- the command ----------------------------------------------------------------

const USAGE: &str = "features list | features show SPEC | features canonical SPEC... \
                     | features resolve [SPEC...] | features image [SPEC...] \
                     | features cached [--build] [SPEC...]";

pub fn main(mut args: impl Iterator<Item = String>) -> ExitCode {
    let command = args.next();
    let args: Vec<String> = args.collect();
    let result: Result<Value> = match (command.as_deref(), args.as_slice()) {
        (Some("list"), []) => Ok(json!(available())),
        (Some("show"), [spec]) => parse_spec(spec)
            .and_then(|(id, settings)| load(&id)?.configured(&settings))
            .map(|f| f.derived_json()),
        (Some("canonical"), specs) if !specs.is_empty() => specs
            .iter()
            .map(|s| canonical(s))
            .collect::<Result<Vec<_>>>()
            .map(|c| json!(c)),
        (Some("resolve"), specs) => {
            resolve(specs).map(|features| features.iter().map(Feature::derived_json).collect())
        }
        (Some("image"), []) => images::base(),
        (Some("image"), specs) => resolve(specs).and_then(|features| images::image(&features)),
        (Some("cached"), [flag]) if flag == "--build" => images::build_cached(),
        (Some("cached"), specs) => resolve(specs).map(|features| images::cached_for(&features)),
        _ => return crate::usage_error(USAGE),
    };
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("folio-cli: {e}");
            ExitCode::from(FAILED)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
