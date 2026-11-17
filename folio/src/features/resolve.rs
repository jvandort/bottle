//! Feature specs (id:option=value,...), and a set of them resolved with their dependencies, in order.

use std::collections::{BTreeMap, BTreeSet};

use crate::quoted;

use super::BASE;
use super::feature::Feature;
use super::load::load;

use super::Result;

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
                            quoted(spec),
                            quoted(key)
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
                        quoted(spec)
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
