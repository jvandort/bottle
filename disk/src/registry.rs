//! Images from a registry, by curl (the system's, with the system's trust, proxies and all): a
//! reference resolved to the manifest for this machine's architecture, its config, and each layer
//! as a stream, straight from the registry, stored nowhere (import checks it as it reads).
//!
//! Manifests and configs are kept by digest, in the store's `registry/`: a reference by digest
//! (`name@sha256:...`) whose stage is made already asks the registry nothing. A tag is asked
//! about each time: it's the registry that says what it's now.
//!
//! Tokens are as registries hand them out to anyone (Docker Hub's, for its public images): no
//! credentials. They're given to curl on its stdin, never its arguments, which anyone can read.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde::Deserialize;

use crate::oci::{self, Image, Layer, sha256};
use crate::other;

const INDEX_TYPES: [&str; 2] = [
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
];
const MANIFEST_TYPES: [&str; 2] = [
    "application/vnd.oci.image.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v2+json",
];

/// Where an image is: `debian:trixie-slim`, `docker.io/library/debian@sha256:...`,
/// `ghcr.io/owner/name:tag`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    /// The registry's host, as its API is reached.
    pub registry: String,
    pub repository: String,
    /// A tag, or a digest (`sha256:...`).
    pub reference: String,
}

impl Reference {
    pub fn parse(s: &str) -> io::Result<Reference> {
        let bad = || other(format!("{s:?}: not an image reference"));
        let (rest, digest) = match s.split_once('@') {
            Some((rest, digest)) if digest.starts_with("sha256:") => (rest, Some(digest)),
            Some(_) => return Err(bad()),
            None => (s, None),
        };
        // A first component with a dot or a colon, or "localhost", is a registry.
        let (registry, path) = match rest.split_once('/') {
            Some((first, path)) if first.contains(['.', ':']) || first == "localhost" => {
                (first.to_string(), path.to_string())
            }
            _ => ("docker.io".to_string(), rest.to_string()),
        };
        let (path, tag) = match path.rsplit_once(':') {
            Some((path, tag)) if !tag.contains('/') => (path.to_string(), Some(tag.to_string())),
            _ => (path, None),
        };
        if path.is_empty() {
            return Err(bad());
        }
        let docker = registry == "docker.io";
        let repository = if docker && !path.contains('/') {
            format!("library/{path}")
        } else {
            path
        };
        Ok(Reference {
            registry: if docker {
                "registry-1.docker.io".into()
            } else {
                registry
            },
            repository,
            reference: match digest {
                Some(d) => d.to_string(),
                None => tag.unwrap_or_else(|| "latest".into()),
            },
        })
    }

    fn by_digest(&self) -> Option<&str> {
        self.reference
            .starts_with("sha256:")
            .then_some(self.reference.as_str())
    }

    fn url(&self, kind: &str, what: &str) -> String {
        format!(
            "https://{}/v2/{}/{kind}/{what}",
            self.registry, self.repository
        )
    }
}

/// A registry, reached for one repository: the token it gave, once it's asked for one.
pub struct Registry {
    reference: Reference,
    token: Option<String>,
    /// Where manifests and configs are kept, by digest.
    cache: PathBuf,
}

/// What curl got: the status, the headers' lines, the body.
struct Fetched {
    status: u32,
    headers: String,
    body: Vec<u8>,
}

impl Registry {
    pub fn new(reference: Reference, cache: &Path) -> Registry {
        Registry {
            reference,
            token: None,
            cache: cache.to_path_buf(),
        }
    }

    /// The image the reference is, for this machine's architecture.
    pub fn image(&mut self) -> io::Result<Image> {
        let reference = self.reference.reference.clone();
        let accept: Vec<&str> = INDEX_TYPES.iter().chain(&MANIFEST_TYPES).copied().collect();
        let (top, media_type) = match self.reference.by_digest().map(String::from) {
            Some(digest) => self.document(&digest, &accept)?,
            None => self.fetch_document(&reference, &accept, None)?,
        };
        let manifest = if INDEX_TYPES.contains(&media_type.as_str()) {
            let digest = platform_manifest(&top)?;
            self.document(&digest, &MANIFEST_TYPES)?.0
        } else {
            top
        };
        let config = serde_json::from_slice::<ManifestConfig>(&manifest)
            .map_err(|e| other(format!("the manifest: {e}")))?
            .config
            .digest;
        let config = self.blob_document(&config)?;
        oci::image_of(&manifest, &config)
    }

    /// The layer's tar, uncompressed, straight from the registry: its end is an error if it isn't
    /// what the layer's diff_id says.
    pub fn layer(&self, layer: &Layer) -> io::Result<Box<dyn Read + Send>> {
        let url = self.reference.url("blobs", &layer.digest);
        let child = self.curl(&url, &[], true)?;
        oci::decode(Download { child, done: false }, layer)
    }

    /// The manifest `digest` (or index), kept, or fetched and kept.
    fn document(&mut self, digest: &str, accept: &[&str]) -> io::Result<(Vec<u8>, String)> {
        let kept = self.kept(digest);
        if let Ok(body) = fs::read(&kept) {
            let media_type = media_type_of(&body);
            return Ok((body, media_type));
        }
        self.fetch_document(digest, accept, Some(digest))
    }

    fn fetch_document(
        &mut self,
        what: &str,
        accept: &[&str],
        digest: Option<&str>,
    ) -> io::Result<(Vec<u8>, String)> {
        let url = self.reference.url("manifests", what);
        let fetched = self.fetch(&url, &[format!("Accept: {}", accept.join(", "))])?;
        if fetched.status != 200 {
            return Err(other(format!("{url}: HTTP {}", fetched.status)));
        }
        let media_type = header(&fetched.headers, "content-type")
            .map(|t| t.split(';').next().unwrap_or("").trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| media_type_of(&fetched.body));
        let got = sha256(&fetched.body);
        if let Some(digest) = digest
            && got != digest
        {
            return Err(other(format!("{url}: {got}, not {digest}")));
        }
        self.keep(&got, &fetched.body)?;
        Ok((fetched.body, media_type))
    }

    /// A small blob (a config), kept, or fetched, checked and kept.
    fn blob_document(&mut self, digest: &str) -> io::Result<Vec<u8>> {
        if let Ok(body) = fs::read(self.kept(digest)) {
            return Ok(body);
        }
        let url = self.reference.url("blobs", digest);
        let fetched = self.fetch(&url, &[])?;
        if fetched.status != 200 {
            return Err(other(format!("{url}: HTTP {}", fetched.status)));
        }
        let got = sha256(&fetched.body);
        if got != digest {
            return Err(other(format!("{url}: {got}, not {digest}")));
        }
        self.keep(digest, &fetched.body)?;
        Ok(fetched.body)
    }

    fn kept(&self, digest: &str) -> PathBuf {
        self.cache.join(digest.replace(':', "-"))
    }

    fn keep(&self, digest: &str, body: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.cache)?;
        let path = self.kept(digest);
        let partial = path.with_extension(format!("partial.{}", std::process::id()));
        fs::write(&partial, body)?;
        fs::rename(&partial, &path)
    }

    /// GETs `url`, with a token if the registry asks for one (once: then it's kept).
    fn fetch(&mut self, url: &str, headers: &[String]) -> io::Result<Fetched> {
        let fetched = self.fetch_once(url, headers)?;
        if fetched.status != 401 || self.token.is_some() {
            return Ok(fetched);
        }
        let challenge = header(&fetched.headers, "www-authenticate")
            .ok_or_else(|| other(format!("{url}: 401, and no way to authenticate")))?;
        self.token = Some(self.token_for(&challenge)?);
        self.fetch_once(url, headers)
    }

    fn fetch_once(&self, url: &str, headers: &[String]) -> io::Result<Fetched> {
        let dir = std::env::temp_dir().join(format!("bottle-pull-{}", std::process::id()));
        fs::create_dir_all(&dir)?;
        let header_file = dir.join("headers");
        let mut args = vec!["-D".to_string(), header_file.display().to_string()];
        args.extend(["-w".into(), "%{http_code}".into(), "-o".into()]);
        let body_file = dir.join("body");
        args.push(body_file.display().to_string());
        let out = self.curl_output(url, headers, &args)?;
        let status = String::from_utf8_lossy(&out).trim().parse().unwrap_or(0);
        let headers = fs::read_to_string(&header_file).unwrap_or_default();
        let body = fs::read(&body_file).unwrap_or_default();
        let _ = fs::remove_dir_all(&dir);
        Ok(Fetched {
            status,
            headers,
            body,
        })
    }

    /// A token from the realm a `WWW-Authenticate: Bearer realm=...,service=...,scope=...` names.
    fn token_for(&self, challenge: &str) -> io::Result<String> {
        let params = bearer_params(challenge)
            .ok_or_else(|| other(format!("can't authenticate as {challenge:?} asks")))?;
        let realm = params
            .iter()
            .find(|(k, _)| k == "realm")
            .map(|(_, v)| v.clone())
            .ok_or_else(|| other("a challenge without a realm".into()))?;
        let query: Vec<String> = params
            .iter()
            .filter(|(k, _)| k != "realm")
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect();
        let url = if query.is_empty() {
            realm
        } else {
            format!("{realm}?{}", query.join("&"))
        };
        let out = Command::new("curl")
            .args(["-fsSL", &url])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| other(format!("curl: {e}")))?;
        if !out.status.success() {
            return Err(other(format!(
                "{url}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        #[derive(Deserialize)]
        struct Token {
            token: Option<String>,
            access_token: Option<String>,
        }
        let t: Token =
            serde_json::from_slice(&out.stdout).map_err(|e| other(format!("{url}: {e}")))?;
        t.token
            .or(t.access_token)
            .ok_or_else(|| other(format!("{url}: no token")))
    }

    /// curl for `url`, the token and `headers` on its stdin, as its config: its output (stdout).
    fn curl_output(&self, url: &str, headers: &[String], args: &[String]) -> io::Result<Vec<u8>> {
        let mut child = Command::new("curl")
            .args(["-sSL", "-K", "-"])
            .args(args)
            .arg(url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| other(format!("curl: {e}")))?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(self.config(headers).as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(other(format!(
                "{url}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(out.stdout)
    }

    /// curl for `url`, streaming its body on stdout; failing (-f) on an HTTP error.
    fn curl(&self, url: &str, headers: &[String], fail: bool) -> io::Result<Child> {
        let mut child = Command::new("curl")
            .args(["-sSL", "-K", "-"])
            .args(fail.then_some("-f"))
            .arg(url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| other(format!("curl: {e}")))?;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(self.config(headers).as_bytes())?;
        drop(stdin);
        Ok(child)
    }

    /// curl's config: the token, and `headers`.
    fn config(&self, headers: &[String]) -> String {
        let mut config = String::new();
        let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        if let Some(token) = &self.token {
            config += &format!("header = \"Authorization: Bearer {}\"\n", quote(token));
        }
        for h in headers {
            config += &format!("header = \"{}\"\n", quote(h));
        }
        config
    }
}

/// A blob, as curl streams it; its end is an error if curl failed.
struct Download {
    child: Child,
    done: bool,
}

impl Read for Download {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.child.stdout.as_mut().unwrap().read(buf)?;
        if n == 0 && !buf.is_empty() && !self.done {
            self.done = true;
            let status = self.child.wait()?;
            if !status.success() {
                let mut said = String::new();
                if let Some(mut stderr) = self.child.stderr.take() {
                    let _ = stderr.read_to_string(&mut said);
                }
                return Err(other(format!("downloading a layer: {}", said.trim())));
            }
        }
        Ok(n)
    }
}

impl Drop for Download {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Deserialize)]
struct ManifestConfig {
    config: Digested,
}

#[derive(Deserialize)]
struct Digested {
    digest: String,
}

/// The manifest, in an index, for this machine's architecture, on Linux.
fn platform_manifest(index: &[u8]) -> io::Result<String> {
    #[derive(Deserialize)]
    struct Index {
        manifests: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        digest: String,
        platform: Option<Platform>,
    }
    #[derive(Deserialize)]
    struct Platform {
        os: String,
        architecture: String,
    }
    let arch = match std::env::consts::ARCH {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        other => other,
    };
    let index: Index =
        serde_json::from_slice(index).map_err(|e| other(format!("the image's index: {e}")))?;
    index
        .manifests
        .into_iter()
        .find(|m| {
            m.platform
                .as_ref()
                .is_some_and(|p| p.os == "linux" && p.architecture == arch)
        })
        .map(|m| m.digest)
        .ok_or_else(|| other(format!("the image has no linux/{arch} manifest")))
}

/// A document's mediaType, as it says it.
fn media_type_of(body: &[u8]) -> String {
    #[derive(Deserialize)]
    struct Typed {
        #[serde(rename = "mediaType")]
        media_type: Option<String>,
    }
    serde_json::from_slice::<Typed>(body)
        .ok()
        .and_then(|t| t.media_type)
        .unwrap_or_default()
}

/// The last value of header `name` in curl's headers' lines (redirects make several responses).
fn header(headers: &str, name: &str) -> Option<String> {
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .filter(|(k, _)| k.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
        .next_back()
}

/// The parameters of a `Bearer k="v",k2="v2"` challenge.
fn bearer_params(challenge: &str) -> Option<Vec<(String, String)>> {
    let rest = challenge.trim().strip_prefix("Bearer ")?;
    let mut params = Vec::new();
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let (key, after) = rest.split_once('=')?;
        let after = after.strip_prefix('"')?;
        let (value, after) = after.split_once('"')?;
        params.push((key.trim().to_string(), value.to_string()));
        rest = after.trim_start_matches(',').trim();
    }
    Some(params)
}

/// `s`, for a URL's query.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> (String, String, String) {
        let r = Reference::parse(s).unwrap();
        (r.registry, r.repository, r.reference)
    }

    #[test]
    fn references_read_as_docker_reads_them() {
        let hub = "registry-1.docker.io".to_string();
        assert_eq!(
            r("debian"),
            (hub.clone(), "library/debian".into(), "latest".into())
        );
        assert_eq!(
            r("debian:trixie-slim"),
            (hub.clone(), "library/debian".into(), "trixie-slim".into())
        );
        assert_eq!(
            r("docker.io/library/debian@sha256:abc"),
            (hub.clone(), "library/debian".into(), "sha256:abc".into())
        );
        assert_eq!(r("owner/name:1"), (hub, "owner/name".into(), "1".into()));
        assert_eq!(
            r("ghcr.io/owner/name:tag"),
            ("ghcr.io".into(), "owner/name".into(), "tag".into())
        );
        assert_eq!(
            r("localhost:5000/x"),
            ("localhost:5000".into(), "x".into(), "latest".into())
        );
        assert!(Reference::parse("x@md5:abc").is_err());
    }

    #[test]
    fn challenges_give_their_parameters() {
        let p = bearer_params(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/debian:pull""#,
        )
        .unwrap();
        assert_eq!(
            p,
            [
                (
                    "realm".to_string(),
                    "https://auth.docker.io/token".to_string()
                ),
                ("service".into(), "registry.docker.io".into()),
                ("scope".into(), "repository:library/debian:pull".into()),
            ]
        );
        assert_eq!(
            encode("repository:library/debian:pull"),
            "repository%3Alibrary%2Fdebian%3Apull"
        );
        assert!(bearer_params("Basic realm=\"x\"").is_none());
    }

    #[test]
    fn the_platforms_manifest_is_picked_from_an_index() {
        let index = br#"{"manifests": [
            {"digest": "sha256:amd", "platform": {"os": "linux", "architecture": "amd64"}},
            {"digest": "sha256:arm", "platform": {"os": "linux", "architecture": "arm64", "variant": "v8"}},
            {"digest": "sha256:att", "platform": {"os": "unknown", "architecture": "unknown"}}
        ]}"#;
        let want = if std::env::consts::ARCH == "aarch64" {
            "sha256:arm"
        } else {
            "sha256:amd"
        };
        assert_eq!(platform_manifest(index).unwrap(), want);
    }

    #[test]
    fn the_last_response_says_the_header() {
        let h = "HTTP/1.1 307\r\nLocation: x\r\nContent-Type: a\r\n\r\nHTTP/2 200\r\ncontent-type: b\r\n";
        assert_eq!(header(h, "Content-Type").as_deref(), Some("b"));
    }
}
