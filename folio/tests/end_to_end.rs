//! A real thread, end to end: made by folio, as you make one, on bottle-cli's machines, with folio
//! serve answering what its machine routes to it; run in through `folio exec`, and bin/bottle for
//! its machine. Only where bottle-cli can run VMs, a Mac, and only with `cargo xtask test
//! --vm` (harness::real_bottle_cli).
//!
//! Makes one thread, `e2e`, from a clone of this repo, its spec `tools` and 8G, and deletes it after.
//! Its BOTTLE_HOME is $BOTTLE_E2E_HOME (default /tmp/bottle-e2e), kept between runs so the images it
//! builds are reused: the first run builds them, which takes minutes. Its folio home, and so its
//! folio serve, is its own, in it. Needs the network: the bottle reaches example.com.
//!
//! One test, its checks in order: they share the thread, which is deleted however it ends.

mod harness;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const THREAD: &str = "e2e";

struct EndToEnd {
    env: Vec<(String, String)>,
    work: PathBuf,
    repo: PathBuf,
    log: PathBuf,
    machine: String,
}

impl EndToEnd {
    fn folio(&self, args: &[&str]) -> Output {
        Command::new(harness::FOLIO)
            .args(args)
            .current_dir(&self.repo)
            .envs(self.env.iter().cloned())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.folio(args);
        assert!(
            out.status.success(),
            "folio {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// bin/bottle, for the thread's machine.
    fn bottle(&self, args: &[&str]) -> Output {
        Command::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../bin/bottle"))
            .args(args)
            .envs(self.env.iter().cloned())
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn in_bottle(&self, argv: &[&str], input: Option<&[u8]>) -> Output {
        let mut all = vec!["exec", THREAD, "--"];
        all.extend(argv);
        let mut child = Command::new(harness::FOLIO)
            .args(&all)
            .current_dir(&self.repo)
            .envs(self.env.iter().cloned())
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            let mut stdin = child.stdin.take().unwrap();
            let input = input.to_vec();
            std::thread::spawn(move || {
                use std::io::Write;
                let _ = stdin.write_all(&input);
            });
        }
        child.wait_with_output().unwrap()
    }

    fn out(&self, argv: &[&str]) -> String {
        let out = self.in_bottle(argv, None);
        assert!(
            out.status.success(),
            "{argv:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn https_status(&self) -> String {
        self.out(&[
            "curl",
            "-sS",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "https://example.com",
        ])
    }
}

impl Drop for EndToEnd {
    fn drop(&mut self) {
        let _ = self.folio(&["delete", THREAD, "--yes"]);
        let _ = self.folio(&["serve", "stop"]);
        let _ = fs::remove_dir_all(&self.work);
    }
}

#[test]
fn a_thread_end_to_end() {
    let Some(cli) = harness::real_bottle_cli() else {
        return;
    };
    let home = PathBuf::from(
        std::env::var("BOTTLE_E2E_HOME").unwrap_or_else(|_| "/tmp/bottle-e2e".into()),
    );
    fs::create_dir_all(&home).unwrap();
    let work = std::env::temp_dir().join(format!("bottle-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).unwrap();
    let repo = work.join("repo");
    let checkout = concat!(env!("CARGO_MANIFEST_DIR"), "/..");
    harness::git(&["clone", "-q", checkout, &repo.display().to_string()]);
    let s = |p: &PathBuf| p.display().to_string();
    let env = vec![
        ("BOTTLE_HOME".to_string(), s(&home)),
        ("FOLIO_HOME".into(), s(&home.join("folio"))),
        ("BOTTLE_CLI".into(), s(&cli)),
    ];
    let mut e = EndToEnd {
        env,
        log: home.join("folio/logs/serve.log"),
        work,
        repo,
        machine: String::new(),
    };
    // Whatever folio serve ran here before: this run's code answers.
    let _ = e.folio(&["serve", "stop"]);
    e.ok(&["configure", "--memory", "8G"]);
    e.ok(&["new", THREAD]);
    // Its bottle's machine: the bottle's name, as its record has it.
    let bottles = fs::read_dir(home.join("folio"))
        .unwrap()
        .flatten()
        .map(|d| d.path().join(THREAD).join("bottles"))
        .find(|p| p.is_dir())
        .unwrap();
    e.machine = fs::read_dir(bottles)
        .unwrap()
        .flatten()
        .find_map(|f| {
            f.path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap();

    its_running(&e);
    cp_in_and_out(&e);
    the_network_works_with_no_proxy(&e);
    a_private_address_is_refused_at_once(&e);
    git_through_bottle_host(&e);
    exec_carries_bytes_and_status(&e);
    the_network_after_a_restart_and_after_folio_serve_restarts(&e);
}

fn its_running(e: &EndToEnd) {
    let listed = e.ok(&["list"]);
    let row = listed
        .lines()
        .skip(1)
        .find(|l| l.split_whitespace().next() == Some(THREAD))
        .unwrap();
    assert_eq!(row.split_whitespace().nth(1), Some("running"), "{listed}");
    let status = e.bottle(&["status", &e.machine]);
    assert_eq!(String::from_utf8_lossy(&status.stdout).trim(), "running");
}

fn cp_in_and_out(e: &EndToEnd) {
    let file = e.work.join("f");
    fs::write(&file, "e2e\n").unwrap();
    e.ok(&[
        "cp",
        &file.display().to_string(),
        &format!("{THREAD}:/tmp/f"),
    ]);
    assert_eq!(e.out(&["cat", "/tmp/f"]), "e2e");
    let back = e.work.join("back");
    e.ok(&[
        "cp",
        &format!("{THREAD}:/tmp/f"),
        &back.display().to_string(),
    ]);
    assert_eq!(fs::read_to_string(back).unwrap(), "e2e\n");
}

fn the_network_works_with_no_proxy(e: &EndToEnd) {
    assert_eq!(e.out(&["sh", "-c", "env | grep -ci proxy || true"]), "0");
    assert!(
        e.out(&["sh", "-c", "cat ~/BOTTLE.md"])
            .contains("works as usual")
    );
    assert!(!e.out(&["getent", "hosts", "example.com"]).is_empty());
    assert_eq!(e.https_status(), "200");
}

fn a_private_address_is_refused_at_once(e: &EndToEnd) {
    let started = Instant::now();
    let out = e.in_bottle(
        &[
            "curl",
            "-s",
            "-m",
            "10",
            "-o",
            "/dev/null",
            "http://192.168.1.1",
        ],
        None,
    );
    assert!(!out.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

fn git_through_bottle_host(e: &EndToEnd) {
    assert!(
        e.out(&["cat", "/etc/hosts"])
            .contains("git.bottle.internal")
    );
    e.out(&["git", "-C", "/workspace", "fetch", "-q", "origin"]);
    e.out(&["sh", "-c", "cd /workspace && git commit -q --allow-empty -m e2e && git push -q agent HEAD:refs/heads/e2e"]);
    let landed = harness::git_in(
        &e.repo,
        &[
            "for-each-ref",
            &format!("refs/namespaces/{THREAD}-out/refs/heads/e2e"),
        ],
    );
    assert!(!landed.is_empty(), "the push didn't land in its lane");
}

fn exec_carries_bytes_and_status(e: &EndToEnd) {
    assert_eq!(
        e.in_bottle(&["head", "-c", "50000000", "/dev/urandom"], None)
            .stdout
            .len(),
        50_000_000
    );
    let counted = e.in_bottle(&["wc", "-c"], Some(&vec![0u8; 20_000_000]));
    assert_eq!(String::from_utf8_lossy(&counted.stdout).trim(), "20000000");
    assert_eq!(
        e.in_bottle(&["sh", "-c", "exit 7"], None).status.code(),
        Some(7)
    );
}

fn the_network_after_a_restart_and_after_folio_serve_restarts(e: &EndToEnd) {
    e.bottle(&["stop", &e.machine]);
    // folio exec starts it again, with its policy.
    assert_eq!(e.https_status(), "200");
    e.ok(&["serve", "stop"]);
    // folio serve, started again, answers it.
    e.out(&["git", "-C", "/workspace", "fetch", "-q", "origin"]);
    assert!(
        fs::read_to_string(&e.log)
            .unwrap_or_default()
            .contains(&e.machine)
    );
}
