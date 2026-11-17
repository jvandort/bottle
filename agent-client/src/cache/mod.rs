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

use http::{Refused, read_head, respond};
use server::Server;

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::IpAddr;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::{ClientConfig, RootCertStore, ServerConfig, ServerConnection, StreamOwned};

use crate::ca::Authority;
use crate::policy::Cached;
use crate::store::Store;

mod http;
mod server;

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

#[cfg(test)]
mod tests {
    use super::http::{body, body_length, read_head, read_response};
    use super::server::follow;
    use super::*;
    use crate::ca::{Authority, certificate, in_home};
    use rustls::ClientConnection;
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

    /// What's kept at `path`, once it is: a file's kept after the bottle has had all of it.
    fn kept(path: &std::path::Path) -> Vec<u8> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match fs::read(path) {
                Ok(data) => return data,
                Err(e) if std::time::Instant::now() > deadline => panic!("{}: {e}", path.display()),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
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
            assert_eq!(kept(&home.join("cache/http/files.example/a.jar")), b"jar");

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
            assert_eq!(kept(&home.join("cache/http/files.example/moved")), b"jar");
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
                kept(&home.join("cache/http/files.example/chunked")),
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
    use rustls::ClientConnection;

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
