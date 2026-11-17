//! Reading a feature's devcontainer-feature.json, and checking it: what folio supports of it, and nothing else.

use std::collections::BTreeMap;
use std::fs;

use serde_json::{Map, Value};

use crate::quoted;

use super::feature::{Apt, Feature, FeatureOption, OptionValue};
use super::names::{is_env_name, is_id, option_env_name, or_none, references, unsupported};
use super::sections::{apt, cache, credential};
use super::{BASE, BOTTLE_KEYS, METADATA_KEYS, OPTION_KEYS, SUPPORTED_KEYS, dir};

use super::Result;

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
            quoted(id),
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
        return Err(fail(format!(
            "id must be {}, its directory name",
            quoted(id)
        )));
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
                quoted(&repository.key)
            )));
        }
    }
    Ok(feature)
}

pub(super) fn options(options: &Value) -> Result<BTreeMap<String, FeatureOption>> {
    let Some(options) = options.as_object() else {
        return Err("options must be an object".into());
    };
    let mut parsed = BTreeMap::new();
    for (name, option) in options {
        let Some(option) = option.as_object() else {
            return Err(format!("option {} must be an object", quoted(name)));
        };
        let extra = unsupported(option, &OPTION_KEYS);
        if !extra.is_empty() {
            return Err(format!(
                "option {}: unsupported: {}",
                quoted(name),
                extra.join(", ")
            ));
        }
        let kind = match option.get("type").and_then(Value::as_str) {
            Some(k @ ("string" | "boolean")) => k.to_string(),
            _ => {
                return Err(format!(
                    "option {}: type must be one of string, boolean",
                    quoted(name)
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
            return Err(format!("option {}: needs a {kind} default", quoted(name)));
        };
        let allowed = match option.get("enum") {
            None => None,
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .filter_map(OptionValue::from_json)
                    .collect::<Vec<_>>(),
            ),
            Some(_) => {
                return Err(format!(
                    "option {}: default isn't in its enum",
                    quoted(name)
                ));
            }
        };
        if let Some(allowed) = &allowed
            && !allowed.contains(&default)
        {
            return Err(format!(
                "option {}: default isn't in its enum",
                quoted(name)
            ));
        }
        if !is_env_name(&option_env_name(name)) {
            return Err(format!(
                "option {}: doesn't make a valid environment variable name",
                quoted(name)
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
pub(super) fn customizations(feature: &mut Feature, customizations: &Value) -> Result<()> {
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
                quoted(name),
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

pub(super) fn container_env(env: &Value) -> Result<BTreeMap<String, String>> {
    let Some(env) = env.as_object().filter(|e| e.values().all(Value::is_string)) else {
        return Err("containerEnv must map names to strings".into());
    };
    let mut parsed = BTreeMap::new();
    for (name, value) in env {
        let value = value.as_str().unwrap();
        if !is_env_name(name) {
            return Err(format!(
                "containerEnv: invalid variable name {}",
                quoted(name)
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

pub(super) fn depends_on(depends_on: &Value) -> Result<Vec<String>> {
    let Some(depends_on) = depends_on.as_object() else {
        return Err("dependsOn must be an object".into());
    };
    for (dependency, options) in depends_on {
        if options.as_object().is_none_or(|o| !o.is_empty()) {
            return Err(format!(
                "dependsOn {}: options for dependencies aren't supported; use {{}}",
                quoted(dependency)
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

pub(super) fn id_list(ids: &Value, field: &str) -> Result<Vec<String>> {
    let Some(ids) = ids.as_array().filter(|i| i.iter().all(Value::is_string)) else {
        return Err(format!("{field} must be a list of feature ids"));
    };
    let mut parsed = Vec::new();
    for id in ids.iter().filter_map(Value::as_str) {
        if !is_id(id) {
            return Err(format!(
                "{field}: {} isn't a local feature id (remote features aren't supported)",
                quoted(id)
            ));
        }
        if !dir().join(id).join("devcontainer-feature.json").is_file() {
            return Err(format!("{field}: no feature named {}", quoted(id)));
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
