//! Credentials a bottle never holds: folio serve attaches them to its requests to the hosts their
//! features name, on HTTPS connections it makes itself.
//!
//! A feature declares which hosts its credential belongs to (features.rs's doc says how); folio
//! serve reads the credential when a bottle is bound, from its properties file's line for the host,
//! else the Keychain, so it goes no further than here. For those hosts and ports a bottle's machine
//! routes its connections here, bottle-cli completing the TLS handshake with a certificate from the
//! machine's CA, which the bottle trusts for its credentials' and cached hosts alone; folio serve
//! reads each request and sends it on over its own verified TLS connection, to the addresses the
//! host's name leads to (never the one the bottle chose), private ones included but never this
//! machine's own (loopback, link-local, multicast, unspecified).
//!
//! The credential goes only on a request that asks for it: one that presents the stand-in, the fake
//! token a tool in the bottle was given, as the header's token, or as the password of Basic
//! credentials (how git sends what a credential helper gave it), or where a rewrite says (another
//! header, or a JSON body's field). Such a request gets the real one in the same form; any other goes
//! on as the bottle sent it. What the credential may be spent on is said by route: a request on one
//! of forbid's never leaves, with the credential or without; when there are allow routes, the
//! credential goes only on those (and rewrites'), and a request on another that presents the
//! stand-in is refused, saying why, to whoever reads it: often an agent.

use std::collections::BTreeMap;
use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use serde_json::{Value, json};

use super::Bottle;
use super::http::{self, Refused};
use super::proxy::Header;
use crate::auth::{self, STANDIN, destination};
use crate::features::{self, Credential, Rewrite};
use crate::log::log;
use crate::routes::Route;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// The most of a request's body read here, to put the credential in it.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Hop-by-hop headers, for this connection only, never forwarded upstream.
const HOP_HEADERS: [&str; 4] = [
    "proxy-connection",
    "proxy-authorization",
    "connection",
    "keep-alive",
];
/// Headers a proxy or load balancer sets to tell the server where a request came from, what it was
/// for, or which method it is: on a request carrying a credential they could make the server (or
/// one in front of it) route or read it as something else, so they're dropped, as Host is replaced.
const ROUTING_HEADERS: [&str; 17] = [
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
    "x-http-method-override",
    "x-http-method",
    "x-method-override",
];
/// Methods a server answers by echoing the request back, credential included.
const ECHOING_METHODS: [&str; 2] = ["TRACE", "TRACK"];
/// How much of a response is kept, to say its status.
const STATUS_PEEK: usize = 256;
/// How long a carried request waits on either side before it's given up.
const A_MINUTE: Timespec = Timespec {
    tv_sec: 60,
    tv_nsec: 0,
};

/// A credential folio serve attaches on a bottle's behalf, for one host.
#[derive(Debug, Clone)]
pub struct Injection {
    /// A pattern, e.g. "teamcity.corp.example.com" or "*.example.com".
    pub host: String,
    pub port: u16,
    pub header: String,
    /// The whole header value, credential included.
    pub value: String,
    /// What a request presents to be given the credential.
    pub standin: String,
    /// The bare credential, for Basic and rewrites.
    pub credential: String,
    pub rewrites: Vec<Rewrite>,
    pub forbid: Vec<Route>,
    pub allow: Vec<Route>,
    /// Whose credential it is, as a refusal names it.
    pub feature: String,
}

impl Injection {
    pub fn matches(&self, host: &str, port: u16) -> bool {
        port == self.port && fnmatch(&host.to_lowercase(), &self.host.to_lowercase())
    }

    /// Whether a request for `path` (canonical) is never sent to this host.
    fn forbids(&self, method: &str, path: &str) -> bool {
        crate::routes::forbids(&self.forbid, method, path)
    }

    fn named(&self) -> String {
        if self.feature.is_empty() {
            "credential".into()
        } else {
            format!("{} credential", self.feature)
        }
    }

    /// Whether the credential may go on a request for `path` (canonical).
    fn allows(&self, method: &str, path: &str) -> bool {
        let rewritten: Vec<Route> = self.rewrites.iter().map(|w| w.route.clone()).collect();
        crate::routes::allows(&self.allow, &rewritten, method, path)
    }

    /// The value to send in place of `presented`, this injection's header as the bottle sent it;
    /// None to leave it.
    fn replacing(&self, presented: &str) -> Option<String> {
        if self.standin.is_empty() {
            return None;
        }
        let presented = presented.trim();
        let (scheme, token) = presented.split_once(' ').unwrap_or((presented, ""));
        if self.standin == scheme || self.standin == token.trim() {
            return Some(self.value.clone());
        }
        if scheme.eq_ignore_ascii_case("basic")
            && !self.credential.is_empty()
            && self.header.eq_ignore_ascii_case("authorization")
        {
            let decoded = base64_decode(token.trim())?;
            let decoded = String::from_utf8(decoded).ok()?;
            let (user, password) = decoded.split_once(':').unwrap_or((&decoded, ""));
            if password == self.standin {
                return Some(format!(
                    "Basic {}",
                    base64(format!("{user}:{}", self.credential).as_bytes(), true)
                ));
            }
        }
        None
    }
}

/// Whether `rewrite` finds the stand-in in `value`, in its encoding.
fn presents(rewrite: &Rewrite, value: &str, standin: &str) -> bool {
    if standin.is_empty() {
        return false;
    }
    if rewrite.encoding == features::BASE64 {
        value.trim().trim_end_matches('=') == base64(standin.as_bytes(), false)
    } else {
        value.trim() == standin
    }
}

fn encoded(rewrite: &Rewrite, value: &str) -> String {
    if rewrite.encoding == features::BASE64 {
        base64(value.as_bytes(), false)
    } else {
        value.to_string()
    }
}

// --- reading credentials ----------------------------------------------------------

/// What's attached to `host` for `credential`: its properties file's line for the host (by the
/// host, or a URL of it; a host with a port its host:port line, else its bare host's), else the
/// Keychain's (`keychain`).
fn value_for(
    credential: &Credential,
    host: &str,
    keychain: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<String>, String> {
    match auth::from_properties(credential, host)? {
        Some(value) => Ok(Some(value)),
        None => Ok(keychain(&credential.name)),
    }
}

/// What folio serve attaches for a bottle with these features: one per injected credential's host.
/// A credential that isn't logged in is left out (the bottle then reaches the host
/// unauthenticated, and the server says so); two credentials claiming a host is a mistake in the
/// features, refused rather than resolved arbitrarily.
pub fn injections_for(
    specs: &[String],
    keychain: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Injection>, String> {
    let mut credentials = Vec::new();
    for feature in features::resolve(specs)? {
        credentials.extend(feature.resolved_credentials()?);
    }
    injections_of(credentials, keychain)
}

/// What's attached for `credentials`, their values from their properties files or `keychain`.
fn injections_of(
    credentials: Vec<Credential>,
    keychain: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Injection>, String> {
    let mut claimed: BTreeMap<String, String> = BTreeMap::new();
    let mut injections = Vec::new();
    for credential in credentials {
        for host in &credential.inject.hosts {
            let owner = claimed
                .entry(host.to_lowercase())
                .or_insert_with(|| credential.name.clone());
            if *owner != credential.name {
                return Err(format!(
                    "credentials {} and {} both claim {host}",
                    crate::quoted(owner),
                    crate::quoted(&credential.name)
                ));
            }
        }
        for host in &credential.inject.hosts {
            let Some(value) = value_for(&credential, host, keychain)? else {
                continue;
            };
            let (pattern, port) = destination(host)?;
            injections.push(Injection {
                host: pattern,
                port,
                header: credential.inject.header.clone(),
                value: credential
                    .inject
                    .value
                    .replace(features::CREDENTIAL_PLACEHOLDER, &value),
                standin: STANDIN.into(),
                credential: value,
                rewrites: credential.inject.rewrites.clone(),
                forbid: credential.inject.forbid.clone(),
                allow: credential.inject.allow.clone(),
                feature: credential.name.clone(),
            });
        }
    }
    Ok(injections)
}

// --- a flow -----------------------------------------------------------------------

/// How a credential's flow went.
pub struct Served {
    pub to: String,
    pub outcome: String,
    pub status: Option<String>,
    pub up: u64,
    pub down: u64,
    pub injected: bool,
}

/// A request inside a credential's host's TLS, made ready for the host.
struct Request {
    method: String,
    /// What's sent on: its head, and its body if a rewrite read it.
    upstream: Vec<u8>,
    path: String,
    injected: bool,
}

/// Is the far end of `stream`, a flow bottle-cli gave here unwrapped, for `injection`'s host.
pub fn serve(
    bottle: &Bottle,
    injection: &Injection,
    header: &Header,
    stream: UnixStream,
) -> Served {
    let name = header.name.clone().unwrap_or_default();
    let mut served = Served {
        to: "-".into(),
        outcome: "ok".into(),
        status: None,
        up: 0,
        down: 0,
        injected: false,
    };
    let _ = stream.set_read_timeout(Some(http::HEADER_TIMEOUT));
    let Ok(read) = stream.try_clone() else {
        served.outcome = "error".into();
        return served;
    };
    let mut reader = BufReader::new(read);
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(_) => {
            served.outcome = "error".into();
            return served;
        }
    };
    let result = (|| -> Result<(), Refused> {
        let request = inside(
            bottle,
            &name,
            header.port,
            injection,
            &mut reader,
            &mut writer,
        )?;
        served.injected = request.injected;
        let addresses = allowed(&name, header.port)?;
        let (ip, conn, tcp) = open(&name, header.port, &addresses)?;
        served.to = ip.to_string();
        let _ = stream.set_read_timeout(None);
        let mut peek = Vec::new();
        // What the bottle sent past the head, already read: the start of the rest.
        let buffered = reader.buffer().to_vec();
        let (up, down) = relay_tls(&stream, conn, tcp, &request.upstream, &buffered, &mut peek)
            .map_err(|_| Refused {
                status: None,
                reason: String::new(),
                outcome: "error",
                body: None,
                content_type: "",
            })?;
        served.up = up;
        served.down = down;
        served.status = status_of(&peek);
        // Each route, and whether it took the credential: what a feature's allow list is made from.
        log!(
            "bottle.egress",
            "{} {} https://{name}{} {} {}",
            bottle.name,
            request.method,
            request.path,
            if request.injected {
                "credential"
            } else {
                "passed"
            },
            served.status.as_deref().unwrap_or("-")
        );
        Ok(())
    })();
    if let Err(refused) = result {
        served.outcome = refused.outcome.into();
        // Answered in plain http: bottle-cli wraps it for the bottle, which spoke TLS.
        http::respond_refused(&mut writer, &refused);
    }
    served
}

/// The request the bottle sent inside TLS terminated for host `name`, made ready for the host: its
/// head, and its body too when a rewrite reads it for the stand-in.
fn inside(
    bottle: &Bottle,
    name: &str,
    port: u16,
    injection: &Injection,
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
) -> Result<Request, Refused> {
    let head = http::read_head(reader)?;
    let (scheme, authority, path, query) = split_target(&head.target);
    if head.method == "CONNECT"
        || scheme.is_some_and(|s| s != "https")
        || authority
            .as_deref()
            .is_some_and(|a| host_of(a) != name.to_lowercase())
    {
        return Err(Refused::bad_request());
    }
    if ECHOING_METHODS.contains(&head.method.to_uppercase().as_str()) {
        // The response would carry the credential back into the bottle.
        return Err(Refused::new(405, "Method Not Allowed", "denied"));
    }
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };
    let logged = if query.is_empty() {
        path.clone()
    } else {
        format!("{path}?{query}")
    };
    let Some(route) = crate::routes::canonical(&path) else {
        return Err(Refused::bad_request());
    };
    let headers = head.headers();
    let log_request = |outcome: &str| {
        log!(
            "bottle.egress",
            "{} {} https://{name}{logged} {outcome} 403",
            bottle.name,
            head.method
        );
    };
    if injection.forbids(&head.method, &route) {
        log_request("forbidden");
        return Err(route_refused(
            &headers,
            &format!(
                "bottle refused {} {path} on {name}: the {}'s feature forbids this route, with the credential or without.",
                head.method,
                injection.named()
            ),
            "forbidden",
        ));
    }
    let values_of = |wanted: &str| -> Vec<String> {
        head.lines
            .iter()
            .filter(|l| http::name_of(l) == wanted.to_lowercase())
            .map(|l| l.split_once(':').map_or("", |(_, v)| v).to_string())
            .collect()
    };
    // The credential, if the bottle presented the stand-in under this name.
    let value = values_of(&injection.header)
        .iter()
        .find_map(|v| injection.replacing(v));
    // And where else this route's tool presents it.
    let rewrites: Vec<&Rewrite> = if injection.credential.is_empty() {
        vec![]
    } else {
        injection
            .rewrites
            .iter()
            .filter(|r| r.route.matches(&head.method, &route))
            .collect()
    };
    let mut rewritten: BTreeMap<String, String> = BTreeMap::new();
    for r in rewrites.iter().filter(|r| !r.header.is_empty()) {
        if values_of(&r.header)
            .iter()
            .any(|v| presents(r, v, &injection.standin))
        {
            rewritten.insert(
                r.header.to_lowercase(),
                format!("{}: {}", r.header, encoded(r, &injection.credential)),
            );
        }
    }
    let (mut body, mut in_body) = (None, false);
    let fields: Vec<&Rewrite> = rewrites
        .iter()
        .copied()
        .filter(|r| !r.json.is_empty())
        .collect();
    if !fields.is_empty() {
        let read = request_body(reader, writer, &headers)?;
        let (with, did) = json_with_credential(&read, &fields, injection);
        body = Some(with);
        in_body = did;
    }
    if (value.is_some() || !rewritten.is_empty() || in_body)
        && !injection.allows(&head.method, &route)
    {
        log_request("not-allowed");
        return Err(route_refused(
            &headers,
            &format!(
                "bottle refused {} {path} on {name}: the {} goes only on the routes its feature allows, and this isn't one of them.",
                head.method,
                injection.named()
            ),
            "not-allowed",
        ));
    }
    // The Host header goes with the destination, and the routing headers go altogether, so a
    // request carrying a credential can't ask one server to answer as another, or for something
    // else. Nothing else differs between the requests that carry it and those that don't.
    let mut dropped: Vec<String> = ["host"]
        .iter()
        .chain(ROUTING_HEADERS.iter())
        .map(|s| s.to_string())
        .collect();
    dropped.extend(HOP_HEADERS.iter().map(|s| s.to_string()));
    if value.is_some() {
        dropped.push(injection.header.to_lowercase());
    }
    dropped.extend(rewritten.keys().cloned());
    if body.is_some() {
        // Read here, and sent whole.
        dropped.extend(["content-length", "transfer-encoding", "expect"].map(String::from));
    }
    let authority = if port == 443 {
        name.to_string()
    } else {
        format!("{name}:{port}")
    };
    let mut lines = vec![format!("Host: {authority}")];
    lines.extend(
        head.lines
            .iter()
            .filter(|l| !dropped.contains(&http::name_of(l)))
            .cloned(),
    );
    lines.extend(rewritten.values().cloned());
    if let Some(value) = &value {
        lines.push(format!("{}: {value}", injection.header));
    }
    if let Some(body) = &body {
        lines.push(format!("Content-Length: {}", body.len()));
    }
    // A WebSocket (Develocity's test distribution) asks to keep the connection, as something else.
    let upgrade = headers.contains_key("upgrade")
        && http::tokens(headers.get("connection").map_or("", String::as_str))
            .iter()
            .any(|t| t == "upgrade");
    lines.push(format!(
        "Connection: {}",
        if upgrade { "Upgrade" } else { "close" }
    ));
    let mut upstream: Vec<u8> = format!("{} {logged} {}\r\n", head.method, head.version)
        .chars()
        .map(|c| c as u8)
        .collect();
    for line in &lines {
        upstream.extend(line.chars().map(|c| c as u8));
        upstream.extend(b"\r\n");
    }
    upstream.extend(b"\r\n");
    if let Some(body) = &body {
        upstream.extend(body);
    }
    Ok(Request {
        method: head.method.clone(),
        upstream,
        path,
        injected: value.is_some() || !rewritten.is_empty() || in_body,
    })
}

/// A request target's scheme and authority (an absolute form's), path and query.
fn split_target(target: &str) -> (Option<String>, Option<String>, String, String) {
    let (rest, query) = target.split_once('?').map_or((target, ""), |(r, q)| (r, q));
    let query = query.split('#').next().unwrap_or("").to_string();
    if let Some((scheme, after)) = rest.split_once("://")
        && !scheme.contains('/')
    {
        let (authority, path) = after
            .find('/')
            .map_or((after, ""), |i| (&after[..i], &after[i..]));
        return (
            Some(scheme.to_lowercase()),
            Some(authority.to_string()),
            path.to_string(),
            query,
        );
    }
    (None, None, rest.to_string(), query)
}

/// An authority's host, lowercase, without its port or user.
fn host_of(authority: &str) -> String {
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host.split(':').next().unwrap_or_default(),
    };
    host.to_lowercase()
}

/// All of a request's body, by its Content-Length or chunked; refused past MAX_BODY_BYTES, or
/// when it isn't all there. A bottle waiting to be told to send it (100-continue) is told here.
fn request_body(
    reader: &mut BufReader<UnixStream>,
    writer: &mut UnixStream,
    headers: &BTreeMap<String, String>,
) -> Result<Vec<u8>, Refused> {
    if http::tokens(headers.get("expect").map_or("", String::as_str))
        .iter()
        .any(|t| t == "100-continue")
    {
        let _ = writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
    }
    match http::read_body(reader, headers, MAX_BODY_BYTES) {
        Ok(Some(body)) => Ok(body),
        Ok(None) => Err(Refused::new(413, "Content Too Large", "bad-request")),
        Err(_) => Err(Refused::bad_request()),
    }
}

/// `body` with the credential in each rewrite's field that holds the stand-in, and whether one did:
/// only a JSON object's own fields, and only one that is exactly the stand-in.
fn json_with_credential(
    body: &[u8],
    rewrites: &[&Rewrite],
    injection: &Injection,
) -> (Vec<u8>, bool) {
    let Ok(Value::Object(mut document)) = serde_json::from_slice::<Value>(body) else {
        return (body.to_vec(), false);
    };
    let mut any = false;
    for r in rewrites {
        if let Some(Value::String(v)) = document.get(&r.json)
            && presents(r, v, &injection.standin)
        {
            document.insert(r.json.clone(), encoded(r, &injection.credential).into());
            any = true;
        }
    }
    if any {
        (serde_json::to_vec(&Value::Object(document)).unwrap(), true)
    } else {
        (body.to_vec(), false)
    }
}

/// A 403 for a route a credential's feature refuses, saying so to whoever reads it: as JSON when
/// the request asked for it, in the fields API clients show (`message`, as GitHub's errors have it,
/// and Anthropic's `error.message`); else as text, which git shows as the remote's message.
fn route_refused(headers: &BTreeMap<String, String>, why: &str, outcome: &'static str) -> Refused {
    let message = format!(
        "{why} This is the bottle's policy, not an outage or a bug: don't retry the request, or look for another \
         way to make it. If your task needs this route, stop and tell the user which request was refused and why \
         you need it: only they can allow it."
    );
    let wants_json = |h: &str| headers.get(h).is_some_and(|v| v.contains("json"));
    let (body, content_type) = if wants_json("accept") || wants_json("content-type") {
        let body = json!({"type": "error", "error": {"type": "permission_error", "message": message}, "message": message});
        (format!("{body}\n").into_bytes(), "application/json")
    } else {
        (format!("{message}\n").into_bytes(), "text/plain")
    };
    Refused {
        status: Some(403),
        reason: "Forbidden".into(),
        outcome,
        body: Some(body),
        content_type,
    }
}

// --- the host -----------------------------------------------------------------------

/// Whether a credential's host may be reached at `ip`: anywhere but this machine itself and its
/// link. A private address is expected: naming the host is what configuring the credential means.
fn reachable(ip: IpAddr) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback() || v4.is_link_local() || v4.is_multicast() || v4.is_unspecified())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback()
                || v6.is_multicast()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

/// The addresses `host` resolves to that a credential's host may be at; refused if none.
/// $FOLIO_SERVE_RESOLVE (`name=ip:port,...`, the tests') answers for its names, wherever they are.
fn allowed(host: &str, port: u16) -> Result<Vec<SocketAddr>, Refused> {
    if let Ok(given) = std::env::var("FOLIO_SERVE_RESOLVE") {
        let found: Vec<SocketAddr> = given
            .split(',')
            .filter_map(|e| e.split_once('='))
            .filter(|(name, _)| name.eq_ignore_ascii_case(host))
            .filter_map(|(_, at)| at.parse().ok())
            .collect();
        if !found.is_empty() {
            return Ok(found);
        }
    }
    let addresses: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| Refused::new(502, "Bad Gateway: cannot resolve host", "unresolved"))?
        .collect();
    let allowed: Vec<SocketAddr> = addresses
        .into_iter()
        .filter(|a| reachable(a.ip()))
        .collect();
    if allowed.is_empty() {
        return Err(Refused::new(
            403,
            "Forbidden: destination not allowed",
            "denied",
        ));
    }
    Ok(allowed)
}

static TLS: LazyLock<Arc<rustls::ClientConfig>> = LazyLock::new(|| {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    // The tests' own CA, where they say.
    if let Some(pem) = std::env::var_os("FOLIO_SERVE_ROOTS").and_then(|p| std::fs::read(p).ok()) {
        for der in pem_certificates(&pem) {
            let _ = roots.add(rustls::pki_types::CertificateDer::from(der));
        }
    }
    Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
});

/// The certificates in a PEM file, as DER.
fn pem_certificates(pem: &[u8]) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(pem);
    let mut out = Vec::new();
    let mut inside: Option<String> = None;
    for line in text.lines() {
        match (line.trim(), &mut inside) {
            ("-----BEGIN CERTIFICATE-----", _) => inside = Some(String::new()),
            ("-----END CERTIFICATE-----", Some(b64)) => {
                if let Some(der) = base64_decode(b64) {
                    out.push(der);
                }
                inside = None;
            }
            (l, Some(b64)) => b64.push_str(l),
            _ => {}
        }
    }
    out
}

/// A verified TLS connection to `host` at one of `addresses`, already judged, so a second lookup
/// can't swap them: always TLS, as a credential is never put on a request that leaves in the clear,
/// and one without it was TLS when the bottle sent it.
fn open(
    host: &str,
    port: u16,
    addresses: &[SocketAddr],
) -> Result<(IpAddr, rustls::ClientConnection, TcpStream), Refused> {
    let server = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| {
        Refused::new(
            502,
            &format!("Bad Gateway: TLS to {host} failed"),
            "tls-failed",
        )
    })?;
    for address in addresses {
        let address = SocketAddr::new(
            address.ip(),
            if address.port() == 0 {
                port
            } else {
                address.port()
            },
        );
        let Ok(mut tcp) = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) else {
            continue;
        };
        let _ = tcp.set_nodelay(true);
        let _ = tcp.set_read_timeout(Some(CONNECT_TIMEOUT));
        let mut conn =
            rustls::ClientConnection::new(TLS.clone(), server.clone()).map_err(|_| {
                Refused::new(
                    502,
                    &format!("Bad Gateway: TLS to {host} failed"),
                    "tls-failed",
                )
            })?;
        while conn.is_handshaking() {
            if conn.complete_io(&mut tcp).is_err() {
                return Err(Refused::new(
                    502,
                    &format!("Bad Gateway: TLS to {host} failed"),
                    "tls-failed",
                ));
            }
        }
        let _ = tcp.set_read_timeout(None);
        return Ok((address.ip(), conn, tcp));
    }
    Err(Refused::new(502, "Bad Gateway: cannot connect", "failed"))
}

/// Sends `head` (and `buffered`, what the bottle sent past it already read) to the host, then
/// relays both ways until the host is done, in one thread (a TLS connection can't be split): the
/// bottle finishing only says so to the host (it may still be answering); the host finishing ends
/// it all. Keeps the start of the response in `peek`. The bytes each way.
fn relay_tls(
    client: &UnixStream,
    mut conn: rustls::ClientConnection,
    mut tcp: TcpStream,
    head: &[u8],
    buffered: &[u8],
    peek: &mut Vec<u8>,
) -> io::Result<(u64, u64)> {
    let mut client = client.try_clone()?;
    let (mut up, mut down) = (0u64, 0u64);
    send_whole(&mut conn, &mut tcp, head)?;
    send_whole(&mut conn, &mut tcp, buffered)?;
    up += (head.len() + buffered.len()) as u64;
    client.set_nonblocking(true)?;
    tcp.set_nonblocking(true)?;
    let mut client_open = true;
    let mut buf = vec![0; 64 * 1024];
    loop {
        // What's to send to the host, sent.
        while conn.wants_write() {
            match conn.write_tls(&mut tcp) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        // More from the bottle only once what it sent before is gone: rustls keeps at most 64 KiB
        // unsent (taking nothing more past it), and the host may take its time.
        let reading = client_open && !conn.wants_write();
        let host_events = if conn.wants_write() {
            PollFlags::IN | PollFlags::OUT
        } else {
            PollFlags::IN
        };
        // The bottle's side only while reading it: its hangup too is left for then.
        let mut fds = vec![PollFd::new(&tcp, host_events)];
        if reading {
            fds.push(PollFd::new(&client, PollFlags::IN));
        }
        match poll(&mut fds, Some(&A_MINUTE)) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let host = fds[0].revents();
        let bottle = fds.get(1).map_or(PollFlags::empty(), PollFd::revents);
        drop(fds);
        if !bottle.is_empty() {
            // No more than rustls takes at once, its buffer empty.
            match client.read(&mut buf[..16 * 1024]) {
                Ok(0) => {
                    client_open = false;
                    conn.send_close_notify();
                }
                Ok(n) => {
                    send_whole(&mut conn, &mut tcp, &buf[..n])?;
                    up += n as u64;
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => client_open = false,
            }
        }
        if host.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
            let read = match conn.read_tls(&mut tcp) {
                Ok(n) => n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
                Err(_) => 0,
            };
            if read > 0 {
                conn.process_new_packets().map_err(io::Error::other)?;
            }
            let mut ended = read == 0;
            loop {
                match conn.reader().read(&mut buf) {
                    Ok(0) => {
                        ended = true;
                        break;
                    }
                    Ok(n) => {
                        if peek.len() < STATUS_PEEK {
                            let take = n.min(STATUS_PEEK - peek.len());
                            peek.extend_from_slice(&buf[..take]);
                        }
                        client.set_nonblocking(false)?;
                        client.write_all(&buf[..n])?;
                        client.set_nonblocking(true)?;
                        down += n as u64;
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    // The host closed without saying so (no close_notify): it's done all the same.
                    Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                        ended = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            if ended {
                let _ = client.shutdown(std::net::Shutdown::Both);
                return Ok((up, down));
            }
        }
    }
}

/// `data` given to rustls whole for the host: what rustls won't take yet waits for what it has to
/// go to the host first, `tcp` blocking or not.
fn send_whole(
    conn: &mut rustls::ClientConnection,
    tcp: &mut TcpStream,
    mut data: &[u8],
) -> io::Result<()> {
    loop {
        let n = conn.writer().write(data)?;
        data = &data[n..];
        if data.is_empty() {
            return Ok(());
        }
        while conn.wants_write() {
            match conn.write_tls(tcp) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    let mut fds = [PollFd::new(&*tcp, PollFlags::OUT)];
                    if poll(&mut fds, Some(&A_MINUTE)) == Ok(0) {
                        return Err(ErrorKind::TimedOut.into());
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }
}

/// The status of an HTTP response that starts with `response`, e.g. "401"; interim responses (100
/// Continue) skipped for the final one, when it's within what was kept.
fn status_of(response: &[u8]) -> Option<String> {
    let (mut status, mut rest) = (None, response);
    while !rest.is_empty() {
        let line_end = rest
            .windows(2)
            .position(|w| w == b"\r\n")
            .unwrap_or(rest.len());
        let parts: Vec<&[u8]> = rest[..line_end].split(|&b| b == b' ').collect();
        if parts.len() < 2
            || !parts[0].starts_with(b"HTTP/")
            || !parts[1].iter().all(u8::is_ascii_digit)
            || parts[1].is_empty()
        {
            break;
        }
        let code = String::from_utf8_lossy(parts[1]).into_owned();
        let interim = code.starts_with('1');
        status = Some(code);
        let Some(head_end) = rest.windows(4).position(|w| w == b"\r\n\r\n") else {
            break;
        };
        if !interim {
            break;
        }
        rest = &rest[head_end + 4..];
    }
    status
}

// --- small things ---------------------------------------------------------------------

/// Whether `name` matches `pattern`, a shell's glob: `*` anything, `?` one character.
fn fnmatch(name: &str, pattern: &str) -> bool {
    let (n, p): (Vec<char>, Vec<char>) = (name.chars().collect(), pattern.chars().collect());
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while i < n.len() {
        if j < p.len() && (p[j] == '?' || p[j] == n[i]) {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '*' {
            star = Some(j);
            mark = i;
            j += 1;
        } else if let Some(s) = star {
            j = s + 1;
            mark += 1;
            i = mark;
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '*' {
        j += 1;
    }
    j == p.len()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64, padded or not.
fn base64(bytes: &[u8], padded: bool) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else if padded {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64, strictly: None if it isn't.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim_end_matches('=');
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u32, 0);
    for c in text.bytes() {
        let v = B64.iter().position(|&b| b == c)? as u32;
        buffer = (buffer << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn injection() -> Injection {
        Injection {
            host: "*.example.com".into(),
            port: 443,
            header: "Authorization".into(),
            value: "Bearer real".into(),
            standin: STANDIN.into(),
            credential: "real".into(),
            rewrites: vec![],
            forbid: vec![Route::parse("POST /api/auth/token").unwrap()],
            allow: vec![],
            feature: "x".into(),
        }
    }

    #[test]
    fn the_stand_in_is_replaced_as_a_token_or_a_basic_password() {
        let i = injection();
        assert_eq!(
            i.replacing(&format!("Bearer {STANDIN}")).as_deref(),
            Some("Bearer real")
        );
        let basic = base64(format!("x-access-token:{STANDIN}").as_bytes(), true);
        assert_eq!(
            i.replacing(&format!("Basic {basic}")),
            Some(format!("Basic {}", base64(b"x-access-token:real", true)))
        );
        assert_eq!(i.replacing("Bearer something-else"), None);
        assert!(i.matches("API.example.com", 443) && !i.matches("example.org", 443));
    }

    #[test]
    fn a_forbidden_route_is_forbidden_with_an_extension_too() {
        let i = injection();
        assert!(i.forbids("POST", "/api/auth/token"));
        assert!(i.forbids("post", "/api/auth/token.json"));
        assert!(!i.forbids("GET", "/api/auth/token"));
    }

    #[test]
    fn a_responses_status_skips_interim_ones() {
        assert_eq!(
            status_of(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\n\r\n").as_deref(),
            Some("201")
        );
        assert_eq!(
            status_of(b"HTTP/1.1 401 Unauthorized\r\n").as_deref(),
            Some("401")
        );
        assert_eq!(status_of(b"garbage"), None);
    }

    #[test]
    fn base64_both_ways_and_properties() {
        assert_eq!(base64(b"ab", true), "YWI=");
        assert_eq!(base64(b"ab", false), "YWI");
        assert_eq!(base64_decode("YWI=").unwrap(), b"ab");
        let dir = std::env::temp_dir().join(format!("props-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("keys.properties");
        std::fs::write(&file, "# c\nhttps\\://ge.example.com=one\nplain:two\n").unwrap();
        let read = auth::read_properties(file.to_str().unwrap()).unwrap();
        assert_eq!(read["https://ge.example.com"], "one");
        assert_eq!(read["plain"], "two");
        std::fs::remove_dir_all(dir).unwrap();
        assert_eq!(
            destination("a.example.com:8443").unwrap(),
            ("a.example.com".into(), 8443)
        );
        assert!(destination("a:x").is_err());
    }

    fn credentials(specs: &[&str]) -> Vec<Credential> {
        let specs: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        let mut credentials = Vec::new();
        for feature in features::resolve(&specs).unwrap() {
            credentials.extend(feature.resolved_credentials().unwrap());
        }
        credentials
    }

    fn logged_in(value: &'static str) -> impl Fn(&str) -> Option<String> {
        move |_| Some(value.to_string())
    }

    fn logged_out(_: &str) -> Option<String> {
        None
    }

    const TEAMCITY: [&str; 2] = ["teamcity:server=https://ci.corp.example.com", "tools"];

    #[test]
    fn only_the_bottles_features_are_injected() {
        let [claude] =
            &injections_of(credentials(&["claude", "tools"]), &logged_in("tok")).unwrap()[..]
        else {
            panic!("one injection")
        };
        assert_eq!(
            (claude.host.as_str(), claude.value.as_str()),
            ("api.anthropic.com", "Bearer tok")
        );
        assert!(
            injections_of(credentials(&["tools"]), &logged_in("tok"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_proxy_gets_the_real_one_in_place_of_the_stand_in() {
        let [i] = &injections_of(credentials(&TEAMCITY), &logged_in("tc-token")).unwrap()[..]
        else {
            panic!("one injection")
        };
        assert_eq!(
            (i.host.as_str(), i.port, i.header.as_str(), i.value.as_str()),
            (
                "ci.corp.example.com",
                443,
                "Authorization",
                "Bearer tc-token"
            )
        );
        // Given in place of the stand-in only, and bare for Basic credentials.
        assert_eq!(
            (i.standin.as_str(), i.credential.as_str()),
            (STANDIN, "tc-token")
        );
    }

    #[test]
    fn nothing_is_injected_until_it_is_logged_in() {
        assert!(
            injections_of(credentials(&TEAMCITY), &logged_out)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn one_host_belongs_to_one_credential() {
        let mut both = credentials(&TEAMCITY);
        let mut other = both[0].clone();
        other.name = "other".into();
        both.push(other);
        let error = injections_of(both, &logged_in("tok")).err().unwrap();
        assert!(error.contains("both claim ci.corp.example.com"), "{error}");
    }

    #[test]
    fn a_properties_files_line_for_each_server_else_the_keychain() {
        let dir = std::env::temp_dir().join(format!("keys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let keys = dir.join("keys.properties");
        std::fs::write(
            &keys,
            "# written by provisionDevelocityAccessKey\nge.example.com=ge-key\n\
             https://dv.corp.example.com/ = dv-key\nother.example.com\\:8443:port-key\n\
             \n! another comment\nempty.example.com=\n",
        )
        .unwrap();
        let keys = keys.display();
        let values =
            |servers: &str, keychain: &dyn Fn(&str) -> Option<String>| -> Vec<(String, String)> {
                let spec = format!("develocity:server={servers},keyFile={keys}");
                injections_of(credentials(&[&spec]), keychain)
                    .unwrap()
                    .into_iter()
                    .map(|i| (i.host, i.value))
                    .collect()
            };
        let unread = |_: &str| -> Option<String> { panic!("the file has it") };
        let pair = |h: &str, v: &str| (h.to_string(), v.to_string());
        assert_eq!(
            values("https://dv.corp.example.com", &unread),
            [pair("dv.corp.example.com", "Bearer dv-key")]
        );
        assert_eq!(
            values(
                "https://ge.example.com,https://dv.corp.example.com",
                &logged_out
            ),
            [
                pair("ge.example.com", "Bearer ge-key"),
                pair("dv.corp.example.com", "Bearer dv-key")
            ]
        );
        assert_eq!(
            values("other.example.com:8443", &unread),
            [pair("other.example.com", "Bearer port-key")]
        );
        assert_eq!(
            values("https://new.example.com", &logged_in("stored-key")),
            [pair("new.example.com", "Bearer stored-key")]
        );
        assert_eq!(values("https://new.example.com", &logged_out), []);
        // An empty line is no credential: the Keychain's, if any.
        assert_eq!(values("empty.example.com", &logged_out), []);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
