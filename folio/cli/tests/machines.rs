//! folio's own bottles, end to end through `folio`: the stand-in bottle-cli, and folio serve for
//! real. What a thread's bottle is made of, in what order, what's rolled back when a step fails,
//! how it's started again, listed and deleted, the images it's built from, its credentials, and a
//! thread's bottle on a runner.

mod harness;

use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use harness::{Folio, current_slot, git_in, read_json, write_json};
use serde_json::{Value, json};

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_string())
        .collect()
}

fn workspace_script(f: &Folio, thread: &str) -> Value {
    let scripts: Vec<Value> = f
        .execs(thread)
        .into_iter()
        .filter(|e| e["step"] == "workspace")
        .collect();
    assert_eq!(scripts.len(), 1, "{scripts:?}");
    scripts[0].clone()
}

// --- a bottle made ---------------------------------------------------------------

#[test]
fn a_thread_gets_a_bottle_made_whole() {
    let f = Folio::new();
    f.ok(&["new", "t"]);
    let head = git_in(&f.repo, &["rev-parse", "HEAD"]);
    let record = f.record("t");
    assert_eq!(record["name"], f.bottle("t"));
    assert_eq!(record["status"], "ready");
    assert_eq!(record["features"], json!(["tools"]));
    assert_eq!(record["runner"], "local");
    let mounts = record["mounts"].as_array().unwrap();
    assert_eq!(
        (
            &mounts[0]["path"],
            &mounts[0]["branch"],
            mounts[0]["commit"].as_str().unwrap()
        ),
        (&json!("/workspace"), &json!("main"), head.as_str())
    );
    let paths: Vec<&Value> = mounts.iter().map(|m| &m["path"]).collect();
    assert_eq!(paths, [&json!("/workspace"), &json!("/notes")]);
    assert_eq!(f.machine("t")["state"], "running");
    // Started, to answer it.
    assert!(f.ok(&["serve", "stop"]).contains("Stopped folio serve"));
}

#[test]
fn its_steps_in_order_the_record_first() {
    let f = Folio::new();
    f.ok(&["new", "t"]);
    let machine = f.bottle("t");
    let order: Vec<String> = f.cli()["calls"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| (c[0] == "create" || c[0] == "start") && c[1] == machine.as_str())
        .map(|c| c[0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(order, ["create", "start"]);
    assert_eq!(f.steps("t"), ["contract", "hosts", "mkdir", "workspace"]);
    let users: Vec<Value> = f
        .execs("t")
        .iter()
        .filter(|e| e["step"] == "hosts" || e["step"] == "mkdir")
        .map(|e| e["user"].clone())
        .collect();
    assert_eq!(users, [json!("root"), json!("root")]);
}

#[test]
fn its_machine_is_made_with_its_image_memory_objects_and_policy() {
    let f = Folio::new();
    f.ok(&["configure", "--memory", "3G", "--disk", "50G"]);
    f.ok(&["new", "t"]);
    let machine = f.machine("t");
    let cpus = std::thread::available_parallelism()
        .unwrap()
        .get()
        .to_string();
    assert_eq!(
        (
            &machine["image"],
            &machine["mem"],
            &machine["storage"],
            &machine["cpus"]
        ),
        (
            &json!("localhost/bottle/base:with-tools"),
            &json!("3072"),
            &json!("50"),
            &json!(cpus)
        )
    );
    assert_eq!(
        (&machine["user"], &machine["workdir"], &machine["labels"]),
        (
            &json!("genie"),
            &json!("/workspace"),
            &json!({"thread": "t"})
        )
    );
    assert_eq!(
        machine["env"],
        json!({"BOTTLE_WORKDIR": "/workspace", "BOTTLE_MIRROR_ENV": "BOTTLE_WORKDIR"})
    );
    assert!(strs(&machine["mounts"]).contains(&format!(
        "{}/.git/objects:/mnt/objects/workspace:ro",
        f.repo.display()
    )));
    let routes = machine["network"]["routes"].as_array().unwrap();
    assert_eq!(routes[0]["address"], "192.0.2.1");
    assert!(
        routes[0]["socket"]
            .as_str()
            .unwrap()
            .ends_with("services/host.sock"),
        "{routes:?}"
    );
    let cached: Vec<&Value> = machine["network"]["cache"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["host"])
        .collect();
    assert!(cached.contains(&&json!("deb.debian.org")));
    assert_eq!(f.record("t")["network_version"], 3);
}

#[test]
fn its_workspace_borrows_the_repos_objects_and_serves_its_lane() {
    let f = Folio::new();
    f.ok(&["new", "t"]);
    let script = workspace_script(&f, "t");
    assert_eq!(script["user"], "genie");
    let text = script["argv"][2].as_str().unwrap();
    for line in [
        "echo /mnt/objects/workspace > /workspace/.git/objects/info/alternates",
        "git -C /workspace remote add origin http://bottle.host/git/workspace/origin",
        "git -C /workspace remote add agent http://bottle.host/git/workspace/agent",
        "git -C /workspace fetch -q --multiple origin host agent user",
        "> \"$HOME/BOTTLE.md\"",
        "running in bottle `",
    ] {
        assert!(text.contains(line), "{line} in {text}");
    }
    let hosts: Vec<Value> = f
        .execs("t")
        .into_iter()
        .filter(|e| e["step"] == "hosts")
        .collect();
    assert!(
        hosts[0]["argv"][2]
            .as_str()
            .unwrap()
            .contains("192.0.2.1 bottle.host")
    );
}

#[test]
fn a_branch_with_odd_characters_is_quoted() {
    let f = Folio::new();
    git_in(&f.repo, &["branch", "we$rd;branch"]);
    f.ok(&["new", "t", "--from", "we$rd;branch"]);
    let script = workspace_script(&f, "t");
    assert!(
        script["argv"][2]
            .as_str()
            .unwrap()
            .contains("git init -q -b 'we$rd;branch' /workspace")
    );
}

#[test]
fn a_branch_it_doesnt_have_is_refused_before_anything_is_made() {
    let f = Folio::new();
    let out = f.folio(&["new", "t", "--from", "nope"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("has no branch or commit 'nope'"),
        "{}",
        out.stderr
    );
    assert!(f.calls("create").is_empty());
}

#[test]
fn images_are_built_once_then_kept() {
    let f = Folio::new();
    f.ok(&["new", "t"]);
    let builds = f.cli()["builds"].clone();
    let tags: Vec<&Value> = builds
        .as_array()
        .unwrap()
        .iter()
        .map(|b| &b["tag"])
        .collect();
    assert_eq!(
        tags,
        [
            &json!("localhost/bottle/base:latest"),
            &json!("localhost/bottle/base:with-tools")
        ]
    );
    assert_eq!(builds[0]["recipe"]["pull"], "debian:trixie-slim");
    assert!(builds[0]["recipe"]["labels"].get("bottle.inputs").is_some());
    f.ok(&["new", "u"]);
    assert_eq!(f.cli()["builds"].as_array().unwrap().len(), 2);
}

#[test]
fn an_image_gone_stale_is_removed_when_a_bottle_goes() {
    let f = Folio::new();
    f.ok(&["new", "t"]);
    let mut state = f.cli();
    state["images"].as_array_mut().unwrap().push(json!({
        "tag": "localhost/bottle/base:with-gone", "stage": "s", "disks": ["s.ext4"],
        "labels": {"bottle.inputs": "old", "bottle.features": "gone"}}));
    f.set_cli(&state);
    f.ok(&["delete", "t", "--yes"]);
    let tags: Vec<Value> = f.cli()["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["tag"].clone())
        .collect();
    assert!(!tags.contains(&json!("localhost/bottle/base:with-gone")));
    assert!(tags.contains(&json!("localhost/bottle/base:with-tools")));
}

// --- rolled back -----------------------------------------------------------------

#[test]
fn a_failure_at_any_step_leaves_nothing() {
    let f = Folio::new();
    for step in ["create", "start", "contract", "hosts", "mkdir", "workspace"] {
        let out = f.with(&["new", "t"], &[("FAKE_CLI_FAIL", step)]);
        assert_eq!(out.code, 1, "{step}: {}", out.stderr);
        assert!(
            out.stderr.contains(&format!("{step} failed")),
            "{}",
            out.stderr
        );
        assert_eq!(f.cli()["machines"], json!({}), "{step}");
        // No thread, and no lane; its notes are kept, as a deleted thread's are.
        assert!(!f.thread_dir("t").join("current").exists(), "{step}");
        assert!(!f.thread_dir("t").join("bottles").exists(), "{step}");
        assert_eq!(git_in(&f.repo, &["remote"]), "", "{step}");
    }
}

#[test]
fn a_failed_cleanup_keeps_a_broken_record() {
    let f = Folio::new();
    let out = f.with(&["new", "t"], &[("FAKE_CLI_FAIL", "workspace,delete")]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("workspace failed; cleanup also failed:"),
        "{}",
        out.stderr
    );
}

#[test]
fn one_that_doesnt_meet_the_contract_is_refused() {
    let f = Folio::new();
    let out = f.with(&["new", "t"], &[("FAKE_CLI_MISSING", "git is installed")]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("base+tools doesn't meet the bottle contract: git is installed"),
        "{}",
        out.stderr
    );
}

// --- interrupted: Ctrl-C is a failure like any other, what was being made undone --------

/// `folio new t`, interrupted as a terminal does (Ctrl-C to folio and what it runs, its process
/// group) while the stand-in is busy at `slow`'s first step, and again once it's at `again_at`. Its
/// exit status and stderr.
fn interrupted(f: &Folio, slow: &str, again_at: Option<&str>) -> (i32, String) {
    let mut command = Command::new(harness::FOLIO);
    command
        .args(["new", "t"])
        .current_dir(&f.repo)
        .env_remove("FOLIO_BOTTLE")
        .envs(f.env.iter().cloned())
        .env("FAKE_CLI_SLOW", slow)
        .env("FAKE_CLI_SLOW_SECONDS", "3")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut folio = command.spawn().unwrap();
    let mut stderr = folio.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stderr.read_to_string(&mut text).unwrap();
        text
    });
    for step in [slow.split(',').next(), again_at].into_iter().flatten() {
        let deadline = Instant::now() + Duration::from_secs(30);
        while f.calls(step).is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(300)); // the stand-in, asleep
        // SAFETY: a signal to the process group folio leads.
        unsafe { libc::killpg(folio.id() as i32, libc::SIGINT) };
    }
    let status = folio.wait().unwrap();
    (status.code().unwrap_or(-1), reader.join().unwrap())
}

#[test]
fn an_interrupted_bottle_is_rolled_back() {
    let f = Folio::new();
    let (code, err) = interrupted(&f, "start", None);
    assert_eq!(code, 130, "{err}");
    assert!(
        err.contains("creating failed, rolling back: interrupted"),
        "{err}"
    );
    assert!(err.ends_with("folio: error: interrupted\n"), "{err}");
    assert_eq!(f.cli()["machines"], json!({}));
    assert!(!f.thread_dir("t").join("bottles").exists());
    assert!(current_slot(&f.thread_dir("t")).is_none());
    assert_eq!(git_in(&f.repo, &["remote"]), "");
}

#[test]
fn its_rollback_isnt_interrupted() {
    let f = Folio::new();
    let (code, err) = interrupted(&f, "start,delete", Some("delete"));
    assert_eq!(code, 130, "{err}");
    assert!(!err.contains("cleanup also failed"), "{err}");
    assert_eq!(f.cli()["machines"], json!({}));
    assert!(!f.thread_dir("t").join("bottles").exists());
}

// --- a command's log -------------------------------------------------------------

/// folio.log's records, in its home's logs/, without when and which process.
fn log(f: &Folio) -> Vec<String> {
    fs::read_to_string(f.home.join("logs/folio.log"))
        .unwrap()
        .lines()
        .map(|l| l.splitn(4, ' ').nth(3).unwrap_or_default().to_string())
        .collect()
}

fn seconds(line: &str, before: &str, after: &str) -> bool {
    line.strip_prefix(before)
        .and_then(|rest| rest.split_once('s'))
        .is_some_and(|(n, rest)| {
            n.split_once('.').is_some_and(|(a, b)| {
                !a.is_empty()
                    && a.chars().all(|c| c.is_ascii_digit())
                    && b.len() == 1
                    && b.chars().all(|c| c.is_ascii_digit())
            }) && rest == after
        })
}

#[test]
fn what_a_command_did_and_how_it_ended() {
    let f = Folio::new();
    f.ok(&["new", "t"]);
    f.ok(&["delete", "t", "--yes"]);
    let log = log(&f);
    assert_eq!(log[0], "command folio new t");
    assert!(
        log.contains(&format!("bottles Creating bottle {}...", f.bottle("t"))),
        "{log:?}"
    );
    let created = format!(
        "bottles {}: created on local, {} at main on /workspace",
        f.bottle("t"),
        f.repo.display()
    );
    assert!(log.iter().any(|l| l.starts_with(&created)), "{log:?}");
    let at = log
        .iter()
        .position(|l| l == "command folio delete t --yes")
        .unwrap();
    assert!(
        seconds(&log[at - 1], "command exited 0 in ", ""),
        "{}",
        log[at - 1]
    );
    assert!(log.contains(&format!("bottles {}: deleted", f.bottle("t"))));
    assert!(log.contains(&"threads t: deleted".to_string()));
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(f.home.join("logs/folio.log"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn a_failure_is_recorded_with_what_was_said() {
    let f = Folio::new();
    f.with(&["new", "t"], &[("FAKE_CLI_FAIL", "start")]);
    let log = log(&f);
    assert!(
        log.contains(&format!(
            "bottles {}: creating failed, rolling back: start failed",
            f.bottle("t")
        )),
        "{log:?}"
    );
    assert!(
        seconds(log.last().unwrap(), "command failed in ", ": start failed"),
        "{log:?}"
    );
}

// --- started, listed, deleted ------------------------------------------------------

fn made() -> Folio {
    let f = Folio::new();
    f.ok(&["new", "t"]);
    f
}

#[test]
fn a_stopped_bottle_is_started_again_bound() {
    let f = made();
    f.set_machine("t", json!({"state": "stopped"}));
    f.ok(&["serve", "stop"]);
    let out = f.folio(&["exec", "t", "--", "true"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let machine = f.machine("t");
    assert_eq!(
        (&machine["state"], &machine["started"]),
        (&json!("running"), &json!(2))
    );
    assert!(machine.get("configured").is_none()); // made with its policy
    assert!(f.ok(&["serve", "stop"]).contains("Stopped folio serve"));
}

#[test]
fn one_made_with_an_older_policy_gets_this_one_and_is_checked_again() {
    let f = made();
    let path = f
        .thread_dir("t")
        .join("bottles")
        .join(format!("{}.json", f.bottle("t")));
    let mut record = read_json(&path).unwrap();
    record["network_version"] = json!(0);
    write_json(&path, &record);
    f.set_machine("t", json!({"state": "stopped"}));
    f.ok(&["exec", "t", "--", "true"]);
    assert_eq!(f.machine("t")["configured"], 1);
    assert_eq!(f.record("t")["network_version"], 3);
    let steps = f.steps("t");
    assert_eq!(&steps[steps.len() - 2..], ["contract", "hosts"]);
}

#[test]
fn a_command_runs_as_genie_at_its_first_mount_and_exits_as_it_does() {
    let f = made();
    let out = f.folio(&["exec", "t", "--", "sh", "-c", "echo out; exit 3"]);
    assert_eq!((out.code, out.stdout.as_str()), (3, "out\n"));
    let last = f.execs("t").pop().unwrap();
    assert_eq!(
        (&last["user"], &last["workdir"], &last["argv"]),
        (
            &json!("genie"),
            &json!("/workspace"),
            &json!(["sh", "-c", "echo out; exit 3"])
        )
    );
}

#[test]
fn listed_by_its_machines_state() {
    let f = made();
    f.ok(&["new", "u"]);
    f.set_machine("u", json!({"state": "stopped"}));
    let states = |f: &Folio| -> Vec<(String, String)> {
        f.rows()
            .into_iter()
            .map(|(t, r)| (t, r[0].clone()))
            .collect()
    };
    assert_eq!(
        states(&f),
        [
            ("t".into(), "running".into()),
            ("u".into(), "stopped".into())
        ]
    );
    let mut state = f.cli();
    state["machines"]
        .as_object_mut()
        .unwrap()
        .remove(&f.bottle("u"));
    f.set_cli(&state);
    assert_eq!(f.rows()["u"][0], "missing");
}

#[test]
fn deleting_takes_every_part() {
    let f = made();
    f.ok(&["delete", "t", "--yes"]);
    assert_eq!(f.cli()["machines"], json!({}));
    assert!(!f.thread_dir("t").join("bottles").exists());
}

#[test]
fn a_failed_delete_leaves_it_broken_and_another_finishes_it() {
    let f = made();
    let out = f.with(&["delete", "t", "--yes"], &[("FAKE_CLI_FAIL", "delete")]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains("try again, and what's left is deleted"),
        "{}",
        out.stderr
    );
    assert_eq!(f.record("t")["status"], "broken");
    f.ok(&["delete", "t", "--yes"]);
    assert_eq!(f.cli()["machines"], json!({}));
}

// --- credentials ------------------------------------------------------------------

#[test]
fn a_feature_with_a_credential_gets_its_stand_in_never_the_credential() {
    let f = Folio::new();
    f.ok(&["configure", "--feature", "github"]);
    let out = f.folio(&["new", "t"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    let env = f.machine("t")["env"].as_object().unwrap().clone();
    assert!(env.values().any(|v| v == harness::STANDIN), "{env:?}");
    let mut others: Vec<&str> = env
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "BOTTLE_MIRROR_ENV")
        .collect();
    others.sort();
    assert_eq!(env["BOTTLE_MIRROR_ENV"], others.join(" "));
    assert!(
        out.stderr.contains("isn't set: `folio auth set github`"),
        "{}",
        out.stderr
    );
    let hosts: Vec<Value> = f.machine("t")["network"]["routes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["host"].clone())
        .collect();
    assert!(hosts.contains(&json!("api.github.com")));
}

/// `folio auth list`'s rows, by credential, its header checked.
fn auth_rows(f: &Folio) -> std::collections::BTreeMap<String, String> {
    let out = f.ok(&["auth", "list"]);
    let mut lines = out.lines();
    assert_eq!(
        lines.next().unwrap().split_whitespace().collect::<Vec<_>>(),
        [
            "CREDENTIAL",
            "FEATURE",
            "STATUS",
            "ATTACHED",
            "TO",
            "DESCRIPTION"
        ]
    );
    lines
        .map(|l| {
            (
                l.split_whitespace().next().unwrap().to_string(),
                l.to_string(),
            )
        })
        .collect()
}

/// Whether `row`'s first columns are `words`, as `folio auth list` spaces them.
fn starts_with_words(row: &str, words: &[&str]) -> bool {
    let mut rest = row;
    for word in words {
        let Some(after) = rest.trim_start().strip_prefix(word) else {
            return false;
        };
        if !after.starts_with(' ') {
            return false;
        }
        rest = after;
    }
    true
}

#[test]
fn credentials_are_listed_with_where_each_goes_and_whether_its_set() {
    let f = Folio::new();
    let rows = auth_rows(&f);
    assert_eq!(
        rows.keys().collect::<Vec<_>>(),
        ["claude", "develocity", "github", "teamcity"]
    );
    assert!(
        starts_with_words(
            &rows["claude"],
            &["claude", "claude", "not", "set", "api.anthropic.com"]
        ),
        "{}",
        rows["claude"]
    );
    assert!(rows["develocity"].contains("~/.gradle/develocity/keys.properties, else not set"));
}

#[test]
fn a_credential_is_set_from_stdin_listed_then_removed() {
    let f = Folio::new();
    let out = f.run(&["auth", "set", "claude"], None, "tok-123\n", &[]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(
        out.stdout,
        "Stored claude. Bottles with it get it when they next start.\n"
    );
    assert!(starts_with_words(
        &auth_rows(&f)["claude"],
        &["claude", "claude", "set"]
    ));
    // Never on a command line.
    assert!(
        !fs::read_to_string(f.tmp.join("keychain/argv"))
            .unwrap()
            .contains("tok-123")
    );
    assert_eq!(f.ok(&["auth", "rm", "claude"]), "Removed claude\n");
    assert_eq!(f.ok(&["auth", "remove", "claude"]), "claude wasn't set\n");
}

#[test]
fn only_a_credential_a_feature_declares() {
    let f = Folio::new();
    let out = f.run(&["auth", "set", "nope"], None, "x", &[]);
    assert_eq!(out.code, 1);
    assert_eq!(
        out.stderr,
        "folio: error: no feature declares a credential named 'nope' (known: claude, develocity, github, teamcity)\n"
    );
}

#[test]
fn a_credential_it_cant_store_is_refused() {
    let f = Folio::new();
    let out = f.run(&["auth", "set", "claude"], None, "a\"b", &[]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("the value is empty or has characters folio can't store")
    );
}

#[test]
fn a_bottle_whose_credential_is_set_is_told_nothing_of_it() {
    let f = Folio::new();
    f.ok(&["configure", "--feature", "claude"]);
    f.run(&["auth", "set", "claude"], None, "tok", &[]);
    let out = f.folio(&["new", "t"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!out.stderr.contains("isn't set"));
    assert_eq!(
        f.machine("t")["env"]["CLAUDE_CODE_OAUTH_TOKEN"],
        harness::STANDIN
    );
}

// --- on a runner -------------------------------------------------------------------

#[test]
fn a_thread_on_a_runner_is_made_there() {
    let mut f = Folio::new();
    let box_ = f.add_runner("box");
    let out = f.folio(&["new", "t", "--runner", "box"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert_eq!(f.cli()["machines"], json!({})); // nothing here
    let machine = box_.machine("t");
    assert_eq!(machine["state"], "running");
    let tags: Vec<Value> = box_.cli()["builds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["tag"].clone())
        .collect();
    assert_eq!(
        tags,
        [
            json!("localhost/bottle/base:latest"),
            json!("localhost/bottle/base:with-tools")
        ]
    );
    // The repo's objects, from a mirror there brought up to date from here.
    let key = format!("project-{}", harness::path_hash(&f.repo.join(".git")));
    let mirror = box_.home.join(".folio/git").join(format!("{key}.git"));
    assert_eq!(
        harness::git(&[
            "--git-dir",
            &mirror.display().to_string(),
            "rev-parse",
            "main"
        ]),
        git_in(&f.repo, &["rev-parse", "main"])
    );
    assert!(strs(&machine["mounts"]).contains(&format!(
        "{}/objects:/mnt/objects/workspace:ro",
        mirror.display()
    )));
    // Its routes to folio serve's sockets there, which folio serve forwards here.
    let socket = machine["network"]["routes"][0]["socket"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        socket.starts_with(&format!("{}/.folio/services/", box_.home.display())),
        "{socket}"
    );
    assert_eq!(f.record("t")["runner"], "box");
    assert_eq!(f.rows()["t"][1], "box");
}

#[test]
fn a_runner_with_bottles_isnt_removed() {
    let mut f = Folio::new();
    let box_ = f.add_runner("box");
    f.ok(&["new", "t", "--runner", "box"]);
    let out = f.folio(&["runner", "rm", "box"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr.contains(&format!(
            "runner box has bottles: {}; delete their threads first",
            f.bottle("t")
        )),
        "{}",
        out.stderr
    );
    let listed: Value = serde_json::from_str(&f.ok(&["runner", "ls", "--json"])).unwrap();
    assert_eq!(listed[0]["name"], "box");
    f.ok(&["delete", "t", "--yes"]);
    assert_eq!(box_.cli()["machines"], json!({}));
    f.ok(&["runner", "rm", "box"]);
}

// --- a bottle's setup script, run for real against folio serve -----------------------

/// A thread's bottle's workspace script, run here (its paths its own) against folio serve answering
/// its machine at bottle.host: the workspace, the notes, genie's home, and the environment git
/// reaches bottle.host with.
struct Workspace {
    f: Folio,
    workspace: PathBuf,
    notes: PathBuf,
    genie: PathBuf,
    git_env: Vec<(String, String)>,
}

impl Workspace {
    fn new() -> Workspace {
        let f = Folio::new();
        f.ok(&["new", "t"]);
        let mut script = workspace_script(&f, "t")["argv"][2]
            .as_str()
            .unwrap()
            .to_string();
        let (workspace, notes, genie) = (
            f.tmp.join("workspace"),
            f.tmp.join("notes-there"),
            f.tmp.join("genie"),
        );
        fs::create_dir_all(&genie).unwrap();
        let record = f.record("t");
        let repos = [
            ("/workspace", f.repo.clone()),
            ("/notes", f.thread_dir("t").join("notes")),
        ];
        // Each mount where it's a path here, not where it's in a remote's URL
        // (bottle.host/git/workspace/...).
        let mut objects = Vec::new();
        for (i, mount) in record["mounts"].as_array().unwrap().iter().enumerate() {
            let path = mount["path"].as_str().unwrap();
            let repo = &repos.iter().find(|(p, _)| *p == path).unwrap().1;
            let placeholder = format!("\u{1}{i}\u{1}");
            script = script.replace(mount["objects"].as_str().unwrap(), &placeholder);
            objects.push((placeholder, repo.join(".git/objects").display().to_string()));
        }
        for (path, here) in [("/workspace", &workspace), ("/notes", &notes)] {
            script = script
                .replace(&format!("/git{path}/"), "\0")
                .replace(path, &here.display().to_string())
                .replace('\0', &format!("/git{path}/"));
        }
        for (placeholder, here) in &objects {
            script = script.replace(placeholder, here);
        }
        let port = harness::bridge(f.home.join("services/host.sock"), f.bottle("t"));
        let mut git_env = f.env.clone();
        for (k, v) in [
            ("HOME", genie.display().to_string()),
            (
                "GIT_CONFIG_GLOBAL",
                genie.join(".gitconfig").display().to_string(),
            ),
            ("GIT_CONFIG_COUNT", "1".into()),
            (
                "GIT_CONFIG_KEY_0",
                format!("url.http://127.0.0.1:{port}/.insteadOf"),
            ),
            ("GIT_CONFIG_VALUE_0", "http://bottle.host/".into()),
        ] {
            git_env.retain(|(key, _)| key != k);
            git_env.push((k.into(), v));
        }
        let w = Workspace {
            workspace,
            notes,
            genie,
            git_env,
            f,
        };
        let out = w.run(&["sh", "-c", &script]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        w
    }

    fn run(&self, argv: &[&str]) -> std::process::Output {
        Command::new(argv[0])
            .args(&argv[1..])
            .current_dir(&self.f.tmp)
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH"))
            .envs(self.git_env.iter().cloned())
            .output()
            .unwrap()
    }
}

#[test]
fn its_workspace_checks_out_the_branch_borrowing_the_repos_objects() {
    let w = Workspace::new();
    let head = git_in(&w.f.repo, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_in(&w.workspace, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
    assert_eq!(git_in(&w.workspace, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        git_in(&w.workspace, &["count-objects"]),
        "0 objects, 0 kilobytes"
    );
    assert_eq!(git_in(&w.workspace, &["status", "--porcelain"]), "");
}

#[test]
fn its_workspace_fetched_its_remotes() {
    let w = Workspace::new();
    assert_eq!(
        git_in(&w.workspace, &["remote"])
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["agent", "host", "origin", "user"]
    );
    let remote_branches = git_in(&w.workspace, &["branch", "-r", "--format=%(refname:short)"]);
    assert!(
        remote_branches.split_whitespace().any(|b| b == "host/main"),
        "{remote_branches}"
    );
    assert_eq!(
        git_in(&w.notes, &["remote"])
            .split_whitespace()
            .collect::<Vec<_>>(),
        ["agent", "host", "user"]
    );
}

#[test]
fn its_workspace_script_writes_what_the_agent_is_told() {
    let w = Workspace::new();
    let told = fs::read_to_string(w.genie.join("BOTTLE.md")).unwrap();
    assert!(
        told.contains(&format!(
            "You're `genie`, running in bottle `{}`",
            w.f.bottle("t")
        )),
        "{told}"
    );
    assert!(told.contains("# Thread `t`"));
}

#[test]
fn a_push_to_agent_lands_in_the_threads_lane() {
    let w = Workspace::new();
    w.f.commit(&w.workspace, "agent work");
    let commit = git_in(&w.workspace, &["rev-parse", "HEAD"]);
    let out = w.run(&[
        "git",
        "-C",
        &w.workspace.display().to_string(),
        "push",
        "-q",
        "agent",
        "main:genie/t-1",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        git_in(
            &w.f.repo,
            &["rev-parse", "refs/namespaces/t-out/refs/heads/genie/t-1"]
        ),
        commit
    );
    w.f.git(&[
        "-C",
        &w.f.repo.display().to_string(),
        "fetch",
        "-q",
        "bottle/t",
    ]);
    assert_eq!(
        git_in(&w.f.repo, &["rev-parse", "bottle/t/genie/t-1"]),
        commit
    );
}
