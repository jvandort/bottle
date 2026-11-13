//! The HTTP cache, at the host side of a bottle's network: what a bottle downloads from a host the
//! policy says to cache (policy.rs's Cached) is answered here, from the store where it can be (see
//! store.rs), as bottled answered it (Gateway._serve_cached in folio/folio/bottle/egress.py, whose rules
//! these are), and kept when it's fetched, for the next bottle.
//!
//! Over https, the bottle's TLS is terminated here, with a certificate for the host from the
//! machine's caching CA (see ca.rs), which the bottle trusts; over plain http (an apt repository's,
//! say), its requests are read as they are. Either way, what's fetched is fetched over https,
//! verified against the system's trust store.
//!
//! GETs and HEADs are answered one after another on the same connection. Anything else, or
//! anything with a body, is the connection's last request, relayed as it was sent. What's kept is
//! only what can't depend on who asked: no query, range, credential or cookie; a whole 200 of a
//! file as it is, unless the host says not to keep it (or the entry is immutable, as an apt
//! repository's packages are), fetched by a request of its own, following redirects, so a file is
//! the same whoever asked first. A cached host's cookies are never passed on.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::{
    ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection, StreamOwned,
};

use crate::ca::Authority;
use crate::policy::Cached;
use crate::store::Store;

const MAX_HEADER_BYTES: usize = 64 * 1024;
/// How long a connection waits for its next request, or for anything, before it's closed.
const IDLE: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 5;
const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];
const HOP_HEADERS: [&str; 4] = [
    "proxy-connection",
    "proxy-authorization",
    "connection",
    "keep-alive",
];
/// What a proxy or load balancer sets to tell a server where a request came from: a bottle's
/// client has no reason to send them.
const ROUTING_HEADERS: [&str; 14] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-forwarded-port",
    "x-forwarded-prefix",
    "x-forwarded-server",
    "x-forwarded-scheme",
    "x-forwarded-ssl",
    "x-real-ip",
    "x-original-url",
    "x-rewrite-url",
    "x-original-host",
    "x-host",
];
/// Headers whose request may be answered for whoever sent it alone.
const PERSONAL_HEADERS: [&str; 2] = ["authorization", "cookie"];

/// What serving a cached host needs: the store, the machine's caching CA, the TLS a fetch is
/// verified with, and where a host may be reached.
pub struct Context {
    pub store: Store,
    /// Whose caching CA (a machine's name, or `bottle-builds`): None, and https isn't cached here.
    pub ca: Option<String>,
    authority: Mutex<Option<Arc<Authority>>>,
    servers: Mutex<HashMap<String, Arc<ServerConfig>>>,
    client: Arc<ClientConfig>,
    /// A cached host's https port: what it's fetched from.
    pub upstream_port: u16,
}

impl Context {
    /// The store bottled keeps, `ca`'s CA, and the system's trusted roots.
    pub fn new(ca: Option<String>) -> Context {
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        Context::with(Store::at_home(), ca, roots, crate::policy::HTTPS_PORT)
    }

    pub fn with(
        store: Store,
        ca: Option<String>,
        roots: RootCertStore,
        upstream_port: u16,
    ) -> Context {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut client = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client.alpn_protocols = vec![b"http/1.1".to_vec()];
        Context {
            store,
            ca,
            authority: Mutex::new(None),
            servers: Mutex::new(HashMap::new()),
            client: Arc::new(client),
            upstream_port,
        }
    }

    /// Whether a host's https can be terminated here: its CA is there, and is for it.
    pub fn terminates(&self, host: &str) -> bool {
        self.authority().is_ok_and(|a| a.permits(host))
    }

    fn authority(&self) -> io::Result<Arc<Authority>> {
        let mut loaded = self.authority.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(authority) = &*loaded {
            return Ok(Arc::clone(authority));
        }
        let name = self
            .ca
            .as_deref()
            .ok_or_else(|| io::Error::other("no caching CA"))?;
        let authority = Arc::new(
            Authority::load(name)?
                .ok_or_else(|| io::Error::other(format!("{name} has no caching CA")))?,
        );
        *loaded = Some(Arc::clone(&authority));
        Ok(authority)
    }

    pub(crate) fn server_config(&self, host: &str) -> io::Result<Arc<ServerConfig>> {
        let mut servers = self.servers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(config) = servers.get(host) {
            return Ok(Arc::clone(config));
        }
        let (chain, key) = self.authority()?.mint(host)?;
        let mut config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .map_err(io::Error::other)?;
        // HTTP/1.1 alone: requests are read here.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        servers.insert(host.to_string(), Arc::clone(&config));
        Ok(config)
    }
}

/// A stream whose first bytes were already read (a flow's, for deciding it): they come first.
struct Prefixed<S> {
    prefix: Vec<u8>,
    at: usize,
    inner: S,
}

impl<S: Read> Read for Prefixed<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.at < self.prefix.len() {
            let n = buf.len().min(self.prefix.len() - self.at);
            buf[..n].copy_from_slice(&self.prefix[self.at..self.at + n]);
            self.at += n;
            return Ok(n);
        }
        self.inner.read(buf)
    }
}

impl<S: Write> Write for Prefixed<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

trait Duplex: Read + Write + Send {}
impl<T: Read + Write + Send> Duplex for T {}

/// Answers the requests a bottle sends `name`, a host `rule` says to cache, on `guest`, whose
/// first bytes `first` were read already; over plain http if `plain`. `reachable` gives where a
/// host may be connected to.
pub fn serve(
    guest: UnixStream,
    first: Vec<u8>,
    name: &str,
    rule: &Cached,
    plain: bool,
    ctx: &Context,
    reachable: &dyn Fn(&str) -> Vec<IpAddr>,
) -> io::Result<()> {
    guest.set_read_timeout(Some(IDLE))?;
    // And a bottle that stops reading isn't waited on for ever.
    guest.set_write_timeout(Some(IDLE))?;
    let stream = Prefixed {
        prefix: first,
        at: 0,
        inner: guest,
    };
    let stream: Box<dyn Duplex> = if plain {
        Box::new(stream)
    } else {
        let tls = ServerConnection::new(ctx.server_config(name)?).map_err(io::Error::other)?;
        Box::new(StreamOwned::new(tls, stream))
    };
    let mut bottle = BufReader::new(stream);
    let scheme = if plain { "http" } else { "https" };
    let server = Server {
        name,
        scheme,
        rule,
        ctx,
        reachable,
    };
    loop {
        let head = match read_head(&mut bottle) {
            Ok(Some(head)) => head,
            Ok(None) | Err(Refused(None, _)) => return Ok(()),
            Err(Refused(Some((status, reason)), why)) => {
                eprintln!("bottle-cli: a request to {name} refused: {why}");
                let _ = respond(bottle.get_mut(), status, reason, &[], Some(b""), false);
                return Ok(());
            }
        };
        match server.answer(&head, &mut bottle) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(Refused(Some((status, reason)), why)) => {
                eprintln!(
                    "bottle-cli: {} {scheme}://{name}{} refused: {why}",
                    head.method, head.target
                );
                let body = format!("{status} {reason}: {why}\n");
                let _ = respond(
                    bottle.get_mut(),
                    status,
                    reason,
                    &["Content-Type: text/plain".into()],
                    Some(body.as_bytes()),
                    false,
                );
                return Ok(());
            }
            Err(Refused(None, why)) => {
                eprintln!(
                    "bottle-cli: {} {scheme}://{name}{}: {why}",
                    head.method, head.target
                );
                return Ok(());
            }
        }
    }
}

/// Why a request couldn't be served: what to answer (status, reason), if anything can be.
struct Refused(Option<(u16, &'static str)>, String);

fn bad_request(why: &str) -> Refused {
    Refused(Some((400, "Bad Request")), why.into())
}

fn bad_gateway(why: impl Into<String>) -> Refused {
    Refused(Some((502, "Bad Gateway")), why.into())
}

/// A request's or a response's head: its first line's three parts and its header lines.
struct Head {
    method: String,
    target: String,
    version: String,
    lines: Vec<String>,
}

impl Head {
    fn headers(&self) -> HashMap<String, String> {
        parsed(&self.lines)
    }
}

/// Headers by lowercase name; one sent on several lines is one, its values joined with commas.
fn parsed(lines: &[String]) -> HashMap<String, String> {
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
fn tokens(value: Option<&String>) -> Vec<String> {
    value.map_or(vec![], |v| {
        v.split(',')
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect()
    })
}

/// The next head on `r`: None if it ends first, refused if it isn't HTTP or is too long.
fn read_head(r: &mut impl BufRead) -> Result<Option<Head>, Refused> {
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
enum Length {
    Known(u64),
    Chunked,
    Close,
}

struct Response {
    status: u16,
    reason: String,
    lines: Vec<String>,
    headers: HashMap<String, String>,
}

/// The head of a server's final response: interim ones (100 Continue) are skipped.
fn read_response(r: &mut impl BufRead) -> Result<Response, Refused> {
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

fn body_length(method: &str, response: &Response) -> Result<Length, Refused> {
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

/// Whether the response to a GET can be kept: a whole 200 of a file as it is, which the server
/// doesn't say not to keep (or check first), unless what's at its path is `immutable`.
fn storable(response: &Response, length: Length, immutable: bool) -> bool {
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

/// A body, as it arrives, without its framing, each piece to `each`; an error if it ends early.
fn body(
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
fn respond(
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
fn to_client(lines: &[String], bodiless: bool) -> Vec<String> {
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
fn to_server(name: &str, lines: &[String]) -> Vec<String> {
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

type Upstream = BufReader<StreamOwned<ClientConnection, TcpStream>>;

struct Server<'a> {
    name: &'a str,
    /// What the bottle asked it with: https, or plain http.
    scheme: &'static str,
    rule: &'a Cached,
    ctx: &'a Context,
    reachable: &'a dyn Fn(&str) -> Vec<IpAddr>,
}

impl Server<'_> {
    fn log(&self, method: &str, path: &str, outcome: &str, status: u16) {
        let (scheme, name) = (self.scheme, self.name);
        eprintln!("bottle-cli: {method} {scheme}://{name}{path} {outcome} {status}");
    }

    /// Answers one request: whether the connection takes another.
    fn answer(
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
    fn relay(
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
    fn fetch(&self, path: &str, agent: &str) -> Result<(Upstream, Response), Refused> {
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
    fn open(&self, host: &str, port: u16) -> Result<Upstream, Refused> {
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
            let tls = ClientConnection::new(Arc::clone(&self.ctx.client), server.clone())
                .map_err(|e| bad_gateway(e.to_string()))?;
            return Ok(BufReader::new(StreamOwned::new(tls, tcp)));
        }
        Err(bad_gateway(format!("can't connect to {host}")))
    }
}

fn upstream_head(method: &str, path_and_query: &str, lines: &[String]) -> Vec<u8> {
    // One request per connection: the host closes when it's done.
    let mut head = vec![format!("{method} {path_and_query} HTTP/1.1")];
    head.extend(lines.iter().cloned());
    head.push("Connection: close".into());
    (head.join("\r\n") + "\r\n\r\n").into_bytes()
}

/// A request target's scheme, authority and path with its query: origin-form has only the last.
fn split_target(target: &str) -> (Option<&str>, Option<&str>, &str) {
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
fn host_of(authority: &str) -> String {
    let host = match authority.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    host.to_lowercase()
}

/// Where a redirect's Location leads from `target` on `host`:`port`, if it's https to be followed.
fn follow(
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

fn split_target_with(scheme: &str, rest: &str) -> (String, String, String) {
    let (authority, path) = rest
        .find('/')
        .map_or((rest, "/"), |i| (&rest[..i], &rest[i..]));
    (scheme.into(), authority.into(), path.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ca::{Authority, certificate, in_home};
    use std::fs;
    use std::net::{Ipv4Addr, TcpListener};

    const HOST: &str = "files.example";

    /// A host serving files over https, from a CA of its own: the paths it was asked for, in order.
    fn upstream() -> (u16, RootCertStore, Arc<Mutex<Vec<String>>>) {
        let pem = certificate("upstream", &[HOST.to_string()])
            .unwrap()
            .unwrap();
        let (chain, key) = Authority::load("upstream")
            .unwrap()
            .unwrap()
            .mint(HOST)
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots
            .add(rustls::pki_types::pem::PemObject::from_pem_slice(pem.as_bytes()).unwrap())
            .unwrap();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(chain, key)
                .unwrap(),
        );
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        std::thread::spawn(move || {
            for tcp in listener.incoming() {
                let tls = ServerConnection::new(Arc::clone(&config)).unwrap();
                let mut stream = BufReader::new(StreamOwned::new(tls, tcp.unwrap()));
                let Ok(Some(head)) = read_head(&mut stream) else {
                    continue;
                };
                seen.lock()
                    .unwrap()
                    .push(format!("{} {}", head.method, head.target));
                let headers = head.headers();
                assert_eq!(headers.get("host").map(String::as_str), Some(HOST));
                assert!(!headers.contains_key("x-forwarded-for"));
                let reply: &[u8] = match head.target.as_str() {
                    "/a.jar" | "/a.jar?x=1" => {
                        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nSet-Cookie: s=1\r\n\r\njar"
                    }
                    "/moved" => b"HTTP/1.1 302 Found\r\nLocation: /a.jar\r\nContent-Length: 0\r\n\r\n",
                    "/chunked" => {
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n"
                    }
                    "/private" => {
                        b"HTTP/1.1 200 OK\r\nCache-Control: no-store\r\nContent-Length: 2\r\n\r\nno"
                    }
                    _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
                };
                let _ = stream.get_mut().write_all(reply);
                let _ = stream.get_mut().flush();
                stream.get_mut().conn.send_close_notify();
                let _ = stream.get_mut().flush();
            }
        });
        (port, roots, asked)
    }

    fn rule(immutable: bool) -> Cached {
        Cached {
            host: HOST.into(),
            exclude: vec!["/private".into()],
            include: vec![],
            ports: vec![443, 80],
            immutable,
        }
    }

    fn local(_: &str) -> Vec<IpAddr> {
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
    }

    /// Sends a request on `bottle`: the response's status, its headers and its body.
    fn get(
        bottle: &mut BufReader<Box<dyn Duplex>>,
        request: &str,
    ) -> (u16, HashMap<String, String>, Vec<u8>) {
        bottle.get_mut().write_all(request.as_bytes()).unwrap();
        bottle.get_mut().flush().unwrap();
        let response = read_response(bottle).ok().unwrap();
        let method = request.split(' ').next().unwrap();
        let length = body_length(method, &response).ok().unwrap();
        let mut data = Vec::new();
        body(bottle, length, |d| {
            data.extend_from_slice(d);
            Ok(())
        })
        .unwrap();
        (response.status, response.headers, data)
    }

    /// A bottle's connection to the cached host, served here, over https or plain http.
    fn connect(ctx: Arc<Context>, plain: bool) -> BufReader<Box<dyn Duplex>> {
        let (guest, host) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            serve(host, Vec::new(), HOST, &rule(false), plain, &ctx, &local).unwrap();
        });
        let stream: Box<dyn Duplex> = if plain {
            Box::new(guest)
        } else {
            let pem = certificate("m1", &[HOST.to_string()]).unwrap().unwrap();
            let mut roots = RootCertStore::empty();
            roots
                .add(rustls::pki_types::pem::PemObject::from_pem_slice(pem.as_bytes()).unwrap())
                .unwrap();
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            let tls = ClientConnection::new(Arc::new(config), HOST.try_into().unwrap()).unwrap();
            Box::new(StreamOwned::new(tls, guest))
        };
        BufReader::new(stream)
    }

    fn context(home: &std::path::Path, port: u16, roots: RootCertStore) -> Arc<Context> {
        let store = Store {
            root: home.join("cache/http"),
        };
        Arc::new(Context::with(store, Some("m1".into()), roots, port))
    }

    #[test]
    fn a_file_is_fetched_once_kept_and_answered_from_the_store_after() {
        in_home(|home| {
            let (port, roots, asked) = upstream();
            certificate("m1", &[HOST.to_string()]).unwrap();
            let ctx = context(home, port, roots);
            assert!(ctx.terminates(HOST));
            assert!(!ctx.terminates("other.example"));
            let mut bottle = connect(Arc::clone(&ctx), false);
            let request =
                format!("GET /a.jar HTTP/1.1\r\nHost: {HOST}\r\nX-Forwarded-For: 1.2.3.4\r\n\r\n");
            let (status, headers, data) = get(&mut bottle, &request);
            assert_eq!((status, &data[..]), (200, &b"jar"[..]));
            assert!(!headers.contains_key("set-cookie"));
            // The same connection, again: from the store.
            let (status, _, data) = get(&mut bottle, &request);
            assert_eq!((status, &data[..]), (200, &b"jar"[..]));
            let head = format!("HEAD /a.jar HTTP/1.1\r\nHost: {HOST}\r\n\r\n");
            let (status, headers, data) = get(&mut bottle, &head);
            assert_eq!((status, data.len()), (200, 0));
            assert_eq!(headers.get("content-length").map(String::as_str), Some("3"));
            assert_eq!(asked.lock().unwrap().clone(), ["GET /a.jar"]);
            assert_eq!(
                fs::read(home.join("cache/http/files.example/a.jar")).unwrap(),
                b"jar"
            );

            // A query's is passed on, and not kept; nor is what the host says not to keep, nor
            // what the rule excludes.
            let (_, _, data) = get(
                &mut bottle,
                &format!("GET /a.jar?x=1 HTTP/1.1\r\nHost: {HOST}\r\n\r\n"),
            );
            assert_eq!(data, b"jar");
            let (_, _, data) = get(
                &mut bottle,
                &format!("GET /private HTTP/1.1\r\nHost: {HOST}\r\n\r\n"),
            );
            assert_eq!(data, b"no");
            assert!(!home.join("cache/http/files.example/private").exists());

            // A redirect's followed, and what it led to kept at the path asked for; a body of
            // unknown length is chunked to the bottle.
            let (status, _, data) = get(
                &mut bottle,
                &format!("GET /moved HTTP/1.1\r\nHost: {HOST}\r\n\r\n"),
            );
            assert_eq!((status, &data[..]), (200, &b"jar"[..]));
            assert_eq!(
                fs::read(home.join("cache/http/files.example/moved")).unwrap(),
                b"jar"
            );
            let (status, headers, data) = get(
                &mut bottle,
                &format!("GET /chunked HTTP/1.1\r\nHost: {HOST}\r\n\r\n"),
            );
            assert_eq!((status, &data[..]), (200, &b"abcde"[..]));
            assert_eq!(
                headers.get("transfer-encoding").map(String::as_str),
                Some("chunked")
            );
            assert_eq!(
                fs::read(home.join("cache/http/files.example/chunked")).unwrap(),
                b"abcde"
            );

            // Another host's request on this connection is refused.
            let (status, _, _) = get(
                &mut bottle,
                "GET https://other.example/a.jar HTTP/1.1\r\nHost: other.example\r\n\r\n",
            );
            assert_eq!(status, 400);
        });
    }

    #[test]
    fn plain_http_is_answered_too_and_fetched_over_https() {
        in_home(|home| {
            let (port, roots, asked) = upstream();
            let ctx = context(home, port, roots);
            let mut bottle = connect(ctx, true);
            let request = format!("GET http://{HOST}/a.jar HTTP/1.1\r\nHost: {HOST}\r\n\r\n");
            let (status, _, data) = get(&mut bottle, &request);
            assert_eq!((status, &data[..]), (200, &b"jar"[..]));
            let (_, _, data) = get(&mut bottle, &request);
            assert_eq!(data, b"jar");
            assert_eq!(asked.lock().unwrap().len(), 1);
        });
    }

    #[test]
    fn empty_lines_before_a_head_are_ignored() {
        let mut r = BufReader::new(&b"\r\n\r\nGET / HTTP/1.1\r\nHost: a\r\n\r\n"[..]);
        let head = read_head(&mut r).ok().unwrap().unwrap();
        assert_eq!((head.method.as_str(), head.lines.len()), ("GET", 1));
        assert!(
            read_head(&mut BufReader::new(&b"\r\n"[..]))
                .ok()
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_redirect_is_followed_over_https_only() {
        let at = |l| follow("a.example", 443, "/x/y.jar?q", l, 443);
        assert_eq!(at("/z"), Some(("a.example".into(), 443, "/z".into())));
        assert_eq!(
            at("z.jar"),
            Some(("a.example".into(), 443, "/x/z.jar".into()))
        );
        assert_eq!(
            at("https://B.example:8443/z"),
            Some(("b.example".into(), 8443, "/z".into()))
        );
        assert_eq!(
            at("//c.example/z"),
            Some(("c.example".into(), 443, "/z".into()))
        );
        assert_eq!(at("http://c.example/z"), None);
    }
}

/// Against the internet: `cargo test -p bottle-agent-client -- --ignored`.
#[cfg(test)]
mod online {
    use super::*;
    use crate::ca::{certificate, in_home};

    #[test]
    #[ignore]
    fn debians_release_file_is_fetched_kept_and_answered_from_the_store() {
        in_home(|home| {
            let ctx = Arc::new(Context::new(Some("m1".into())));
            let rule = Cached {
                host: "deb.debian.org".into(),
                exclude: vec![],
                include: vec![],
                ports: vec![443],
                immutable: false,
            };
            let pem = certificate("m1", &["deb.debian.org".to_string()])
                .unwrap()
                .unwrap();
            let path = "/debian/pool/main/h/hello/hello_2.10-3_amd64.deb";
            for _ in 0..2 {
                let (guest, host) = UnixStream::pair().unwrap();
                let (ctx, rule) = (Arc::clone(&ctx), rule.clone());
                let served = std::thread::spawn(move || {
                    let public = |h: &str| {
                        use std::net::ToSocketAddrs;
                        (h, 443).to_socket_addrs().map_or(vec![], |a| {
                            a.map(|a| a.ip()).filter(IpAddr::is_ipv4).collect()
                        })
                    };
                    serve(
                        host,
                        Vec::new(),
                        "deb.debian.org",
                        &rule,
                        false,
                        &ctx,
                        &public,
                    )
                });
                let mut roots = RootCertStore::empty();
                roots
                    .add(rustls::pki_types::pem::PemObject::from_pem_slice(pem.as_bytes()).unwrap())
                    .unwrap();
                let config = ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth();
                let tls =
                    ClientConnection::new(Arc::new(config), "deb.debian.org".try_into().unwrap())
                        .unwrap();
                let mut bottle = StreamOwned::new(tls, guest);
                write!(
                    bottle,
                    "GET {path} HTTP/1.1\r\nHost: deb.debian.org\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                let mut response = Vec::new();
                let _ = bottle.read_to_end(&mut response);
                let head_end = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                assert!(
                    response.starts_with(b"HTTP/1.1 200"),
                    "{}",
                    String::from_utf8_lossy(&response[..head_end])
                );
                // A .deb: an ar archive.
                assert!(response[head_end..].starts_with(b"!<arch>\n"));
                served.join().unwrap().unwrap();
            }
            let kept =
                std::fs::read(home.join(format!("cache/http/deb.debian.org{path}"))).unwrap();
            assert!(kept.starts_with(b"!<arch>\n"));
        });
    }
}
