//! A drone: a thread's bottle on a runner, its machine answered by folio serve here, through the
//! forward folio serve keeps to the runner (the stand-in ssh makes one a symlink there to the
//! socket here). The test is the runner's bottle-cli: it gives the socket the machine's routes name
//! a connection its guest opened, PROXY header and all. And a runner detached to be turned off, and
//! attached again.

mod harness;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use harness::{Folio, Runner};
use serde_json::json;

const FETCH: &[u8] = b"GET /workspace/origin/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: git.bottle.internal\r\n\r\n";
const PUSH: &[u8] = b"GET /workspace/agent/info/refs?service=git-receive-pack HTTP/1.1\r\nHost: git.bottle.internal\r\n\r\n";

/// A thread `t` on runner box, its repo a clone of an upstream that gets commits the repo never
/// fetched (named as the repo is, so the repo's name, which is its origin's, stays the same).
struct Drone {
    f: Folio,
    box_: Runner,
    upstream: PathBuf,
    machine: String,
}

impl Drone {
    fn new() -> Drone {
        let mut f = Folio::new();
        let box_ = f.add_runner("box");
        let upstream = f.make_repo("upstream/project");
        let repo = f.repo.display().to_string();
        f.git(&[
            "-C",
            &repo,
            "remote",
            "add",
            "origin",
            &upstream.display().to_string(),
        ]);
        f.git(&["-C", &repo, "fetch", "-q", "origin"]);
        f.ok(&["new", "t", "--runner", "box"]);
        let machine = f.bottle("t");
        Drone {
            f,
            box_,
            upstream,
            machine,
        }
    }

    /// The socket the machine's routes name, there.
    fn there(&self) -> String {
        self.box_.machine("t")["network"]["routes"][0]["socket"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A connection from the machine's guest to git.bottle.internal, as bottle-cli there gives it to the
    /// socket its routes name, from the machine its PROXY header says (its own, unless `said`);
    /// what came back. One closed unanswered may be reset, if it hadn't read all that was sent.
    fn guest_connects(&self, first: &[u8], said: Option<&str>) -> Vec<u8> {
        let there = self.there();
        let deadline = Instant::now() + Duration::from_secs(10);
        // Its runner's forward, made in the background as it was bound.
        loop {
            match UnixStream::connect(&there) {
                Ok(mut s) => {
                    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                    s.write_all(&harness::proxy_header(said.unwrap_or(&self.machine)))
                        .unwrap();
                    s.write_all(first).unwrap();
                    let mut answer = Vec::new();
                    let _ = s.read_to_end(&mut answer);
                    return answer;
                }
                Err(e) => {
                    assert!(Instant::now() < deadline, "nothing at {there}: {e}");
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

fn has(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn its_fetch_reaches_the_hub_here_through_its_runners_forward() {
    let d = Drone::new();
    let new = d.f.commit(&d.upstream, "since the hub last fetched");
    let answer = d.guest_connects(FETCH, None);
    assert!(
        has(&answer, b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&answer)
    );
    // The hub fetched origin, and served it, through the runner's forward.
    assert!(has(&answer, format!("{new} refs/heads/main").as_bytes()));
}

#[test]
fn it_fetches_and_pushes_again_once_folio_serve_is_back() {
    let d = Drone::new();
    assert!(has(&d.guest_connects(FETCH, None), b"HTTP/1.1 200"));
    d.f.ok(&["serve", "stop"]);
    harness::eventually("folio serve gone", || !d.f.home.join("serve.sock").exists());
    // Its forward's gone with it.
    assert!(UnixStream::connect(d.there()).is_err());
    // Binding again the bottles whose machines run, wherever they are.
    d.f.ok(&["serve", "start"]);
    assert!(has(&d.guest_connects(FETCH, None), b"HTTP/1.1 200"));
    // A push, which needs the bottle's side here, finds it, though folio serve is new.
    assert!(has(&d.guest_connects(PUSH, None), b"HTTP/1.1 200"));
}

#[test]
fn a_machine_no_bottle_is_gets_nothing() {
    let d = Drone::new();
    assert_eq!(d.guest_connects(FETCH, Some("stranger")), b"");
}

// --- detached to be turned off, and attached again ----------------------------------------

#[test]
fn with_its_bottles_running_and_no_one_to_ask_it_stops_nothing_and_stays_attached() {
    let d = Drone::new();
    let out = d.f.folio(&["runner", "detach", "box"]);
    assert_ne!(out.code, 0);
    assert!(
        out.stderr.contains(&format!(
            "box is still attached, and {} still running",
            d.machine
        )),
        "{}",
        out.stderr
    );
    assert_eq!(d.box_.machine("t")["state"], "running");
    assert!(!d.f.ok(&["runner", "ls"]).contains("(detached)"));
}

#[test]
fn detaching_pauses_its_threads_so_they_resume_anywhere() {
    let d = Drone::new();
    std::fs::write(d.box_.workspace("t").join("draft.txt"), "draft\n").unwrap();
    let (code, out) = d.f.on_terminal(
        &["runner", "detach", "box"],
        &[(b"Pause and stop them now? [y/N] ", b"y\r")],
    );
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("Paused thread t"), "{out}");
    assert!(out.contains("safe to turn off"), "{out}");
    // Its bottle's gone from the runner; its snapshot is here.
    assert!(d.box_.machine("t").is_null());
    d.box_.power(false);
    assert_eq!(d.f.rows()["t"][0], "paused");
    // And it resumes here, the runner off.
    d.f.ok(&["configure", "t", "--runner", "local"]);
    d.f.ok(&["shell", "t"]);
    assert_eq!(
        std::fs::read_to_string(d.f.workspace("t").join("draft.txt")).unwrap(),
        "draft\n"
    );
}

#[test]
fn detached_its_bottles_are_detached_and_it_isnt_asked() {
    let d = Drone::new();
    d.box_.set_machine("t", json!({"state": "stopped"}));
    d.f.ok(&["runner", "detach", "box"]);
    d.box_.power(false);
    let rows = d.f.rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows["t"][0], "detached");
}

#[test]
fn one_that_cant_be_reached_is_detached_only_offline() {
    let d = Drone::new();
    d.box_.power(false);
    assert!(
        d.f.folio(&["runner", "detach", "box"])
            .stderr
            .contains("--offline")
    );
    assert!(!d.f.ok(&["runner", "ls"]).contains("(detached)"));
    d.f.ok(&["runner", "detach", "box", "--offline"]);
    assert!(d.f.ok(&["runner", "ls"]).contains("(detached)"));
}

#[test]
fn using_it_again_attaches_it_once_its_back() {
    let d = Drone::new();
    d.box_.power(false);
    d.f.ok(&["runner", "detach", "box", "--offline"]);
    let out = d.f.folio(&["exec", "t", "--", "true"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("box is detached, and doesn't answer yet"),
        "{}",
        out.stderr
    );
    assert!(out.stderr.contains("turn it on"), "{}", out.stderr);
    assert!(d.f.ok(&["runner", "ls"]).contains("(detached)"));
    d.box_.power(true);
    let out = d.f.folio(&["exec", "t", "--", "true"]);
    assert_eq!(out.code, 0, "{}", out.stderr);
    assert!(!d.f.ok(&["runner", "ls"]).contains("(detached)"));
}

#[test]
fn a_paused_thread_it_cant_resume_on_is_told_it_can_resume_here() {
    let d = Drone::new();
    d.f.ok(&["pause", "t"]);
    d.box_.power(false);
    d.f.ok(&["runner", "detach", "box", "--offline"]);
    let out = d.f.folio(&["claude", "t"]);
    assert_eq!(out.code, 1);
    assert!(
        out.stderr
            .contains("box is detached, and doesn't answer yet"),
        "{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("`folio claude t --runner local`"),
        "{}",
        out.stderr
    );
    assert!(d.f.ok(&["runner", "list"]).contains("(detached)"));
}
