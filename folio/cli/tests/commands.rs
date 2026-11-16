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
