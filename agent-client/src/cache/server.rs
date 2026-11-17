//! One connection's requests, answered from the store where they can be, else from the host (kept, where they may be).

use std::io::Write;

use crate::policy::CacheRule;

use std::io::{self, BufReader};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;

use rustls::pki_types::ServerName;
use rustls::{ClientConnection, StreamOwned};

use crate::policy::Cached;

use super::http::{
    Head, Length, Refused, Response, bad_gateway, bad_request, body, body_length, host_of,
    read_response, split_target, split_target_with, to_client, to_server, tokens, upstream_head,
};
use super::{CONNECT_TIMEOUT, Context, Duplex, IDLE, MAX_REDIRECTS, PERSONAL_HEADERS, REDIRECTS};

/// Whether the response to a GET can be kept: a whole 200 of a file as it is, which the server
/// doesn't say not to keep (or check first), unless what's at its path is `immutable`.
pub(super) fn storable(response: &Response, length: Length, immutable: bool) -> bool {
    let mut directives = tokens(response.headers.get("cache-control"));
    if tokens(response.headers.get("pragma"))
        .iter()
        .any(|t| t == "no-cache")
    {
        directives.push("no-cache".into());
    }
    let refused = ["no-store", "no-cache", "private", "max-age=0", "s-maxage=0"];
    response.status == 200
        && length != Length::Close
        && response
            .headers
            .get("content-encoding")
            .is_none_or(|e| e.eq_ignore_ascii_case("identity"))
        && (immutable || !directives.iter().any(|d| refused.contains(&d.as_str())))
}

pub(super) type Upstream = BufReader<StreamOwned<ClientConnection, TcpStream>>;

pub(super) struct Server<'a> {
    pub(super) name: &'a str,
    /// What the bottle asked it with: https, or plain http.
    pub(super) scheme: &'static str,
    pub(super) rule: &'a Cached,
    pub(super) ctx: &'a Context,
    pub(super) reachable: &'a dyn Fn(&str) -> Vec<IpAddr>,
}

impl Server<'_> {
    pub(super) fn log(&self, method: &str, path: &str, outcome: &str, status: u16) {
        let (scheme, name) = (self.scheme, self.name);
        eprintln!("bottle-cli: {method} {scheme}://{name}{path} {outcome} {status}");
    }

    /// Answers one request: whether the connection takes another.
    pub(super) fn answer(
        &self,
        head: &Head,
        bottle: &mut BufReader<Box<dyn Duplex>>,
    ) -> Result<bool, Refused> {
        let (name, scheme) = (self.name, self.scheme);
        // Its target: origin-form, or absolute-form for this host and scheme.
        let (url_scheme, authority, path_and_query) = split_target(&head.target);
        if head.method == "CONNECT"
            || url_scheme.is_some_and(|s| s != scheme)
            || authority.is_some_and(|a| host_of(a) != name.to_lowercase())
        {
            return Err(bad_request("not a request for this host"));
        }
        let headers = head.headers();
        let (path, query) = path_and_query
            .split_once('?')
            .map_or((path_and_query, ""), |(p, q)| (p, q));
        let path = if path.is_empty() { "/" } else { path };
        let mut keep = head.version == "HTTP/1.1"
            && !tokens(headers.get("connection"))
                .iter()
                .any(|t| t == "close");
        if !["GET", "HEAD"].contains(&head.method.as_str())
            || headers.contains_key("transfer-encoding")
            || headers.get("content-length").is_some_and(|n| n != "0")
        {
            self.relay(head, path_and_query, bottle)?;
            return Ok(false);
        }
        let cacheable = query.is_empty()
            && !headers.contains_key("range")
            && !PERSONAL_HEADERS.iter().any(|h| headers.contains_key(*h))
            && !self.rule.excludes(path)
            && self.ctx.store.file(name, path).is_some();
        let fresh_asked = tokens(headers.get("cache-control"))
            .iter()
            .chain(tokens(headers.get("pragma")).iter())
            .any(|t| t == "no-cache");
        if cacheable
            && !fresh_asked
            && let Some(mut file) = self.ctx.store.open(name, path)
        {
            let size = file
                .metadata()
                .map_err(|e| Refused(None, e.to_string()))?
                .len();
            let out = bottle.get_mut();
            let mut head_lines = vec![
                "HTTP/1.1 200 OK".to_string(),
                format!("Content-Length: {size}"),
            ];
            head_lines.push(
                if keep {
                    "Connection: keep-alive"
                } else {
                    "Connection: close"
                }
                .into(),
            );
            let sent = (|| -> io::Result<()> {
                out.write_all((head_lines.join("\r\n") + "\r\n\r\n").as_bytes())?;
                if head.method == "GET" {
                    io::copy(&mut file, out)?;
                }
                out.flush()
            })();
            sent.map_err(|e| Refused(None, format!("the bottle went away: {e}")))?;
            self.log(&head.method, path, "hit", 200);
            return Ok(keep);
        }
        let fetching = cacheable && head.method == "GET";
        let (mut upstream, response) = if fetching {
            let agent = headers.get("user-agent").map_or("", String::as_str);
            self.fetch(path, agent)?
        } else {
            let mut upstream = self.open(name, self.ctx.upstream_port)?;
            let sent = to_server(name, &head.lines);
            let request = upstream_head(&head.method, path_and_query, &sent);
            upstream
                .get_mut()
                .write_all(&request)
                .map_err(|e| bad_gateway(e.to_string()))?;
            let response = read_response(&mut upstream)?;
            (upstream, response)
        };
        let length = body_length(&head.method, &response)?;
        let mut partial = if fetching && storable(&response, length, self.rule.immutable) {
            self.ctx.store.partial(name, path)
        } else {
            None
        };
        let lines = to_client(&response.lines, length == Length::Known(0));
        let out = bottle.get_mut();
        // Framed here: its length when it's known; chunked, on a kept connection, when it isn't;
        // else ended with the connection.
        let chunked = !matches!(length, Length::Known(_)) && keep;
        let mut head_lines = vec![
            format!("HTTP/1.1 {} {}", response.status, response.reason)
                .trim_end()
                .to_string(),
        ];
        head_lines.extend(lines);
        match length {
            Length::Known(n) if n > 0 => head_lines.push(format!("Content-Length: {n}")),
            _ if chunked => head_lines.push("Transfer-Encoding: chunked".into()),
            _ => {}
        }
        keep = keep && (matches!(length, Length::Known(_)) || chunked);
        head_lines.push(
            if keep {
                "Connection: keep-alive"
            } else {
                "Connection: close"
            }
            .into(),
        );
        let mut whole = true;
        let sent = (|| -> io::Result<()> {
            out.write_all((head_lines.join("\r\n") + "\r\n\r\n").as_bytes())?;
            if length != Length::Known(0) {
                let read = body(&mut upstream, length, |data| {
                    // A file that can't be written isn't kept: the bottle still gets it.
                    if partial.as_mut().is_some_and(|p| p.write(data).is_err()) {
                        partial = None;
                    }
                    if chunked {
                        out.write_all(format!("{:x}\r\n", data.len()).as_bytes())?;
                        out.write_all(data)?;
                        out.write_all(b"\r\n")
                    } else {
                        out.write_all(data)
                    }
                });
                if read.is_err() {
                    whole = false;
                }
                read?;
                if chunked {
                    out.write_all(b"0\r\n\r\n")?;
                }
            }
            out.flush()
        })();
        let kept = whole && sent.is_ok() && partial.take().is_some_and(|p| p.commit());
        let outcome = if kept {
            "stored"
        } else if fetching {
            "fetched"
        } else {
            "passed"
        };
        self.log(&head.method, path_and_query, outcome, response.status);
        sent.map_err(|e| Refused(None, format!("serving it: {e}")))?;
        Ok(keep)
    }

    /// The bottle's request as it sent it, its body after, to the host, and the host's answer
    /// back, until either ends: the connection's last.
    pub(super) fn relay(
        &self,
        head: &Head,
        path_and_query: &str,
        bottle: &mut BufReader<Box<dyn Duplex>>,
    ) -> Result<(), Refused> {
        let upstream = self.open(self.name, self.ctx.upstream_port)?;
        let mut upstream = upstream.into_inner();
        let request = upstream_head(
            &head.method,
            path_and_query,
            &to_server(self.name, &head.lines),
        );
        upstream
            .write_all(&request)
            .map_err(|e| bad_gateway(e.to_string()))?;
        // What the bottle sent of its body already, then the rest, then the answer. One thread:
        // the request first, all of it, then the response (a body is what Content-Length or
        // chunked framing says, and the bottle waits for the answer after sending it).
        let headers = head.headers();
        let length = if tokens(headers.get("transfer-encoding"))
            .iter()
            .any(|t| t == "chunked")
        {
            Length::Chunked
        } else {
            Length::Known(
                headers
                    .get("content-length")
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0),
            )
        };
        let forwarded = match length {
            Length::Chunked => {
                // Passed on with its framing, as it was sent.
                let mut raw = Vec::new();
                body(bottle, Length::Chunked, |data| {
                    raw.extend_from_slice(format!("{:x}\r\n", data.len()).as_bytes());
                    raw.extend_from_slice(data);
                    raw.extend_from_slice(b"\r\n");
                    Ok(())
                })
                .and_then(|()| {
                    raw.extend_from_slice(b"0\r\n\r\n");
                    upstream.write_all(&raw)
                })
            }
            _ => body(bottle, length, |data| upstream.write_all(data)),
        };
        forwarded.map_err(|e| Refused(None, format!("relaying a request: {e}")))?;
        let out = bottle.get_mut();
        io::copy(&mut upstream, out)
            .map_err(|e| Refused(None, format!("relaying a response: {e}")))?;
        let _ = out.flush();
        self.log(&head.method, path_and_query, "passed", 0);
        Ok(())
    }

    /// GETs `path` to keep, following redirects: the response, its body still to read. The
    /// request is this one's own, not the bottle's, so the file kept for a path is the same
    /// whoever asked first. A redirect that can't be followed (to plain http, or somewhere that
    /// can't be reached) is the response.
    pub(super) fn fetch(&self, path: &str, agent: &str) -> Result<(Upstream, Response), Refused> {
        let (mut host, mut port, mut target) = (
            self.name.to_string(),
            self.ctx.upstream_port,
            path.to_string(),
        );
        for hop in 0..=MAX_REDIRECTS {
            let mut upstream = self.open(&host, port)?;
            let authority = if port == self.ctx.upstream_port {
                host.clone()
            } else {
                format!("{host}:{port}")
            };
            let request = [
                format!("GET {target} HTTP/1.1"),
                format!("Host: {authority}"),
                format!(
                    "User-Agent: {}",
                    if agent.is_empty() { "bottle" } else { agent }
                ),
                "Accept: */*".into(),
                "Accept-Encoding: identity".into(),
                "Connection: close".into(),
            ]
            .join("\r\n")
                + "\r\n\r\n";
            upstream
                .get_mut()
                .write_all(request.as_bytes())
                .map_err(|e| bad_gateway(e.to_string()))?;
            let response = read_response(&mut upstream)?;
            if !REDIRECTS.contains(&response.status) || hop == MAX_REDIRECTS {
                return Ok((upstream, response));
            }
            let next = response
                .headers
                .get("location")
                .and_then(|l| follow(&host, port, &target, l, self.ctx.upstream_port))
                .filter(|(h, _, _)| !(self.reachable)(h).is_empty());
            let Some((h, p, t)) = next else {
                return Ok((upstream, response));
            };
            (host, port, target) = (h, p, t);
        }
        unreachable!("the last hop is always returned")
    }

    /// A TLS connection to `host`, at the addresses it may be reached at.
    pub(super) fn open(&self, host: &str, port: u16) -> Result<Upstream, Refused> {
        let addresses = (self.reachable)(host);
        if addresses.is_empty() {
            return Err(Refused(
                Some((403, "Forbidden")),
                format!("{host} can't be reached"),
            ));
        }
        let server =
            ServerName::try_from(host.to_string()).map_err(|_| bad_gateway("not a host name"))?;
        for ip in addresses {
            let Ok(tcp) = TcpStream::connect_timeout(&SocketAddr::new(ip, port), CONNECT_TIMEOUT)
            else {
                continue;
            };
            let _ = tcp.set_read_timeout(Some(IDLE));
            let _ = tcp.set_write_timeout(Some(IDLE));
            let _ = tcp.set_nodelay(true);
            let tls = ClientConnection::new(Arc::clone(&self.ctx.client), server.clone())
                .map_err(|e| bad_gateway(e.to_string()))?;
            return Ok(BufReader::new(StreamOwned::new(tls, tcp)));
        }
        Err(bad_gateway(format!("can't connect to {host}")))
    }
}

/// Where a redirect's Location leads from `target` on `host`:`port`, if it's https to be followed.
pub(super) fn follow(
    host: &str,
    port: u16,
    target: &str,
    location: &str,
    https: u16,
) -> Option<(String, u16, String)> {
    let (scheme, authority, path) = if let Some(rest) = location.strip_prefix("//") {
        split_target_with("https", rest)
    } else if location.contains("://") {
        let (scheme, authority, path) = split_target(location);
        (
            scheme?.to_string(),
            authority?.to_string(),
            path.to_string(),
        )
    } else if location.starts_with('/') {
        ("https".into(), String::new(), location.to_string())
    } else {
        // Relative to the target's directory.
        let base = target.split('?').next().unwrap_or("/");
        let dir = &base[..=base.rfind('/').unwrap_or(0)];
        ("https".into(), String::new(), format!("{dir}{location}"))
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    if authority.is_empty() {
        return Some((host.to_string(), port, path));
    }
    let next_host = host_of(&authority);
    let next_port = match authority.rsplit_once(':') {
        Some((_, p)) if !authority.ends_with(']') => p.parse().ok()?,
        _ => https,
    };
    (!next_host.is_empty()).then_some((next_host, next_port, path))
}
