//! The messages service: messages between bottles, a POST to http://messages.bottle.internal/ of
//! {"to": ..., "text": ...}, and the addresses that say who may write to whom, without a table.
//!
//! A bottle sends a message to `to`: a bottle's name, or an address, `<name>~<signature>` (names
//! can't hold a `~`). An address is issued to one bottle, the holder, for reaching another, the
//! target: its signature is an HMAC, under a key that never leaves the controller (messages.key in
//! folio's home), of both their names. folio serve hands one out as the `from` of every message it
//! delivers (the sender's address, issued to the recipient), so a bottle can answer whoever wrote
//! to it, and no one else. A bare name is for bottles with the message-any permission, which start
//! conversations. Deleting the key revokes every address at once.
//!
//! A message is delivered by `bottle-event send` in the recipient (the claude feature's channel),
//! which queues it and waits until its session takes it, or says none did in time (exit 75).

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use serde_json::Value;

use super::http;
use super::service::{Bound, Caller, Flow, Service};
use super::{Bottle, exec_command};
use crate::log::log;

const MARK: char = '~';
const KEY_BYTES: usize = 32;
/// 26 base32 characters: 130 bits of the HMAC, well past guessing.
const SIGNATURE_CHARS: usize = 26;
/// How long a message waits for its bottle's session to take it, and how much longer the exec that
/// waits is given.
const DELIVERY_TIMEOUT: u64 = 10;
const DELIVERY_GRACE: u64 = 15;
const BOTTLE_EVENT: &str = "/usr/local/bin/bottle-event";
/// What bottle-event exits when no session took the event in time (EX_TEMPFAIL).
const NOT_TAKEN: i32 = 75;

/// The most of a message.
const MAX_MESSAGE: usize = 1024 * 1024;

/// The messages service.
pub struct Messages;

impl Service for Messages {
    fn serve(&self, caller: &Caller, stream: UnixStream) -> Flow {
        // Read past the most a message is, to say so of one a little over it.
        let (head, body, mut writer) = match http::read_request(stream, 2 * MAX_MESSAGE) {
            Ok(request) => request,
            Err(outcome) => return Flow::new("messages", outcome),
        };
        if head.method != "POST" || head.path_and_query().0 != "/" {
            http::respond(
                &mut writer,
                405,
                "Method Not Allowed",
                b"POST a message to /: {\"to\": ..., \"text\": ...}\n",
                "text/plain",
            );
            return Flow::new("messages", "bad-message");
        }
        let message = match parse_message(&body) {
            Ok(message) => message,
            Err(problem) => {
                http::respond(
                    &mut writer,
                    400,
                    "Bad Request",
                    format!("{problem}\n").as_bytes(),
                    "text/plain",
                );
                return Flow::new("messages", "bad-message");
            }
        };
        match deliver(caller.bound, caller.bottle, &message.to, &message.text) {
            Ok(()) => {
                http::respond(&mut writer, 200, "OK", b"delivered\n", "text/plain");
                Flow::new("messages", "message")
            }
            Err(refused) => {
                http::respond(
                    &mut writer,
                    refused.0,
                    refused.1,
                    format!("{}\n", refused.2).as_bytes(),
                    "text/plain",
                );
                Flow::new("messages", "undelivered")
            }
        }
    }
}

/// A message a bottle sends: to a destination, its text.
struct Message {
    to: String,
    text: String,
}

/// The message a body is, {"to": a destination, "text": the message}; else what's wrong with it.
fn parse_message(body: &[u8]) -> Result<Message, String> {
    if body.len() > MAX_MESSAGE {
        return Err(format!("a message is at most {MAX_MESSAGE} bytes"));
    }
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return Err("a message is JSON: {\"to\": ..., \"text\": ...}".into());
    };
    let Some(fields) = message.as_object() else {
        return Err("a message is a JSON object: {\"to\": ..., \"text\": ...}".into());
    };
    let extra: Vec<&str> = fields
        .keys()
        .map(String::as_str)
        .filter(|k| !["to", "text"].contains(k))
        .collect();
    if !extra.is_empty() {
        return Err(format!(
            "a message has only to and text, not {}",
            extra.join(", ")
        ));
    }
    let Some(to) = message["to"].as_str().filter(|t| !t.trim().is_empty()) else {
        return Err("a message needs to: the address or bottle it's for".into());
    };
    let Some(text) = message["text"].as_str() else {
        return Err("a message needs text".into());
    };
    Ok(Message {
        to: to.into(),
        text: text.into(),
    })
}

/// Why a message wasn't delivered: an HTTP status, its reason, and what the sender is told.
pub type Undelivered = (u16, &'static str, String);

/// The key, made the first time it's needed; only its owner can read it.
fn key() -> std::io::Result<Vec<u8>> {
    key_at(&crate::home().join("messages.key"))
}

fn key_at(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    if let Ok(key) = fs::read(path) {
        return Ok(key);
    }
    fs::create_dir_all(path.parent().unwrap())?;
    let mut fresh = [0u8; KEY_BYTES];
    fs::File::open("/dev/urandom")?.read_exact(&mut fresh)?;
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(mut file) => file.write_all(&fresh)?,
        // Made by someone else since.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    fs::read(path)
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| block.map(|b| b ^ byte);
    let inner = Sha256::new()
        .chain_update(pad(0x36))
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(pad(0x5c))
        .chain_update(inner)
        .finalize()
        .into()
}

/// RFC 4648 base32, lowercase, unpadded.
fn base32(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let (mut out, mut buffer, mut bits) = (String::new(), 0u32, 0);
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            out.push(ALPHABET[((buffer >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn signature(key: &[u8], target: &str, holder: &str) -> String {
    let digest = hmac_sha256(key, format!("{target}\0{holder}").as_bytes());
    base32(&digest)[..SIGNATURE_CHARS].to_string()
}

/// The address by which `holder` reaches `target`.
pub fn address(target: &str, holder: &str) -> std::io::Result<String> {
    Ok(format!(
        "{target}{MARK}{}",
        signature(&key()?, target, holder)
    ))
}

/// The bottle `to` reaches, when `holder` sends to it; None if it isn't an address issued to
/// `holder`.
fn target(to: &str, holder: &str) -> Option<String> {
    target_by(&key().ok()?, to, holder)
}

fn target_by(key: &[u8], to: &str, holder: &str) -> Option<String> {
    let (name, given) = to.rsplit_once(MARK)?;
    let expected = signature(key, name, holder);
    // In constant time: a signature's prefix right says nothing.
    let same = given.len() == expected.len()
        && given
            .bytes()
            .zip(expected.bytes())
            .fold(0, |d, (a, b)| d | (a ^ b))
            == 0;
    (!name.is_empty() && same).then(|| name.to_string())
}

/// Hands `text` from `sender` to the bottle `to` reaches, as an event from `sender` (its address,
/// issued to the recipient, so it can answer): returns once the recipient's session has taken it.
pub fn deliver(
    bound: &dyn Bound,
    sender: &Bottle,
    to: &str,
    text: &str,
) -> Result<(), Undelivered> {
    let named = if to.contains(MARK) {
        target(to, &sender.name)
    } else {
        sender
            .permissions
            .iter()
            .any(|p| p == "message-any")
            .then(|| to.to_string())
    };
    if named.as_deref() == Some(sender.name.as_str()) {
        return Err((
            400,
            "Bad Request",
            "a bottle can't send a message to itself".into(),
        ));
    }
    // The same whether there's no such bottle or the sender may not reach it: a refusal gives
    // nothing away.
    let Some(recipient) = named.and_then(|n| bound.bound(&n)) else {
        return Err((
            404,
            "Not Found",
            format!("no running bottle you can reach as {to}"),
        ));
    };
    let reply_to = address(&sender.name, &recipient.name)
        .map_err(|e| (502, "Bad Gateway", format!("couldn't deliver to {to}: {e}")))?;
    let timeout = DELIVERY_TIMEOUT.to_string();
    let command = exec_command(
        &recipient,
        &[BOTTLE_EVENT, "send", "-t", &timeout, "-f", &reply_to],
        "genie",
    );
    let child = Command::new(&command[0])
        .args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child =
        child.map_err(|e| (502, "Bad Gateway", format!("couldn't deliver to {to}: {e}")))?;
    let _ = child.stdin.take().unwrap().write_all(text.as_bytes());
    // Read as it comes: one that says a lot would otherwise wait for it to be read, and never exit.
    let mut stderr = child.stderr.take().unwrap();
    let said = std::thread::spawn(move || {
        let mut said = String::new();
        let _ = stderr.read_to_string(&mut said);
        said
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None)
                if started.elapsed() > Duration::from_secs(DELIVERY_TIMEOUT + DELIVERY_GRACE) =>
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err((
                    504,
                    "Gateway Timeout",
                    format!("couldn't reach {to} to deliver the message"),
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err((502, "Bad Gateway", format!("couldn't deliver to {to}: {e}"))),
        }
    };
    if status.code() == Some(NOT_TAKEN) {
        return Err((
            504,
            "Gateway Timeout",
            format!("{to} has no session that took the message within {DELIVERY_TIMEOUT}s"),
        ));
    }
    if !status.success() {
        let said = said.join().unwrap_or_default();
        let why = if said.trim().is_empty() {
            format!("exit {}", status.code().unwrap_or(-1))
        } else {
            said.trim().to_string()
        };
        return Err((
            502,
            "Bad Gateway",
            format!("couldn't deliver to {to}: {why}"),
        ));
    }
    log!(
        "bottle.daemon",
        "{}: delivered a message from {}",
        recipient.name,
        sender.name
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_and_base32_as_their_rfcs_have_them() {
        // RFC 4231's test case 2.
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(base32(b"foobar"), "mzxw6ytboi");
    }

    #[test]
    fn an_address_reaches_its_target_from_its_holder_alone() {
        let key = b"k";
        let address = format!("broker{MARK}{}", signature(key, "broker", "foo"));
        assert_eq!(target_by(key, &address, "foo").as_deref(), Some("broker"));
        assert_eq!(target_by(key, &address, "bar"), None);
        assert_eq!(target_by(b"another key", &address, "foo"), None);
        let signature = address.rsplit_once(MARK).unwrap().1;
        assert_eq!(
            target_by(key, &format!("other{MARK}{signature}"), "foo"),
            None
        );
        assert_eq!(target_by(key, &format!("{MARK}{signature}"), "foo"), None);
        let mut changed = address.clone();
        let last = changed.pop().unwrap();
        changed.push(if last == 'a' { 'b' } else { 'a' });
        assert_eq!(target_by(key, &changed, "foo"), None);
        assert_eq!(target_by(key, "broker", "foo"), None);
    }

    #[test]
    fn the_key_is_made_once_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("msgkey-{}", std::process::id()));
        let path = dir.join("messages.key");
        let key = key_at(&path).unwrap();
        assert_eq!(key.len(), KEY_BYTES);
        assert_eq!(key_at(&path).unwrap(), key);
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(dir).unwrap();
    }
}
