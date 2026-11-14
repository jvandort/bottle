//! `folio-cli serve`, end to end: started as folio starts it, on sockets of its own, driven as
//! bottle-cli drives it (a PROXY header, then the bottle's bytes) and as folio does (JSON lines on its
//! control socket). bottle-cli itself is a stand-in, for ssh: its exec runs `cat`; a credential's
//! host is a TLS server here, with a CA of the test's own.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const STANDIN: &str = "the-egress-proxy-holds-the-real-one";

struct Serve {
    dir: PathBuf,
    child: Child,
}

impl Serve {
    fn start(tag: &str, env: &[(&str, String)], restore: Value) -> Serve {
        // Short: unix socket paths are.
        let dir = PathBuf::from(format!("/tmp/fs{tag}{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let log = fs::File::create(dir.join("serve.log")).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_folio-cli"));
        command
            .arg("serve")
            .arg("--control")
            .arg(dir.join("serve.sock"))
            .args(["host", "ssh", "credentials"].iter().flat_map(|s| {
                [
                    "--service".to_string(),
                    format!("{s}={}", dir.join(format!("s/{s}.sock")).display()),
                ]
            }))
            .arg("--restore")
            .env("FOLIO_HOME", dir.join("home"))
            .stdin(Stdio::piped())
            .stdout(log.try_clone().unwrap())
            .stderr(log);
        for (k, v) in env {
            command.env(k, v);
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(restore.to_string().as_bytes())
            .unwrap();
        let serve = Serve { dir, child };
        serve.eventually("it answers", || {
            serve.control(json!({"op": "ping"})).is_some()
        });
        serve
    }

    fn eventually(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(
                Instant::now() < deadline,
                "{what}; its log:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn log(&self) -> String {
        fs::read_to_string(self.dir.join("serve.log")).unwrap_or_default()
    }

    /// Its answer to `message` on its control socket; None if it took none.
    fn control(&self, message: Value) -> Option<Value> {
        let mut s = UnixStream::connect(self.dir.join("serve.sock")).ok()?;
        s.write_all(format!("{message}\n").as_bytes()).ok()?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).ok()?;
        serde_json::from_str(&line).ok()
    }

    fn ok(&self, message: Value) {
        let answer = self.control(message.clone()).unwrap();
        assert_eq!(
            answer["ok"],
            true,
            "{message}: {answer}; its log:\n{}",
            self.log()
        );
    }

    /// A connection `machine`'s guest opened to `address`:`port`, as bottle-cli gives it here; what
    /// came back.
    fn flow(
        &self,
        service: &str,
        address: [u8; 4],
        port: u16,
        name: Option<&str>,
        tls: bool,
        machine: &str,
        sent: &[u8],
    ) -> Vec<u8> {
        let mut s = UnixStream::connect(self.dir.join(format!("s/{service}.sock"))).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.write_all(&header(address, port, name, tls, machine))
            .unwrap();
        s.write_all(sent).unwrap();
        // Done sending, as a client that's said all it will says so; but for one whose TLS
        // folio serve carries on, where that would be the end of the conversation.
        if service != "credentials" {
            let _ = s.shutdown(std::net::Shutdown::Write);
        }
        let mut answer = Vec::new();
        let _ = s.read_to_end(&mut answer);
        answer
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// A PROXY protocol v2 header, as bottle-cli writes one.
fn header(address: [u8; 4], port: u16, name: Option<&str>, tls: bool, machine: &str) -> Vec<u8> {
    let mut fields = Vec::new();
    let mut field = |kind: u8, value: &[u8]| {
        fields.push(kind);
        fields.extend((value.len() as u16).to_be_bytes());
        fields.extend(value);
    };
    if let Some(name) = name {
        field(0x02, name.as_bytes());
    }
    if tls {
        field(0x20, &[1, 0, 0, 0, 0]);
    }
    field(0xE0, machine.as_bytes());
    let mut body = vec![10, 0, 0, 2];
    body.extend(address);
    body.extend(40000u16.to_be_bytes());
    body.extend(port.to_be_bytes());
    body.extend(fields);
    let mut out = b"\r\n\r\n\x00\r\nQUIT\n".to_vec();
    out.extend([0x21, 0x11]);
    out.extend((body.len() as u16).to_be_bytes());
    out.extend(body);
    out
}

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

fn bind(bottle: &str, machine: &str, extra: Value) -> Value {
    let mut message = json!({"op": "bind", "bottle": bottle, "machine": machine, "runner": "local", "id": "1",
                             "features": [], "repos": [], "permissions": [], "labels": {}, "ssh": []});
    for (k, v) in extra.as_object().unwrap() {
        message[k] = v.clone();
    }
    message
}

#[test]
fn bottle_host_serves_its_repos_messages_and_bottles_and_it_stops_with_its_last() {
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

    let fetch = b"GET /git/workspace/origin/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: bottle.host\r\n\r\n";
    let answer = String::from_utf8_lossy(&serve.flow(
        "host",
        [192, 0, 2, 1],
        80,
        Some("bottle.host"),
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
    let push = b"GET /git/workspace/origin/info/refs?service=git-receive-pack HTTP/1.1\r\n\r\n";
    let answer =
        String::from_utf8_lossy(&serve.flow("host", [192, 0, 2, 1], 80, None, false, "m1", push))
            .into_owned();
    assert!(
        answer.starts_with("HTTP/1.1 403") && answer.contains("read-only; push to agent instead"),
        "{answer}"
    );
    let push = b"GET /git/workspace/agent/info/refs?service=git-receive-pack HTTP/1.1\r\n\r\n";
    let answer =
        String::from_utf8_lossy(&serve.flow("host", [192, 0, 2, 1], 80, None, false, "m1", push))
            .into_owned();
    assert!(
        answer.starts_with("HTTP/1.1 200") && answer.contains("git-receive-pack"),
        "{answer}"
    );

    // A machine no bottle is gets nothing.
    assert!(
        serve
            .flow("host", [192, 0, 2, 1], 80, None, false, "stranger", fetch)
            .is_empty()
    );
    // Nor does one of bottle.host's ports that isn't a service.
    let answer = String::from_utf8_lossy(&serve.flow(
        "host",
        [192, 0, 2, 1],
        8080,
        None,
        false,
        "m1",
        fetch,
    ))
    .into_owned();
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");

    // Messages: a bare name is for bottles that may message any.
    let message = |to: &str| {
        let body = json!({"to": to, "text": "hi"}).to_string();
        format!(
            "POST /messages HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let answer = String::from_utf8_lossy(&serve.flow(
        "host",
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
        "host",
        [192, 0, 2, 1],
        80,
        None,
        false,
        "m1",
        b"POST /messages HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}",
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
    let list = b"GET /bottles HTTP/1.1\r\n\r\n";
    let answer =
        String::from_utf8_lossy(&serve.flow("host", [192, 0, 2, 1], 80, None, false, "m1", list))
            .into_owned();
    assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
    let answer =
        String::from_utf8_lossy(&serve.flow("host", [192, 0, 2, 1], 80, None, false, "m2", list))
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
fn a_credential_goes_on_requests_that_present_the_stand_in_and_never_on_forbidden_ones() {
    let dir = PathBuf::from(format!("/tmp/fscred{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // The host: TLS, with a certificate of a CA of the test's own, answering with what it was sent.
    let made = rcgen::generate_simple_self_signed(vec!["api.example.com".to_string()]).unwrap();
    fs::write(dir.join("ca.pem"), made.cert.pem()).unwrap();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![made.cert.der().clone()],
                rustls::pki_types::PrivateKeyDer::Pkcs8(made.signing_key.serialize_der().into()),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let config = config.clone();
            std::thread::spawn(move || {
                let conn = rustls::ServerConnection::new(config).unwrap();
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                let mut head = Vec::new();
                let mut byte = [0];
                while !head.ends_with(b"\r\n\r\n") && tls.read(&mut byte).unwrap_or(0) == 1 {
                    head.push(byte[0]);
                }
                let body = String::from_utf8_lossy(&head).into_owned();
                let _ = write!(
                    tls,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    // The feature that declares it, and its value, where the Keychain would be.
    let feature = dir.join("features/tok");
    fs::create_dir_all(&feature).unwrap();
    fs::write(feature.join("install.sh"), "#!/bin/sh\n").unwrap();
    fs::write(
        feature.join("devcontainer-feature.json"),
        json!({"id": "tok", "version": "1", "customizations": {"bottle": {"credentials": {"tok": {
            "description": "a token",
            "inject": {"hosts": [format!("api.example.com:{port}")], "value": "Bearer ${credential}",
                       "forbid": ["POST /api/auth/token"]}}}}}})
        .to_string(),
    )
    .unwrap();
    fs::write(
        dir.join("credentials.json"),
        json!({"tok": "the-real-one"}).to_string(),
    )
    .unwrap();
    let serve = Serve::start(
        "c",
        &[
            ("FOLIO_FEATURES", dir.join("features").display().to_string()),
            (
                "FOLIO_CREDENTIALS",
                dir.join("credentials.json").display().to_string(),
            ),
            (
                "FOLIO_SERVE_RESOLVE",
                format!("api.example.com=127.0.0.1:{port}"),
            ),
            (
                "FOLIO_SERVE_ROOTS",
                dir.join("ca.pem").display().to_string(),
            ),
        ],
        json!([]),
    );
    serve.ok(bind("b", "m1", json!({"features": ["tok"]})));
    let ask = |request: &str| {
        let answer = serve.flow(
            "credentials",
            [93, 184, 216, 34],
            port,
            Some("api.example.com"),
            true,
            "m1",
            request.as_bytes(),
        );
        String::from_utf8_lossy(&answer).into_owned()
    };
    // Presented, the stand-in is replaced; the Host and routing headers are the host's.
    let answer = ask(&format!(
        "GET /repos?x=1 HTTP/1.1\r\nHost: evil.example\r\nX-Forwarded-Host: evil\r\nAuthorization: Bearer {STANDIN}\r\n\r\n"
    ));
    assert!(
        answer.starts_with("HTTP/1.1 200"),
        "{answer}\n{}",
        serve.log()
    );
    assert!(answer.contains("GET /repos?x=1 HTTP/1.1"), "{answer}");
    assert!(
        answer.contains("Authorization: Bearer the-real-one"),
        "{answer}"
    );
    assert!(
        answer.contains(&format!("Host: api.example.com:{port}")) && !answer.contains("evil"),
        "{answer}"
    );
    // Not presented, nothing is attached.
    let answer = ask("GET /public HTTP/1.1\r\n\r\n");
    assert!(
        answer.starts_with("HTTP/1.1 200") && !answer.contains("the-real-one"),
        "{answer}"
    );
    // Forbidden: never sent, with the credential or without; said as JSON to a client that asks so.
    let answer = ask(
        "POST /api/auth/token.json HTTP/1.1\r\nAccept: application/json\r\nContent-Length: 0\r\n\r\n",
    );
    assert!(
        answer.starts_with("HTTP/1.1 403") && answer.contains("permission_error"),
        "{answer}"
    );
    assert!(answer.contains("forbids this route"), "{answer}");
    // TRACE would echo it back.
    assert!(ask("TRACE / HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 405"));
    assert!(
        serve.log().contains(&format!(
            "b GET https://api.example.com/repos credential 200"
        )),
        "{}",
        serve.log()
    );
    let _ = fs::remove_dir_all(&dir);
}
