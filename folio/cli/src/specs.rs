//! Specs: a bottle's environment -- its features and its memory -- as a file.
//!
//! ```json
//! {"features": ["claude", "jvm:version=25", "tools"], "memory": "8G"}
//! ```
//!
//! A spec can also give the bottle permissions, what folio serve lets it do beyond what every bottle
//! may (see PERMISSIONS), and labels: its creator's names for it, which bottle doesn't read (folio's
//! say which thread it's for), shown with its machine (`bottle-cli ls`) and to bottles with
//! list-bottles:
//!
//! ```json
//! {"features": ["claude"], "memory": "4G", "permissions": ["message-any"], "labels": {"thread": "fix-it"}}
//! ```
//!
//! Whoever creates a bottle owns its spec (folio: a repo's, a sidecar's), and canonical checks it
//! and gives its canonical form: each feature's options sorted and every default left out, features
//! sorted, and the memory as a size in M or G. Every spec gives its memory: a bottle has the size
//! it's given, and no more. A feature's dependencies aren't written in: they're the feature's to
//! declare, and resolved again whenever a bottle is created.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::features;

pub const KEYS: [&str; 5] = ["features", "memory", "disk", "permissions", "labels"];
/// What a bottle may be allowed beyond answering messages (serve/messages.rs), and what its agent
/// is told it can do with each (bottles' context).
pub const PERMISSIONS: [(&str, &str); 2] = [
    (
        "list-bottles",
        "You can list the other running bottles: `GET http://bottle.host/bottles` gives each one's name, \
         repo and labels (its creator's: a folio thread's says which thread).",
    ),
    (
        "message-any",
        "You can send a message to any running bottle by its name, not only answer one: its session gets \
         it as an event from you. `POST http://bottle.host/messages` with `{\"to\": NAME, \"text\": ...}`.",
    ),
];
const LABEL_VALUE_MAX: usize = 200;

/// A spec, written as folio's Python wrote one: its keys in KEYS' order, then any others.
pub struct SpecJson<'a>(pub &'a Value);

impl Serialize for SpecJson<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let Some(map) = self.0.as_object() else {
            return self.0.serialize(serializer);
        };
        let mut out = serializer.serialize_map(Some(map.len()))?;
        for key in KEYS {
            if let Some(value) = map.get(key) {
                out.serialize_entry(key, value)?;
            }
        }
        for (key, value) in map {
            if !KEYS.contains(&key.as_str()) {
                out.serialize_entry(key, value)?;
            }
        }
        out.end()
    }
}

fn permission_names() -> String {
    PERMISSIONS
        .iter()
        .map(|(p, _)| *p)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Validate a spec and put it in canonical form. Given `memory_mib`, the memory of the machine it's
/// for (`place`), its memory must fit it too: which machine a spec is for, its bottle knows and a
/// spec doesn't, since the same spec makes bottles on any runner.
pub fn canonical(data: &Value, memory_mib: Option<u64>, place: &str) -> Result<Value, String> {
    let Some(data) = data.as_object() else {
        return Err(r#"a spec is a JSON object, e.g. {"features": ["tools"]}"#.into());
    };
    let mut unknown: Vec<&str> = data
        .keys()
        .map(String::as_str)
        .filter(|k| !KEYS.contains(k))
        .collect();
    unknown.sort();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown {} {}; a spec has {} and {}",
            if unknown.len() == 1 { "key" } else { "keys" },
            unknown.join(", "),
            KEYS[..KEYS.len() - 1].join(", "),
            KEYS[KEYS.len() - 1]
        ));
    }
    let specs = match data.get("features") {
        None => Vec::new(),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => items
            .iter()
            .map(|s| s.as_str().unwrap().to_string())
            .collect(),
        Some(_) => {
            return Err(
                r#"features is a list of feature specs, e.g. ["tools", "jvm:version=25"]"#.into(),
            );
        }
    };
    let mut canonical_specs = canonical_features(&specs)?;
    canonical_specs.sort();
    features::resolve(&canonical_specs)?; // every dependency exists, and none is cyclic
    let memory = match data.get("memory") {
        None | Some(Value::Null) => {
            return Err(
                r#"a spec needs its memory, a size in M or G, e.g. {"memory": "8G"}"#.into(),
            );
        }
        Some(Value::String(memory)) => memory,
        Some(_) => return Err(r#"memory is a string: a size in M or G, e.g. "8G""#.into()),
    };
    let mut spec = Map::new();
    spec.insert("features".into(), json!(canonical_specs));
    spec.insert(
        "memory".into(),
        canonical_memory(memory, memory_mib, place)?.into(),
    );
    match data.get("disk") {
        None | Some(Value::Null) => {}
        Some(Value::String(disk)) if is_disk(disk) => {
            spec.insert("disk".into(), disk.to_uppercase().into());
        }
        Some(disk) => {
            return Err(format!(
                r#"invalid disk {}: give a size in G, e.g. "50G""#,
                python_repr(disk)
            ));
        }
    }
    let permissions = match data.get("permissions") {
        None => Vec::new(),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => items
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect::<Vec<_>>(),
        Some(_) => {
            return Err(format!(
                r#"permissions is a list of permissions, e.g. ["message-any"]; there are {}"#,
                permission_names()
            ));
        }
    };
    let mut unknown: Vec<&str> = permissions
        .iter()
        .map(String::as_str)
        .filter(|p| !PERMISSIONS.iter().any(|(known, _)| known == p))
        .collect();
    unknown.sort();
    unknown.dedup();
    if !unknown.is_empty() {
        return Err(format!(
            "unknown {} {}; there are {}",
            if unknown.len() == 1 {
                "permission"
            } else {
                "permissions"
            },
            unknown.join(", "),
            permission_names()
        ));
    }
    if !permissions.is_empty() {
        let mut sorted = permissions;
        sorted.sort();
        sorted.dedup();
        spec.insert("permissions".into(), json!(sorted));
    }
    let given = match data.get("labels") {
        None => Map::new(),
        Some(Value::Object(map)) => map.clone(),
        Some(_) => return Err(r#"labels map names to values, e.g. {"thread": "fix-it"}"#.into()),
    };
    for (name, value) in &given {
        if !is_label(name) {
            return Err(format!(
                "invalid label {}: lowercase letters, digits, '.' and '-', starting with a letter",
                crate::repr(name)
            ));
        }
        let ok = value
            .as_str()
            .is_some_and(|v| v.chars().count() <= LABEL_VALUE_MAX && v.chars().all(is_printable));
        if !ok {
            return Err(format!(
                "label {name}'s value must be printable text, at most {LABEL_VALUE_MAX} characters"
            ));
        }
    }
    if !given.is_empty() {
        let sorted: BTreeMap<String, Value> = given.into_iter().collect();
        spec.insert("labels".into(), json!(sorted));
    }
    Ok(Value::Object(spec))
}

/// Whether Python's str.isprintable would call `c` printable: not a control, a separator but the
/// space, or a format character. (Unassigned code points, which Python also refuses, pass here:
/// std has no table of them.)
fn is_printable(c: char) -> bool {
    let format = matches!(c, '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}' | '\u{6dd}' | '\u{70f}'
        | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}');
    c == ' ' || !(c.is_control() || c.is_whitespace() || format)
}

fn is_label(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
}

fn is_disk(disk: &str) -> bool {
    let Some(number) = disk.strip_suffix(['G', 'g']) else {
        return false;
    };
    is_size(number)
}

fn is_size(number: &str) -> bool {
    number.starts_with(|c: char| ('1'..='9').contains(&c))
        && number.chars().all(|c| c.is_ascii_digit())
}

/// A JSON value as Python's repr has it, in a message: a string quoted, others as they are.
pub fn python_repr(value: &Value) -> String {
    match value {
        Value::String(s) => crate::repr(s),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

/// A spec's labels; one saved before labels, a thread's, as the label thread.
pub fn labels(spec: &Value) -> BTreeMap<String, String> {
    if let Some(labels) = spec.get("labels").and_then(Value::as_object) {
        return labels
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
            .collect();
    }
    match spec.get("thread").and_then(Value::as_str) {
        Some(thread) if !thread.is_empty() => BTreeMap::from([("thread".into(), thread.into())]),
        _ => BTreeMap::new(),
    }
}

/// Validate feature specs and put each in canonical form, keeping their order.
pub fn canonical_features(specs: &[String]) -> Result<Vec<String>, String> {
    let mut result: Vec<(String, String)> = Vec::new();
    for spec in specs {
        let canonical = features::canonical(spec)?;
        let id = canonical.split(':').next().unwrap_or_default().to_string();
        match result.iter().find(|(i, _)| *i == id) {
            Some((_, earlier)) if *earlier != canonical => {
                return Err(format!("feature {id} given twice with different options"));
            }
            Some(_) => {}
            None => result.push((id, canonical)),
        }
    }
    Ok(result.into_iter().map(|(_, c)| c).collect())
}

/// Validate a memory setting: a size in M or G (e.g. 8G), and no bigger than `memory_mib` if it's
/// given.
pub fn canonical_memory(
    value: &str,
    memory_mib: Option<u64>,
    place: &str,
) -> Result<String, String> {
    let invalid = || {
        format!(
            "invalid memory {}: give a size in M or G, e.g. 8G",
            crate::repr(value)
        )
    };
    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let unit = unit.to_uppercase();
    if !(unit == "M" || unit == "G") || !is_size(number) {
        return Err(invalid());
    }
    let amount: u64 = number.parse().map_err(|_| invalid())?;
    if let Some(mib) = memory_mib
        && amount.saturating_mul(if unit == "G" { 1024 } else { 1 }) > mib
    {
        return Err(format!("memory {value} is more than {place} has ({mib}M)"));
    }
    Ok(format!("{amount}{unit}"))
}

/// This machine's memory, in MiB: a Mac's from sysctl, Linux's from /proc/meminfo.
pub fn host_memory_mib() -> Result<u64, String> {
    if cfg!(target_os = "linux") {
        let meminfo =
            std::fs::read_to_string("/proc/meminfo").map_err(|e| format!("/proc/meminfo: {e}"))?;
        for line in meminfo.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                let kib: u64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|n| n.parse().ok())
                    .ok_or("/proc/meminfo has no MemTotal")?;
                return Ok(kib / 1024);
            }
        }
        return Err("/proc/meminfo has no MemTotal".into());
    }
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .map_err(|e| format!("sysctl: {e}"))?;
    let bytes: u64 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| "sysctl -n hw.memsize said no number".to_string())?;
    Ok(bytes / (1024 * 1024))
}

/// The spec's disk in GiB; None: bottle-cli's default (64G, sparse).
pub fn disk_gib(spec: &Value) -> Option<u64> {
    spec.get("disk")
        .and_then(Value::as_str)
        .and_then(|d| d[..d.len() - 1].parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(data: Value) -> Result<Value, String> {
        canonical(&data, None, "this machine")
    }

    #[test]
    fn features_are_sorted_with_defaults_left_out() {
        let spec = canon(
            json!({"features": ["tools", "jvm:version=25,additionalVersions=17,21", "claude"], "memory": "2G"}),
        );
        assert_eq!(
            spec.unwrap(),
            json!({"features": ["claude", "jvm:additionalVersions=17,21", "tools"], "memory": "2G"})
        );
    }

    #[test]
    fn a_spec_gives_its_memory_as_a_size() {
        assert_eq!(canon(json!({"memory": "8g"})).unwrap()["memory"], "8G");
        assert_eq!(
            canon(json!({"memory": "2G"})).unwrap(),
            json!({"features": [], "memory": "2G"})
        );
        for data in [
            json!({}),
            json!({"features": ["tools"]}),
            json!({"memory": "all"}),
        ] {
            assert!(canon(data.clone()).is_err(), "{data}");
        }
    }

    #[test]
    fn dependencies_are_left_to_the_feature_and_its_its_own_canonical_form() {
        // Resolved, so a missing or cyclic dependency fails here; but the spec keeps what was asked
        // for, not what it pulls in.
        assert_eq!(
            canon(json!({"features": ["chromium"], "memory": "2G"})).unwrap()["features"],
            json!(["chromium"])
        );
        let spec =
            canon(json!({"features": ["tools", "jvm:version=21"], "memory": "2048m"})).unwrap();
        assert_eq!(canon(spec.clone()).unwrap(), spec);
    }

    #[test]
    fn refuses() {
        for (data, message) in [
            (json!([]), "a spec is a JSON object"),
            (json!({"feature": ["tools"]}), "unknown key feature"),
            (json!({"features": "tools"}), "features is a list"),
            (json!({"features": [1]}), "features is a list"),
            (json!({"features": ["nope"]}), "no feature named 'nope'"),
            (
                json!({"features": ["jvm:version=21", "jvm"]}),
                "given twice with different options",
            ),
            (json!({"features": ["teamcity"]}), "needs its server option"),
            (json!({}), "a spec needs its memory"),
            (json!({"memory": 8}), "memory is a string"),
            (json!({"memory": "lots"}), "invalid memory"),
            (
                json!({"memory": "2G", "permissions": "message-any"}),
                "permissions is a list",
            ),
            (
                json!({"memory": "2G", "permissions": ["root"]}),
                "unknown permission root; there are list-bottles, message-any",
            ),
            (
                json!({"memory": "2G", "labels": ["thread"]}),
                "labels map names to values",
            ),
            (
                json!({"memory": "2G", "labels": {"Thread": "x"}}),
                "invalid label 'Thread'",
            ),
            (
                json!({"memory": "2G", "labels": {"thread": 1}}),
                "label thread's value must be printable text",
            ),
            (
                json!({"memory": "2G", "labels": {"thread": "a\nb"}}),
                "label thread's value must be printable text",
            ),
        ] {
            let refused = canon(data.clone()).unwrap_err();
            assert!(refused.contains(message), "{data}: {refused}");
        }
    }

    #[test]
    fn permissions_are_sorted_once_each_and_left_out_when_none() {
        let spec = canon(
            json!({"memory": "2G", "permissions": ["message-any", "list-bottles", "message-any"]}),
        )
        .unwrap();
        assert_eq!(spec["permissions"], json!(["list-bottles", "message-any"]));
        assert!(
            canon(json!({"memory": "2G", "permissions": []}))
                .unwrap()
                .get("permissions")
                .is_none()
        );
        assert_eq!(canon(spec.clone()).unwrap(), spec);
    }

    #[test]
    fn labels_its_creators_and_one_saved_before_them() {
        let spec =
            canon(json!({"memory": "2G", "labels": {"thread": "fix-it", "repo": "app"}})).unwrap();
        assert_eq!(spec["labels"], json!({"repo": "app", "thread": "fix-it"}));
        assert!(
            canon(json!({"memory": "2G"}))
                .unwrap()
                .get("labels")
                .is_none()
        );
        let pairs = |p: &[(&str, &str)]| {
            p.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(
            labels(&spec),
            pairs(&[("repo", "app"), ("thread", "fix-it")])
        );
        assert_eq!(
            labels(&json!({"memory": "2G", "thread": "old"})),
            pairs(&[("thread", "old")])
        );
        assert_eq!(labels(&json!({"memory": "2G"})), pairs(&[]));
    }

    #[test]
    fn a_disk_in_whole_g_or_none() {
        let spec = canon(json!({"memory": "2G", "disk": "50g"})).unwrap();
        assert_eq!(
            (spec["disk"].as_str(), disk_gib(&spec)),
            (Some("50G"), Some(50))
        );
        assert_eq!(canon(spec.clone()).unwrap(), spec);
        assert_eq!(disk_gib(&canon(json!({"memory": "2G"})).unwrap()), None);
        for disk in [
            json!("50"),
            json!("512M"),
            json!("0G"),
            json!(50),
            json!("1.5G"),
        ] {
            assert!(
                canon(json!({"memory": "2G", "disk": disk}))
                    .unwrap_err()
                    .contains("invalid disk"),
                "{disk}"
            );
        }
    }

    #[test]
    fn a_spec_alone_asks_for_any_memory_and_one_for_a_runner_fits_it() {
        // Which machine it's for is its bottle's to know, and to check it against.
        assert_eq!(canon(json!({"memory": "512G"})).unwrap()["memory"], "512G");
        assert_eq!(
            canonical(&json!({"memory": "8G"}), Some(16384), "runner box").unwrap()["memory"],
            "8G"
        );
        assert_eq!(
            canonical(&json!({"memory": "32G"}), Some(16384), "runner box").unwrap_err(),
            "memory 32G is more than runner box has (16384M)"
        );
        assert!(host_memory_mib().unwrap() > 0);
    }

    #[test]
    fn memory_is_a_size_in_m_or_g_that_fits() {
        assert_eq!(canonical_memory("8g", None, "this machine").unwrap(), "8G");
        assert_eq!(canonical_memory("512M", Some(1024), "x").unwrap(), "512M");
        assert_eq!(
            canonical_memory("2G", Some(1024), "runner box").unwrap_err(),
            "memory 2G is more than runner box has (1024M)"
        );
        for bad in ["", "G", "08G", "1.5G", "1T", "all"] {
            assert_eq!(
                canonical_memory(bad, None, "x").unwrap_err(),
                format!(
                    "invalid memory {}: give a size in M or G, e.g. 8G",
                    crate::repr(bad)
                )
            );
        }
    }

    #[test]
    fn written_in_its_keys_order() {
        let spec = json!({"labels": {"thread": "t"}, "memory": "2G", "features": ["tools"], "disk": "50G"});
        assert_eq!(
            crate::pyjson::dumps(&SpecJson(&spec), false),
            "{\n  \"features\": [\n    \"tools\"\n  ],\n  \"memory\": \"2G\",\n  \"disk\": \"50G\",\n  \"labels\": {\n    \"thread\": \"t\"\n  }\n}"
        );
    }

    #[test]
    fn labels_or_a_thread_from_before_them() {
        assert_eq!(
            labels(&json!({"thread": "x"})),
            BTreeMap::from([("thread".to_string(), "x".to_string())])
        );
        assert_eq!(labels(&json!({})), BTreeMap::new());
    }
}
