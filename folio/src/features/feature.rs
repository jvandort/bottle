//! A feature, as folio has it once loaded: its options, credentials, cache, apt and the rest.

#[cfg(test)]
use serde_json::{Map, json};

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::quoted;
use crate::routes::Route;

use super::names::{is_package, option_env_name, or_none, references, substitute, whole_reference};

use super::Result;

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

    #[cfg(test)]
    pub(super) fn json(&self) -> Value {
        match self {
            OptionValue::String(s) => s.clone().into(),
            OptionValue::Bool(b) => (*b).into(),
        }
    }

    pub(super) fn from_json(v: &Value) -> Option<OptionValue> {
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
    pub(super) fn filled(&self, text: &str) -> String {
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

    pub(super) fn expand(
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
    pub(super) fn substitute_host(
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
        let place = format!("{}: credential {}", self.id, quoted(&credential.name));
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
                quoted(filled)
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
                        quoted(name)
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
                    quoted(name),
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
                            quoted(raw)
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
                    quoted(raw)
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
    #[cfg(test)]
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

    #[cfg(test)]
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

#[cfg(test)]
pub(super) fn credential_json(c: &Credential) -> Value {
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
