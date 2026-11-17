//! The sections of a feature's customizations.bottle: credentials (inject, rewrites), cache and apt.

use super::*;

pub(super) fn credential(
    name: &str,
    spec: &Value,
    options: &BTreeMap<String, FeatureOption>,
) -> Result<Credential> {
    let place = format!("credential {}", quoted(name));
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
                        quoted(p)
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

pub(super) fn inject(
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
                quoted(host)
            ));
        }
        for option in references(host) {
            match options.get(option) {
                None => {
                    return Err(format!(
                        "{place}: inject.hosts: {} names no option of this feature (options: {})",
                        quoted(host),
                        or_none(&known)
                    ));
                }
                Some(o) if o.kind != "string" => {
                    return Err(format!(
                        "{place}: inject.hosts: option {} is a {}, not a host",
                        quoted(option),
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
                    quoted(&route.text),
                    quoted(&forbidden.text)
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

pub(super) fn routes(routes: &Value, place: &str) -> Result<Vec<Route>> {
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

pub(super) fn rewrite(rewrite: &Value, place: &str) -> Result<Rewrite> {
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
            quoted(&header)
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

pub(super) fn cache(cache: &Value) -> Result<Cache> {
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
                quoted(host)
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

pub(super) fn apt(spec: &Value, options: &BTreeMap<String, FeatureOption>) -> Result<Apt> {
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
                    r["uri"]
                )
            })?;
        let suite = r["suite"]
            .as_str()
            .filter(|s| name_chars(s, "._/-"))
            .ok_or_else(|| {
                format!(
                    "{place}.repositories: suite {} must be a suite's name, e.g. stable",
                    r["suite"]
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
                r["key"]
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
pub(super) fn is_repository_uri(uri: &str) -> bool {
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
