//! `folio-cli serve`'s credentials, end to end: a bottle's request to a credential's host, as
//! bottle-cli gives it here (its TLS taken off), reaching the host over TLS, with the credential
//! where the request presents the stand-in, and never on what the feature forbids. The host is a
//! TLS server here, with a CA of the test's own, that says what it was sent.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

mod harness;
use harness::{STANDIN, Serve, bind};

const REAL: &str = "real-token";
/// How much a host that answers as it's sent (duplex) answers.
const ANSWER: usize = 4 << 20;
/// Where the bottle thought it was connecting: an address anywhere.
const CONNECTED_TO: [u8; 4] = [93, 184, 216, 34];

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| n << 8 | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// A credential's host, and folio serve with a bottle `b` whose feature `inject`s a credential
/// there: its hosts are upstream.test (this host, at its port), and whatever else `inject` says.
struct Host {
    serve: Serve,
    port: u16,
    /// What the host was sent, a request at a time: its head and body.
    received: Receiver<String>,
    dir: PathBuf,
}

impl Host {
    fn start(tag: &str, name: &str, mut inject: Value) -> Host {
        let dir = PathBuf::from(format!("/tmp/fcr{tag}{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let made = rcgen::generate_simple_self_signed(vec!["upstream.test".to_string()]).unwrap();
        fs::write(dir.join("ca.pem"), made.cert.pem()).unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    vec![made.cert.der().clone()],
                    rustls::pki_types::PrivateKeyDer::Pkcs8(
                        made.signing_key.serialize_der().into(),
                    ),
                )
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sent, received) = channel();
        let sent = Arc::new(Mutex::new(sent));
        std::thread::spawn(move || {
            for tcp in listener.incoming().flatten() {
                let (config, sent) = (config.clone(), sent.clone());
                std::thread::spawn(move || answer(config, tcp, &sent));
            }
        });
        let hosts = inject["hosts"].as_array().cloned().unwrap_or_default();
        inject["hosts"] = json!([format!("upstream.test:{port}")]);
        inject["hosts"].as_array_mut().unwrap().extend(hosts);
        inject["header"] = json!("Authorization");
        inject["value"] = json!("Bearer ${credential}");
        let feature = dir.join(format!("features/{name}"));
        fs::create_dir_all(&feature).unwrap();
        fs::write(feature.join("install.sh"), "#!/bin/sh\n").unwrap();
        fs::write(
            feature.join("devcontainer-feature.json"),
            json!({"id": name, "version": "1", "customizations": {"bottle": {"credentials": {name: {
                "description": "a token", "inject": inject}}}}})
            .to_string(),
        )
        .unwrap();
        fs::write(
            dir.join("credentials.json"),
            json!({name: REAL}).to_string(),
        )
        .unwrap();
        let path = |p: &str| dir.join(p).display().to_string();
        let serve = Serve::start(
            &format!("r{tag}"),
            &[
                ("FOLIO_FEATURES", path("features")),
                ("FOLIO_CREDENTIALS", path("credentials.json")),
                (
                    "FOLIO_SERVE_RESOLVE",
                    format!("upstream.test=127.0.0.1:{port},mismatched.test=127.0.0.1:{port}"),
                ),
                ("FOLIO_SERVE_ROOTS", path("ca.pem")),
            ],
            json!([]),
        );
        serve.ok(bind("b", "m1", json!({"features": [name]})));
        Host {
            serve,
            port,
            received,
            dir,
        }
    }

    /// What the bottle gets back for `request` to upstream.test.
    fn send(&self, request: &[u8]) -> String {
        self.send_to("upstream.test", self.port, true, request)
    }

    fn send_to(&self, name: &str, port: u16, tls: bool, request: &[u8]) -> String {
        let answer = self.serve.flow(
            "credentials",
            CONNECTED_TO,
            port,
            Some(name),
            tls,
            "m1",
            request,
        );
        String::from_utf8_lossy(&answer).into_owned()
    }

    /// What the host was sent for `request`.
    fn sent_for(&self, request: &[u8]) -> String {
        let answer = self.send(request);
        self.received
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("nothing reached the host; the bottle got {answer}"))
    }

    /// Whether nothing reached the host.
    fn nothing_sent(&self) -> bool {
        self.received.try_recv().is_err()
    }

    fn log(&self) -> String {
        // Its flows' lines come after the bottle has its answer.
        std::thread::sleep(Duration::from_millis(100));
        self.serve.log()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// The host's side of a connection: each request it's sent, as it was sent (a body by its
/// Content-Length, a big one after a pause); "hello" back, or, asked to upgrade, 101 and whatever
/// comes after, echoed.
fn answer(
    config: Arc<rustls::ServerConfig>,
    tcp: std::net::TcpStream,
    sent: &Mutex<std::sync::mpsc::Sender<String>>,
) {
    let conn = rustls::ServerConnection::new(config).unwrap();
    let mut tls = BufReader::new(rustls::StreamOwned::new(conn, tcp));
    let mut head = String::new();
    loop {
        let mut line = Vec::new();
        if tls.read_until(b'\n', &mut line).unwrap_or(0) == 0 {
            return;
        }
        head.push_str(&String::from_utf8_lossy(&line));
        if line == b"\r\n" {
            break;
        }
    }
    let length = head
        .lines()
        .find_map(|l| {
            l.to_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    if head.starts_with("POST /duplex ") {
        return duplex(tls, head, length, sent);
    }
    if length > 64 * 1024 {
        // Taken slowly, as a far host might: what sends it has to wait for it.
        std::thread::sleep(Duration::from_millis(300));
    }
    let mut body = vec![0; length];
    let _ = tls.read_exact(&mut body);
    let upgrade = head.to_lowercase().contains("upgrade: websocket");
    let mut whole = head.into_bytes();
    whole.extend(&body);
    let _ = sent
        .lock()
        .unwrap()
        .send(String::from_utf8_lossy(&whole).into_owned());
    let tls = tls.get_mut();
    if upgrade {
        let _ = tls.write_all(
            b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
        );
        let _ = tls.flush();
        let mut buf = [0; 1024];
        while let Ok(n @ 1..) = tls.read(&mut buf) {
            let _ = tls.write_all(&buf[..n]);
            let _ = tls.flush();
        }
    } else {
        let _ = tls
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello");
    }
    tls.conn.send_close_notify();
    let _ = tls.flush();
}

/// The host's side of a request to /duplex, answered as it's sent, as a host that streams does: the
/// answer (ANSWER bytes) written from the head on, the body read meanwhile, on a thread of its own,
/// rustls's state shared between them (never held while reading or writing the connection).
fn duplex(
    tls: BufReader<rustls::StreamOwned<rustls::ServerConnection, std::net::TcpStream>>,
    head: String,
    length: usize,
    sent: &Mutex<std::sync::mpsc::Sender<String>>,
) {
    let early = tls.buffer().to_vec();
    let rustls::StreamOwned { conn, sock } = tls.into_inner();
    let conn = Arc::new(Mutex::new(conn));
    let mut tcp = sock.try_clone().unwrap();
    let reader = {
        let (conn, mut tcp) = (conn.clone(), sock);
        std::thread::spawn(move || {
            let mut body = early;
            let (mut buf, mut plain) = (vec![0; 64 * 1024], vec![0; 64 * 1024]);
            // What rustls has decrypted, taken: it takes no more while it holds too much.
            let take =
                |conn: &mut rustls::ServerConnection, body: &mut Vec<u8>, plain: &mut [u8]| {
                    while let Ok(n @ 1..) = conn.reader().read(plain) {
                        body.extend_from_slice(&plain[..n]);
                    }
                };
            take(&mut conn.lock().unwrap(), &mut body, &mut plain);
            while body.len() < length {
                let n = tcp.read(&mut buf).unwrap_or(0);
                if n == 0 {
                    break;
                }
                let mut conn = conn.lock().unwrap();
                let mut rest = &buf[..n];
                while !rest.is_empty() {
                    conn.read_tls(&mut rest).unwrap();
                    conn.process_new_packets().unwrap();
                    take(&mut conn, &mut body, &mut plain);
                }
            }
            body
        })
    };
    // What rustls has to send, written by this thread alone.
    let flush = |tcp: &mut std::net::TcpStream| {
        let mut out = Vec::new();
        {
            let mut conn = conn.lock().unwrap();
            while conn.wants_write() {
                conn.write_tls(&mut out).unwrap();
            }
        }
        tcp.write_all(&out)
    };
    let mut answer =
        format!("HTTP/1.1 200 OK\r\nContent-Length: {ANSWER}\r\nConnection: close\r\n\r\n")
            .into_bytes();
    answer.resize(answer.len() + ANSWER, b'a');
    for chunk in answer.chunks(16 * 1024) {
        conn.lock().unwrap().writer().write_all(chunk).unwrap();
        if flush(&mut tcp).is_err() {
            return;
        }
    }
    let body = reader.join().unwrap();
    let mut whole = head.into_bytes();
    whole.extend(&body);
    let _ = sent
        .lock()
        .unwrap()
        .send(String::from_utf8_lossy(&whole).into_owned());
    conn.lock().unwrap().send_close_notify();
    let _ = flush(&mut tcp);
}

fn count(text: &str, header: &str) -> usize {
    text.to_lowercase()
        .matches(&format!("\r\n{}:", header.to_lowercase()))
        .count()
}

#[test]
fn the_stand_in_is_replaced_and_anything_else_goes_as_it_was_sent() {
    let host = Host::start("a", "tok", json!({}));
    // Presented as the header's token: the host gets the real one, and the rest as it was.
    let request = format!(
        "GET /app/rest/server HTTP/1.1\r\nHost: upstream.test\r\nAuthorization: Bearer {STANDIN}\r\nAccept: */*\r\n\r\n"
    );
    let answer = host.send(request.as_bytes());
    let sent = host.received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(answer.ends_with("hello"), "{answer}");
    assert!(
        sent.starts_with("GET /app/rest/server HTTP/1.1\r\n"),
        "{sent}"
    );
    assert!(sent.contains(&format!("Authorization: Bearer {REAL}")) && !sent.contains(STANDIN));
    assert!(sent.contains("Accept: */*"), "{sent}");

    // Not presented: nothing is attached, over TLS all the same.
    let sent =
        host.sent_for(b"GET /releases/download/x.zip HTTP/1.1\r\nHost: upstream.test\r\n\r\n");
    assert_eq!(count(&sent, "authorization"), 0, "{sent}");

    // A token of the bottle's own goes as it was sent.
    for own in [
        "Bearer made-in-the-bottle".to_string(),
        format!("Basic {}", base64(b"me:my-password")),
    ] {
        let sent =
            host.sent_for(format!("GET / HTTP/1.1\r\nAuthorization: {own}\r\n\r\n").as_bytes());
        assert!(
            sent.contains(&format!("Authorization: {own}")) && !sent.contains(REAL),
            "{sent}"
        );
    }

    // Basic credentials with the stand-in as their password (git's, from its credential helper)
    // get the real one as theirs.
    let basic = base64(format!("x-access-token:{STANDIN}").as_bytes());
    let sent =
        host.sent_for(format!("GET / HTTP/1.1\r\nAuthorization: Basic {basic}\r\n\r\n").as_bytes());
    let real = base64(format!("x-access-token:{REAL}").as_bytes());
    assert!(
        sent.contains(&format!("Authorization: Basic {real}")),
        "{sent}"
    );
    assert_eq!(count(&sent, "authorization"), 1);

    // The header is found whatever its case, and with the credential, nothing else the bottle
    // sent under its name leaves.
    let sent = host.sent_for(
        format!("GET / HTTP/1.1\r\nauthorization: bearer {STANDIN}\r\nAUTHORIZATION: Bearer guessed\r\n\r\n")
            .as_bytes(),
    );
    assert!(
        sent.contains(&format!("Authorization: Bearer {REAL}")),
        "{sent}"
    );
    assert_eq!(count(&sent, "authorization"), 1, "{sent}");
    assert!(!sent.contains("guessed"));

    // The log says what was attached and what the host answered, and nothing of the credential or
    // the query.
    host.sent_for(
        format!("GET /v1/models?key=signed HTTP/1.1\r\nAuthorization: Bearer {STANDIN}\r\n\r\n")
            .as_bytes(),
    );
    host.sent_for(b"GET /v1/public HTTP/1.1\r\n\r\n");
    let log = host.log();
    let port = host.port;
    assert!(
        log.contains("b GET https://upstream.test/v1/models credential 200"),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "b 93.184.216.34:{port} (upstream.test) +credential -> 127.0.0.1 ok 200"
        )),
        "{log}"
    );
    assert!(
        log.contains("b GET https://upstream.test/v1/public passed 200"),
        "{log}"
    );
    assert!(!log.contains(REAL) && !log.contains("signed"), "{log}");
}

#[test]
fn what_would_send_it_elsewhere_or_echo_it_never_leaves() {
    let host = Host::start(
        "b",
        "tok",
        json!({"hosts": ["localhost", "nowhere.invalid", "mismatched.test"]}),
    );
    for auth in ["", &format!("Authorization: Bearer {STANDIN}\r\n")] {
        // The Host header is the host's, and headers that would reroute the request are dropped.
        let sent = host.sent_for(
            format!(
                "GET / HTTP/1.1\r\nHost: elsewhere.test\r\nX-Forwarded-Host: elsewhere.test\r\n{auth}\
                 forwarded: host=elsewhere.test\r\nX-Original-URL: /admin\r\nX-HTTP-Method-Override: DELETE\r\n\
                 X-Real-IP: 10.0.0.1\r\nX-GitHub-Api-Version: 2022-11-28\r\n\r\n"
            )
            .as_bytes(),
        );
        assert!(
            sent.contains(&format!("Host: upstream.test:{}", host.port)),
            "{sent}"
        );
        for gone in ["elsewhere.test", "/admin", "DELETE", "10.0.0.1"] {
            assert!(!sent.contains(gone), "{gone}: {sent}");
        }
        assert!(sent.contains("X-GitHub-Api-Version: 2022-11-28"), "{sent}");
    }
    // A request for another host inside the TLS.
    let answer = host.send(b"GET https://elsewhere.test/ HTTP/1.1\r\n\r\n");
    assert!(answer.starts_with("HTTP/1.1 400 "), "{answer}");
    // A request the host would answer with what it was sent, credential and all.
    for method in ["TRACE", "trace", "TRACK"] {
        let answer = host.send(format!("{method} / HTTP/1.1\r\n\r\n").as_bytes());
        assert!(answer.starts_with("HTTP/1.1 405 "), "{method}: {answer}");
    }
    assert!(host.nothing_sent());
    // A host at this machine's address, one that isn't anywhere, and one whose certificate isn't
    // its own.
    for (name, status) in [
        ("localhost", "403"),
        ("nowhere.invalid", "502"),
        ("mismatched.test", "502"),
    ] {
        let answer = host.send_to(name, 443, true, b"GET / HTTP/1.1\r\n\r\n");
        assert!(
            answer.starts_with(&format!("HTTP/1.1 {status} ")),
            "{name}: {answer}"
        );
    }
    // Plain HTTP to its host and port isn't the credential's: a Host header doesn't claim one.
    assert_eq!(
        host.send_to("upstream.test", host.port, false, b"GET / HTTP/1.1\r\n\r\n"),
        ""
    );
    assert!(host.nothing_sent());
}

#[test]
fn a_forbidden_route_never_leaves_and_an_allowed_one_alone_gets_the_credential() {
    let host = Host::start(
        "c",
        "develocity",
        json!({"forbid": ["POST /api/auth/token"], "allow": ["GET /api/**"]}),
    );
    // However the path is written, with the credential or without.
    for target in [
        "/api/auth/token?expiresInHours=4",
        "/API/Auth/Token",
        "/api//auth/token/",
        "/api/auth/%74oken",
        "/api/auth/%2574oken",
        "/api/x/../auth/token",
        "/api/auth/token;jsessionid=1",
        "/api/auth/token.json",
        "/api%2Fauth%2Ftoken",
        "/api\\auth/token",
    ] {
        for auth in ["", &format!("Authorization: Bearer {STANDIN}\r\n")] {
            let answer = host.send(
                format!("POST {target} HTTP/1.1\r\n{auth}Content-Length: 0\r\n\r\n").as_bytes(),
            );
            assert!(answer.starts_with("HTTP/1.1 403 "), "{target}: {answer}");
        }
    }
    // A path no server should be asked for.
    for target in ["/api/%00", "/%25252525252574"] {
        let answer = host.send(format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes());
        assert!(answer.starts_with("HTTP/1.1 400 "), "{target}: {answer}");
    }
    assert!(host.nothing_sent());
    // Its other methods, and its neighbours, are allowed if allow says so.
    let sent = host.sent_for(
        format!("GET /api/auth/token HTTP/1.1\r\nAuthorization: Bearer {STANDIN}\r\n\r\n")
            .as_bytes(),
    );
    assert!(sent.contains(&format!("Bearer {REAL}")), "{sent}");

    // A refusal tells whoever reads it to ask the user, not to work around it.
    let answer = host.send(
        format!("POST /api/auth/token HTTP/1.1\r\nAuthorization: Bearer {STANDIN}\r\n\r\n")
            .as_bytes(),
    );
    assert!(
        answer.starts_with("HTTP/1.1 403 Forbidden\r\nContent-Type: text/plain\r\n"),
        "{answer}"
    );
    assert!(
        answer.contains("the develocity credential's feature forbids this route"),
        "{answer}"
    );
    assert!(answer.contains("don't retry the request"), "{answer}");
    // Presented on a route allow doesn't have, it's refused, as JSON to an API client: gh shows
    // `message`, Anthropic's clients `error.message`.
    let answer = host.send(
        format!(
            "POST /api/builds HTTP/1.1\r\nAccept: application/vnd.github+json\r\nAuthorization: Bearer {STANDIN}\r\nContent-Length: 0\r\n\r\n"
        )
        .as_bytes(),
    );
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    assert!(
        head.starts_with("HTTP/1.1 403 ") && head.contains("Content-Type: application/json"),
        "{head}"
    );
    let error: Value = serde_json::from_str(body).unwrap();
    assert_eq!(error["message"], error["error"]["message"]);
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("bottle refused POST /api/builds on upstream.test: the develocity credential goes only on the routes its feature allows"), "{message}");
    assert!(message.contains("tell the user"), "{message}");
    assert!(host.nothing_sent());
    // Not presented, a request goes as it would from the machine.
    let sent = host.sent_for(b"GET /releases/x.zip HTTP/1.1\r\n\r\n");
    assert_eq!(count(&sent, "authorization"), 0, "{sent}");

    let log = host.log();
    assert!(
        log.contains("POST https://upstream.test/api/auth/token forbidden 403"),
        "{log}"
    );
    assert!(log.contains("(upstream.test) -> - forbidden "), "{log}");
    assert!(!log.contains(REAL), "{log}");
}

#[test]
fn the_stand_in_is_replaced_where_a_rewrite_says_and_in_kind() {
    let host = Host::start(
        "d",
        "develocity",
        json!({"allow": ["GET /api/**"], "rewrite": [
            {"route": "POST /scans/publish/*/*/token", "json": "accessKey"},
            {"route": "GET /distribution-broker/**", "header": "X-User-Access-Key", "encoding": "base64"}]}),
    );
    let post = |path: &str, body: &str| {
        format!(
            "POST {path} HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    // In a JSON body's field: that field alone, exactly the stand-in.
    let body =
        format!(r#"{{"accessKey": "{STANDIN}", "buildToolVersion": "9.1", "note": "{STANDIN}"}}"#);
    let sent = host.sent_for(post("/scans/publish/gradle/4.6.1/token", &body).as_bytes());
    let (head, sent_body) = sent.split_once("\r\n\r\n").unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(sent_body).unwrap(),
        json!({"accessKey": REAL, "buildToolVersion": "9.1", "note": STANDIN})
    );
    assert!(
        head.contains(&format!("Content-Length: {}", sent_body.len())),
        "{head}"
    );
    assert_eq!(count(&sent, "content-length"), 1);
    for body in [
        format!(r#"{{"accessKey": "{STANDIN}-and-more"}}"#),
        format!(r#"{{"other": {{"accessKey": "{STANDIN}"}}}}"#),
        format!(r#"["{STANDIN}"]"#),
        format!(r#"{{"accessKey": "{STANDIN}""#),
    ] {
        let sent = host.sent_for(post("/scans/publish/gradle/4.6.1/token", &body).as_bytes());
        assert!(
            sent.ends_with(&format!("\r\n\r\n{body}")) && !sent.contains(REAL),
            "{sent}"
        );
    }
    // Another route's body isn't read for it.
    let body = format!(r#"{{"accessKey": "{STANDIN}"}}"#);
    let sent = host.sent_for(post("/scans/publish/gradle/4.6.1/upload", &body).as_bytes());
    assert!(sent.ends_with(&body), "{sent}");
    // A body too big to read.
    let answer = host.send(
        b"POST /scans/publish/gradle/4.6.1/token HTTP/1.1\r\nContent-Length: 1048577\r\n\r\n",
    );
    assert!(answer.starts_with("HTTP/1.1 413 "), "{answer}");

    // A chunked body the bottle waits to be told to send is read, and sent whole.
    let mut s = host.serve.connect(
        "credentials",
        CONNECTED_TO,
        host.port,
        Some("upstream.test"),
        true,
        "m1",
    );
    s.write_all(b"POST /scans/publish/maven/1.0/token HTTP/1.1\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n")
        .unwrap();
    let mut continued = [0; 25];
    s.read_exact(&mut continued).unwrap();
    assert_eq!(&continued, b"HTTP/1.1 100 Continue\r\n\r\n");
    let (first, rest) = body.split_at(16);
    write!(
        s,
        "{:x}\r\n{first}\r\n{:x}\r\n{rest}\r\n0\r\n\r\n",
        first.len(),
        rest.len()
    )
    .unwrap();
    let sent = host.received.recv_timeout(Duration::from_secs(10)).unwrap();
    let (head, sent_body) = sent.split_once("\r\n\r\n").unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(sent_body).unwrap(),
        json!({"accessKey": REAL})
    );
    assert!(
        !head.to_lowercase().contains("chunked") && !head.to_lowercase().contains("expect"),
        "{head}"
    );
    drop(s);

    // In a header of the tool's own, base64, padded or not: replaced in kind.
    let unpadded = |b: &[u8]| base64(b).trim_end_matches('=').to_string();
    for presented in [unpadded(STANDIN.as_bytes()), base64(STANDIN.as_bytes())] {
        let sent = host.sent_for(
            format!("GET /distribution-broker/build/v2/?buildInvocationId=1 HTTP/1.1\r\nX-User-Access-Key: {presented}\r\n\r\n")
                .as_bytes(),
        );
        assert!(
            sent.contains(&format!(
                "X-User-Access-Key: {}\r\n",
                unpadded(REAL.as_bytes())
            )),
            "{sent}"
        );
        assert_eq!(count(&sent, "x-user-access-key"), 1);
    }
    let other = base64(b"made-in-the-bottle");
    let sent = host.sent_for(
        format!(
            "GET /distribution-broker/build/v2/ HTTP/1.1\r\nX-User-Access-Key: {other}\r\n\r\n"
        )
        .as_bytes(),
    );
    assert!(
        sent.contains(&format!("X-User-Access-Key: {other}")),
        "{sent}"
    );
}

#[test]
fn a_body_bigger_than_what_tls_keeps_unsent_goes_whole() {
    let host = Host::start(
        "f",
        "develocity",
        json!({"rewrite": [{"route": "POST /scans/publish/*/*/token", "json": "accessKey"}]}),
    );
    let post = |path: &str, body: &str| {
        format!(
            "POST {path} HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    // Relayed as the bottle sends it, many times over what rustls keeps unsent (64 KiB), to a
    // host slow to take it: a Claude conversation's request, say.
    let body = "x".repeat(4 << 20);
    let answer = host.send(post("/v1/messages", &body).as_bytes());
    let sent = host.received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(answer.ends_with("hello"), "{answer}");
    let (_, sent_body) = sent.split_once("\r\n\r\n").unwrap();
    assert!(
        sent_body == body,
        "{} of {} bytes",
        sent_body.len(),
        body.len()
    );

    // Read whole for the credential, and sent whole after the head.
    let body = json!({"accessKey": STANDIN, "pad": "x".repeat(900 * 1024)}).to_string();
    let sent = host.sent_for(post("/scans/publish/gradle/4.6.1/token", &body).as_bytes());
    let (_, sent_body) = sent.split_once("\r\n\r\n").unwrap();
    let sent_body: Value = serde_json::from_str(sent_body).unwrap();
    assert_eq!(sent_body["accessKey"], REAL);
    assert_eq!(sent_body["pad"].as_str().unwrap().len(), 900 * 1024);
}

#[test]
fn a_websocket_upgrade_keeps_its_connection() {
    let host = Host::start("e", "tok", json!({}));
    let mut s = host.serve.connect(
        "credentials",
        CONNECTED_TO,
        host.port,
        Some("upstream.test"),
        true,
        "m1",
    );
    s.write_all(
        format!(
            "GET /socket HTTP/1.1\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nAuthorization: Bearer {STANDIN}\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();
    let sent = host.received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        sent.contains("\r\nConnection: Upgrade\r\n") && sent.contains("\r\nUpgrade: websocket\r\n"),
        "{sent}"
    );
    assert!(!sent.contains("close") && sent.contains(REAL), "{sent}");
    let mut reader = BufReader::new(s.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.starts_with("HTTP/1.1 101"), "{line}");
    while line != "\r\n" {
        line.clear();
        reader.read_line(&mut line).unwrap();
    }
    // Both ways, after.
    s.write_all(b"ping").unwrap();
    let mut echoed = [0; 4];
    reader.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"ping");
    drop((s, reader));
    // Anything else, the connection closes after it.
    let sent = host.sent_for(b"GET / HTTP/1.1\r\nConnection: keep-alive\r\n\r\n");
    assert!(sent.contains("\r\nConnection: close\r\n"), "{sent}");
}

#[test]
fn an_answer_that_comes_while_the_request_does_goes_both_ways() {
    let host = Host::start("g", "tok", json!({}));
    let mut s = host.serve.connect(
        "credentials",
        CONNECTED_TO,
        host.port,
        Some("upstream.test"),
        true,
        "m1",
    );
    // The bottle sends the whole request before it reads any of the answer, which the host starts
    // on with the head: folio has to keep taking the request while the bottle has no room for the
    // answer, as the host would, directly.
    let body = vec![b'b'; 8 << 20];
    let head = format!(
        "POST /duplex HTTP/1.1\r\nAuthorization: Bearer {STANDIN}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.set_write_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(head.as_bytes()).unwrap();
    if let Err(e) = s.write_all(&body) {
        panic!("the request stopped going while its answer went unread: {e}");
    }
    let mut answer = Vec::new();
    s.read_to_end(&mut answer).unwrap();
    let head_end = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(
        answer.starts_with(b"HTTP/1.1 200"),
        "{:?}",
        &answer[..head_end]
    );
    assert_eq!(answer.len() - head_end, ANSWER);
    let sent = host.received.recv_timeout(Duration::from_secs(10)).unwrap();
    let (sent_head, sent_body) = sent.split_once("\r\n\r\n").unwrap();
    assert!(sent_head.contains(REAL), "{sent_head}");
    assert_eq!(sent_body.len(), body.len());
}
