//! Names a feature uses (ids, variables, headers, packages), and `${references}` to its options.

use serde_json::{Map, Value};

/// `[a-z0-9][a-z0-9-]*`
pub(super) fn is_id(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `[A-Za-z_][A-Za-z0-9_]*`
pub(super) fn is_env_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// An HTTP header's name: a token.
pub(super) fn is_header_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+.^_`|~-".contains(c))
}

/// An apt package's name, with ${option} where an option's value goes, and an :architecture.
pub(super) fn is_package(s: &str) -> bool {
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
pub(super) fn whole_reference(text: &str) -> Option<&str> {
    let inner = text.strip_prefix("${")?.strip_suffix('}')?;
    (!inner.contains('}')).then_some(inner)
}

/// `text` with each `${name}` replaced by `value(name)`.
pub(super) fn substitute(text: &str, value: impl Fn(&str) -> String) -> String {
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

pub(super) fn option_env_name(option: &str) -> String {
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

pub(super) fn or_none(names: &[String]) -> String {
    let mut sorted = names.to_vec();
    sorted.sort();
    if sorted.is_empty() {
        "none".into()
    } else {
        sorted.join(", ")
    }
}

/// A JSON object's keys outside `known`, sorted.
pub(super) fn unsupported(object: &Map<String, Value>, known: &[&str]) -> Vec<String> {
    object
        .keys()
        .filter(|k| !known.contains(&k.as_str()))
        .cloned()
        .collect()
}

// --- loading --------------------------------------------------------------------
