//! HTTP/1.1, as much of it as folio serve reads and writes: a request's head and body, and a
//! response, one per connection.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// How long a request has to send its head (and a body read here).
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(30);
/// The most of a request's head read.
const MAX_HEAD: usize = 64 * 1024;

/// Why a request isn't served: the status to answer with, if any, and what the log says.
#[derive(Debug)]
pub struct Refused {
    pub status: Option<u16>,
    pub reason: String,
    pub outcome: &'static str,
    /// What the bottle is told, when it's more than the reason.
    pub body: Option<Vec<u8>>,
    pub content_type: &'static str,
}

impl Refused {
    pub fn new(status: u16, reason: &str, outcome: &'static str) -> Refused {
        Refused {
            status: Some(status),
            reason: reason.into(),
            outcome,
            body: None,
            content_type: "text/plain",
        }
    }

    pub fn bad_request() -> Refused {
        Refused::new(400, "Bad Request", "bad-request")
    }
}

/// A request's head: its method, target, HTTP version and header lines.
#[derive(Debug, Clone)]
pub struct Head {
    pub method: String,
    pub target: String,
    pub version: String,
    pub lines: Vec<String>,
}

impl Head {
    /// Headers by lowercase name; one sent on several lines is one, its values joined with commas.
    pub fn headers(&self) -> BTreeMap<String, String> {
        let mut parsed: BTreeMap<String, String> = BTreeMap::new();
        for line in &self.lines {
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            let (name, value) = (name.trim().to_lowercase(), value.trim().to_string());
            parsed
                .entry(name)
                .and_modify(|v| *v = format!("{v}, {value}"))
                .or_insert(value);
        }
        parsed
    }

    /// The target's path and query, split at the first `?`.
    pub fn path_and_query(&self) -> (&str, &str) {
        self.target.split_once('?').unwrap_or((&self.target, ""))
    }
}

/// A header line's lowercase name.
pub fn name_of(line: &str) -> String {
    line.split_once(':')
        .map_or(line, |(n, _)| n)
        .trim()
        .to_lowercase()
}

/// A comma-separated header value's tokens, lowercase.
pub fn tokens(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|t| t.trim().to_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// A request's head, up to its empty line.
pub fn read_head(reader: &mut impl BufRead) -> Result<Head, Refused> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        let read = reader
            .take((MAX_HEAD - head.len()) as u64 + 1)
            .read_until(b'\n', &mut head);
        match read {
            Ok(0) | Err(_) => return Err(Refused::bad_request()),
            Ok(_) => {}
        }
        if head.len() > MAX_HEAD {
            return Err(Refused::new(
                431,
                "Request Header Fields Too Large",
                "bad-request",
            ));
        }
        if head[before..] == *b"\r\n" || head[before..] == *b"\n" {
            break;
        }
    }
    let text: String = head.iter().map(|&b| b as char).collect(); // latin-1
    let mut lines: Vec<String> = text.split("\r\n").map(String::from).collect();
    lines.retain(|l| !l.is_empty());
    if lines.is_empty() {
        return Err(Refused::bad_request());
    }
    let request = lines.remove(0);
    let parts: Vec<&str> = request.split(' ').collect();
    if parts.len() != 3 || !parts[2].starts_with("HTTP/") {
        return Err(Refused::bad_request());
    }
    Ok(Head {
        method: parts[0].into(),
        target: parts[1].into(),
        version: parts[2].into(),
        lines,
    })
}

/// A request's body, by its Content-Length or chunked, `max` bytes at most; None if it's more, an
/// error if it isn't one.
pub fn read_body(
    reader: &mut impl BufRead,
    headers: &BTreeMap<String, String>,
    max: usize,
) -> io::Result<Option<Vec<u8>>> {
    let invalid = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_string());
    if let Some(coding) = headers.get("transfer-encoding") {
        if tokens(coding) != ["chunked"] {
            return Err(invalid("a transfer coding other than chunked"));
        }
        let mut body = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| invalid("not a chunk's size"))?;
            if size == 0 {
                break;
            }
            if body.len() + size > max {
                return Ok(None);
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..])?;
            let mut end = String::new();
            reader.read_line(&mut end)?;
            if !end.trim().is_empty() {
                return Err(invalid("a chunk longer than it said"));
            }
        }
        // Trailers, until the empty line.
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                break;
            }
        }
        return Ok(Some(body));
    }
    let length = headers.get("content-length").map_or("0", String::as_str);
    let length: usize = length
        .parse()
        .map_err(|_| invalid("a Content-Length that isn't one"))?;
    if length > max {
        return Ok(None);
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

/// A whole response: `status`, `reason`, and `body` as `content_type`; the connection closes after.
pub fn respond(
    writer: &mut impl Write,
    status: u16,
    reason: &str,
    body: &[u8],
    content_type: &str,
) {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = writer.write_all(head.as_bytes());
    let _ = writer.write_all(body);
    let _ = writer.flush();
}

/// What a refusal says: its body, or its reason; its status line's reason, up to a colon.
pub fn respond_refused(writer: &mut impl Write, refused: &Refused) {
    let Some(status) = refused.status else { return };
    let body = refused
        .body
        .clone()
        .unwrap_or_else(|| format!("{}\n", refused.reason).into_bytes());
    let reason = refused.reason.split(':').next().unwrap_or_default();
    respond(writer, status, reason, &body, refused.content_type);
}

/// A request read off `stream`, whole: its head, and its body (at most `max_body` bytes); and the
/// stream to answer it on. One that isn't is refused, answered so, and the log's outcome is the error.
pub fn read_request(
    stream: UnixStream,
    max_body: usize,
) -> Result<(Head, Vec<u8>, UnixStream), String> {
    let _ = stream.set_read_timeout(Some(HEADER_TIMEOUT));
    let Ok(read) = stream.try_clone() else {
        return Err("error".into());
    };
    let mut reader = BufReader::new(read);
    let mut writer = stream;
    let request = (|| -> Result<(Head, Vec<u8>), Refused> {
        let head = read_head(&mut reader)?;
        if !head.target.starts_with('/') {
            return Err(Refused::bad_request());
        }
        let body = read_body(&mut reader, &head.headers(), max_body)
            .map_err(|_| Refused::bad_request())?
            .ok_or_else(Refused::bad_request)?;
        Ok((head, body))
    })();
    match request {
        Ok((head, body)) => {
            let _ = writer.set_read_timeout(None);
            Ok((head, body, writer))
        }
        Err(refused) => {
            respond_refused(&mut writer, &refused);
            Err(refused.outcome.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_and_a_body_by_length_or_in_chunks() {
        let mut input: &[u8] =
            b"POST /x HTTP/1.1\r\nHost: git.bottle.internal\r\nX-A: 1\r\nx-a: 2\r\n\r\nhello";
        let head = read_head(&mut input).unwrap();
        assert_eq!(
            (head.method.as_str(), head.target.as_str()),
            ("POST", "/x")
        );
        let mut headers = head.headers();
        assert_eq!(headers["x-a"], "1, 2");
        headers.insert("content-length".into(), "5".into());
        assert_eq!(
            read_body(&mut input, &headers, 10).unwrap().unwrap(),
            b"hello"
        );
        let mut chunked: &[u8] = b"3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n";
        let headers = BTreeMap::from([("transfer-encoding".to_string(), "chunked".to_string())]);
        assert_eq!(
            read_body(&mut chunked, &headers, 10).unwrap().unwrap(),
            b"abcde"
        );
        assert!(read_head(&mut &b"nonsense\r\n\r\n"[..]).is_err());
    }
}
