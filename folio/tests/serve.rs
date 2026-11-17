//! `folio-cli serve`, end to end: started as folio starts it, on sockets of its own, driven as
//! bottle-cli drives it (a PROXY header, then the bottle's bytes) and as folio does (JSON lines on its
//! control socket). bottle-cli itself is a stand-in, for ssh and messages. Credentials are
//! tests/credentials.rs'.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

mod harness;
use harness::{Serve, bind};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A hub: a repo cloned from an upstream, so it has refs/remotes/origin.
fn hub(dir: &Path) -> (PathBuf, String) {
    let upstream = dir.join("upstream");
    fs::create_dir_all(&upstream).unwrap();
    git(&upstream, &["init", "-q", "-b", "main"]);
    git(&upstream, &["commit", "-q", "--allow-empty", "-m", "first"]);
    let hub = dir.join("hub");
    git(
        dir,
        &[
            "clone",
            "-q",
            upstream.to_str().unwrap(),
            hub.to_str().unwrap(),
        ],
    );
    let head = git(&hub, &["rev-parse", "HEAD"]);
    (hub.join(".git"), head)
}

#[test]
fn its_services_answer_a_bottle_each_on_its_socket_and_it_stops_with_its_last() {
    let dir = PathBuf::from(format!("/tmp/fsrepo{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let (git_dir, head) = hub(&dir);
    let repos = json!([{"git_dir": git_dir, "at": "workspace",
                        "remotes": ["origin:fetch=upstream", "agent:fetch=ns/t-out,push=ns/t-out"]}]);
    // Restored: bound before it answers anything.
    let serve = Serve::start("h", &[], json!([bind("b", "m1", json!({"repos": repos}))]));
    let code = serve.control(json!({"op": "ping"})).unwrap()["code"].clone();
    assert!(code.as_str().is_some_and(|c| c.len() == 16), "{code}");

    let fetch = b"GET /workspace/origin/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: git.bottle.internal\r\n\r\n";
    let answer = String::from_utf8_lossy(&serve.flow(
        "git",
        [192, 0, 2, 1],
        80,
        Some("git.bottle.internal"),
        false,
        "m1",
        fetch,
    ))
    .into_owned();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(
        answer.contains(&format!("{head} refs/heads/main")),
        "the upstream, as branches: {answer}"
    );
    // A push to a read-only remote is refused, saying where to push.
    let push = b"GET /workspace/origin/info/refs?service=git-receive-pack HTTP/1.1\r\n\r\n";
    let answer =
        String::from_utf8_lossy(&serve.flow("git", [192, 0, 2, 1], 80, None, false, "m1", push))
            .into_owned();
    assert!(
        answer.starts_with("HTTP/1.1 403") && answer.contains("read-only; push to agent instead"),
        "{answer}"
    );
    let push = b"GET /workspace/agent/info/refs?service=git-receive-pack HTTP/1.1\r\n\r\n";
    let answer =
        String::from_utf8_lossy(&serve.flow("git", [192, 0, 2, 1], 80, None, false, "m1", push))
            .into_owned();
    assert!(
        answer.starts_with("HTTP/1.1 200") && answer.contains("git-receive-pack"),
        "{answer}"
    );

    // A machine no bottle is gets nothing.
    assert!(
        serve
            .flow("git", [192, 0, 2, 1], 80, None, false, "stranger", fetch)
            .is_empty()
    );
    // A request for no remote of its own is answered so.
    let answer = String::from_utf8_lossy(&serve.flow(
        "git",
        [192, 0, 2, 1],
        80,
        None,
        false,
        "m1",
        b"GET /nowhere/origin/info/refs HTTP/1.1\r\n\r\n",
    ))
    .into_owned();
    assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");

    // Messages: a bare name is for bottles that may message any.
    let message = |to: &str| {
        let body = json!({"to": to, "text": "hi"}).to_string();
        format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let answer = String::from_utf8_lossy(&serve.flow(
        "messages",
        [192, 0, 2, 1],
        80,
        None,
        false,
        "m1",
        message("b2").as_bytes(),
    ))
    .into_owned();
    assert!(answer.starts_with("HTTP/1.1 404"), "{answer}");
    let answer = String::from_utf8_lossy(&serve.flow(
        "messages",
        [192, 0, 2, 1],
        80,
        None,
        false,
        "m1",
        b"POST / HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
    ))
    .into_owned();
    assert!(
        answer.starts_with("HTTP/1.1 400") && answer.contains("needs to"),
        "{answer}"
    );

    // The bottles: with list-bottles, the others running.
    serve.ok(bind(
        "b2",
        "m2",
        json!({"permissions": ["list-bottles"], "repo": "r", "labels": {"thread": "t"}}),
    ));
    let list = b"GET / HTTP/1.1\r\n\r\n";
    let answer = String::from_utf8_lossy(&serve.flow(
        "bottles",
        [192, 0, 2, 1],
        80,
        None,
        false,
        "m1",
        list,
    ))
    .into_owned();
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    let answer = String::from_utf8_lossy(&serve.flow(
        "bottles",
        [192, 0, 2, 1],
        80,
        None,
        false,
        "m2",
        list,
    ))
    .into_owned();
    assert!(
        answer.ends_with("[{\"labels\":{},\"name\":\"b\",\"repo\":null}]\n"),
        "{answer}"
    );

    // Released, all of them: it stops, and is gone.
    serve.ok(json!({"op": "release", "bottle": "b2"}));
    assert!(serve.control(json!({"op": "ping"})).is_some());
    serve.ok(json!({"op": "release", "bottle": "b"}));
    serve.eventually("it's gone", || !serve.dir.join("serve.sock").exists());
    assert!(serve.log().contains("no bottle left to answer: stopping"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn ssh_reaches_the_bottle_granted_at_an_address_and_nothing_else() {
    let dir = PathBuf::from(format!("/tmp/fsssh{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    // bottle-cli's exec: whatever it's asked to run, cat.
    let fake = dir.join("bottle-cli");
    fs::write(&fake, "#!/bin/sh\nexec cat\n").unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let serve = Serve::start(
        "s",
        &[("BOTTLE_CLI", fake.display().to_string())],
        json!([]),
    );
    serve.ok(bind("sidecar", "m-side", json!({"id": "7"})));
    serve.ok(bind("b", "m1", json!({"ssh": [{"name": "sidecar", "bottle": "sidecar", "id": "7", "address": "192.0.2.2"}]})));
    let echoed = serve.flow(
        "ssh",
        [192, 0, 2, 2],
        22,
        None,
        false,
        "m1",
        b"SSH-2.0-hello\n",
    );
    assert_eq!(echoed, b"SSH-2.0-hello\n", "{}", serve.log());
    // An address granted nothing, and a bottle made again since its grant.
    assert!(
        serve
            .flow("ssh", [192, 0, 2, 3], 22, None, false, "m1", b"x")
            .is_empty()
    );
    serve.ok(bind("sidecar", "m-side", json!({"id": "8"})));
    assert!(
        serve
            .flow("ssh", [192, 0, 2, 2], 22, None, false, "m1", b"x")
            .is_empty()
    );
    serve.ok(json!({"op": "shutdown"}));
    serve.eventually("it's gone", || !serve.dir.join("serve.sock").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn its_control_socket_says_its_code_binds_restores_and_shuts_it_down() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    // Restored: one whose features are gone is left out, and the rest are bound.
    let serve = Serve::start(
        "ctl",
        &[],
        json!([
            bind("bad", "m-bad", json!({"features": ["gone"]})),
            bind("good", "m-good", json!({}))
        ]),
    );
    let program = fs::read(env!("CARGO_BIN_EXE_folio")).unwrap();
    let code: String = Sha256::digest(&program)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        serve.control(json!({"op": "ping"})).unwrap(),
        json!({"ok": true, "code": code[..16]})
    );
    assert!(serve.log().contains("bad: couldn't restore its decisions"));
    let list = b"GET / HTTP/1.1\r\n\r\n";
    let answer = |machine| {
        String::from_utf8_lossy(&serve.flow(
            "bottles",
            [192, 0, 2, 1],
            80,
            None,
            false,
            machine,
            list,
        ))
        .into_owned()
    };
    assert!(answer("m-good").starts_with("HTTP/1.1 403"));
    assert!(answer("m-bad").is_empty());

    // Its sockets are its owner's alone.
    for socket in [
        "serve.sock",
        "s/git.sock",
        "s/messages.sock",
        "s/bottles.sock",
        "s/ssh.sock",
        "s/credentials.sock",
    ] {
        let mode = fs::metadata(serve.dir.join(socket))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "{socket}");
    }
    let unknown = serve.control(json!({"op": "nope"})).unwrap();
    assert_eq!(unknown["ok"], false);
    assert!(
        unknown["error"]
            .as_str()
            .unwrap()
            .contains("unknown variant `nope`"),
        "{unknown}"
    );
    let refused = serve
        .control(bind(
            "b",
            "m",
            json!({"repos": [{"git_dir": "/x", "at": "w", "remotes": ["origin:fetch=nowhere"]}]}),
        ))
        .unwrap();
    assert_eq!(refused["ok"], false, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("invalid remote"),
        "{refused}"
    );

    // Another, on the same control socket, leaves it be.
    let second = Command::new(env!("CARGO_BIN_EXE_folio"))
        .args(["serve", "--control"])
        .arg(serve.dir.join("serve.sock"))
        .env("FOLIO_HOME", serve.dir.join("home"))
        .output()
        .unwrap();
    assert!(second.status.success());
    assert!(
        String::from_utf8_lossy(&second.stderr).contains("another folio serve is already running")
    );
    assert!(serve.control(json!({"op": "ping"})).is_some());

    // Shut down, with a bottle bound: its sockets go, and so does it.
    serve.ok(json!({"op": "shutdown"}));
    serve.eventually("it's gone", || !serve.dir.join("serve.sock").exists());
    assert!(!serve.dir.join("s/git.sock").exists());
}

#[test]
fn a_message_goes_to_the_bottle_its_address_names_as_an_event_from_the_sender() {
    let dir = PathBuf::from(format!("/tmp/fsmsg{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // bottle-cli's exec: what it was asked to run and given, and it exits as $FAKE_DIR/exit says,
    // saying $FAKE_DIR/err.
    let fake = dir.join("bottle-cli");
    fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FAKE_DIR/argv\"\ncat > \"$FAKE_DIR/stdin\"\n\
         cat \"$FAKE_DIR/err\" >&2 2>/dev/null\nexit \"$(cat \"$FAKE_DIR/exit\" 2>/dev/null || echo 0)\"\n",
    )
    .unwrap();
    Command::new("chmod").arg("+x").arg(&fake).status().unwrap();
    let serve = Serve::start(
        "m",
        &[
            ("BOTTLE_CLI", fake.display().to_string()),
            ("FAKE_DIR", dir.display().to_string()),
        ],
        json!([]),
    );
    serve.ok(bind(
        "broker",
        "m-broker",
        json!({"permissions": ["list-bottles", "message-any"], "repo": "memories"}),
    ));
    for thread in ["foo", "bar"] {
        serve.ok(bind(
            &format!("app-{thread}"),
            &format!("m-{thread}"),
            json!({"repo": "app", "labels": {"thread": thread}}),
        ));
    }
    let send = |from: &str, to: &str, text: &str| -> String {
        let _ = fs::remove_file(dir.join("argv"));
        let body = json!({"to": to, "text": text}).to_string();
        let request = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        String::from_utf8_lossy(&serve.flow(
            "messages",
            [192, 0, 2, 1],
            80,
            None,
            false,
            from,
            request.as_bytes(),
        ))
        .into_owned()
    };
    // What bottle-cli was asked to run, in which machine, and from whom it said it was.
    let ran = || -> Option<(String, Vec<String>, String)> {
        let argv: Vec<String> = fs::read_to_string(dir.join("argv"))
            .ok()?
            .lines()
            .map(String::from)
            .collect();
        let after = argv.iter().position(|a| a == "--").unwrap() + 1;
        let stdin = fs::read_to_string(dir.join("stdin")).unwrap();
        Some((argv[1].clone(), argv[after..].to_vec(), stdin))
    };

    // A bottle that may message any sends by name, as an address the recipient can answer.
    let answer = send("m-broker", "app-foo", "status?\nall of it");
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let (machine, argv, stdin) = ran().unwrap();
    assert_eq!(
        (machine.as_str(), stdin.as_str()),
        ("m-foo", "status?\nall of it")
    );
    assert_eq!(
        argv[..5],
        ["/usr/local/bin/bottle-event", "send", "-t", "10", "-f"]
    );
    let broker_for_foo = argv[5].clone();
    assert!(broker_for_foo.starts_with("broker~"), "{broker_for_foo}");

    // The recipient answers by it; the sender gets an address of the recipient's, its own.
    let answer = send("m-foo", &broker_for_foo, "done");
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let (machine, argv, _) = ran().unwrap();
    assert_eq!(machine, "m-broker");
    assert!(argv[5].starts_with("app-foo~"), "{argv:?}");

    // An address works for the bottle it was issued to alone; a bottle without message-any can't
    // send by name; and a forged address is refused as a missing bottle is.
    for (from, to) in [
        ("m-bar", broker_for_foo.as_str()),
        ("m-foo", "broker"),
        ("m-foo", &format!("broker~{}", "a".repeat(26))),
        ("m-broker", "nobody"),
    ] {
        let answer = send(from, to, "hi");
        assert!(
            answer.starts_with("HTTP/1.1 404")
                && answer.ends_with(&format!("no running bottle you can reach as {to}\n")),
            "{from} to {to}: {answer}"
        );
        assert!(ran().is_none(), "{from} to {to}");
    }
    let answer = send("m-broker", "broker", "hi");
    assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");

    // No session taking it in time; and any other failure, saying what went wrong.
    fs::write(dir.join("exit"), "75").unwrap();
    let answer = send("m-broker", "app-foo", "hi");
    assert!(
        answer.starts_with("HTTP/1.1 504") && answer.contains("no session"),
        "{answer}"
    );
    fs::write(dir.join("exit"), "127").unwrap();
    fs::write(dir.join("err"), "bottle-event: not found").unwrap();
    let answer = send("m-broker", "app-foo", "hi");
    assert!(
        answer.starts_with("HTTP/1.1 502") && answer.contains("bottle-event: not found"),
        "{answer}"
    );

    // The other bottles running, by name, repo and labels, without a host's paths.
    let list = String::from_utf8_lossy(&serve.flow(
        "bottles",
        [192, 0, 2, 1],
        80,
        None,
        false,
        "m-broker",
        b"GET / HTTP/1.1\r\n\r\n",
    ))
    .into_owned();
    let body: Value = serde_json::from_str(list.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(
        body,
        json!([{"name": "app-bar", "repo": "app", "labels": {"thread": "bar"}},
               {"name": "app-foo", "repo": "app", "labels": {"thread": "foo"}}])
    );
    let _ = fs::remove_dir_all(&dir);
}
