//! What folio keeps, from before format 1, converted to it.

use std::fs;
use std::io::Read;

use serde_json::json;

use super::*;
use crate::bottles::Bottle;
use crate::tempdir;
use crate::threads::{Settings, Thread};

#[test]
fn a_threads_runner_sidecar_and_last_active_from_before_settings() {
    let old = json!({"name": "t", "bottle": "b", "created": 1, "runner": "desktop", "last_active": 2,
                     "sidecar": {"runner": "box", "spec": {"features": ["jvm"], "memory": "32G", "disk": "9G"}}});
    let thread: Thread = serde_json::from_value(thread_json(old)).unwrap();
    assert_eq!(thread.settings.runner.as_deref(), Some("desktop"));
    assert!(!thread.settings.changes_spec());
    assert_eq!((thread.base, thread.broker), (None, false));
    assert_eq!(
        thread.sidecar.unwrap(),
        Settings {
            features: vec!["jvm".into()],
            memory: Some("32G".into()),
            disk: Some("9G".into()),
            runner: Some("box".into()),
        }
    );
    // One with neither reads as one with no settings of its own and no sidecar.
    let thread: Thread = serde_json::from_value(thread_json(
        json!({"name": "t", "bottle": "b", "created": 1}),
    ))
    .unwrap();
    assert_eq!(
        (thread.settings, thread.sidecar),
        (Settings::default(), None)
    );
}

#[test]
fn a_thread_in_this_format_is_as_it_was() {
    let record = json!({"name": "t", "bottle": "b", "created": 1.5, "base": null, "broker": false,
                        "settings": {"memory": "8G"}, "sidecar": {"runner": "box"}});
    assert_eq!(thread_json(record.clone()), record);
}

#[test]
fn a_bottles_record_from_an_older_folio() {
    let old = json!({"name": "b", "id": "i", "image": "base", "created": 1.5, "driver": "smolvm",
                     "machine_name": "b", "memory": "4G",
                     "mounts": [{"path": "/workspace", "repo": "/r", "commit": "c",
                                 "remotes": [{"name": "origin", "fetch": "upstream", "gone": 1}]}]});
    let new = bottle_json(old);
    assert!(
        new.get("driver").is_none()
            && new.get("machine_name").is_none()
            && new.get("memory").is_none()
    );
    let bottle: Bottle = serde_json::from_value(new).unwrap();
    assert_eq!(
        (
            bottle.status.as_str(),
            bottle.runner.as_str(),
            bottle.network_version
        ),
        ("creating", "local", 0)
    );
    assert_eq!(bottle.spec["memory"], "4G");
    assert_eq!(bottle.machine(), "b");
    assert_eq!(bottle.mounts[0].objects, "/mnt/objects/workspace");
    assert_eq!(
        bottle.mounts[0].url("origin"),
        "http://bottle.host/git/workspace/origin"
    );
    assert!(bottle.features.is_empty());
}

/// A gzipped tar of `files` (path, content).
fn archive(files: &[(&str, &str)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    for (path, content) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, path, content.as_bytes())
            .unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}

fn paths_in(path: &Path) -> Vec<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
    archive
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
        .collect()
}

#[test]
fn the_brokers_snapshot_from_before_notes() {
    let tmp = tempdir::Dir::new();
    let dir = tmp.path().join("broker");
    store::write_slot(
        &dir,
        vec![
            ("thread.json", Content::Bytes(b"{}".to_vec())),
            (
                "workspace.tar.gz",
                Content::Bytes(archive(&[
                    ("./.git/HEAD", "ref: refs/heads/main\n"),
                    ("./mine.md", "unpushed\n"),
                ])),
            ),
            (
                "claude.tar.gz",
                Content::Bytes(archive(&[
                    ("./projects/-workspace/s.jsonl", "talk\n"),
                    ("./settings.json", "{}"),
                ])),
            ),
        ],
    )
    .unwrap();
    broker_snapshot(&dir).unwrap();
    let slot = store::current_slot(&dir).unwrap();
    assert!(!slot.join("workspace.tar.gz").exists());
    // As the tar crate reads them: without ./.
    assert_eq!(
        paths_in(&slot.join("notes.tar.gz")),
        [".git/HEAD", "mine.md"]
    );
    assert_eq!(
        paths_in(&slot.join("claude.tar.gz")),
        ["projects/-notes/s.jsonl", "settings.json"]
    );
    assert!(slot.join("thread.json").exists());
    // Once is enough: a snapshot in this format is left as it is.
    broker_snapshot(&dir).unwrap();
    assert_eq!(store::current_slot(&dir).unwrap(), slot);
}

#[test]
fn a_home_is_converted_once_and_says_so() {
    let env = tempdir::isolated();
    let home = env.home().to_path_buf();
    let thread = home.join("repo-0123abcd/t");
    store::write_slot(
        &thread,
        vec![(
            "thread.json",
            Content::Bytes(
                br#"{"name": "t", "bottle": "repo-t", "created": 1, "runner": "desktop"}"#.to_vec(),
            ),
        )],
    )
    .unwrap();
    ensure().unwrap();
    assert_eq!(
        fs::read_to_string(home.join("format")).unwrap(),
        format!("{FORMAT}\n")
    );
    let record = store::read_json(&store::current_slot(&thread).unwrap().join("thread.json"))
        .unwrap()
        .unwrap();
    assert_eq!(record["settings"]["runner"], "desktop");
    assert!(record.get("runner").is_none());
    // Stamped, it isn't looked at again.
    let slot = store::current_slot(&thread).unwrap();
    ensure().unwrap();
    assert_eq!(store::current_slot(&thread).unwrap(), slot);
}

#[test]
fn bottle_clis_runners_become_folios() {
    let env = tempdir::isolated();
    let tmp = tempdir::Dir::new();
    tempdir::set(&env, "BOTTLE_HOME", tmp.path().to_str().unwrap());
    fs::write(
        tmp.path().join("runners.json"),
        r#"{"runners": {"box": {"ssh": "box"}}}"#,
    )
    .unwrap();
    runners(env.home()).unwrap();
    let registry = store::read_json(&env.home().join("runners.json"))
        .unwrap()
        .unwrap();
    assert_eq!(
        registry,
        json!({"version": 1, "runners": {"box": {"ssh": "box"}}})
    );
}
