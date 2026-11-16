//! `folio exec`: a command in a thread's bottle, its exit status folio's; and `folio cp`, into and
//! out of a thread's bottle, as `cp -r` would.

mod harness;

use std::fs;

use harness::Folio;

fn started() -> Folio {
    let f = Folio::new();
    f.ok(&["new", "fix-it"]);
    f
}

#[test]
fn exec_runs_in_the_bottle_and_exits_as_the_command_does() {
    let f = started();
    f.ok(&[
        "exec",
        "fix-it",
        "--",
        "sh",
        "-c",
        "echo made > /workspace/made",
    ]);
    assert_eq!(
        fs::read_to_string(f.workspace("fix-it").join("made")).unwrap(),
        "made\n"
    );
    assert_eq!(f.folio(&["exec", "fix-it", "sh", "-c", "exit 3"]).code, 3);
}

#[test]
fn a_terminal_is_given_when_its_asked_for() {
    let f = started();
    f.ok(&["exec", "-t", "fix-it", "--", "true"]);
    assert_eq!(f.execs("fix-it").last().unwrap()["tty"], true);
}

#[test]
fn exec_needs_a_command() {
    let f = started();
    let out = f.folio(&["exec", "fix-it"]);
    assert_ne!(out.code, 0);
    assert!(out.stderr.contains("give the command"), "{}", out.stderr);
}

#[test]
fn a_thread_without_a_sidecar_has_none_to_run_in() {
    let f = started();
    assert!(
        f.folio(&["sidecar", "exec", "fix-it", "--", "true"])
            .stderr
            .contains("has no sidecar")
    );
}

#[test]
fn cp_out_of_a_bottle_and_into_one() {
    let f = started();
    let workspace = f.workspace("fix-it");
    fs::write(workspace.join("report.txt"), "a report").unwrap();
    let out = f.tmp.join("out");
    fs::create_dir_all(&out).unwrap();
    assert_eq!(
        f.ok(&["cp", "fix-it:report.txt", &out.display().to_string()])
            .trim(),
        out.join("report.txt").display().to_string()
    );
    assert_eq!(
        fs::read_to_string(out.join("report.txt")).unwrap(),
        "a report"
    );
    let tree = f.tmp.join("fixtures");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("sub/f"), "a fixture").unwrap();
    assert_eq!(
        f.ok(&["cp", &tree.display().to_string(), "fix-it:"]).trim(),
        "fix-it:/workspace/fixtures"
    );
    assert_eq!(
        fs::read_to_string(workspace.join("fixtures/sub/f")).unwrap(),
        "a fixture"
    );
}

#[test]
fn one_side_names_a_thread() {
    let f = started();
    assert!(
        f.folio(&["cp", "a", "b"])
            .stderr
            .contains("name one, THREAD:PATH")
    );
}

#[test]
fn a_sidecar_has_the_same_commands() {
    let f = started();
    assert!(
        f.folio(&["sidecar", "cp", "fix-it:report.txt", "."])
            .stderr
            .contains("has no sidecar")
    );
    assert!(
        f.folio(&["sidecar", "cp", "a", "b"])
            .stderr
            .contains("thread's sidecar: name one")
    );
}
