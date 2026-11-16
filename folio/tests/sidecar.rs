//! A thread's sidecar: a bottle of its own on a runner of its own, made and deleted with the
//! thread's, which that one ssh-es to. On the stand-in bottle-cli, here and on runner desktop.

mod harness;

use harness::{Folio, Runner, argv};
use serde_json::{Value, json};

/// A thread fix-it, and runner desktop, which has room for a big sidecar.
struct Sidecar {
    f: Folio,
    desktop: Runner,
    bottle: String,
    sidecar: String,
}

impl Sidecar {
    fn new() -> Sidecar {
        let mut f = Folio::new();
        // What the runner says it has: room for 32G and 64G sidecars.
        f.set("FAKE_CLI_MEMORY", "131072");
        let desktop = f.add_runner("desktop");
        f.ok(&["new", "fix-it"]);
        let bottle = f.bottle("fix-it");
        let sidecar = format!("{bottle}-sidecar");
        Sidecar {
            f,
            desktop,
            bottle,
            sidecar,
        }
    }

    fn give_it_one(&self) {
        self.f.ok(&[
            "sidecar",
            "configure",
            "fix-it",
            "--runner",
            "desktop",
            "--feature",
            "jvm:version=21",
            "--memory",
            "32G",
        ]);
    }

    /// Given one, paused and resumed: its sidecar made.
    fn with_one() -> Sidecar {
        let s = Sidecar::new();
        s.give_it_one();
        s.f.ok(&["pause", "fix-it"]);
        s.f.ok(&["claude", "fix-it"]);
        s
    }

    fn thread_sidecar(&self) -> Value {
        self.f.thread("fix-it")["sidecar"].clone()
    }

    fn sidecar_record(&self) -> Value {
        self.f.bottle_record("fix-it", &self.sidecar)
    }

    /// The machines there are, here and on desktop.
    fn machines(&self) -> Vec<String> {
        let mut all: Vec<String> = self.f.cli()["machines"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        all.extend(
            self.desktop.cli()["machines"]
                .as_object()
                .unwrap()
                .keys()
                .cloned(),
        );
        all.sort();
        all
    }

    /// The thread's bottle's ssh grants, by name: the bottle each is.
    fn grants(&self) -> Value {
        let record = self.f.record("fix-it");
        record["ssh"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| (g["name"].as_str().unwrap().to_string(), g["bottle"].clone()))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }

    fn state(&self) -> String {
        self.f.rows()["fix-it"][0].clone()
    }
}

#[test]
fn given_one_it_says_it_comes_with_the_next_bottle() {
    let s = Sidecar::new();
    let out =
        s.f.folio(&[
            "sidecar",
            "configure",
            "fix-it",
            "--runner",
            "desktop",
            "--memory",
            "32G",
        ])
        .stdout;
    assert!(
        out.contains("Set thread fix-it's sidecar: features [], memory 32G, runner desktop"),
        "{out}"
    );
    assert!(
        out.contains("`folio pause fix-it`, then `folio claude fix-it`"),
        "{out}"
    );
    assert!(s.desktop.machine_named(&s.sidecar).is_null());
    assert_eq!(
        s.thread_sidecar(),
        json!({"runner": "desktop", "memory": "32G"})
    );
}

#[test]
fn a_resume_makes_it_first_and_the_threads_bottle_ssh_es_to_it() {
    let s = Sidecar::with_one();
    let sidecar = s.sidecar_record();
    assert_eq!(sidecar["runner"], "desktop");
    assert_eq!(sidecar["spec"]["features"], json!(["jvm:version=21"]));
    assert_eq!(sidecar["spec"]["memory"], "32G");
    assert_eq!(s.desktop.machine_named(&s.sidecar)["state"], "running");
    // Made first: the thread's bottle is let in to it, which needs it there.
    assert_eq!(s.grants(), json!({"sidecar": s.sidecar}));
    // The thread's repos and remotes, pushes included: its lane is the thread's.
    let bottle = s.f.record("fix-it");
    let paths: Vec<&Value> = sidecar["mounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| &m["path"])
        .collect();
    assert_eq!(paths, [&json!("/workspace"), &json!("/notes")]);
    let remotes = |r: &Value| -> Vec<Value> {
        r["mounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["remotes"].clone())
            .collect()
    };
    assert_eq!(remotes(&sidecar), remotes(&bottle));
    assert!(
        sidecar["mounts"][0]["remotes"]
            .as_array()
            .unwrap()
            .contains(&json!({"name": "agent", "fetch": "ns/fix-it-out", "push": "ns/fix-it-out"}))
    );
    let context = bottle["context"].as_str().unwrap();
    assert!(context.contains("a push from there lands in this thread's lane"));
    assert!(context.contains("`ssh sidecar` reaches this thread's sidecar"));
    assert!(
        context.contains("32G of memory and features `jvm:version=21`"),
        "{context}"
    );
    assert!(
        sidecar["context"]
            .as_str()
            .unwrap()
            .contains("This bottle is the sidecar of thread `fix-it`")
    );
}

#[test]
fn a_pause_deletes_it_and_a_delete_too() {
    let s = Sidecar::with_one();
    s.f.ok(&["pause", "fix-it"]);
    assert!(s.desktop.machine_named(&s.sidecar).is_null());
    s.f.ok(&["claude", "fix-it"]);
    s.f.ok(&["delete", "fix-it", "--yes"]);
    assert!(s.machines().is_empty(), "{:?}", s.machines());
}

#[test]
fn it_starts_with_the_thread() {
    let s = Sidecar::with_one();
    s.desktop
        .set_machine_named(&s.sidecar, json!({"state": "stopped"}));
    s.f.set_machine("fix-it", json!({"state": "stopped"}));
    s.f.ok(&["shell", "fix-it"]);
    assert_eq!(s.desktop.machine_named(&s.sidecar)["state"], "running");
}

#[test]
fn configured_as_the_repos_spec_is_shown_and_removed() {
    let mut s = Sidecar::new();
    s.f.add_runner("big");
    assert_eq!(
        s.f.ok(&["sidecar", "configure", "fix-it"]).trim(),
        "Thread fix-it has no sidecar"
    );
    s.give_it_one();
    s.f.ok(&[
        "sidecar",
        "configure",
        "fix-it",
        "--feature",
        "tools",
        "--memory",
        "64G",
        "--disk",
        "100G",
    ]);
    assert_eq!(
        s.f.ok(&["sidecar", "configure", "fix-it"]).trim(),
        format!(
            "features [jvm:version=21, tools], memory 64G, disk 100G, runner desktop; bottle {}, missing",
            s.sidecar
        )
    );
    assert_eq!(s.f.rows()["fix-it"][2], "desktop");
    s.f.ok(&[
        "sidecar",
        "configure",
        "fix-it",
        "--no-feature",
        "jvm",
        "--no-disk",
        "--runner",
        "big",
    ]);
    assert_eq!(
        s.thread_sidecar(),
        json!({"runner": "big", "features": ["tools"], "memory": "64G"})
    );
    let out =
        s.f.folio(&["sidecar", "configure", "fix-it", "--no-runner"]);
    assert!(
        out.stderr
            .contains("`folio sidecar remove fix-it` takes it away"),
        "{}",
        out.stderr
    );
    s.f.ok(&["sidecar", "remove", "fix-it"]);
    assert!(s.thread_sidecar().is_null());
    assert_eq!(
        s.f.ok(&["sidecar", "configure", "fix-it"]).trim(),
        "Thread fix-it has no sidecar"
    );
}

#[test]
fn a_new_one_needs_a_runner_and_a_spec_folio_takes() {
    let s = Sidecar::new();
    let out =
        s.f.folio(&["sidecar", "configure", "fix-it", "--memory", "8G"]);
    assert!(
        out.stderr
            .contains("give it one with `folio sidecar configure fix-it --runner RUNNER ...`"),
        "{}",
        out.stderr
    );
    let out = s.f.folio(&[
        "sidecar",
        "configure",
        "fix-it",
        "--runner",
        "desktop",
        "--feature",
        "nope",
    ]);
    assert!(
        out.stderr.contains("no feature named \"nope\""),
        "{}",
        out.stderr
    );
    assert!(s.thread_sidecar().is_null());
}

#[test]
fn a_restore_that_fails_leaves_it_paused_with_neither_bottle() {
    let s = Sidecar::new();
    s.give_it_one();
    s.f.ok(&["pause", "fix-it"]);
    let out = s.f.with(
        &["claude", "fix-it"],
        &[(
            "FAKE_CLI_FAIL_EXEC",
            "find . -mindepth 1 -maxdepth 1 -exec rm -rf",
        )],
    );
    assert_eq!(out.code, 1, "{}", out.stderr);
    assert!(s.machines().is_empty(), "{:?}", s.machines());
}

#[test]
fn a_resume_that_fails_leaves_it_paused_with_neither_bottle() {
    let s = Sidecar::new();
    s.give_it_one();
    s.f.ok(&["pause", "fix-it"]);
    // The sidecar made, then the thread's bottle not.
    let out = s.f.with(
        &["claude", "fix-it"],
        &[("FAKE_CLI_FAIL", &format!("create:{}", s.bottle))],
    );
    assert_ne!(out.code, 0);
    assert_eq!(s.state(), "paused");
    assert!(s.machines().is_empty(), "{:?}", s.machines());
    s.f.ok(&["claude", "fix-it"]);
    assert_eq!(s.machines(), [s.bottle.clone(), s.sidecar.clone()]);
}

#[test]
fn a_resume_goes_on_without_a_sidecar_it_cant_make() {
    let s = Sidecar::new();
    s.give_it_one();
    s.f.ok(&["pause", "fix-it"]);
    let out = s.f.with(
        &["claude", "fix-it"],
        &[("FAKE_CLI_FAIL", &format!("create:{}", s.sidecar))],
    );
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(
        out.stderr
            .contains("thread fix-it goes on without its sidecar ("),
        "{}",
        out.stderr
    );
    assert_eq!(s.grants(), json!({}));
    assert!(
        !s.f.record("fix-it")["context"]
            .as_str()
            .unwrap()
            .contains("ssh sidecar")
    );
    assert!(
        s.f.folio(&["sidecar", "shell", "fix-it"])
            .stderr
            .contains("comes with its next bottle")
    );
}

#[test]
fn one_left_when_it_was_taken_away_is_deleted_with_the_next_bottle() {
    let s = Sidecar::with_one();
    s.desktop.power(false);
    // Left on its runner.
    let out = s.f.folio(&["pause", "fix-it"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    s.f.ok(&["sidecar", "remove", "fix-it"]);
    let out = s.f.folio(&["claude", "fix-it"]);
    assert!(
        out.stderr
            .contains("old sidecar is left on a runner that's off"),
        "{}",
        out.stderr
    );
    assert_eq!(s.machines(), std::slice::from_ref(&s.sidecar));
    s.desktop.power(true);
    s.f.ok(&["claude", "fix-it"]);
    assert_eq!(s.machines(), std::slice::from_ref(&s.bottle));
    assert_eq!(s.grants(), json!({}));
}

#[test]
fn a_failed_resume_says_why_though_its_sidecar_isnt_deleted() {
    let s = Sidecar::new();
    s.give_it_one();
    s.f.ok(&["pause", "fix-it"]);
    let fail = format!("create:{},delete:{}", s.bottle, s.sidecar);
    let out = s.f.with(&["claude", "fix-it"], &[("FAKE_CLI_FAIL", &fail)]);
    assert_ne!(out.code, 0);
    let said: Vec<&str> = out.stderr.lines().collect();
    let cleanup = said[said.len() - 2];
    assert!(
        cleanup.starts_with("folio: ") && cleanup.contains(&s.sidecar),
        "{}",
        out.stderr
    );
    assert!(
        said[said.len() - 1].contains("create failed"),
        "{}",
        out.stderr
    );
}

#[test]
fn its_runner_turned_off_holds_nothing_up() {
    let s = Sidecar::with_one();
    s.desktop.power(false);
    let line = s.f.ok(&["ls"]).lines().nth(1).unwrap().to_string();
    assert!(line.contains("desktop (unreachable)"), "{line}");
    let out = s.f.folio(&["claude", "fix-it"]);
    assert!(out.stderr.contains("didn't start"), "{}", out.stderr);
    // A pause leaves it there, and a resume goes on without it.
    let out = s.f.folio(&["pause", "fix-it"]);
    assert!(
        out.stderr.contains(&format!(
            "{}, is left on runner desktop, which is off",
            s.sidecar
        )),
        "{}",
        out.stderr
    );
    assert!(!s.desktop.machine_named(&s.sidecar).is_null());
    let out = s.f.folio(&["claude", "fix-it"]);
    assert!(
        out.stderr.contains(
            "goes on without its sidecar (its old one is on runner desktop, which is off)"
        ),
        "{}",
        out.stderr
    );
    assert_eq!(s.grants(), json!({}));
    assert!(
        s.f.folio(&["claude", "fix-it"])
            .stderr
            .contains("comes with its next bottle")
    );
    // Back on: the next bottle has it, the old one gone first.
    s.desktop.power(true);
    s.f.ok(&["pause", "fix-it"]);
    s.f.ok(&["claude", "fix-it"]);
    assert_eq!(s.grants(), json!({"sidecar": s.sidecar}));
    let deletes = s.desktop.calls("delete");
    assert_eq!(
        deletes.iter().filter(|c| c[1] == s.sidecar).count(),
        1,
        "{deletes:?}"
    );
    assert!(
        s.f.ok(&["ls"])
            .lines()
            .nth(1)
            .unwrap()
            .contains("desktop (running)")
    );
}

#[test]
fn shell_only_while_the_thread_runs() {
    let s = Sidecar::new();
    assert!(
        s.f.folio(&["sidecar", "shell", "fix-it"])
            .stderr
            .contains("has no sidecar")
    );
    s.give_it_one();
    assert!(
        s.f.folio(&["sidecar", "shell", "fix-it"])
            .stderr
            .contains("comes with its next bottle")
    );
    s.f.ok(&["pause", "fix-it"]);
    assert!(
        s.f.folio(&["sidecar", "shell", "fix-it"])
            .stderr
            .contains("thread fix-it is paused")
    );
    // Nothing made or started for it.
    assert!(s.desktop.machine_named(&s.sidecar).is_null());
    s.f.ok(&["claude", "fix-it"]);
    s.f.set_machine("fix-it", json!({"state": "stopped"}));
    assert!(
        s.f.folio(&["sidecar", "shell", "fix-it"])
            .stderr
            .contains("thread fix-it is stopped")
    );
    s.f.ok(&["shell", "fix-it"]);
    s.f.ok(&["sidecar", "shell", "fix-it"]);
    assert_eq!(
        s.desktop.ran(&s.sidecar).last().unwrap(),
        &argv(&["bash", "-l"])
    );
}
