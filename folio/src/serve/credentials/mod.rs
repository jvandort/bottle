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
use super::service::{Caller, Flow, Service};
use crate::auth::{self, STANDIN, destination};
use crate::features::{self, Credential, Rewrite};
use crate::log::log;
use crate::routes::Route;

/// The credentials service: a bottle's flow to one of its credentials' hosts, over TLS bottle-cli
/// took off, with that credential put in.
pub struct Credentials;

impl Service for Credentials {
    fn serve(&self, caller: &Caller, stream: UnixStream) -> Flow {
        let header = caller.header;
        // A credential goes only on TLS, to a host and port one of the bottle's is for.
        let injection = match (&header.name, header.tls) {
            (Some(name), true) => caller
                .bottle
                .injections
                .iter()
                .find(|i| i.matches(name, header.port)),
            _ => None,
        };
        match injection {
            Some(injection) => serve(caller.bottle, injection, header, stream),
            None => Flow::new("-", "not-a-service"),
        }
    }
}

mod encoding;
mod flow;
mod upstream;

use encoding::*;
pub use flow::*;
use upstream::*;

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
