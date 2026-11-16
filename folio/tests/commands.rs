//! folio's commands, from outside: this program as a user runs it, in a home of its own.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const FOLIO: &str = env!("CARGO_BIN_EXE_folio");

/// A home for folio, short (its sockets are in it), gone with the test.
struct Home(PathBuf);

impl Home {
    fn new(tag: &str) -> Home {
        let path = PathBuf::from(format!("/tmp/fc{tag}{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Home(path)
    }

    fn run(&self, program: &Path, args: &[&str]) -> Output {
        Command::new(program)
            .args(args)
            .env("FOLIO_HOME", &self.0)
            .env_remove("FOLIO_CLI")
            .output()
            .unwrap()
    }

    fn folio(&self, args: &[&str]) -> Output {
        self.run(Path::new(FOLIO), args)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = Command::new(FOLIO)
            .args(["serve", "stop"])
            .env("FOLIO_HOME", &self.0)
            .output();
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn said(output: &Output) -> (String, String) {
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn folio_serve_starts_and_stops_and_says_when_it_was_already() {
    let home = Home::new("s");
    assert_eq!(
        said(&home.folio(&["serve", "stop"])).0,
        "folio serve isn't running\n"
    );
    assert_eq!(
        said(&home.folio(&["serve", "start"])).0,
        "Started folio serve\n"
    );
    assert_eq!(
        said(&home.folio(&["serve", "start"])).0,
        "folio serve is already running\n"
    );
    assert!(home.0.join("serve.sock").exists());
    assert!(home.0.join("logs/serve.log").exists());
    assert_eq!(
        said(&home.folio(&["serve", "stop"])).0,
        "Stopped folio serve\n"
    );
    assert_eq!(
        said(&home.folio(&["serve", "stop"])).0,
        "folio serve isn't running\n"
    );
}

#[test]
fn one_running_other_code_is_restarted_first() {
    let home = Home::new("r");
    // This program, but for a byte at its end: other code, by its hash.
    let other = home.0.join("folio-other");
    let mut copy = fs::read(FOLIO).unwrap();
    copy.push(0);
    let mut file = fs::File::create(&other).unwrap();
    file.write_all(&copy).unwrap();
    drop(file);
    fs::set_permissions(&other, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        said(&home.run(&other, &["serve", "start"])).0,
        "Started folio serve\n"
    );
    let (out, err) = said(&home.folio(&["serve", "start"]));
    assert_eq!(
        err,
        "folio: folio serve was running other code; restarting it\n"
    );
    assert_eq!(out, "Started folio serve\n");
    let (out, err) = said(&home.folio(&["serve", "start"]));
    assert_eq!(
        (out.as_str(), err.as_str()),
        ("folio serve is already running\n", "")
    );
}

/// A folio serve that isn't this one, on `home`'s control socket: answering a ping with `code`
/// (None: one from before it said) while it runs, and once it's told to shut down (or from the
/// start, if `winding_up`), taking `closing` more connections and closing them unanswered (what
/// one winding up does) before it's gone. What it was asked, in order ("closed" for each it closed).
fn pretend(
    home: &Path,
    code: Option<&str>,
    closing: usize,
    winding_up: bool,
) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    use std::io::{BufRead, BufReader};
    let socket = home.join("serve.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (seen, code) = (calls.clone(), code.map(String::from));
    std::thread::spawn(move || {
        let (mut stopping, mut closing) = (winding_up, closing);
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            if stopping {
                seen.lock().unwrap().push("closed".to_string());
                closing = closing.saturating_sub(1);
                drop(stream);
                if closing == 0 {
                    let _ = fs::remove_file(&socket);
                    return;
                }
                continue;
            }
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let message: serde_json::Value = serde_json::from_str(&line).unwrap();
            let op = message["op"].as_str().unwrap().to_string();
            seen.lock().unwrap().push(op.clone());
            let reply = match (op.as_str(), &code) {
                ("ping", None) => serde_json::json!({"ok": true}),
                ("ping", Some(code)) => serde_json::json!({"ok": true, "code": code}),
                _ => serde_json::json!({"ok": true}),
            };
            writeln!(stream, "{reply}").unwrap();
            if op == "shutdown" {
                stopping = true;
                if closing == 0 {
                    let _ = fs::remove_file(&socket);
                    return;
                }
            }
        }
    });
    calls
}

fn asked(calls: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
    calls.lock().unwrap().clone()
}

#[test]
fn one_that_closes_connections_as_it_stops_is_waited_out() {
    let home = Home::new("w");
    let calls = pretend(&home.0, Some("old"), 2, false);
    let (out, err) = said(&home.folio(&["serve", "start"]));
    assert_eq!(
        err,
        "folio: folio serve was running other code; restarting it\n"
    );
    assert_eq!(out, "Started folio serve\n");
    // Asked whether it runs, then its code.
    assert_eq!(
        asked(&calls),
        ["ping", "ping", "shutdown", "closed", "closed"]
    );
    assert_eq!(
        said(&home.folio(&["serve", "start"])).0,
        "folio serve is already running\n"
    );
}

#[test]
fn one_from_before_it_said_its_code_is_restarted() {
    let home = Home::new("b");
    let calls = pretend(&home.0, None, 0, false);
    assert_eq!(
        said(&home.folio(&["serve", "start"])).0,
        "Started folio serve\n"
    );
    assert_eq!(asked(&calls), ["ping", "ping", "shutdown"]);
}

#[test]
fn starting_one_on_its_way_out_waits_it_out_and_starts_this() {
    let home = Home::new("o");
    let calls = pretend(&home.0, Some("old"), 2, true);
    assert_eq!(
        said(&home.folio(&["serve", "start"])).0,
        "Started folio serve\n"
    );
    assert_eq!(asked(&calls), ["closed", "closed"]);
}

#[test]
fn stopping_one_waits_till_its_gone() {
    let home = Home::new("g");
    let calls = pretend(&home.0, Some("old"), 2, false);
    assert_eq!(
        said(&home.folio(&["serve", "stop"])).0,
        "Stopped folio serve\n"
    );
    assert_eq!(asked(&calls), ["shutdown", "closed", "closed"]);
    assert!(!home.0.join("serve.sock").exists());
}
