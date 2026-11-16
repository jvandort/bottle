//! What folio keeps, brought up to date once. A home says which format it's in (`format`, a
//! number, in it); a command finding an older one converts it to this one, FORMAT, before it reads
//! anything, so the rest of folio reads only the format it writes. Converting is idempotent, done
//! under a lock, and stamped when it's done.
//!
//! Format 1 is the first: what a home from before had that format 1 says otherwise.
//!
//! - **A thread's record**: its runner at the top (it's its settings' now); its sidecar's whole
//!   spec (a sidecar's settings are an overlay on a new one's); last_active (its bottle's now).
//! - **A bottle's record**: machine_name (its machine is its name) and memory (its spec's); fields
//!   a record from before them hadn't (status, spec, runner, ssh, network_version...), and its
//!   mounts' objects and paths under bottle.host, written out; keys since removed dropped.
//! - **The broker's snapshot from before notes**: its vault archived as a workspace's, and its
//!   conversation kept under /workspace's name; both made /notes'.
//! - **Runners**: bottle-cli's list ($BOTTLE_HOME/runners.json, from when runners were its), taken
//!   as folio's if folio has none; the list's version written out.
//! - **Credentials** kept under bottle's Keychain service, from before folio had its own: moved to
//!   folio's.

use std::fs;
use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::store::{self, Content};

/// The format folio writes.
pub const FORMAT: u32 = 1;
/// Where a home says its format.
const STAMP: &str = "format";

fn stamped(home: &Path) -> u32 {
    fs::read_to_string(home.join(STAMP))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// folio's home in this format: converted, if it's in an older one. A home that isn't there yet
/// has nothing to convert.
pub fn ensure() -> Result<(), String> {
    let home = crate::home();
    if !home.is_dir() || stamped(&home) >= FORMAT {
        return Ok(());
    }
    let lock = fs::File::create(home.join(".format.lock"))
        .map_err(|e| format!("{}: {e}", home.display()))?;
    // SAFETY: flock on a file this holds, released as it's closed.
    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
    if stamped(&home) >= FORMAT {
        return Ok(()); // another command did it meanwhile
    }
    convert(&home)?;
    store::write_bytes(&home.join(STAMP), format!("{FORMAT}\n").as_bytes())?;
    crate::log::log!("migrate", "{}: now in format {FORMAT}", home.display());
    Ok(())
}

/// Everything in `home` in this format.
fn convert(home: &Path) -> Result<(), String> {
    for repo in subdirectories(home) {
        for thread in subdirectories(&repo) {
            thread_record(&thread)?;
            for record in fs::read_dir(thread.join("bottles"))
                .into_iter()
                .flatten()
                .flatten()
            {
                let path = record.path();
                if path.extension().is_some_and(|e| e == "json") {
                    bottle_record(&path)?;
                }
            }
        }
    }
    broker_snapshot(&crate::broker::repo().directory().join(crate::broker::NAME))?;
    runners(home)?;
    crate::auth::move_from_bottles();
    Ok(())
}

fn subdirectories(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    found.sort();
    found
}

/// `json`'s object, made one if it isn't.
fn object(json: &mut Value) -> &mut serde_json::Map<String, Value> {
    if !json.is_object() {
        *json = json!({});
    }
    json.as_object_mut().unwrap()
}

// --- a thread's record ----------------------------------------------------------------------

fn thread_record(dir: &Path) -> Result<(), String> {
    let Some(slot) = store::current_slot(dir) else {
        return Ok(());
    };
    let path = slot.join(crate::threads::RECORD);
    let Some(old) = store::read_json(&path)? else {
        return Ok(());
    };
    let new = thread_json(old.clone());
    if new != old {
        store::write_slot(
            dir,
            vec![(
                crate::threads::RECORD,
                Content::Bytes(store::json(&new).into_bytes()),
            )],
        )?;
    }
    Ok(())
}

/// A thread's record in this format.
pub fn thread_json(mut record: Value) -> Value {
    let top = object(&mut record);
    top.remove("last_active");
    let runner = top
        .remove("runner")
        .filter(|r| r.as_str().is_some_and(|r| !r.is_empty()));
    let settings = object(top.entry("settings").or_insert_with(|| json!({})));
    if let Some(runner) = runner
        && !settings.contains_key("runner")
    {
        settings.insert("runner".into(), runner);
    }
    if let Some(sidecar) = top.get_mut("sidecar").filter(|s| !s.is_null()) {
        overlay(object(sidecar));
    }
    for key in ["base", "sidecar"] {
        top.entry(key).or_insert(Value::Null);
    }
    top.entry("broker").or_insert(json!(false));
    record
}

/// A sidecar's settings from before they were an overlay: its whole spec's features, memory and
/// disk made its own, over what a new one has (none, and 2G, which a whole spec said anyway).
fn overlay(sidecar: &mut serde_json::Map<String, Value>) {
    let Some(spec) = sidecar.remove("spec") else {
        return;
    };
    let features: Vec<String> = crate::repos::features_of(&spec);
    let own: Vec<String> = sidecar
        .get("features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|f| f.as_str().map(String::from))
        .collect();
    let merged = crate::repos::merge_features(&features, &own);
    if !merged.is_empty() {
        sidecar.insert("features".into(), json!(merged));
    }
    for key in ["memory", "disk"] {
        if let Some(value) = spec.get(key).filter(|v| v.is_string())
            && !sidecar.contains_key(key)
        {
            sidecar.insert(key.into(), value.clone());
        }
    }
}

// --- a bottle's record ----------------------------------------------------------------------

fn bottle_record(path: &Path) -> Result<(), String> {
    let Some(old) = store::read_json(path)? else {
        return Ok(());
    };
    let new = bottle_json(old.clone());
    if new != old {
        store::write_json(path, &new)?;
    }
    Ok(())
}

/// A bottle's record in this format.
pub fn bottle_json(mut record: Value) -> Value {
    let top = object(&mut record);
    let memory = top.remove("memory");
    top.remove("machine_name");
    for (key, default) in [
        ("status", json!("creating")),
        ("features", json!([])),
        ("spec", json!({})),
        ("mounts", json!([])),
        ("context", json!("")),
        ("runner", json!(crate::runners::LOCAL)),
        ("ssh", json!([])),
        ("network_version", json!(0)),
        ("last_active", json!(0.0)),
    ] {
        top.entry(key).or_insert(default);
    }
    // Its memory, where a record from before had it alone.
    if let Some(memory) = memory.filter(|m| m.is_string()) {
        object(&mut top["spec"]).entry("memory").or_insert(memory);
    }
    for mount in top["mounts"].as_array_mut().into_iter().flatten() {
        let mount = object(mount);
        let path = mount
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if mount
            .get("objects")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            mount.insert("objects".into(), json!(format!("/mnt/objects{path}")));
        }
        if mount.get("at").is_none_or(Value::is_null) {
            mount.insert("at".into(), json!(path.trim_start_matches('/')));
        }
        mount.entry("branch").or_insert(Value::Null);
        // A remote is its name, what it fetches and what it pushes; options since removed go.
        for remote in mount
            .entry("remotes")
            .or_insert(json!([]))
            .as_array_mut()
            .into_iter()
            .flatten()
        {
            let remote = object(remote);
            remote.retain(|k, _| ["name", "fetch", "push"].contains(&k.as_str()));
            remote.entry("push").or_insert(Value::Null);
        }
    }
    // Keys since removed (a driver, say) go: only what a bottle is.
    const KEYS: [&str; 13] = [
        "name",
        "id",
        "image",
        "created",
        "status",
        "features",
        "spec",
        "mounts",
        "context",
        "runner",
        "ssh",
        "network_version",
        "last_active",
    ];
    top.retain(|k, _| KEYS.contains(&k.as_str()));
    record
}

// --- the broker's snapshot from before notes ---------------------------------------------

fn broker_snapshot(dir: &Path) -> Result<(), String> {
    let Some(slot) = store::current_slot(dir) else {
        return Ok(());
    };
    use crate::snapshots::{CLAUDE_ARCHIVE, NOTES_ARCHIVE, WORKSPACE_ARCHIVE};
    let vault = slot.join(WORKSPACE_ARCHIVE);
    if !vault.exists() || slot.join(NOTES_ARCHIVE).exists() {
        return Ok(());
    }
    // Made beside the slots, then moved in: the new slot has them, the old one stays whole till
    // then.
    let claude = slot.join(CLAUDE_ARCHIVE);
    let moved = dir.join(format!(".{CLAUDE_ARCHIVE}.migrate"));
    if claude.exists() {
        renamed_in_archive(&claude, &moved, "projects/-workspace", "projects/-notes")?;
    }
    let notes = dir.join(format!(".{NOTES_ARCHIVE}.migrate"));
    fs::copy(&vault, &notes).map_err(|e| format!("{}: {e}", vault.display()))?;
    let mut files = vec![
        (NOTES_ARCHIVE, Content::Move(notes)),
        (WORKSPACE_ARCHIVE, Content::Leave),
    ];
    if claude.exists() {
        files.push((CLAUDE_ARCHIVE, Content::Move(moved)));
    }
    store::write_slot(dir, files)?;
    Ok(())
}

/// The gzipped tar at `from` written to `to`, each entry under `old` (as a path's first parts, with
/// or without ./) put under `new` instead.
fn renamed_in_archive(from: &Path, to: &Path, old: &str, new: &str) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("{}: {e}", from.display());
    let mut bytes = Vec::new();
    fs::File::open(from)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(failed)?;
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
    let out = fs::File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        out,
        flate2::Compression::default(),
    ));
    for entry in archive.entries().map_err(failed)? {
        let mut entry = entry.map_err(failed)?;
        let path = entry.path().map_err(failed)?.to_string_lossy().into_owned();
        let (dot, rest) = path
            .strip_prefix("./")
            .map_or(("", path.as_str()), |r| ("./", r));
        let renamed = match rest.strip_prefix(old) {
            Some(after) if after.is_empty() || after.starts_with('/') => {
                format!("{dot}{new}{after}")
            }
            _ => path.clone(),
        };
        let mut header = entry.header().clone();
        let link = entry.link_name().map_err(failed)?.map(|l| l.into_owned());
        if let Some(link) = link {
            builder
                .append_link(&mut header, &renamed, link)
                .map_err(failed)?;
        } else {
            builder
                .append_data(&mut header, &renamed, &mut entry)
                .map_err(failed)?;
        }
    }
    builder
        .into_inner()
        .and_then(|gz| gz.finish())
        .map(|_| ())
        .map_err(|e| format!("{}: {e}", to.display()))
}

// --- runners ----------------------------------------------------------------------------

fn runners(home: &Path) -> Result<(), String> {
    let path = home.join("runners.json");
    let mut registry = match store::read_json(&path)? {
        Some(registry) => registry,
        None => {
            let bottles = std::env::var_os("BOTTLE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".bottle")
                })
                .join("runners.json");
            match store::read_json(&bottles)? {
                Some(registry) => registry,
                None => return Ok(()),
            }
        }
    };
    let top = object(&mut registry);
    top.entry("version").or_insert(json!(1));
    top.entry("runners").or_insert(json!({}));
    store::write_json(&path, &registry)
}

#[cfg(test)]
#[path = "migrate_tests.rs"]
mod tests;
