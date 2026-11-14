//! What folio-cli serve's tests share: folio-cli serve started as folio starts it, and driven as
//! bottle-cli and folio drive it.
#![allow(dead_code)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

pub const STANDIN: &str = "the-egress-proxy-holds-the-real-one";

pub struct Serve {
    pub dir: PathBuf,
    child: Child,
}

impl Serve {
    pub fn start(tag: &str, env: &[(&str, String)], restore: Value) -> Serve {
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

    pub fn eventually(&self, what: &str, mut done: impl FnMut() -> bool) {
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

    pub fn log(&self) -> String {
        fs::read_to_string(self.dir.join("serve.log")).unwrap_or_default()
    }

    /// Its answer to `message` on its control socket; None if it took none.
    pub fn control(&self, message: Value) -> Option<Value> {
        let mut s = UnixStream::connect(self.dir.join("serve.sock")).ok()?;
        s.write_all(format!("{message}\n").as_bytes()).ok()?;
        let mut line = String::new();
        BufReader::new(s).read_line(&mut line).ok()?;
        serde_json::from_str(&line).ok()
    }

    pub fn ok(&self, message: Value) {
        let answer = self.control(message.clone()).unwrap();
        assert_eq!(
            answer["ok"],
            true,
            "{message}: {answer}; its log:\n{}",
            self.log()
        );
    }

    /// A connection `machine`'s guest opened to `address`:`port`, as bottle-cli gives it here, to
    /// carry on.
    pub fn connect(
        &self,
        service: &str,
        address: [u8; 4],
        port: u16,
        name: Option<&str>,
        tls: bool,
        machine: &str,
    ) -> UnixStream {
        let mut s = UnixStream::connect(self.dir.join(format!("s/{service}.sock"))).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        s.write_all(&header(address, port, name, tls, machine))
            .unwrap();
        s
    }

    /// A connection `machine`'s guest opened to `address`:`port`, as bottle-cli gives it here; what
    /// came back.
    #[allow(clippy::too_many_arguments)]
    pub fn flow(
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
pub fn header(
    address: [u8; 4],
    port: u16,
    name: Option<&str>,
    tls: bool,
    machine: &str,
) -> Vec<u8> {
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

pub fn bind(bottle: &str, machine: &str, extra: Value) -> Value {
    let mut message = json!({"op": "bind", "bottle": bottle, "machine": machine, "runner": "local", "id": "1",
                             "features": [], "repos": [], "permissions": [], "labels": {}, "ssh": []});
    for (k, v) in extra.as_object().unwrap() {
        message[k] = v.clone();
    }
    message
}
