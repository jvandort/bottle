//! HTTP/1.1 as the cache reads and writes it: heads, bodies by their framing, and what's passed on each way.

use std::collections::HashMap;
use std::io::{self, BufRead, Read, Write};

use super::{HOP_HEADERS, MAX_HEADER_BYTES, ROUTING_HEADERS};

/// Why a request couldn't be served: what to answer (status, reason), if anything can be.
pub(super) struct Refused(pub(super) Option<(u16, &'static str)>, pub(super) String);

pub(super) fn bad_request(why: &str) -> Refused {
    Refused(Some((400, "Bad Request")), why.into())
}

pub(super) fn bad_gateway(why: impl Into<String>) -> Refused {
    Refused(Some((502, "Bad Gateway")), why.into())
}

/// A request's or a response's head: its first line's three parts and its header lines.
pub(super) struct Head {
    pub(super) method: String,
    pub(super) target: String,
    pub(super) version: String,
    pub(super) lines: Vec<String>,
}

impl Head {
    pub(super) fn headers(&self) -> HashMap<String, String> {
        parsed(&self.lines)
    }
}

/// Headers by lowercase name; one sent on several lines is one, its values joined with commas.
pub(super) fn parsed(lines: &[String]) -> HashMap<String, String> {
    let mut headers: HashMap<String, String> = HashMap::new();
    for line in lines {
        let (name, value) = line.split_once(':').unwrap_or((line, ""));
        let (name, value) = (name.trim().to_lowercase(), value.trim().to_string());
        headers
            .entry(name)
            .and_modify(|v| *v = format!("{v}, {value}"))
            .or_insert(value);
    }
    headers
}

/// A comma-separated header value's tokens, lowercase: Connection's, or Cache-Control's.
pub(super) fn tokens(value: Option<&String>) -> Vec<String> {
    value.map_or(vec![], |v| {
        v.split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect()
    })
}

/// The next head on `r`: None if it ends first, refused if it isn't HTTP or is too long.
pub(super) fn read_head(r: &mut impl BufRead) -> Result<Option<Head>, Refused> {
    let mut lines = Vec::new();
    let mut size = 0;
    loop {
        let mut line = Vec::new();
        let n = r
            .take((MAX_HEADER_BYTES - size + 1) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|e| Refused(None, format!("reading a request: {e}")))?;
        if n == 0 {
            return if lines.is_empty() {
                Ok(None)
            } else {
                Err(Refused(None, "ended mid-head".into()))
            };
        }
        size += n;
        if size > MAX_HEADER_BYTES {
            return Err(Refused(
                Some((431, "Request Header Fields Too Large")),
                "too long".into(),
            ));
        }
        // Latin-1, as HTTP has it: a byte, a character.
        let text: String = line.iter().map(|&b| b as char).collect();
        let text = text.trim_end_matches(['\r', '\n']).to_string();
        if text.is_empty() {
            // Empty lines before a head are ignored, as HTTP has a server do: some clients end a
            // body with an extra CRLF.
            if lines.is_empty() {
                continue;
            }
            break;
        }
        lines.push(text);
    }
    let first = lines.remove(0);
    let parts: Vec<&str> = first.splitn(3, ' ').collect();
    if parts.len() != 3 {
        return Err(bad_request("not an HTTP head"));
    }
    Ok(Some(Head {
        method: parts[0].into(),
        target: parts[1].into(),
        version: parts[2].into(),
        lines,
    }))
}

/// How a body ends: its length, chunked framing, or the connection's end.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Length {
    Known(u64),
    Chunked,
    Close,
}

pub(super) struct Response {
    pub(super) status: u16,
    pub(super) reason: String,
    pub(super) lines: Vec<String>,
    pub(super) headers: HashMap<String, String>,
}

/// The head of a server's final response: interim ones (100 Continue) are skipped.
pub(super) fn read_response(r: &mut impl BufRead) -> Result<Response, Refused> {
    loop {
        let head = read_head(r)
            .map_err(|_| bad_gateway("no response from the server"))?
            .ok_or_else(|| bad_gateway("no response from the server"))?;
        // A status line: HTTP/1.x, a three-digit status, a reason.
        let status: u16 = head
            .target
            .parse()
            .ok()
            .filter(|_| head.method.starts_with("HTTP/1.") && head.target.len() == 3)
            .ok_or_else(|| bad_gateway("the server's response isn't HTTP"))?;
        if (100..200).contains(&status) && status != 101 {
            continue;
        }
        let headers = head.headers();
        return Ok(Response {
            status,
            reason: head.version,
            lines: head.lines,
            headers,
        });
    }
}

pub(super) fn body_length(method: &str, response: &Response) -> Result<Length, Refused> {
    if method == "HEAD" || [101, 204, 304].contains(&response.status) {
        return Ok(Length::Known(0));
    }
    if tokens(response.headers.get("transfer-encoding"))
        .iter()
        .any(|t| t == "chunked")
    {
        return Ok(Length::Chunked);
    }
    match response.headers.get("content-length") {
        None => Ok(Length::Close),
        Some(n) => n
            .parse()
            .map(Length::Known)
            .map_err(|_| bad_gateway("the server's response has no length to read")),
    }
}

/// A body, as it arrives, without its framing, each piece to `each`; an error if it ends early.
pub(super) fn body(
    r: &mut impl BufRead,
    length: Length,
    mut each: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let early = || io::Error::other("the server's response ended early");
    let exactly =
        |r: &mut dyn BufRead, mut n: u64, each: &mut dyn FnMut(&[u8]) -> io::Result<()>| {
            let mut buf = vec![0u8; 64 * 1024];
            while n > 0 {
                let want = n.min(buf.len() as u64) as usize;
                let got = r.read(&mut buf[..want])?;
                if got == 0 {
                    return Err(early());
                }
                each(&buf[..got])?;
                n -= got as u64;
            }
            Ok(())
        };
    match length {
        Length::Known(n) => exactly(r, n, &mut each),
        Length::Close => {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let got = r.read(&mut buf)?;
                if got == 0 {
                    return Ok(());
                }
                each(&buf[..got])?;
            }
        }
        Length::Chunked => loop {
            let mut line = String::new();
            if r.read_line(&mut line)? == 0 {
                return Err(early());
            }
            let size = u64::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
                .map_err(|_| io::Error::other("a chunk's size isn't one"))?;
            if size == 0 {
                // Trailers, until the empty line.
                loop {
                    let mut trailer = String::new();
                    if r.read_line(&mut trailer)? == 0 || trailer.trim().is_empty() {
                        return Ok(());
                    }
                }
            }
            exactly(r, size, &mut each)?;
            let mut end = String::new();
            r.read_line(&mut end)?;
            if !end.trim().is_empty() {
                return Err(io::Error::other("a chunk longer than it said"));
            }
        },
    }
}

/// Sends the bottle a response with all its body (or none), framed here; whether the connection
/// can take another request.
pub(super) fn respond(
    w: &mut dyn Write,
    status: u16,
    reason: &str,
    lines: &[String],
    body: Option<&[u8]>,
    keep: bool,
) -> io::Result<bool> {
    let mut head = vec![format!("HTTP/1.1 {status} {reason}").trim_end().to_string()];
    head.extend(lines.iter().cloned());
    if let Some(body) = body {
        head.push(format!("Content-Length: {}", body.len()));
    }
    head.push(
        if keep {
            "Connection: keep-alive"
        } else {
            "Connection: close"
        }
        .into(),
    );
    w.write_all((head.join("\r\n") + "\r\n\r\n").as_bytes())?;
    if let Some(body) = body {
        w.write_all(body)?;
    }
    w.flush()?;
    Ok(keep)
}

/// A server's header lines for the bottle, without the framing set here: a response with no body
/// (to HEAD, a 304) keeps its Content-Length, which is the length of the body it's about. No
/// cookies: one the bottle sent back would keep its requests from being answered from the store.
pub(super) fn to_client(lines: &[String], bodiless: bool) -> Vec<String> {
    lines
        .iter()
        .filter(|line| {
            let name = line.split(':').next().unwrap_or("").trim().to_lowercase();
            !HOP_HEADERS.contains(&name.as_str())
                && name != "transfer-encoding"
                && (bodiless || name != "content-length")
                && name != "set-cookie"
                && name != "set-cookie2"
        })
        .cloned()
        .collect()
}

/// A bottle's header lines for a request to `name`, which it gets as its Host, without the
/// routing and hop-by-hop ones.
pub(super) fn to_server(name: &str, lines: &[String]) -> Vec<String> {
    let mut sent = vec![format!("Host: {name}")];
    sent.extend(
        lines
            .iter()
            .filter(|line| {
                let header = line.split(':').next().unwrap_or("").trim().to_lowercase();
                header != "host"
                    && !HOP_HEADERS.contains(&header.as_str())
                    && !ROUTING_HEADERS.contains(&header.as_str())
            })
            .cloned(),
    );
    sent
}

pub(super) fn upstream_head(method: &str, path_and_query: &str, lines: &[String]) -> Vec<u8> {
    // One request per connection: the host closes when it's done.
    let mut head = vec![format!("{method} {path_and_query} HTTP/1.1")];
    head.extend(lines.iter().cloned());
    head.push("Connection: close".into());
    (head.join("\r\n") + "\r\n\r\n").into_bytes()
}

/// A request target's scheme, authority and path with its query: origin-form has only the last.
pub(super) fn split_target(target: &str) -> (Option<&str>, Option<&str>, &str) {
    match target.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, path) = rest
                .find('/')
                .map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
            (Some(scheme), Some(authority), path)
        }
        None => (None, None, target),
    }
}

/// An authority's host, lowercase, without its port.
pub(super) fn host_of(authority: &str) -> String {
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host.to_lowercase()
}

pub(super) fn split_target_with(scheme: &str, rest: &str) -> (String, String, String) {
    let (authority, path) = rest
        .find('/')
        .map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    (scheme.into(), authority.into(), path.into())
}
