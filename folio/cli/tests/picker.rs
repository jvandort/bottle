//! A command given no thread asks the picker, on a terminal, and says why not without one.

mod harness;

use harness::{ATTACH, Folio, argv, session_command};

/// The picker's entry for a new thread.
const NEW: &str = "+ new thread";

fn two() -> Folio {
    let f = Folio::new();
    f.ok(&["new", "older"]);
    f.ok(&["new", "newer"]);
    f
}

/// `folio COMMAND`, given no thread, on a terminal: `keys` typed in the picker, then `more`.
fn pick(f: &Folio, command: &str, keys: &[u8], more: &[(&[u8], &[u8])]) -> (i32, String) {
    let mut steps: Vec<(&[u8], &[u8])> = vec![(b"choose a thread", keys)];
    steps.extend(more);
    f.on_terminal(&[command], &steps)
}

/// The machine attached to last, if any was.
fn attached(f: &Folio) -> Option<String> {
    f.cli()["execs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["argv"].as_array().unwrap().iter().any(|a| a == "attach"))
        .next_back()
        .map(|e| e["machine"].as_str().unwrap().to_string())
}

fn attach_argv(session: &str) -> Vec<String> {
    argv(&[
        "sh",
        "-c",
        ATTACH,
        "attach",
        session,
        &session_command(session),
        "/workspace",
    ])
}

#[test]
fn without_a_terminal_it_says_to_name_one() {
    let f = two();
    for command in ["claude", "shell", "pause", "delete"] {
        let out = f.folio(&[command]);
        assert_eq!(out.code, 1, "{command}");
        assert!(
            out.stderr
                .contains(&format!("give the thread: `folio {command} THREAD`")),
            "{}",
            out.stderr
        );
    }
}

#[test]
fn it_lists_the_threads_most_recent_first() {
    let f = two();
    let (code, out) = pick(&f, "pause", b"q", &[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.find("newer  running   just now").unwrap()
            < out.find("older  running   just now").unwrap(),
        "{out}"
    );
}

#[test]
fn it_attaches_to_the_one_chosen() {
    let f = two();
    // Past + new thread and newer, to older.
    let (code, out) = pick(&f, "claude", b"jj\r", &[]);
    assert_eq!(code, 0, "{out}");
    assert!(out.find(NEW).unwrap() < out.find("newer").unwrap());
    assert_eq!(attached(&f), Some(f.bottle("older")));
    assert_eq!(f.ran("older").last().unwrap(), &attach_argv("claude"));
}

#[test]
fn a_new_thread_from_the_picker_is_made_and_attached_to() {
    let f = two();
    let (code, out) = pick(
        &f,
        "shell",
        b"\r",
        &[(b"The new thread's name: ", b"fresh\r")],
    );
    assert_eq!(code, 0, "{out}");
    assert_eq!(f.ran("fresh").last().unwrap(), &attach_argv("shell"));
}

#[test]
fn only_claude_and_shell_offer_a_new_thread() {
    let f = two();
    for command in ["pause", "delete"] {
        let (_, out) = pick(&f, command, b"q", &[]);
        assert!(!out.contains(NEW), "{command}: {out}");
    }
}

#[test]
fn leaving_without_choosing_does_nothing() {
    let f = two();
    for command in ["claude", "pause", "delete"] {
        let (code, out) = pick(&f, command, b"q", &[]);
        assert_eq!(code, 0, "{command}: {out}");
        assert_eq!(attached(&f), None);
    }
    assert_eq!(f.cli()["machines"].as_object().unwrap().len(), 2);
    // A new thread, but no name.
    pick(&f, "claude", b"\r", &[(b"The new thread's name: ", b"\r")]);
    assert_eq!(attached(&f), None);
    assert_eq!(f.cli()["machines"].as_object().unwrap().len(), 2);
}

#[test]
fn with_no_threads_only_claude_and_shell_have_anything_to_offer() {
    let f = two();
    f.ok(&["delete", "older", "--yes"]);
    f.ok(&["delete", "newer", "--yes"]);
    let (code, out) = f.on_terminal(&["pause"], &[]);
    assert_eq!(code, 1);
    assert!(
        out.contains("has no threads; start one with `folio new THREAD`"),
        "{out}"
    );
    let (_, out) = pick(&f, "claude", b"q", &[]);
    assert!(out.contains(NEW), "{out}");
}

#[test]
fn leaving_with_q_pauses_nothing() {
    let f = Folio::new();
    f.ok(&["new", "only"]);
    let (code, _) = f.on_terminal(&["pause"], &[(b"choose a thread", b"q")]);
    assert_eq!(code, 0);
    assert!(f.calls("delete").is_empty());
}
