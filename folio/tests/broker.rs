//! The broker: a thread of folio's own, which mounts only its notes, and has a broker's
//! permissions; and its notes, opened in Obsidian (stand-ins for `open`, `pgrep` and `osascript`).

mod harness;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use harness::{Folio, argv, git_in, read_json};
use serde_json::{Value, json};

const GITIGNORE: &str = ".obsidian/\n.trash/\n";

fn vault(f: &Folio) -> PathBuf {
    f.broker_dir().join("notes")
}

fn record(f: &Folio) -> Value {
    read_json(&f.broker_dir().join("bottles/broker.json")).unwrap()
}

fn creates(f: &Folio) -> usize {
    f.calls("create")
        .iter()
        .filter(|c| c[1] == "broker")
        .count()
}

#[test]
fn the_broker_is_a_thread_that_mounts_only_its_notes() {
    let f = Folio::new();
    f.ok(&["broker"]);
    let vault = vault(&f);
    assert_eq!(git_in(&vault, &["symbolic-ref", "--short", "HEAD"]), "main");
    assert_eq!(
        fs::read_to_string(vault.join(".gitignore")).unwrap(),
        GITIGNORE
    );
    assert_eq!(git_in(&vault, &["status", "--porcelain"]), "");
    let mounts: Vec<(Value, Value)> = record(&f)["mounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["path"].clone(), m["repo"].clone()))
        .collect();
    assert_eq!(mounts, [(json!("/notes"), json!(vault))]);
    assert!(!f.home.join("repos/broker").exists());
    let ran = f.ran_in("broker");
    let last = ran.last().unwrap();
    assert_eq!(
        last[last.len() - 4..last.len() - 2],
        argv(&["attach", "claude"])
    );
}

#[test]
fn its_spec_is_folios_with_a_brokers_permissions() {
    let f = Folio::new();
    f.ok(&["broker"]);
    let spec = record(&f)["spec"].clone();
    assert_eq!(
        (&spec["features"], &spec["memory"], &spec["permissions"]),
        (
            &json!(["claude", "tools"]),
            &json!("2G"),
            &json!(["list-bottles", "message-any"])
        )
    );
}

#[test]
fn the_vaults_main_tracks_its_lane_as_any_threads_notes() {
    let f = Folio::new();
    f.ok(&["broker"]);
    assert_eq!(
        record(&f)["mounts"][0]["remotes"],
        json!([{"name": "host", "fetch": "heads", "push": null},
               {"name": "agent", "fetch": "ns/notes-out", "push": "ns/notes-out"},
               {"name": "user", "fetch": "ns/notes-in", "push": null}])
    );
    // So bottle's follow moves it as the broker pushes main.
    assert_eq!(
        git_in(
            &vault(&f),
            &["for-each-ref", "--format=%(upstream)", "refs/heads/main"]
        ),
        "refs/remotes/bottle/main"
    );
}

#[test]
fn its_context_is_the_brokers() {
    let f = Folio::new();
    f.ok(&["broker"]);
    let record = record(&f);
    let context = record["context"].as_str().unwrap();
    for told in [
        "You are the broker",
        "git push agent main",
        "**`For you.md`**, at the root, is the user's",
        "**Your most important job is the user's priorities.**",
        "this overrides any default of committing",
        "## Snapshots and pauses",
    ] {
        assert!(context.contains(told), "{told}");
    }
    // A thread's branches aren't its memory.
    assert!(!context.contains("genie/broker-1"));
}

#[test]
fn shell_and_claude_are_its_sessions() {
    let f = Folio::new();
    f.ok(&["broker", "shell"]);
    let ran = f.ran_in("broker");
    let last = ran.last().unwrap();
    assert_eq!(
        last[last.len() - 4..last.len() - 2],
        argv(&["attach", "shell"])
    );
    f.ok(&["broker", "claude"]);
    // The same broker.
    assert_eq!(creates(&f), 1);
}

#[test]
fn pause_and_resume_keep_it_the_broker() {
    let f = Folio::new();
    f.ok(&["broker"]);
    f.ok(&["broker", "pause"]);
    assert!(f.cli()["machines"].get("broker").is_none());
    f.ok(&["broker"]);
    assert_eq!(creates(&f), 2);
    assert!(
        record(&f)["spec"]["permissions"]
            .as_array()
            .unwrap()
            .contains(&json!("message-any"))
    );
    assert_eq!(
        git_in(&vault(&f), &["config", "branch.main.remote"]),
        "bottle"
    );
}

#[test]
fn pausing_no_broker() {
    let f = Folio::new();
    let out = f.folio(&["broker", "pause"]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("no broker to pause"), "{}", out.stderr);
}

#[test]
fn a_repos_threads_never_get_its_permissions() {
    let f = Folio::new();
    f.ok(&["broker"]);
    f.ok(&["new", "fix-it"]);
    assert!(f.record("fix-it")["spec"].get("permissions").is_none());
}

#[test]
fn notes_say_where_the_vault_is_without_obsidian() {
    let f = Folio::new();
    let out = f.folio(&["broker", "notes"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stdout.contains(&vault(&f).display().to_string()),
        "{}",
        out.stdout
    );
    // Nothing started.
    assert!(f.cli()["machines"].get("broker").is_none());
}

/// Stand-ins for `open` (Obsidian is installed), `pgrep` (it's running, or not) and `osascript`
/// (quitting it works); each logs its arguments. Obsidian's list of vaults is `vaults`: None for
/// none, else as written. Its log, and where the list is.
fn obsidian(f: &mut Folio, vaults: Option<&str>, running: bool) -> (PathBuf, PathBuf) {
    let log = f.tmp.join("commands");
    let flag = f.tmp.join("obsidian-running");
    if running {
        fs::write(&flag, "").unwrap();
    }
    let flag = flag.display().to_string();
    for (name, body) in [
        ("open", String::new()),
        ("pgrep", format!("[ -e {flag} ]")),
        ("osascript", format!("rm -f {flag}")),
    ] {
        let script = f.tmp.join(name);
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '{name} %s\\n' \"$*\" >> {}\n{body}\n",
                log.display()
            ),
        )
        .unwrap();
        Command::new("chmod")
            .arg("+x")
            .arg(&script)
            .status()
            .unwrap();
        f.set(
            &format!("FOLIO_{}", name.to_uppercase()),
            script.display().to_string(),
        );
    }
    let config = f.tmp.join("obsidian/obsidian.json");
    if let Some(vaults) = vaults {
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, vaults).unwrap();
    }
    f.set("FOLIO_OBSIDIAN_CONFIG", config.display().to_string());
    (log, config)
}

fn opened(log: &PathBuf) -> Vec<String> {
    fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("open obsidian:"))
        .map(|l| l.trim_start_matches("open ").to_string())
        .collect()
}

#[test]
fn the_vault_has_obsidians_folder_and_git_ignores_it() {
    let f = Folio::new();
    f.ok(&["broker", "notes"]);
    assert!(vault(&f).join(".obsidian").is_dir());
    assert_eq!(
        git_in(&vault(&f), &["status", "--porcelain", "--ignored=no"]),
        ""
    );
}

#[test]
fn notes_open_a_vault_obsidian_knows_by_its_id() {
    let mut f = Folio::new();
    // Makes the vault.
    f.ok(&["broker", "notes"]);
    let vaults = json!({"vaults": {
        // A vault around it isn't it.
        "0f0f0f0f0f0f0f0f": {"path": f.tmp, "ts": 1},
        "a1b2c3d4e5f6a7b8": {"path": vault(&f), "ts": 1},
    }, "theme": "x"});
    let (log, config) = obsidian(&mut f, Some(&vaults.to_string()), false);
    let before = fs::read_to_string(&config).unwrap();
    let out = f.ok(&["broker", "notes"]);
    assert_eq!(opened(&log), ["obsidian://open?vault=a1b2c3d4e5f6a7b8"]);
    assert_eq!(fs::read_to_string(&config).unwrap(), before);
    assert!(out.contains("Opened"), "{out}");
}

#[test]
fn notes_add_a_vault_obsidian_doesnt_know_and_open_it() {
    for vaults in [
        None,
        Some(json!({})),
        Some(json!({"0f0f0f0f0f0f0f0f": {"path": "/elsewhere", "ts": 1}})),
    ] {
        let mut f = Folio::new();
        let written = vaults
            .as_ref()
            .map(|v| json!({"vaults": v, "theme": "x"}).to_string());
        let (log, config) = obsidian(&mut f, written.as_deref(), false);
        let out = f.ok(&["broker", "notes"]);
        let listed = read_json(&config).unwrap();
        let added: Vec<&String> = listed["vaults"]
            .as_object()
            .unwrap()
            .iter()
            .filter(|(_, v)| v["path"] == json!(vault(&f)))
            .map(|(id, _)| id)
            .collect();
        assert_eq!(added.len(), 1, "{listed}");
        let added = added[0];
        assert!(
            added.len() == 16
                && added
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{added}"
        );
        assert!(listed["vaults"][added]["ts"].is_u64());
        for (id, other) in vaults
            .as_ref()
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            assert_eq!(&listed["vaults"][id], other);
        }
        if vaults.is_some() {
            // The rest of it is kept.
            assert_eq!(listed["theme"], "x");
        }
        assert_eq!(opened(&log), [format!("obsidian://open?vault={added}")]);
        assert!(out.contains("Opened"), "{out}");
    }
}

#[test]
fn a_running_obsidian_isnt_quit_without_asking() {
    let mut f = Folio::new();
    let (log, config) = obsidian(
        &mut f,
        Some(&json!({"vaults": {}, "theme": "x"}).to_string()),
        true,
    );
    // No terminal to ask on.
    let out = f.ok(&["broker", "notes"]);
    assert!(out.contains("has to restart"), "{out}");
    assert_eq!(
        read_json(&config).unwrap(),
        json!({"vaults": {}, "theme": "x"})
    );
    assert!(
        !fs::read_to_string(&log)
            .unwrap_or_default()
            .contains("osascript")
    );
    assert!(opened(&log).is_empty());
}

#[test]
fn a_running_obsidian_is_quit_first_when_the_user_says_so() {
    let mut f = Folio::new();
    // Makes the vault.
    f.ok(&["broker", "notes"]);
    let (log, config) = obsidian(
        &mut f,
        Some(&json!({"vaults": {}, "theme": "x"}).to_string()),
        true,
    );
    let (code, said) = f.on_terminal(
        &["broker", "notes"],
        &[(b"Quit and reopen it? [y/N] ", b"y\r")],
    );
    assert_eq!(code, 0, "{said}");
    let commands: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    let quit_at = commands
        .iter()
        .position(|c| c.starts_with("osascript"))
        .unwrap();
    let open_at = commands
        .iter()
        .position(|c| c.starts_with("open obsidian:"))
        .unwrap();
    assert!(quit_at < open_at);
    assert_eq!(
        read_json(&config).unwrap()["vaults"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    assert!(said.contains("Opened"), "{said}");
}

#[test]
fn an_obsidian_json_folio_doesnt_understand_is_left_alone() {
    for written in [
        "not json".to_string(),
        json!({"vaults": []}).to_string(),
        json!(["x"]).to_string(),
    ] {
        let mut f = Folio::new();
        let (log, config) = obsidian(&mut f, Some(&written), false);
        let out = f.ok(&["broker", "notes"]);
        assert_eq!(fs::read_to_string(&config).unwrap(), written);
        assert_eq!(opened(&log), ["obsidian://choose-vault"]);
        assert!(out.contains("Open folder as vault"), "{out}");
    }
}
